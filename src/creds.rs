// Credentials come in per boot and never land on a VM's disk (docs/design.md,
// Guards), so no disk generation or fork holds a key.
//
// The host reads each one from a systemd credential (`LoadCredential=` in the
// VM's unit puts a file in $CREDENTIALS_DIRECTORY) or else its own environment,
// never from a file in the repo or a VM's directory. It keeps the ones the VM's
// allowlist names and, after boot, sends them to `runner-guest` over vsock.
// The guest holds them in memory and sets them in the environment of each
// session and run it starts. Nothing here writes a value to a file or prints one.
use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::str::FromStr;

/// A credential a VM's allowlist can name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
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

    // The variables it can arrive as, in the order the host looks; a VM gets
    // only the first one found. The OAuth token comes first: an API key would
    // win inside Claude Code and bill per token.
    fn vars(self) -> &'static [&'static str] {
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

impl FromStr for Cred {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        Cred::ALL
            .into_iter()
            .find(|c| c.name() == s)
            .ok_or_else(|| format!("{s:?} isn't a credential: use claude or github"))
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
    pub fn new(dir: Option<PathBuf>, env: impl IntoIterator<Item = (String, String)>) -> Self {
        let env = env.into_iter().collect();
        Source { dir, env }
    }

    /// This process's: what systemd loaded for its unit, then its environment.
    pub fn from_process() -> Self {
        let dir = std::env::var_os("CREDENTIALS_DIRECTORY").map(PathBuf::from);
        let known = |k: &String| Cred::of_var(k).is_some();
        Source::new(dir, std::env::vars().filter(|(k, _)| known(k)))
    }

    // Trimmed, since a token pasted into a file usually ends in a newline;
    // blank counts as absent.
    fn get(&self, var: &str) -> io::Result<Option<Secret>> {
        let from_dir = match &self.dir {
            Some(dir) => match fs::read_to_string(dir.join(var)) {
                Ok(v) => Some(v),
                Err(e) if e.kind() == io::ErrorKind::NotFound => None,
                Err(e) => return Err(io::Error::new(e.kind(), format!("credential {var}: {e}"))),
            },
            None => None,
        };
        let value = from_dir.or_else(|| self.env.get(var).cloned());
        let value = value
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());
        if value.as_ref().is_some_and(|v| v.contains('\0')) {
            return Err(bad(format!("credential {var} holds a NUL byte")));
        }
        Ok(value.map(Secret))
    }
}

// Past any real token, small enough that a bad length can't exhaust memory.
const MAX_HANDOFF: u32 = 64 * 1024;

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

    /// The variables to set in a session's environment, as `Command::envs` takes them.
    pub fn vars(&self) -> impl Iterator<Item = (&str, &str)> {
        self.held.iter().map(|(_, k, v)| (*k, v.0.as_str()))
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
            let (cred, var) =
                Cred::of_var(var).ok_or_else(|| bad(format!("{var:?} isn't a credential")))?;
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
