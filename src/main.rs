use clap::{Parser, Subcommand};
use runner::store::{BASE, Checkpoint, Store, Vm};
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
    /// Serve these commands as MCP tools on stdin and stdout.
    Mcp,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let store = Store::new(cli.root);
    let shown = match cli.cmd {
        Cmd::Import { name, disk } => store.import(&name, &disk).map(|vm| vec![line(&vm)]),
        Cmd::Create { name, from } => store.fork(&from, &name).map(|vm| vec![line(&vm)]),
        Cmd::Undo { name } => store.undo(&name).map(|vm| vec![line(&vm)]),
        Cmd::Delete { name } => store
            .delete(&name)
            .map(|vm| vec![format!("deleted {}", line(&vm))]),
        Cmd::List => store.list().map(|vms| match &vms[..] {
            [] => vec!["no VMs yet".to_string()],
            vms => vms.iter().map(line).collect(),
        }),
        Cmd::Checkpoint { name, comment } => store
            .checkpoint(&name, comment.as_deref())
            .map(|c| vec![row(&c)]),
        Cmd::Checkpoints { name } => store
            .checkpoints(&name)
            .map(|cs| cs.iter().map(row).collect()),
        Cmd::Restore { name, id } => store.restore(&name, id).map(|vm| vec![line(&vm)]),
        Cmd::Mcp => return serve(store),
    };
    match shown {
        Ok(lines) => lines.iter().for_each(|l| println!("{l}")),
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
