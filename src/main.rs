use clap::{Parser, Subcommand};
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
    /// Print a VM's address, which stays the same for as long as the VM exists.
    Ip { name: String },
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
        Cmd::Ip { name } => {
            return match store.net(&name) {
                Ok(net) => {
                    println!("{}", net.address);
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
