use clap::{Parser, Subcommand};
use runner::creds::Cred;
use runner::doctor::{self, System};
use runner::exec;
use runner::proto::{Exit, Request};
use runner::sessions::{self, Session};
use runner::store::{BASE, Checkpoint, Config, KEEP, Store, Vm, parse_mem};
use runner::systemd::{self, VmStatus};
use std::error::Error;
use std::io::IsTerminal;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// Firecracker microVMs for AI agents. Runs on the KVM host, usually as
/// `ssh -t host runner ...`; `runner mcp` serves the same commands to Claude Code.
#[derive(Parser)]
#[command(name = "runner", version)]
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
    },
    /// List VMs, their current disk generation and the space their disks use.
    List,
    /// Boot a VM under systemd. Returns once its guest has started.
    Start { name: String },
    /// Shut a VM down: Ctrl-Alt-Del, and a kill after 30 seconds.
    Stop { name: String },
    /// Say whether a VM is stopped, booting, running or stopping.
    Status { name: String },
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
    /// Freeze a copy of a VM's disk to restore later. The VM must be stopped.
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
    /// List a running VM's tmux sessions: name, the command in its active
    /// pane, when it started, and whether a terminal is on it.
    Sessions {
        name: String,
        /// Whose sessions.
        #[arg(long, env = "RUNNER_USER", default_value = exec::USER)]
        user: String,
    },
    /// Attach this terminal to a tmux session in a running VM.
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
        } => {
            let config = Config {
                cpus,
                mem_mib: mem.as_deref().map(parse_mem).transpose()?,
                kernel: kernel.map(absolute_kernel).transpose()?,
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
            let names: Vec<_> = got.iter().map(|c| c.name()).collect();
            let list = if names.is_empty() {
                "none".to_string()
            } else {
                names.join(" ")
            };
            println!("{name}  creds {list}");
        }
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
            println!("{}", row(&store.checkpoint(&name, comment.as_deref())?));
        }
        Cmd::Checkpoints { name } => {
            store
                .checkpoints(&name)?
                .iter()
                .for_each(|c| println!("{}", row(c)));
        }
        Cmd::Restore { name, id } => show(&store.restore(&name, id)?),
        Cmd::Mcp { user } => return Ok(serve(store, user)),
        Cmd::Boot {
            name,
            firecracker,
            ip,
        } => runner::boot::boot(&store, &name, &firecracker, &ip)?,
        Cmd::Halt { name } => runner::boot::halt(&store, &name)?,
    }
    Ok(ExitCode::SUCCESS)
}

// The command's exit code, or 128 plus the signal that killed it, as a
// shell has it. 255 is the runner's own failure, as with ssh.
fn attach(open: impl FnOnce() -> Result<UnixStream, Box<dyn Error>>, req: &Request) -> ExitCode {
    match open().and_then(|conn| Ok(exec::attach(conn, req)?)) {
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
    let open = || -> Result<UnixStream, Box<dyn Error>> {
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

// The comment is quoted, so one with a newline in it still takes one line.
fn row(c: &Checkpoint) -> String {
    let mark = if c.current { '*' } else { ' ' };
    let time = c.time.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let kind = if c.auto { "auto" } else { "checkpoint" };
    let comment = c.comment.as_ref().map(|c| format!("  {c:?}"));
    let comment = comment.unwrap_or_default();
    format!(
        "{mark} {}  {time}  {kind}  {} bytes{comment}",
        c.id, c.bytes
    )
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
    println!("{}  {}", line(&status.vm), status.state);
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
