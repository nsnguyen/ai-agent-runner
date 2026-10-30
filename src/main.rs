use clap::{Parser, Subcommand};
use runner::creds::Cred;
use runner::doctor::{self, System};
use runner::store::{BASE, Config, Store, Vm, parse_mem};
use runner::systemd::{self, VmStatus};
use std::error::Error;
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
    /// List VMs and their current disk generation.
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
    /// Check this host can run VMs: one line per check, failing if any fails.
    Doctor,
    /// Serve these commands as MCP tools on stdin and stdout.
    Mcp,
    /// Run a VM until it shuts down: the unit's main process.
    #[command(hide = true)]
    Boot {
        name: String,
        #[arg(long, env = "RUNNER_FIRECRACKER", default_value = "firecracker")]
        firecracker: PathBuf,
    },
    /// Shut a VM down and wait for it: the unit's ExecStop.
    #[command(hide = true)]
    Halt { name: String },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let store = Store::new(&cli.root);
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
            vms.iter().for_each(show);
        }
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
        Cmd::Mcp => return Ok(serve(store)),
        Cmd::Boot { name, firecracker } => runner::boot::boot(&store, &name, &firecracker)?,
        Cmd::Halt { name } => runner::boot::halt(&store, &name)?,
    }
    Ok(ExitCode::SUCCESS)
}

// The store keeps an absolute path, and the file has to be there now.
fn absolute_kernel(path: PathBuf) -> Result<PathBuf, String> {
    std::fs::canonicalize(&path).map_err(|e| format!("no kernel file at {}: {e}", path.display()))
}

fn line(vm: &Vm) -> String {
    format!("{}  gen {}  {} bytes", vm.name, vm.generation, vm.bytes)
}

fn show(vm: &Vm) {
    println!("{}", line(vm));
}

fn show_status(status: &VmStatus) {
    println!("{}  {}", line(&status.vm), status.state);
}

fn serve(store: Store) -> ExitCode {
    let served = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(Into::into)
        .and_then(|rt| rt.block_on(runner::mcp::serve(store)));
    match served {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("runner mcp: {e}");
            ExitCode::FAILURE
        }
    }
}
