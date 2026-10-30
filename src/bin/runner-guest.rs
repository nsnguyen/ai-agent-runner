// runner-guest: the agent inside each VM (src/guest.rs). systemd runs it as
// root in the base image; the host reaches it on vsock port proto::PORT.
use clap::Parser;
use runner::{guest, proto};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::ExitCode;
use vsock::{VMADDR_CID_ANY, VsockListener};

/// Runs commands for the runner host over vsock, in place of sshd.
#[derive(Parser)]
#[command(name = "runner-guest", version)]
struct Args {
    /// Listen on this Unix socket instead of vsock, as the tests do.
    #[arg(long)]
    unix: Option<PathBuf>,
}

fn main() -> ExitCode {
    let args = Args::parse();
    let served = match args.unix {
        Some(path) => UnixListener::bind(path).map(|l| guest::serve(l.incoming())),
        None => VsockListener::bind_with_cid_port(VMADDR_CID_ANY, proto::PORT)
            .map(|l| guest::serve(l.incoming())),
    };
    match served {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("runner-guest: listening: {e}");
            ExitCode::FAILURE
        }
    }
}
