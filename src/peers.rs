// `runner peer`: what another host may run here. The laptop's `runner hosts
// add` puts every host's key in every other host's
// ~runner/.ssh/authorized_keys behind OPTIONS, so for that key sshd runs
// `runner peer` whatever the other host asked for, and leaves what it asked
// for in SSH_ORIGINAL_COMMAND. Of that, only `runner mcp` and `runner import
// <vm> -`, each with its own flags, run, in this process and never through a
// shell. So one host drives another's VMs but gets no shell there
// (docs/design.md, Hosts trust each other).
//
// And the other end: `Remote`, a peer that the first host's `runner mcp
// --peer …` dials with this host's key, as one `ssh <peer> runner mcp`,
// kept open for every call after.
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, CallToolResponse, CallToolResult};
use rmcp::service::{Peer, RoleClient, RunningService, ServiceError};
use std::ffi::OsStr;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader};

/// Where sshd leaves the command a key's forced command stands in for.
pub const ORIGINAL: &str = "SSH_ORIGINAL_COMMAND";

/// What every host's key line on the others starts with: only from the
/// tailnet, no terminal or forwarding, and `runner peer` for any command.
/// client/runner writes the lines; tests/hosts.rs holds the two the same.
pub const OPTIONS: &str =
    r#"from="100.64.0.0/10,fd7a:115c:a1e0::/48",restrict,command="/usr/local/bin/runner peer""#;

const ONLY: &str = "a peer host's key runs only `runner mcp` and `runner import <vm> -` here";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{ONLY}, and no command came")]
    NoCommand,
    #[error("{ONLY}, not {0:?}")]
    Refused(String),
}

/// The runner arguments a peer's command is, `runner` first, if it's one a
/// peer may run: `runner mcp`, with `--user`, or `runner import <vm> -`, with
/// `--creds` and `--replace`, as words of only the characters VM names,
/// credential lists and these flags take, so there's no quoting to undo and
/// nothing a shell would read. clap parses them after, as for any command.
pub fn vet(original: Option<&OsStr>) -> Result<Vec<String>, Error> {
    let original = original.ok_or(Error::NoCommand)?;
    let refused = || Error::Refused(original.to_string_lossy().into_owned());
    let command = original.to_str().ok_or_else(refused)?;
    if !command.chars().all(|c| c == ' ' || safe(c)) {
        return Err(refused());
    }
    let words: Vec<&str> = command.split(' ').filter(|w| !w.is_empty()).collect();
    let fine = match words.as_slice() {
        ["runner", "mcp", rest @ ..] => mcp(rest),
        ["runner", "import", rest @ ..] => import(rest),
        _ => false,
    };
    if !fine {
        return Err(refused());
    }
    Ok(words.into_iter().map(String::from).collect())
}

// What client/runner leaves unquoted.
fn safe(c: char) -> bool {
    c.is_ascii_alphanumeric() || "@%+=:,./_-".contains(c)
}

// A flag's value: there, and not another flag.
fn value(v: Option<&&str>) -> bool {
    v.is_some_and(|v| !v.is_empty() && !v.starts_with('-'))
}

// mcp's own flag, --user.
fn mcp(rest: &[&str]) -> bool {
    let mut words = rest.iter();
    while let Some(w) = words.next() {
        let fine = match *w {
            "--user" => value(words.next()),
            w => value(w.strip_prefix("--user=").as_ref()),
        };
        if !fine {
            return false;
        }
    }
    true
}

// import's own flags, --creds and --replace, around the VM's name and `-`.
fn import(rest: &[&str]) -> bool {
    let mut positional = Vec::new();
    let mut words = rest.iter();
    while let Some(w) = words.next() {
        let fine = match *w {
            "--replace" => true,
            "--creds" => value(words.next()),
            "-" => {
                positional.push("-");
                true
            }
            w if w.starts_with("--creds=") => value(w.strip_prefix("--creds=").as_ref()),
            w if w.starts_with('-') => false,
            w => {
                positional.push(w);
                true
            }
        };
        if !fine {
            return false;
        }
    }
    matches!(positional.as_slice(), [vm, "-"] if *vm != "-")
}

/// ssh's options for a peer: this host's own key alone, so an agent's keys
/// never stand in for it; no prompt; the peer's host key taken on first
/// sight (the tailnet already vouches for the node); and keepalives, so a
/// peer that stops answering ends its session, and its calls fail, in
/// about 15 seconds.
pub const SSH: [&str; 15] = [
    "-T",
    "-o",
    "IdentitiesOnly=yes",
    "-i",
    "~/.ssh/id_ed25519",
    "-o",
    "BatchMode=yes",
    "-o",
    "StrictHostKeyChecking=accept-new",
    "-o",
    "ConnectTimeout=5",
    "-o",
    "ServerAliveInterval=5",
    "-o",
    "ServerAliveCountMax=2",
];

/// How long a peer gets to answer MCP's initialize once ssh has started.
pub const HELLO: Duration = Duration::from_secs(15);

/// How long ssh gets to say why it failed, once its session has.
const LAST_WORDS: Duration = Duration::from_secs(2);

/// Why a call to a peer got no answer of its own.
#[derive(Debug, Clone)]
pub struct Gone {
    /// The peer, as MCP names it.
    pub host: String,
    /// What went wrong, like what ssh said.
    pub why: String,
    // Its session ended or never began, rather than MCP saying no.
    down: bool,
}

impl std::fmt::Display for Gone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.down {
            write!(f, "{} didn't answer: {}", self.host, self.why)
        } else {
            write!(f, "{}: {}", self.host, self.why)
        }
    }
}

