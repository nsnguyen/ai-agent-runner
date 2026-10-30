// Credentials from the host's source to a session's environment in the guest,
// over a Unix socket in place of vsock: no KVM needed. Sessions run with a
// cleared environment, so a token this process happens to hold can't leak in.
use proptest::prelude::*;
use runner::creds::{Cred, DEFAULT, Keyring, Source};
use std::fs;
use std::io::Cursor;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::Command;

const TOKEN: &str = "fake-claude-token-for-tests";

fn env(pairs: &[(&str, &str)]) -> Source {
    let pairs = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string()));
    Source::new(None, pairs)
}

// What the guest holds once the host has handed `allow` across.
fn handed(source: &Source, allow: &[Cred]) -> Keyring {
    let (mut host, mut guest) = UnixStream::pair().unwrap();
    Keyring::gather(allow, source)
        .unwrap()
        .send(&mut host)
        .unwrap();
    Keyring::receive(&mut guest).unwrap()
}

// A session the guest starts: `sh -c script` with the keyring's variables.
fn session(keyring: &Keyring, dir: &Path, script: &str) -> String {
    let out = Command::new("sh")
        .args(["-c", script])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", dir)
        .env("TMPDIR", dir)
        .current_dir(dir)
        .envs(keyring.vars())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn files_holding(dir: &Path, needle: &[u8]) -> Vec<String> {
    let mut found = Vec::new();
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            found.extend(files_holding(&path, needle));
        } else if fs::read(&path)
            .unwrap()
            .windows(needle.len())
            .any(|w| w == needle)
        {
            found.push(path.display().to_string());
        }
    }
    found
}

#[test]
fn a_session_has_the_claude_token_in_its_environment() {
    let dir = tempfile::tempdir().unwrap();
    let keyring = handed(&env(&[("CLAUDE_CODE_OAUTH_TOKEN", TOKEN)]), DEFAULT);
    let seen = session(
        &keyring,
        dir.path(),
        "printf %s \"$CLAUDE_CODE_OAUTH_TOKEN\"",
    );
    assert_eq!(seen, TOKEN);
}

#[test]
fn the_guest_never_writes_a_credential_to_a_file() {
    let dir = tempfile::tempdir().unwrap();
    let keyring = handed(&env(&[("CLAUDE_CODE_OAUTH_TOKEN", TOKEN)]), DEFAULT);
    session(&keyring, dir.path(), "echo work > out.txt; mkdir -p .cache");
    assert_eq!(
        files_holding(dir.path(), TOKEN.as_bytes()),
        Vec::<String>::new()
    );
}

#[test]
fn a_vm_the_allowlist_leaves_out_gets_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let source = env(&[("CLAUDE_CODE_OAUTH_TOKEN", TOKEN), ("GH_TOKEN", "ghp-x")]);
    let script = "printf '%s|%s' \"$CLAUDE_CODE_OAUTH_TOKEN\" \"$GH_TOKEN\"";

    let none = handed(&source, &[]);
    assert_eq!(none.creds(), vec![]);
    assert_eq!(session(&none, dir.path(), script), "|");

    let default = handed(&source, DEFAULT);
    assert_eq!(default.creds(), vec![Cred::Claude]);
    assert_eq!(session(&default, dir.path(), script), format!("{TOKEN}|"));

    let both = handed(&source, &[Cred::Claude, Cred::Github]);
    assert_eq!(both.creds(), vec![Cred::Claude, Cred::Github]);
    assert_eq!(session(&both, dir.path(), script), format!("{TOKEN}|ghp-x"));
}

#[test]
fn github_is_off_by_default() {
    assert_eq!(DEFAULT, &[Cred::Claude]);
}

#[test]
fn a_systemd_credential_wins_over_the_environment() {
    let creds = tempfile::tempdir().unwrap();
    fs::write(
        creds.path().join("CLAUDE_CODE_OAUTH_TOKEN"),
        "from-systemd\n",
    )
    .unwrap();
    let pairs = [(
        "CLAUDE_CODE_OAUTH_TOKEN".to_string(),
        "from-env".to_string(),
    )];
    let source = Source::new(Some(creds.path().to_path_buf()), pairs);
    let keyring = Keyring::gather(DEFAULT, &source).unwrap();
    let vars: Vec<_> = keyring.vars().collect();
    assert_eq!(vars, vec![("CLAUDE_CODE_OAUTH_TOKEN", "from-systemd")]);
}

