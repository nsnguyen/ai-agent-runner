// Stand-ins for what the host provides: Firecracker's API socket, a
// `firecracker` binary, and systemctl and journalctl on PATH. Each test file
// uses its own share of them.
#![allow(dead_code)]

use serde_json::Value;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

pub fn script(path: &Path, body: &str) {
    fs::write(path, format!("#!/bin/sh\n{body}")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// Polls until `done` holds, or panics after a few seconds.
pub fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Value,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

pub type Reply = (u16, Option<String>);

/// Firecracker's API on a Unix socket: records every request and answers
/// with what `respond` says. 204 with no body is Firecracker's "done".
pub struct FakeApi {
    requests: Arc<Mutex<Vec<Request>>>,
}

impl FakeApi {
    pub fn serve(sock: &Path, respond: impl Fn(&Request) -> Reply + Send + 'static) -> FakeApi {
        let listener = UnixListener::bind(sock).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(conn) = conn else { return };
                let mut reader = BufReader::new(conn.try_clone().unwrap());
                let mut writer = conn;
                // Several requests may share one connection, as with keep-alive.
                while let Some(req) = read_request(&mut reader) {
                    seen.lock().unwrap().push(req.clone());
                    let (status, body) = respond(&req);
                    let body = body.unwrap_or_default();
                    let head = if body.is_empty() {
                        format!("HTTP/1.1 {status} \r\nServer: Firecracker API\r\n\r\n")
                    } else {
                        format!(
                            "HTTP/1.1 {status} \r\nServer: Firecracker API\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                            body.len()
                        )
                    };
                    if writer
                        .write_all(format!("{head}{body}").as_bytes())
                        .is_err()
                    {
                        break;
                    }
                }
            }
        });
        FakeApi { requests }
    }

    pub fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }

    /// Each request as `METHOD path body`, for comparing a whole conversation.
    pub fn calls(&self) -> Vec<(String, Value)> {
        self.requests()
            .into_iter()
            .map(|r| (format!("{} {}", r.method, r.path), r.body))
            .collect()
    }
}

fn read_request(reader: &mut impl BufRead) -> Option<Request> {
    let mut line = String::new();
    if reader.read_line(&mut line).ok()? == 0 {
        return None;
    }
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();
    let mut headers = Vec::new();
    loop {
        let mut h = String::new();
        reader.read_line(&mut h).ok()?;
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        let (k, v) = h.split_once(':')?;
        headers.push((k.trim().to_string(), v.trim().to_string()));
    }
    let len: usize = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .map_or(0, |(_, v)| v.parse().unwrap());
    let mut body = vec![0; len];
    reader.read_exact(&mut body).ok()?;
    let body = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body).unwrap()
    };
    Some(Request {
        method,
        path,
        headers,
        body,
    })
}

/// A `firecracker` that prints the version it's told to, records its argv,
/// writes a line of "console", then exits with whatever number is written
/// to its `exit` fifo. The test serves its API socket.
#[derive(Clone)]
pub struct FakeFirecracker {
    pub dir: PathBuf,
    pub bin: PathBuf,
}

impl FakeFirecracker {
    pub fn new(dir: &Path) -> FakeFirecracker {
        fs::create_dir_all(dir).unwrap();
        let bin = dir.join("firecracker");
        script(
            &bin,
            r#"if [ "$1" = --version ]; then printf 'Firecracker %s\n\n' "$FAKE_FC_VERSION"; exit 0; fi
grep '^CapAmb' /proc/$$/status > "$FAKE_FC/caps"
printf '%s\n' "$@" > "$FAKE_FC/argv.tmp" && mv "$FAKE_FC/argv.tmp" "$FAKE_FC/argv"
echo "fake console: Linux version 6.1"
read -r code < "$FAKE_FC/exit"
exit "$code"
"#,
        );
        let fifo = dir.join("exit");
        let made = Command::new("mkfifo").arg(&fifo).status().unwrap();
        assert!(made.success());
        FakeFirecracker {
            dir: dir.to_path_buf(),
            bin,
        }
    }

    /// Its argv, once it has started.
    pub fn argv(&self) -> Vec<String> {
        let path = self.dir.join("argv");
        wait_for("firecracker to start", || path.exists());
        fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(String::from)
            .collect()
    }

    pub fn started(&self) -> bool {
        self.dir.join("argv").exists()
    }

    /// Makes it exit with `code`, from another thread so a reply isn't held up.
    pub fn exit_with(&self, code: i32) {
        let fifo = self.dir.join("exit");
        std::thread::spawn(move || fs::write(fifo, format!("{code}\n")).unwrap());
    }
}

/// systemctl and journalctl on PATH. systemctl logs its argv to `calls`,
/// keeps a unit's ActiveState in `state`, and fails with the text in `fail`
/// if that file exists. journalctl logs its argv and prints one line.
pub struct FakeSystemd {
    pub bin: PathBuf,
}

impl FakeSystemd {
    pub fn new(dir: &Path) -> FakeSystemd {
        let bin = dir.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let d = bin.display();
        script(
            &bin.join("systemctl"),
            &format!(
                r#"echo "systemctl $*" >> "{d}/calls"
if [ -f "{d}/fail" ]; then cat "{d}/fail" >&2; exit 1; fi
case "$1" in
  start) echo active > "{d}/state" ;;
  stop) echo inactive > "{d}/state" ;;
  show) cat "{d}/state" 2>/dev/null || echo inactive ;;
esac
"#
            ),
        );
        script(
            &bin.join("journalctl"),
            &format!(
                "echo \"journalctl $*\" >> \"{d}/calls\"\necho 'fake console: Linux version 6.1'\n"
            ),
        );
        FakeSystemd { bin }
    }

    /// PATH with the fakes first.
    pub fn path(&self) -> String {
        format!("{}:{}", self.bin.display(), std::env::var("PATH").unwrap())
    }

    pub fn set_state(&self, state: &str) {
        fs::write(self.bin.join("state"), format!("{state}\n")).unwrap();
    }

    pub fn fail_with(&self, stderr: &str) {
        fs::write(self.bin.join("fail"), stderr).unwrap();
    }

    pub fn calls(&self) -> Vec<String> {
        fs::read_to_string(self.bin.join("calls"))
            .unwrap_or_default()
            .lines()
            .map(String::from)
            .collect()
    }
}
