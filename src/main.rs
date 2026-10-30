use clap::{Parser, Subcommand};
use runner::creds::Cred;
use runner::store::{BASE, Store, Vm};
use std::path::PathBuf;
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
    },
    /// List VMs and their current disk generation.
    List,
    /// Roll a VM back to its previous committed disk generation.
    Undo { name: String },
    /// Remove a VM and all its disk generations.
    Delete { name: String },
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
    /// Serve these commands as MCP tools on stdin and stdout.
    Mcp,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let store = Store::new(cli.root);
    let shown = match cli.cmd {
        Cmd::Import { name, disk } => store.import(&name, &disk).map(|vm| vec![vm]),
        Cmd::Create { name, from } => store.fork(&from, &name).map(|vm| vec![vm]),
        Cmd::Undo { name } => store.undo(&name).map(|vm| vec![vm]),
        Cmd::Delete { name } => {
            return match store.delete(&name) {
                Ok(vm) => {
                    println!("deleted {}", line(&vm));
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("runner: {e}");
                    ExitCode::FAILURE
                }
            };
        }
        Cmd::Creds { name, creds, none } => {
            let set = if none || !creds.is_empty() {
                store.set_creds(&name, &creds)
            } else {
                store.creds(&name)
            };
            return match set {
                Ok(creds) => {
                    let names: Vec<_> = creds.iter().map(|c| c.name()).collect();
                    let shown = if names.is_empty() {
                        "none".into()
                    } else {
                        names.join(" ")
                    };
                    println!("{name}  creds {shown}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("runner: {e}");
                    ExitCode::FAILURE
                }
            };
        }
        Cmd::List => store.list(),
        Cmd::Mcp => return serve(store),
    };
    match shown {
        Ok(vms) if vms.is_empty() => println!("no VMs yet"),
        Ok(vms) => vms.iter().for_each(|vm| println!("{}", line(vm))),
        Err(e) => {
            eprintln!("runner: {e}");
            return ExitCode::FAILURE;
        }
    }
    ExitCode::SUCCESS
}

fn line(vm: &Vm) -> String {
    format!("{}  gen {}  {} bytes", vm.name, vm.generation, vm.bytes)
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