#[test]
fn the_oauth_token_is_preferred_and_an_api_key_is_the_fallback() {
    let both = env(&[
        ("ANTHROPIC_API_KEY", "key"),
        ("CLAUDE_CODE_OAUTH_TOKEN", TOKEN),
    ]);
    let vars: Vec<_> = Keyring::gather(DEFAULT, &both)
        .unwrap()
        .vars()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    assert_eq!(vars, vec![("CLAUDE_CODE_OAUTH_TOKEN".into(), TOKEN.into())]);

    let key = Keyring::gather(DEFAULT, &env(&[("ANTHROPIC_API_KEY", "key")])).unwrap();
    assert_eq!(
        key.vars().collect::<Vec<_>>(),
        vec![("ANTHROPIC_API_KEY", "key")]
    );
    assert_eq!(key.creds(), vec![Cred::Claude]);
}

#[test]
fn a_credential_the_host_lacks_or_left_empty_is_skipped() {
    let keyring = Keyring::gather(DEFAULT, &env(&[("CLAUDE_CODE_OAUTH_TOKEN", " \n")])).unwrap();
    assert_eq!(keyring.creds(), vec![]);
    let keyring = Keyring::gather(&[Cred::Github], &env(&[])).unwrap();
    assert_eq!(keyring.creds(), vec![]);
}

#[test]
fn debug_output_never_shows_a_value() {
    let keyring = handed(&env(&[("CLAUDE_CODE_OAUTH_TOKEN", TOKEN)]), DEFAULT);
    let shown = format!("{keyring:?}");
    assert!(!shown.contains(TOKEN), "{shown}");
    assert!(shown.contains("CLAUDE_CODE_OAUTH_TOKEN"), "{shown}");
}

#[test]
fn cred_names_parse_and_print() {
    assert_eq!("claude".parse::<Cred>(), Ok(Cred::Claude));
    assert_eq!("github".parse::<Cred>(), Ok(Cred::Github));
    assert!("aws".parse::<Cred>().is_err());
    assert_eq!(Cred::Github.to_string(), "github");
}

fn frame(body: &[u8]) -> Vec<u8> {
    let mut out = (body.len() as u32).to_be_bytes().to_vec();
    out.extend_from_slice(body);
    out
}

#[test]
fn the_guest_takes_only_credential_variables() {
    for body in [
        &b"LD_PRELOAD=/tmp/x.so\0"[..],
        b"PATH=/tmp\0",
        b"CLAUDE_CODE_OAUTH_TOKEN=\0",
        b"CLAUDE_CODE_OAUTH_TOKEN=a\0CLAUDE_CODE_OAUTH_TOKEN=b\0",
        b"CLAUDE_CODE_OAUTH_TOKEN=unterminated",
        b"CLAUDE_CODE_OAUTH_TOKEN",
    ] {
        let got = Keyring::receive(&mut Cursor::new(frame(body)));
        assert!(got.is_err(), "took {:?}", String::from_utf8_lossy(body));
    }
}

#[test]
fn an_oversized_or_truncated_handoff_is_refused() {
    let huge = (1u32 << 20).to_be_bytes();
    assert!(Keyring::receive(&mut Cursor::new(huge)).is_err());
    let mut cut = frame(b"GH_TOKEN=ghp-x\0");
    cut.pop();
    assert!(Keyring::receive(&mut Cursor::new(cut)).is_err());
    assert!(Keyring::receive(&mut Cursor::new([0u8, 0])).is_err());
}

#[test]
fn an_empty_handoff_is_an_empty_keyring() {
    let keyring = Keyring::receive(&mut Cursor::new(frame(b""))).unwrap();
    assert_eq!(keyring.creds(), vec![]);
}

proptest! {
    #[test]
    fn handoff_round_trips(
        claude in proptest::option::of("[^\0\\s][^\0]{0,200}[^\0\\s]"),
        github in proptest::option::of("[^\0\\s]{1,100}"),
    ) {
        let mut pairs = Vec::new();
        if let Some(v) = &claude { pairs.push(("CLAUDE_CODE_OAUTH_TOKEN".to_string(), v.clone())); }
        if let Some(v) = &github { pairs.push(("GH_TOKEN".to_string(), v.clone())); }
        let sent = Keyring::gather(&[Cred::Claude, Cred::Github], &Source::new(None, pairs)).unwrap();
        let mut wire = Vec::new();
        sent.send(&mut wire).unwrap();
        let got = Keyring::receive(&mut Cursor::new(&wire)).unwrap();
        prop_assert_eq!(&got, &sent);
        // Every byte short of the whole frame is refused, never misread.
        for cut in 0..wire.len() {
            prop_assert!(Keyring::receive(&mut Cursor::new(&wire[..cut])).is_err());
        }
    }

    #[test]
    fn receive_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..64)) {
        let _ = Keyring::receive(&mut Cursor::new(bytes));
    }
}
