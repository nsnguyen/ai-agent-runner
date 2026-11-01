// runner-guest: the agent inside each VM (src/guest.rs). systemd runs it as
// root in the base image; the host reaches it on vsock port proto::PORT.
// Its subcommands are the agent's own calls to the host (src/calls.rs), for
// checkpoints of the VM it's in.
use clap::{Parser, Subcommand};
use runner::calls::{self, Answer, Call};
use runner::creds::{self, Keyring};
use runner::{guest, proto};
use std::io;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;
use vsock::{VMADDR_CID_ANY, VMADDR_CID_HOST, VsockListener, VsockStream};

// The host hands them over as soon as it's asked; this is for one that never does.
const CREDS_WAIT: Duration = Duration::from_secs(10);

/// Runs commands for the runner host over vsock, in place of sshd. Its
/// subcommands checkpoint and restore the VM it's in, from inside:
/// /.runner/llm.txt says how.
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
    /// Make a subcommand's call to the host on this Unix socket instead of
    /// vsock, as the tests do.
    #[arg(long, global = true)]
    host_unix: Option<PathBuf>,
    #[command(subcommand)]
    call: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Freeze a copy of this VM's disk to restore later. The VM carries on;
    /// it pauses for a moment while the copy is made.
    Checkpoint {
        /// A note to know it by.
        #[arg(short = 'm', long)]
        comment: Option<String>,
    },
    /// List this VM's checkpoints, newest first, and the generations in its
    /// history. `*` marks the one the next boot starts from.
    Checkpoints,
    /// Reboot this VM onto a copy of a checkpoint. Everything since it is
    /// gone, and every process in the VM ends, this command's caller too.
    Restore { id: u64 },
}

fn main() -> ExitCode {
    let args = Args::parse();
    if let Some(cmd) = args.call {
        return call(args.host_unix.as_deref(), cmd);
    }
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

// One call to the host, and what it said, as `runner` would print it.
fn call(unix: Option<&Path>, cmd: Cmd) -> ExitCode {
    let call = match cmd {
        Cmd::Checkpoint { comment } => Call::Checkpoint { comment },
        Cmd::Checkpoints => Call::Checkpoints,
        Cmd::Restore { id } => {
            eprintln!(
                "runner-guest: restoring checkpoint {id} reboots this VM: everything written \
                 since that checkpoint is gone, and every process in it ends, this one's caller too"
            );
            Call::Restore { id }
        }
    };
    let answer = match unix {
        Some(path) => UnixStream::connect(path)
            .map_err(proto::Error::from)
            .and_then(|mut conn| calls::ask(&mut conn, &call)),
        None => VsockStream::connect_with_cid_port(VMADDR_CID_HOST, calls::PORT)
            .map_err(proto::Error::from)
            .and_then(|mut conn| calls::ask(&mut conn, &call)),
    };
    match answer {
        Ok(Answer::Made(made)) => println!("{}", made.row()),
        Ok(Answer::Listed(listed)) => listed.iter().for_each(|c| println!("{}", c.row())),
        Ok(Answer::Restoring { id, generation }) => {
            println!("restoring checkpoint {id} as generation {generation}: this VM reboots now")
        }
        Ok(Answer::Refused(why)) => {
            eprintln!("runner-guest: {why}");
            return ExitCode::FAILURE;
        }
        Err(e) => {
            eprintln!(
                "runner-guest: can't reach the runner host on vsock port {}: {e}. \
                 Is this a runner VM, on a runner new enough to take these calls?",
                calls::PORT
            );
            return ExitCode::FAILURE;
        }
    }
    ExitCode::SUCCESS
}
