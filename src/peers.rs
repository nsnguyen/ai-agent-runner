// `runner peer`: what another host may run here. The laptop's `runner hosts
// add` puts every host's key in every other host's
// ~runner/.ssh/authorized_keys behind OPTIONS, so for that key sshd runs
// `runner peer` whatever the other host asked for, and leaves what it asked
// for in SSH_ORIGINAL_COMMAND. Of that, only `runner mcp` and `runner import
// <vm> -`, each with its own flags, run, in this process and never through a
// shell. So one host drives another's VMs but gets no shell there
// (docs/design.md, Hosts trust each other).
use std::ffi::OsStr;

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
