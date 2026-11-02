use clap::{Parser, Subcommand, ValueEnum};
use runner::checkpoint;
use runner::claude::{self, Claude};
use runner::creds::Cred;
use runner::doctor::{self, System};
use runner::exec;
use runner::idle::Idle;
use runner::proto::{Exit, Request};
use runner::runs::{self, AgentRun};
use runner::services::{self, InVm};
use runner::sessions::{self, Session};
use runner::store::{BASE, Config, KEEP, Store, Vm, parse_mem};
use runner::systemd::{self, VmStatus};
use runner::url;
use std::error::Error;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// Firecracker microVMs for AI agents. Runs on the KVM host, usually as
/// `ssh -t host runner ...`; `runner mcp` serves the same commands to Claude Code.
#[derive(Parser)]
#[command(
    name = "runner",
    version,
    after_help = "On the laptop, `runner proxy <vm> <port>` forwards a VM's port to localhost (README.md, Ports)."
)]
struct Cli {
    /// Where VMs live.
    #[arg(long, env = "RUNNER_ROOT", default_value = "/var/lib/runner")]
    root: PathBuf,
    /// How many disk generations gc keeps besides the current one and the
    /// checkpoints, at every commit and in `gc`.
    #[arg(long, env = "RUNNER_KEEP", default_value_t = KEEP, global = true)]
    keep: usize,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Bring an ext4 disk in as a new VM, usually `base`.
    Import { name: String, disk: PathBuf },
    /// Make a VM whose disk is a copy-on-write fork of another's.
    Create {
        name: String,
        /// The VM whose current disk to fork.
        #[arg(long, default_value = BASE)]
        from: String,
        /// vCPUs: 1 or an even number up to 32. Defaults to that VM's, else 2.
        #[arg(long)]
        cpus: Option<u32>,
        /// Memory, like 4G or 4096 (MiB). Defaults to that VM's, else 2G.
        #[arg(long)]
        mem: Option<String>,
        /// A kernel for this VM, in place of the host's <root>/kernel/vmlinux.
        #[arg(long)]
        kernel: Option<PathBuf>,
        /// How long it goes unused before it sleeps: minutes, or with s, m
        /// or h, like 90s or 2h, or never. Defaults to that VM's, else 10.
        #[arg(long)]
        idle: Option<Idle>,
    },
    /// List VMs, their current disk generation and the space their disks use.
    List,
    /// Boot a VM under systemd, or wake it if it's sleeping. Returns once its
    /// guest has started.
    Start { name: String },
    /// Shut a VM down: Ctrl-Alt-Del, and a kill after 30 seconds. A sleeping
    /// VM is woken first, so it shuts down cleanly.
    Stop { name: String },
    /// Say whether a VM is stopped, booting, running, sleeping or stopping.
    Status { name: String },
    /// Put a running VM to sleep: its memory goes to a snapshot on the host
    /// and its Firecracker stops, so it takes no CPU or RAM. `wake`, `start`
    /// or `exec` brings it back in about a second, every process as it was.
    /// Open shells and execs end; tmux sessions live on. Refused while a run
    /// is going.
    Sleep { name: String },
    /// Bring a sleeping VM back where it was, and set its clock.
    Wake { name: String },
    /// Show or set how long a VM goes unused before it sleeps. Use is a
    /// shell, exec, attached session, run or MCP call on it; a detached tmux
    /// session doesn't count, and sleeps with the VM. Anything that needs the
    /// VM wakes it. A running VM counts from the next look, every 10 seconds.
    Idle {
        name: String,
        /// Minutes, or with s, m or h, like 90s or 2h, or never. Ten minutes
        /// until set.
        idle: Option<Idle>,
    },
    /// Print a VM's URL on the tailnet, http://<host>:<port>, with a port of
    /// its own that goes to port 8080 in the VM. It answers while the VM is
    /// up, and a connection to a sleeping VM wakes it. Only the tailnet can
    /// reach it. `off` turns it off, `on` back on.
    Url {
        name: String,
        switch: Option<Switch>,
        /// The port in the VM it goes to, from now on.
        #[arg(long)]
        port: Option<u16>,
    },
    /// Show a VM's serial console and boot notes, from journald.
    Logs {
        name: String,
        /// Keep printing as lines come.
        #[arg(short, long)]
        follow: bool,
        /// Only the last N lines.
        #[arg(short = 'n', long)]
        lines: Option<u32>,
    },
    /// Roll a VM back to its previous committed disk generation.
    Undo { name: String },
    /// Remove a VM and all its disk generations.
    Delete { name: String },
    /// Delete old disk generations: all but the current one, every checkpoint
    /// and the newest --keep others. Every commit does this too.
    Gc {
        /// Only this VM. Every VM if left out.
        name: Option<String>,
        /// Print what would be freed, and free nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Freeze a copy of a VM's disk to restore later. A running VM is synced
    /// and paused for the copy, and carries on; a sleeping one is woken first.
    Checkpoint {
        name: String,
        /// A note to know it by.
        #[arg(short = 'm', long)]
        comment: Option<String>,
    },
    /// List a VM's checkpoints, and the generations runs made, newest first.
    /// `*` marks the current one.
    Checkpoints { name: String },
    /// Make a copy of a checkpoint the VM's current disk. `undo` reverses it.
    /// A running or sleeping VM is stopped first and started again after,
    /// awake; a stopping one is waited out and stays stopped.
    Restore { name: String, id: u64 },
    /// Print the firecracker@.service unit for this binary and root.
    Unit {
        /// The user VMs run as, who owns the root.
        #[arg(long, env = "USER")]
        user: String,
    },
    /// Show or set which credentials a VM gets at boot: claude, github.
    /// A VM gets claude until told otherwise.
    Creds {
        name: String,
        /// Give it exactly these from the next boot on.
        creds: Vec<Cred>,
        /// Give it none from the next boot on.
        #[arg(long, conflicts_with = "creds")]
        none: bool,
    },
    /// Print a VM's address, which stays the same for as long as the VM exists.
    Ip { name: String },
    /// Open a login shell in a VM, starting the VM first if it's stopped. It
    /// gets a terminal of its own when stdin is one, as with ssh.
    Shell {
        name: String,
        /// The user to log in as.
        #[arg(long, env = "RUNNER_USER", default_value = exec::USER)]
        user: String,
        /// Attach to this tmux session instead, making it if it's new. Detach
        /// with tmux's prefix key and d; a dropped connection detaches too.
        #[arg(short, long, allow_hyphen_values = true)]
        session: Option<String>,
    },
    /// List a VM's tmux sessions, waking it if it's sleeping: name, the
    /// command in its active pane, when it started, and whether a terminal is
    /// on it. A stopped VM has none, and isn't started to look.
    Sessions {
        name: String,
        /// Whose sessions.
        #[arg(long, env = "RUNNER_USER", default_value = exec::USER)]
        user: String,
    },
    /// Attach this terminal to a tmux session in a VM that's up, waking it if
    /// it's sleeping.
    Attach {
        name: String,
        #[arg(allow_hyphen_values = true)]
        session: String,
        #[arg(long, env = "RUNNER_USER", default_value = exec::USER)]
        user: String,
    },
    /// End a tmux session in a VM, and what runs in it.
    Kill {
        name: String,
        #[arg(allow_hyphen_values = true)]
        session: String,
        #[arg(long, env = "RUNNER_USER", default_value = exec::USER)]
        user: String,
    },
    /// Services: commands systemd keeps running in a VM, from every boot on.
    Service {
        #[command(subcommand)]
        cmd: ServiceCmd,
    },
    /// Claude Code in a VM, on this terminal. Makes the VM from base if it's
    /// new and starts it if it's stopped, then attaches to its tmux session
    /// named claude, starting claude there if it isn't running. Detach with
    /// tmux's prefix key and d; a dropped connection detaches too.
    Claude {
        name: String,
        /// The VM a new one forks.
        #[arg(long)]
        from: Option<String>,
        /// Where a new session starts claude.
        #[arg(short, long, default_value = claude::DIR)]
        dir: String,
        /// The user claude runs as.
        #[arg(long, env = "RUNNER_USER", default_value = exec::USER)]
        user: String,
        /// Arguments for claude, after `--`, like --continue.
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// Run a command in a VM, starting the VM first if it's stopped. Stdin
    /// passes through, and it exits with the command's exit code, or 128
    /// plus the signal that killed it. 255 means it couldn't run.
    Exec {
        name: String,
        /// Run it on a terminal of its own, as `ssh -t` does.
        #[arg(short, long)]
        tty: bool,
        /// Set a variable for it. Repeat for more.
        #[arg(short, long = "env", value_name = "K=V", value_parser = parse_env)]
        env: Vec<(String, String)>,
        /// The directory to run it in. Defaults to the user's home.
        #[arg(short, long)]
        dir: Option<String>,
        /// The user to run it as.
        #[arg(long, env = "RUNNER_USER", default_value = exec::USER)]
        user: String,
        /// The program and its arguments, after `--`.
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        argv: Vec<String>,
    },
    /// Start Claude Code on a prompt in a VM, headless and detached, and
    /// print the run's id at once. A stopped VM is booted for the run and
    /// shut down after it: exit 0 commits what it wrote to its disk, and
    /// anything else rolls that back. A running VM is checkpointed and joined
    /// as it is: exit 0 leaves it running, and anything else restores that
    /// checkpoint, which restarts the VM. One run per VM at a time.
    Run {
        name: String,
        /// What to ask Claude Code. It reaches claude on its stdin.
        #[arg(allow_hyphen_values = true)]
        prompt: String,
        /// Where Claude Code runs. It has to exist.
        #[arg(short, long, default_value = claude::DIR)]
        dir: String,
        /// Carry on this Claude Code session: the session_id in an earlier
        /// run's init event.
        #[arg(long)]
        resume: Option<String>,
        /// The user Claude Code runs as.
        #[arg(long, env = "RUNNER_USER", default_value = exec::USER)]
        user: String,
    },
    /// List a VM's runs, newest first: id, when it started, how it stands,
    /// and its prompt.
    Runs { name: String },
    /// Print a run's stream-json, an event per line: what's there now, or
    /// with --follow, all of it as it comes.
    Output {
        run: String,
        /// Start this many bytes in: what earlier reads printed.
        #[arg(long, default_value_t = 0)]
        from: u64,
        /// Keep printing until the run ends, then say how on stderr.
        #[arg(short, long)]
        follow: bool,
    },
    /// Print a run's result as JSON once it has ended. It has the exit code,
    /// the duration, whether the disk generation the run booted was
    /// committed or rolled back, and the paths that changed on that disk,
    /// which the runner reads from the disks itself. What the agent says it
    /// did is kept apart, marked untrusted.
    Result { run: String },
    /// Stop a run: Claude Code gets SIGTERM, then SIGKILL, and a VM the run
    /// booted rolls back and shuts down, while one it joined running gets the
    /// checkpoint it took back, which restarts it. A run whose Claude Code is done
    /// stops measuring its disk. Returns once the run has ended, or after two
    /// minutes.
    Cancel { run: String },
    /// Check this host can run VMs: one line per check, failing if any fails.
    Doctor,
    /// Serve these commands as MCP tools on stdin and stdout.
    Mcp {
        /// The user MCP's exec runs commands as.
        #[arg(long, env = "RUNNER_USER", default_value = exec::USER)]
        user: String,
    },
    /// Run a VM until it shuts down: the unit's main process.
    #[command(hide = true)]
    Boot {
        name: String,
        #[arg(long, env = "RUNNER_FIRECRACKER", default_value = "firecracker")]
        firecracker: PathBuf,
        #[arg(long, env = "RUNNER_IP", default_value = runner::host::IP)]
        ip: PathBuf,
    },
    /// Shut a VM down and wait for it: the unit's ExecStop.
    #[command(hide = true)]
    Halt { name: String },
    /// Drive a run to its end: what `runner run` starts.
    #[command(hide = true)]
    Drive {
        name: String,
        n: u64,
        /// The program a run starts in the VM.
        #[arg(long, env = "RUNNER_CLAUDE", default_value = "claude")]
        claude: String,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum Switch {
    On,
    Off,
}

#[derive(Subcommand)]
enum ServiceCmd {
    /// Run a command in a VM as a service: systemd starts it now and at
    /// every boot, and again a second after it exits. Starts a stopped VM,
    /// or wakes a sleeping one, first. Returns once the program has started, or fails, leaving
    /// nothing behind, with the journal's say if it can't start.
    Add {
        name: String,
        /// The service's name.
        service: String,
        /// The directory it runs in, and a relative program is taken from.
        #[arg(short, long, default_value = claude::DIR)]
        dir: String,
        /// The user it runs as.
        #[arg(long, env = "RUNNER_USER", default_value = exec::USER)]
        user: String,
        /// The program and its arguments, after `--`, run as they are, not
        /// through a shell: sh -c '...' for shell syntax.
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        argv: Vec<String>,
    },
    /// List a VM's services: name, systemd's state, directory and command.
    /// Starts a stopped VM, or wakes a sleeping one, first.
    List { name: String },
    /// Show a service's log, from the VM's journal.
    Logs {
        name: String,
        service: String,
        /// Keep printing as lines come.
        #[arg(short, long)]
        follow: bool,
        /// Only the last N lines.
        #[arg(short = 'n', long)]
        lines: Option<u32>,
    },
    /// Stop a service and remove it, so no boot starts it again.
    Rm { name: String, service: String },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let store = Store::new(&cli.root).keeping(cli.keep);
    match run(&cli.root, store, cli.cmd) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("runner: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(root: &Path, store: Store, cmd: Cmd) -> Result<ExitCode, Box<dyn Error>> {
    match cmd {
        Cmd::Import { name, disk } => show(&store.import(&name, &disk)?),
        Cmd::Create {
            name,
            from,
            cpus,
            mem,
            kernel,
            idle,
        } => {
            let config = Config {
                cpus,
                mem_mib: mem.as_deref().map(parse_mem).transpose()?,
                kernel: kernel.map(absolute_kernel).transpose()?,
                idle,
                url: None,
            };
            show(&store.fork_with(&from, &name, &config)?);
        }
        Cmd::List => {
            let vms = store.list()?;
            if vms.is_empty() {
                println!("no VMs yet");
            }
            vms.iter()
                .for_each(|vm| println!("{}  {} bytes used", line(vm), vm.used));
        }
        Cmd::Gc { name, dry_run } => return Ok(gc(&store, name, dry_run)),
        Cmd::Start { name } => show_status(&systemd::start(&store, &name)?),
        Cmd::Stop { name } => show_status(&systemd::stop(&store, &name)?),
        Cmd::Status { name } => show_status(&systemd::status(&store, &name)?),
        Cmd::Sleep { name } => show_status(&systemd::sleep(&store, &name)?),
        Cmd::Wake { name } => show_status(&systemd::wake(&store, &name)?),
        Cmd::Idle { name, idle } => {
            let idle = match idle {
                Some(idle) => store.set_idle(&name, idle)?,
                None => store.idle(&name)?,
            };
            println!("{name}  idle {idle}");
        }
        Cmd::Url { name, switch, port } => {
            let mut setting = store.url(&name)?;
            if switch.is_some() || port.is_some() {
                if let Some(switch) = switch {
                    setting.off = matches!(switch, Switch::Off);
                }
                setting.port = port.or(setting.port);
                store.set_url(&name, setting)?;
            }
            match url::url(&store, &name)? {
                Some(url) => println!("{url}"),
                None => println!("{name}'s URL is off: `runner url {name} on` turns it on"),
            }
        }
        Cmd::Logs {
            name,
            follow,
            lines,
        } => {
            let status = systemd::logs(&store, &name, follow, lines)?;
            return Ok(if status.success() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            });
        }
        Cmd::Undo { name } => show(&store.undo(&name)?),
        Cmd::Delete { name } => println!("deleted {}", line(&store.delete(&name)?)),
        Cmd::Ip { name } => println!("{}", store.net(&name)?.address),
        Cmd::Shell {
            name,
            user,
            session: Some(session),
        } => return Ok(on_session(&store, &name, &session, &user, true)),
        Cmd::Shell {
            name,
            user,
            session: None,
        } => {
            // Without a terminal here, its input ends with a close, which a
            // PTY would ignore.
            let tty = std::io::stdin().is_terminal();
            let req = Request {
                argv: exec::LOGIN.map(String::from).to_vec(),
                env: if tty { term() } else { Vec::new() },
                cwd: None,
                user: Some(user),
                tty: tty.then(exec::terminal_size),
            };
            return Ok(attach(|| Ok(exec::open(&store, &name)?), &req));
        }
        Cmd::Sessions { name, user } => {
            let all = sessions::list(&store, &name, &user)?;
            if all.is_empty() {
                println!("no sessions");
            }
            all.iter().for_each(|s| println!("{}", session_row(s)));
        }
        Cmd::Attach {
            name,
            session,
            user,
        } => return Ok(on_session(&store, &name, &session, &user, false)),
        Cmd::Kill {
            name,
            session,
            user,
        } => {
            sessions::kill(&store, &name, &user, &session)?;
            println!("killed {session}");
        }
        Cmd::Service { cmd } => return service(&store, cmd),
        Cmd::Claude {
            name,
            from,
            dir,
            user,
            args,
        } => {
            let c = Claude {
                vm: name,
                from,
                dir,
                user,
                argv: std::iter::once("claude".to_string()).chain(args).collect(),
                env: term(),
                size: exec::terminal_size(),
            };
            return Ok(exit_code(open_claude(&store, &c)));
        }
        Cmd::Exec {
            name,
            tty,
            env,
            dir,
            user,
            argv,
        } => {
            let req = Request {
                argv,
                env: if tty { term() } else { Vec::new() }
                    .into_iter()
                    .chain(env)
                    .collect(),
                cwd: dir,
                user: Some(user),
                tty: tty.then(exec::terminal_size),
            };
            return Ok(attach(|| Ok(exec::open(&store, &name)?), &req));
        }
        Cmd::Unit { user } => {
            let exe = std::env::current_exe()?;
            let root = std::path::absolute(root)?;
            print!("{}", systemd::render_unit(&exe, &root, &user)?);
        }
        Cmd::Creds { name, creds, none } => {
            let got = if none || !creds.is_empty() {
                store.set_creds(&name, &creds)?
            } else {
                store.creds(&name)?
            };
            println!("{name}  creds {}", names(&got));
        }
        Cmd::Run {
            name,
            prompt,
            dir,
            resume,
            user,
        } => {
            let start = runs::Start {
                vm: &name,
                prompt: &prompt,
                dir: &dir,
                resume: resume.as_deref(),
                user: &user,
            };
            let run = runs::start(&store, &std::env::current_exe()?, &start)?;
            println!("{}", run.id);
        }
        Cmd::Runs { name } => {
            let all = runs::list(&store, &name)?;
            if all.is_empty() {
                println!("no runs yet");
            }
            all.iter().for_each(|r| println!("{}", run_row(r)));
        }
        Cmd::Output { run, from, follow } => {
            let run = runs::output(&store, &run, from, follow, &mut std::io::stdout().lock())?;
            if follow {
                eprintln!("runner: {} {}", run.id, runs::describe(&run));
            }
        }
        Cmd::Result { run } => {
            let result = runs::result(&store, &run)?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        Cmd::Cancel { run } => println!("{}", run_row(&runs::cancel(&store, &run)?)),
        Cmd::Doctor => {
            let report = doctor::check(&System, &store);
            report.lines().for_each(|l| println!("{l}"));
            return Ok(if report.ok {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            });
        }
        Cmd::Checkpoint { name, comment } => {
            let made = checkpoint::checkpoint(&store, &name, comment.as_deref())?;
            println!("{}", made.row());
        }
        Cmd::Checkpoints { name } => {
            store
                .checkpoints(&name)?
                .iter()
                .for_each(|c| println!("{}", c.row()));
        }
        Cmd::Restore { name, id } => show(&checkpoint::restore(&store, &name, id)?),
        Cmd::Mcp { user } => return Ok(serve(store, user)),
        Cmd::Boot {
            name,
            firecracker,
            ip,
        } => {
            let ended = runner::boot::boot(&store, &name, &firecracker, &ip)?;
            if ended == runner::boot::Ended::Restart {
                return Ok(ExitCode::from(runner::boot::RESTART));
            }
        }
        Cmd::Halt { name } => runner::boot::halt(&store, &name)?,
        Cmd::Drive { name, n, claude } => runs::drive(&store, &name, n, &claude)?,
    }
    Ok(ExitCode::SUCCESS)
}

fn service(store: &Store, cmd: ServiceCmd) -> Result<ExitCode, Box<dyn Error>> {
    match cmd {
        ServiceCmd::Add {
            name,
            service,
            dir,
            user,
            argv,
        } => {
            let spec = services::Spec {
                name: service,
                argv,
                dir,
                user,
            };
            let added = services::add(&InVm { store, vm: &name }, &spec)?;
            println!("{}", services::row(&added));
        }
        ServiceCmd::List { name } => {
            let all = services::list(&InVm { store, vm: &name })?;
            if all.is_empty() {
                println!("no services");
            }
            all.iter().for_each(|s| println!("{}", services::row(s)));
        }
        ServiceCmd::Logs {
            name,
            service,
            follow,
            lines,
        } => {
            let argv = services::logs_argv(services::UNIT_DIR, &name, &service, follow, lines)?;
            let req = services::as_root(argv);
            return Ok(attach(|| Ok(exec::open(store, &name)?), &req));
        }
        ServiceCmd::Rm { name, service } => {
            services::remove(&InVm { store, vm: &name }, &service)?;
            println!("removed {service}");
        }
    }
    Ok(ExitCode::SUCCESS)
}

// The command's exit code, or 128 plus the signal that killed it, as a
// shell has it. 255 is the runner's own failure, as with ssh.
fn attach(open: impl FnOnce() -> Result<exec::Conn, Box<dyn Error>>, req: &Request) -> ExitCode {
    // In use until the command is done.
    let attached = |exec::Conn {
                        stream,
                        in_use: _in_use,
                    }| Ok(exec::attach(stream, req)?);
    exit_code(open().and_then(attached))
}

fn exit_code(done: Result<Exit, Box<dyn Error>>) -> ExitCode {
    match done {
        Ok(Exit::Code(code)) => ExitCode::from(code as u8),
        Ok(Exit::Signal(sig)) => ExitCode::from(128u8.wrapping_add(sig as u8)),
        Err(e) => {
            eprintln!("runner: {e}");
            ExitCode::from(255)
        }
    }
}

// This terminal on a tmux session, made first if `create`. Only that boots a
// stopped VM, which has no sessions to attach to.
fn on_session(store: &Store, name: &str, session: &str, user: &str, create: bool) -> ExitCode {
    let open = || -> Result<exec::Conn, Box<dyn Error>> {
        sessions::check_name(session)?;
        if !std::io::stdin().is_terminal() {
            return Err("a session needs a terminal: run this from one, or over ssh -t".into());
        }
        if create {
            return Ok(exec::open(store, name)?);
        }
        let no_session = || sessions::Error::NoSession {
            vm: name.to_string(),
            session: session.to_string(),
        };
        if !sessions::exists(store, name, user, session)? {
            return Err(no_session().into());
        }
        Ok(exec::open_running(store, name)?.ok_or_else(no_session)?)
    };
    let req = Request {
        argv: sessions::attach_argv(session, create),
        env: term(),
        cwd: None,
        user: Some(user.to_string()),
        tty: Some(exec::terminal_size()),
    };
    attach(open, &req)
}

// Checked first, so a VM isn't made or booted for tmux to fail on.
fn open_claude(store: &Store, c: &Claude) -> Result<Exit, Box<dyn Error>> {
    if !std::io::stdin().is_terminal() {
        return Err("claude needs a terminal: run this from one, or over ssh -t".into());
    }
    Ok(claude::open(
        &claude::Host(store),
        c,
        &mut std::io::stderr(),
    )?)
}

// A terminal of the guest's speaks this one's TERM, as over ssh.
fn term() -> Vec<(String, String)> {
    std::env::var("TERM")
        .ok()
        .filter(|t| !t.is_empty())
        .map(|t| vec![("TERM".to_string(), t)])
        .unwrap_or_default()
}

fn parse_env(kv: &str) -> Result<(String, String), String> {
    match kv.split_once('=') {
        Some((k, v)) if !k.is_empty() => Ok((k.to_string(), v.to_string())),
        _ => Err(format!("{kv:?} isn't K=V")),
    }
}

// The store keeps an absolute path, and the file has to be there now.
fn absolute_kernel(path: PathBuf) -> Result<PathBuf, String> {
    std::fs::canonicalize(&path).map_err(|e| format!("no kernel file at {}: {e}", path.display()))
}

fn line(vm: &Vm) -> String {
    format!("{}  gen {}  {} bytes", vm.name, vm.generation, vm.bytes)
}

// One VM failing, say because a run holds it, doesn't stop the rest; it
// fails the command.
fn gc(store: &Store, name: Option<String>, dry_run: bool) -> ExitCode {
    let names = match name {
        Some(name) => vec![name],
        None => match store.list() {
            Ok(vms) => vms.into_iter().map(|vm| vm.name).collect(),
            Err(e) => {
                eprintln!("runner: {e}");
                return ExitCode::FAILURE;
            }
        },
    };
    if names.is_empty() {
        println!("no VMs yet");
    }
    let verb = if dry_run { "would free" } else { "freed" };
    let mut code = ExitCode::SUCCESS;
    for name in names {
        match store.gc(&name, dry_run) {
            Ok(f) if f.generations.is_empty() => println!("{name}  nothing to free"),
            Ok(f) => {
                let gens: Vec<String> = f.generations.iter().map(u64::to_string).collect();
                // Up to: blocks a reflink shares with a disk that stays aren't freed.
                println!(
                    "{name}  {verb} gen {}  up to {} bytes",
                    gens.join(" "),
                    f.bytes
                );
            }
            Err(e) => {
                eprintln!("runner: {e}");
                code = ExitCode::FAILURE;
            }
        }
    }
    code
}

// The prompt is quoted, so one with a newline in it still takes one line.
fn run_row(r: &AgentRun) -> String {
    let started = r.started.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    format!("{}  {started}  {}  {:?}", r.id, runs::describe(r), r.prompt)
}

fn session_row(s: &Session) -> String {
    let started = s.started.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let on = if s.attached { "attached" } else { "detached" };
    format!("{}  {}  {started}  {on}", s.name, s.command)
}

fn show(vm: &Vm) {
    println!("{}", line(vm));
}

fn show_status(status: &VmStatus) {
    let creds = status
        .creds
        .as_deref()
        .map(|c| format!("  creds {}", names(c)));
    let creds = creds.unwrap_or_default();
    println!("{}  {}{creds}", line(&status.vm), status.state);
}

fn names(creds: &[Cred]) -> String {
    if creds.is_empty() {
        return "none".to_string();
    }
    let names: Vec<_> = creds.iter().map(|c| c.name()).collect();
    names.join(" ")
}

fn serve(store: Store, user: String) -> ExitCode {
    let served = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(Into::into)
        .and_then(|rt| rt.block_on(runner::mcp::serve(store, user)));
    match served {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("runner mcp: {e}");
            ExitCode::FAILURE
        }
    }
}
