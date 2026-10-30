use clap::{Parser, Subcommand};
use runner::store::{BASE, KEEP, Store, Vm};
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
    },
    /// List VMs, their current disk generation and the space their disks use.
    List,
    /// Roll a VM back to its previous committed disk generation.
    Undo { name: String },
    /// Remove a VM and all its disk generations.
    Delete { name: String },
    /// Delete old disk generations: all but the current one, checkpoints with
    /// a comment and the newest --keep others. Every commit does this too.
    Gc {
        /// Only this VM. Every VM if left out.
        name: Option<String>,
        /// Print what would be freed, and free nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Serve these commands as MCP tools on stdin and stdout.
    Mcp,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let store = Store::new(cli.root).keeping(cli.keep);
    let listing = matches!(cli.cmd, Cmd::List);
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
        Cmd::Gc { name, dry_run } => return gc(&store, name, dry_run),
        Cmd::List => store.list(),
        Cmd::Mcp => return serve(store),
    };
    match shown {
        Ok(vms) if vms.is_empty() => println!("no VMs yet"),
        Ok(vms) if listing => vms
            .iter()
            .for_each(|vm| println!("{}  {} bytes used", line(vm), vm.used)),
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
