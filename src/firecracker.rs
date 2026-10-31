// Firecracker, driven through its API socket: HTTP/1.1 over a Unix socket,
// one request per connection. The calls and bodies are the ones in the
// pinned release's src/firecracker/swagger/firecracker.yaml.
use crate::net::Net;
use serde_json::{Value, json};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// The Firecracker release the runner is built against. host/firecracker-version
/// is the one place it's pinned; the host's install reads that file too.
pub const VERSION: &str = include_str!("../host/firecracker-version").trim_ascii();

/// The guest's kernel command line; Firecracker adds the root disk to it.
/// The console is on serial, so it lands in the unit's journal. The i8042
/// flags skip slow probing but keep the keyboard that carries Ctrl-Alt-Del,
/// and `reboot=k` resets through it, which is how Firecracker learns the
/// guest is done. Not `i8042.nopnp`, which Firecracker's docs list too: the
/// keyboard is found through ACPI, and without it the probe fails and every
/// shutdown hangs. There's no `panic=`: a panicked guest hangs instead of
/// rebooting, so unless the guest sets kernel.panic itself, Firecracker
/// doesn't exit 0 for it and its run isn't committed.
pub const BOOT_ARGS: &str = "console=ttyS0 reboot=k pci=off i8042.noaux i8042.nomux i8042.dumbkbd";

/// The guest's vsock address. Every VM has its own vsock socket, so they can share it.
pub const GUEST_CID: u32 = 3;

// Long enough for InstanceStart, short enough that a wedged Firecracker
// fails the boot instead of hanging it.
const TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("firecracker isn't listening on {}", .0.display())]
    NotListening(PathBuf),
    #[error("firecracker refused PUT {path} ({status}): {message}")]
    Api {
        path: String,
        status: u16,
        message: String,
    },
    #[error("talking to firecracker on {}: {source}", sock.display())]
    Io { sock: PathBuf, source: io::Error },
    #[error(
        "{} is Firecracker {found}, but runner is pinned to {VERSION} (host/firecracker-version)",
        bin.display()
    )]
    Version { bin: PathBuf, found: String },
    #[error("running {}: {source}", bin.display())]
    Run { bin: PathBuf, source: io::Error },
}

pub type Result<T> = std::result::Result<T, Error>;

/// What one boot of a VM is made of.
#[derive(Debug, Clone)]
pub struct Guest<'a> {
    pub kernel: &'a Path,
    pub disk: &'a Path,
    pub cpus: u32,
    pub mem_mib: u32,
    pub vsock: &'a Path,
    /// The tap its eth0 is on, and its MAC.
    pub net: &'a Net,
    /// Its address, gateway and DNS, as the kernel's `ip=` argument.
    pub ip_arg: &'a str,
}

#[derive(Debug, Clone)]
pub struct Api {
    sock: PathBuf,
}

impl Api {
    pub fn new(sock: impl Into<PathBuf>) -> Api {
        Api { sock: sock.into() }
    }

    /// Everything a guest needs before it can start.
    pub fn configure(&self, guest: &Guest) -> Result<()> {
        let args = format!("{BOOT_ARGS} {}", guest.ip_arg);
        let boot = json!({"kernel_image_path": guest.kernel, "boot_args": args});
        self.put("/boot-source", &boot)?;
        let rootfs = json!({
            "drive_id": "rootfs",
            "path_on_host": guest.disk,
            "is_root_device": true,
            "is_read_only": false,
        });
        self.put("/drives/rootfs", &rootfs)?;
        let machine = json!({"vcpu_count": guest.cpus, "mem_size_mib": guest.mem_mib});
        self.put("/machine-config", &machine)?;
        let vsock = json!({"guest_cid": GUEST_CID, "uds_path": guest.vsock});
        self.put("/vsock", &vsock)?;
        let eth0 = json!({
            "iface_id": "eth0",
            "host_dev_name": guest.net.tap,
            "guest_mac": guest.net.mac,
        });
        self.put("/network-interfaces/eth0", &eth0)
    }

    pub fn start(&self) -> Result<()> {
        self.action("InstanceStart")
    }

    /// Asks the guest to shut down. Firecracker exits once it has.
    pub fn ctrl_alt_del(&self) -> Result<()> {
        self.action("SendCtrlAltDel")
    }

    fn action(&self, action: &str) -> Result<()> {
        self.put("/actions", &json!({"action_type": action}))
    }

    pub fn put(&self, path: &str, body: &Value) -> Result<()> {
        let io = |source| Error::Io {
            sock: self.sock.clone(),
            source,
        };
        let mut conn = UnixStream::connect(&self.sock).map_err(|e| match e.kind() {
            // No socket, or one left by a Firecracker that's gone.
            io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => {
                Error::NotListening(self.sock.clone())
            }
            _ => io(e),
        })?;
        conn.set_read_timeout(Some(TIMEOUT)).map_err(io)?;
        conn.set_write_timeout(Some(TIMEOUT)).map_err(io)?;
        let body = body.to_string();
        let request = format!(
            "PUT {path} HTTP/1.1\r\nHost: localhost\r\nAccept: application/json\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        conn.write_all(request.as_bytes()).map_err(io)?;
        let (status, reply) = read_response(&mut BufReader::new(conn)).map_err(io)?;
        if (200..300).contains(&status) {
            return Ok(());
        }
        let message = serde_json::from_slice::<Value>(&reply)
            .ok()
            .and_then(|v| v["fault_message"].as_str().map(String::from))
            .unwrap_or_else(|| String::from_utf8_lossy(&reply).trim().to_string());
        Err(Error::Api {
            path: path.to_string(),
            status,
            message,
        })
    }
}

// Read to the body's length, not to EOF: Firecracker keeps the connection open.
fn read_response(r: &mut impl BufRead) -> io::Result<(u16, Vec<u8>)> {
    let bad = |what: String| io::Error::new(io::ErrorKind::InvalidData, what);
    let mut line = String::new();
    r.read_line(&mut line)?;
    let status = line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| bad(format!("not an HTTP reply: {:?}", line.trim_end())))?;
    let mut len = 0;
    loop {
        let mut header = String::new();
        if r.read_line(&mut header)? == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some((k, v)) = header.split_once(':')
            && k.trim().eq_ignore_ascii_case("content-length")
        {
            len = v
                .trim()
                .parse()
                .ok()
                .filter(|&n: &usize| n <= 1 << 20)
                .ok_or_else(|| bad(format!("bad Content-Length: {v}")))?;
        }
    }
    let mut body = vec![0; len];
    r.read_exact(&mut body)?;
    Ok((status, body))
}

/// Fails unless `bin` is the pinned Firecracker, whose API this module speaks.
pub fn check_version(bin: &Path) -> Result<()> {
    let out = Command::new(bin)
        .arg("--version")
        .output()
        .map_err(|source| Error::Run {
            bin: bin.to_path_buf(),
            source,
        })?;
    // "Firecracker v1.17.0", then a blank line.
    let text = String::from_utf8_lossy(&out.stdout);
    let first = text.lines().next().unwrap_or("").trim();
    let found = first.strip_prefix("Firecracker ").unwrap_or(first);
    if out.status.success() && found == VERSION {
        return Ok(());
    }
    let found = if found.is_empty() {
        format!("unknown ({})", out.status)
    } else {
        found.to_string()
    };
    Err(Error::Version {
        bin: bin.to_path_buf(),
        found,
    })
}
