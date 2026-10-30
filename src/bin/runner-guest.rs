// runner-guest: the agent inside each VM (src/guest.rs). systemd runs it as
// root in the base image; the host reaches it on vsock port proto::PORT.
use clap::Parser;
use runner::creds::{self, Keyring};
use runner::{guest, proto};
use std::io;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;
use vsock::{VMADDR_CID_ANY, VMADDR_CID_HOST, VsockListener, VsockStream};

// The host hands them over as soon as it's asked; this is for one that never does.
const CREDS_WAIT: Duration = Duration::from_secs(10);

/// Runs commands for the runner host over vsock, in place of sshd.
#[derive(Parser)]
#[command(name = "runner-guest", version)]
struct Args {
    /// Listen on this Unix socket instead of vsock, as the tests do.
    #[arg(long)]
    unix: Option<PathBuf>,
    /// Take credentials from this Unix socket instead of the host's vsock
    /// port, as the tests do. With --unix and without this, there are none.
    #[arg(long)]
    creds_unix: Option<PathBuf>,
}

fn main() -> ExitCode {
    let args = Args::parse();
    // Before listening, so no command runs without them.
    let keyring = Arc::new(take_creds(&args));
    let served = match args.unix {
        Some(path) => UnixListener::bind(path).map(|l| guest::serve(l.incoming(), keyring)),
        None => VsockListener::bind_with_cid_port(VMADDR_CID_ANY, proto::PORT)
            .map(|l| guest::serve(std::iter::repeat_with(|| guest::accept_vsock(&l)), keyring)),
    };
    match served {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("runner-guest: listening: {e}");
            ExitCode::FAILURE
        }
    }
}

// The credentials this VM may have, from the host, at every start: a
// restarted agent asks again. Only the host is at the other end of CID 2. A
// host that has none for it, or doesn't answer, leaves commands to run
// without, as they would on a host that has no token.
fn take_creds(args: &Args) -> Keyring {
    let taken = match (&args.creds_unix, &args.unix) {
        (Some(path), _) => UnixStream::connect(path).and_then(|mut conn| {
            conn.set_read_timeout(Some(CREDS_WAIT))?;
            Keyring::receive(&mut conn)
        }),
        (None, Some(_)) => return Keyring::default(),
        (None, None) => {
            VsockStream::connect_with_cid_port(VMADDR_CID_HOST, creds::PORT).and_then(|mut conn| {
                conn.set_read_timeout(Some(CREDS_WAIT))?;
                Keyring::receive(&mut conn)
            })
        }
    };
    taken.unwrap_or_else(|e: io::Error| {
        eprintln!("runner-guest: running without credentials: {e}");
        Keyring::default()
    })
}
