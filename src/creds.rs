// Credentials reach a VM's memory per boot and never its disk: the host finds
// them, the guest holds them for its sessions (docs/design.md, Credentials).
// Nothing here writes a value to a file or prints one.
//
// runner-guest asks for them each time it starts, before it takes a command:
// it connects to the host on vsock PORT, which Firecracker passes on to
// `vsock.sock_<PORT>` in the VM's directory, where `runner boot` hands it the
// VM's keyring (`send`, then `receive` in the guest).
use rmcp::schemars;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::Command;
use std::str::FromStr;

/// The host's vsock port runner-guest takes its credentials from.
pub const PORT: u32 = 10023;

/// A credential a VM's allowlist can name.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum Cred {
    /// Claude Code's login: a `claude setup-token` token or an API key.
    Claude,
    /// A GitHub token, for `git push` and `gh`.
    Github,
}

/// What a VM gets until its allowlist says otherwise.
pub const DEFAULT: &[Cred] = &[Cred::Claude];

impl Cred {
    pub const ALL: [Cred; 2] = [Cred::Claude, Cred::Github];

    pub fn name(self) -> &'static str {
        match self {
            Cred::Claude => "claude",
            Cred::Github => "github",
        }
    }

    /// The variables it can arrive as, in the order the host looks; a VM gets
    /// only the first one found. Each is also the name of the systemd
    /// credential the host keeps it under.
    // The OAuth token comes first: an API key would win inside Claude Code
    // and bill per token.
    pub fn vars(self) -> &'static [&'static str] {
        match self {
            Cred::Claude => &["CLAUDE_CODE_OAUTH_TOKEN", "ANTHROPIC_API_KEY"],
            Cred::Github => &["GH_TOKEN"],
        }
    }

    fn of_var(var: &str) -> Option<(Cred, &'static str)> {
        Cred::ALL
            .into_iter()
            .find_map(|c| c.vars().iter().find(|v| **v == var).map(|v| (c, *v)))
    }
}

/// A request that sets a credential's variable itself.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0} is a credential: credentials reach a VM only through `runner creds`")]
pub struct Refused(pub &'static str);

/// Refuses a request's variables if any is one a credential arrives as. A VM
/// gets those only from the host, as its allowlist says; the guest would
/// drop them anyway (`Keyring::apply`), so this says so instead.
pub fn refuse<'a>(vars: impl IntoIterator<Item = &'a str>) -> Result<(), Refused> {
    match vars.into_iter().find_map(Cred::of_var) {
        Some((_, var)) => Err(Refused(var)),
        None => Ok(()),
    }
}

impl FromStr for Cred {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        Cred::ALL
            .into_iter()
            .find(|c| c.name() == s)
            .ok_or_else(|| {
                let names: Vec<_> = Cred::ALL.iter().map(|c| c.name()).collect();
                format!("{s:?} isn't a credential: use {}", names.join(" or "))
            })
    }
}

impl fmt::Display for Cred {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

// A value that never shows up in a log, a panic or `runner status`.
#[derive(PartialEq, Eq)]
struct Secret(String);

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

/// Where the host finds credentials: a systemd credentials directory, then
/// an environment.
pub struct Source {
    dir: Option<PathBuf>,
    env: HashMap<String, String>,
}

impl Source {
    /// A relative or empty `dir` is ignored: it would read the token from
    /// wherever the runner happens to run, a repo checkout included.
    pub fn new(dir: Option<PathBuf>, env: impl IntoIterator<Item = (String, String)>) -> Self {
        let dir = dir.filter(|d| d.is_absolute());
        let env = env.into_iter().collect();
        Source { dir, env }
    }

    /// This process's: what systemd loaded for its unit, then its environment.
    /// Only credential variables are read, one by one: `std::env::vars()`
    /// panics on any variable that isn't UTF-8 and prints its value.
    pub fn from_process() -> Self {
        let dir = std::env::var_os("CREDENTIALS_DIRECTORY").map(PathBuf::from);
        let vars = Cred::ALL.iter().flat_map(|c| c.vars());
        let env = vars.filter_map(|v| Some((v.to_string(), std::env::var(v).ok()?)));
        Source::new(dir, env)
    }