/// Another host's runner, reached as `ssh <target> runner mcp --user <user>`
/// on first use, and again after its session ends.
pub struct Remote {
    /// The host as the laptop's hosts file lists it, like runner@kvm-host-2.
    pub target: String,
    /// What MCP calls it: the target without its user@, as the laptop does.
    pub name: String,
    user: String,
    conn: tokio::sync::Mutex<Option<Conn>>,
    // Each session's number, so a call that saw one end drops only that one.
    dialed: std::sync::atomic::AtomicU64,
    // The last dial that failed, and when, so calls that waited on it take
    // its answer rather than each dialing again.
    failed: Mutex<Option<(Instant, Gone)>>,
}

struct Conn {
    n: u64,
    service: RunningService<RoleClient, ()>,
    // ssh's last line on stderr, which says why when it fails.
    said: Arc<Mutex<String>>,
    ssh: tokio::process::Child,
}

impl Conn {
    // Whether it can still carry a call: ssh hasn't exited, as it does when
    // the peer goes while nothing is asked of it, and the session is open.
    fn open(&mut self) -> bool {
        matches!(self.ssh.try_wait(), Ok(None)) && !self.service.is_closed()
    }
}

impl Remote {
    /// `user` is who the peer's exec runs commands as, as here.
    pub fn new(target: &str, user: &str) -> Remote {
        let name = target.rsplit_once('@').map_or(target, |(_, h)| h);
        Remote {
            target: target.to_string(),
            name: name.to_string(),
            user: user.to_string(),
            conn: tokio::sync::Mutex::new(None),
            dialed: std::sync::atomic::AtomicU64::new(0),
            failed: Mutex::new(None),
        }
    }

    /// Whether `host` names this one: as listed, or by its name.
    pub fn is(&self, host: &str) -> bool {
        host == self.target || host == self.name
    }

    /// A tool call, sent on as it came, and the peer's result as it gave it.
    pub async fn call(&self, params: CallToolRequestParams) -> Result<CallToolResult, Gone> {
        let (n, peer) = self.peer().await?;
        match peer.call_tool_once(params).await {
            Ok(CallToolResponse::Complete(result)) => Ok(result),
            Ok(_) => Err(self.said("it gave an answer runner doesn't take".into())),
            Err(ServiceError::McpError(e)) => Err(self.said(e.message.into_owned())),
            // The session is gone: the next call dials again. Not this one,
            // which may have run there.
            Err(e) => Err(self.hung_up(n, e.to_string()).await),
        }
    }

    // The open session, or a new one if there's none or it has ended. A
    // dial that failed while this call waited its turn is its answer too.
    async fn peer(&self) -> Result<(u64, Peer<RoleClient>), Gone> {
        let asked = Instant::now();
        let mut conn = self.conn.lock().await;
        if let Some(c) = conn.as_mut()
            && c.open()
        {
            return Ok((c.n, c.service.peer().clone()));
        }
        *conn = None;
        let failed = self
            .failed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some((at, gone)) = failed
            && at >= asked
        {
            return Err(gone);
        }
        let made = match self.dial().await {
            Ok(made) => made,
            Err(gone) => {
                let mut failed = self.failed.lock().unwrap_or_else(|e| e.into_inner());
                *failed = Some((Instant::now(), gone.clone()));
                return Err(gone);
            }
        };
        let got = (made.n, made.service.peer().clone());
        *conn = Some(made);
        Ok(got)
    }

    async fn dial(&self) -> Result<Conn, Gone> {
        let mut ssh = tokio::process::Command::new("ssh")
            .args(SSH)
            .arg(&self.target)
            .args(["runner", "mcp", "--user", &self.user])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| self.down(format!("running ssh: {e}")))?;
        let (Some(stdout), Some(stdin), Some(stderr)) =
            (ssh.stdout.take(), ssh.stdin.take(), ssh.stderr.take())
        else {
            return Err(self.down("ssh came without its pipes".into()));
        };
        // Passed on to this runner's stderr, a line at a time, under the
        // peer's name; stdout here is the laptop's protocol.
        let said = Arc::new(Mutex::new(String::new()));
        let (last, name) = (said.clone(), self.name.clone());
        let reading = tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                eprintln!("runner: {name}: {line}");
                if !line.trim().is_empty() {
                    *last.lock().unwrap_or_else(|e| e.into_inner()) = line;
                }
            }
        });
        let why = match tokio::time::timeout(HELLO, ().serve((stdout, stdin))).await {
            Ok(Ok(service)) => {
                let n = self
                    .dialed
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return Ok(Conn {
                    n,
                    service,
                    said,
                    ssh,
                });
            }
            Ok(Err(e)) => e.to_string(),
            Err(_) => format!("no answer in {} seconds", HELLO.as_secs()),
        };
        let _ = ssh.start_kill();
        let _ = tokio::time::timeout(LAST_WORDS, reading).await;
        Err(self.down(last_words(&said).unwrap_or(why)))
    }

    // Why session n ended, once ssh has had its say, and it's dropped.
    async fn hung_up(&self, n: u64, why: String) -> Gone {
        let mut conn = self.conn.lock().await;
        let Some(c) = conn.take_if(|c| c.n == n) else {
            return self.down(why);
        };
        drop(c.service);
        let mut ssh = c.ssh;
        let _ = tokio::time::timeout(LAST_WORDS, ssh.wait()).await;
        self.down(last_words(&c.said).unwrap_or(why))
    }

    fn down(&self, why: String) -> Gone {
        Gone {
            host: self.name.clone(),
            why,
            down: true,
        }
    }

    fn said(&self, why: String) -> Gone {
        Gone {
            host: self.name.clone(),
            why,
            down: false,
        }
    }
}

fn last_words(said: &Mutex<String>) -> Option<String> {
    let said = said.lock().unwrap_or_else(|e| e.into_inner());
    (!said.is_empty()).then(|| said.clone())
}