    // Trimmed, since a token pasted into a file usually ends in a newline;
    // blank counts as absent, in the directory and the environment alike.
    fn get(&self, var: &str) -> io::Result<Option<Secret>> {
        let given = |v: &str| Some(v.trim().to_string()).filter(|v| !v.is_empty());
        let from_dir = match &self.dir {
            Some(dir) => match fs::read_to_string(dir.join(var)) {
                Ok(v) => given(&v),
                Err(e) if e.kind() == io::ErrorKind::NotFound => None,
                Err(e) => return Err(io::Error::new(e.kind(), format!("credential {var}: {e}"))),
            },
            None => None,
        };
        let value = from_dir.or_else(|| self.env.get(var).and_then(|v| given(v)));
        if value.as_ref().is_some_and(|v| v.contains('\0')) {
            return Err(bad(format!("credential {var} holds a NUL byte")));
        }
        Ok(value.map(Secret))
    }
}

/// The largest handoff body: past any real token, small enough that a bad
/// length can't exhaust memory.
pub const MAX_HANDOFF: u32 = 64 * 1024;

/// Credentials in memory, each by the variable a session sees it as.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Keyring {
    held: Vec<(Cred, &'static str, Secret)>,
}

impl Keyring {
    /// What the host has of the credentials `allow` names. One it lacks is
    /// left out; the caller can compare `creds()` against `allow` to warn.
    pub fn gather(allow: &[Cred], source: &Source) -> io::Result<Keyring> {
        let mut held = Vec::new();
        for cred in Cred::ALL.into_iter().filter(|c| allow.contains(c)) {
            for var in cred.vars() {
                if let Some(value) = source.get(var)? {
                    held.push((cred, *var, value));
                    break;
                }
            }
        }
        Ok(Keyring { held })
    }

    /// Which credentials it holds, by name only: what `runner status` shows.
    pub fn creds(&self) -> Vec<Cred> {
        self.held.iter().map(|(c, _, _)| *c).collect()
    }

    /// The variables it holds and their values.
    pub fn vars(&self) -> impl Iterator<Item = (&str, &str)> {
        self.held.iter().map(|(_, k, v)| (*k, v.0.as_str()))
    }

    /// Sets a session's environment to exactly these credentials. Any other
    /// credential variable is removed, so one the guest's own environment
    /// holds can't slip past the allowlist. `Command`'s Debug output prints
    /// these values: never log the command after this.
    pub fn apply(&self, cmd: &mut Command) {
        for var in Cred::ALL.iter().flat_map(|c| c.vars()) {
            cmd.env_remove(var);
        }
        cmd.envs(self.vars());
    }

    /// The host's half of the handoff: a big-endian u32 length, then
    /// `VAR=value\0` for each credential, like /proc/<pid>/environ.
    pub fn send(&self, w: &mut impl Write) -> io::Result<()> {
        let body: Vec<u8> = self
            .vars()
            .flat_map(|(k, v)| [k.as_bytes(), b"=", v.as_bytes(), b"\0"].concat())
            .collect();
        let len = u32::try_from(body.len())
            .ok()
            .filter(|n| *n <= MAX_HANDOFF)
            .ok_or_else(|| bad("credentials too large to hand off".into()))?;
        w.write_all(&len.to_be_bytes())?;
        w.write_all(&body)?;
        w.flush()
    }

    /// The guest's half. It takes only the variables credentials arrive as,
    /// once each, so a bad or hostile handoff can't set PATH or LD_PRELOAD.
    pub fn receive(r: &mut impl Read) -> io::Result<Keyring> {
        let mut len = [0; 4];
        r.read_exact(&mut len)?;
        let len = u32::from_be_bytes(len);
        if len > MAX_HANDOFF {
            return Err(bad(format!("a credential handoff of {len} bytes")));
        }
        let mut body = vec![0; len as usize];
        r.read_exact(&mut body)?;
        let mut held: Vec<(Cred, &'static str, Secret)> = Vec::new();
        for entry in body.split_inclusive(|b| *b == 0) {
            let entry = entry
                .strip_suffix(b"\0")
                .ok_or_else(|| bad("credential entry isn't NUL-terminated".into()))?;
            let entry = std::str::from_utf8(entry).map_err(|e| bad(e.to_string()))?;
            let (var, value) = entry
                .split_once('=')
                .ok_or_else(|| bad("credential entry without =".into()))?;
            // Not echoed: a sender that lost the name would log part of a value.
            let (cred, var) = Cred::of_var(var)
                .ok_or_else(|| bad("a handoff entry isn't a credential variable".into()))?;
            if value.is_empty() || held.iter().any(|(c, _, _)| *c == cred) {
                return Err(bad(format!("{var} is empty or given twice")));
            }
            held.push((cred, var, Secret(value.to_string())));
        }
        Ok(Keyring { held })
    }
}

fn bad(why: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, why)
}
