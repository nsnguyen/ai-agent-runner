// Credentials from the host's source to a session's environment in the guest,
// over a Unix socket in place of vsock: no KVM needed. Sessions start from a
// cleared environment, so a token this process happens to hold can't leak in.
use proptest::prelude::*;
use runner::creds::{Cred, DEFAULT, Keyring, MAX_HANDOFF, Source};
use runner::store::Store;
use std::ffi::OsStr;
use std::fs;
use std::io::{Cursor, ErrorKind};
use std::os::unix::ffi::OsStrExt;
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

// A session the guest starts: `sh -c script`, from a guest whose own
// environment already holds stale credentials that `apply` must not pass on.
fn session(keyring: &Keyring, dir: &Path, script: &str) -> String {
    let mut cmd = Command::new("sh");
    cmd.args(["-c", script])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", dir)
        .env("TMPDIR", dir)
        .env("CLAUDE_CODE_OAUTH_TOKEN", "stale")
        .env("ANTHROPIC_API_KEY", "stale")
        .env("GH_TOKEN", "stale")
        .current_dir(dir);
    keyring.apply(&mut cmd);
    let out = cmd.output().unwrap();
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

// Every step short of a boot: the allowlist, gathering from systemd's
// directory, the handoff and a session. Only the credentials directory,
// which systemd keeps on a tmpfs outside the VM, may hold the token.
#[test]
fn the_guest_never_writes_a_credential_to_a_file() {
    let creds = tempfile::tempdir().unwrap();
    fs::write(creds.path().join("CLAUDE_CODE_OAUTH_TOKEN"), TOKEN).unwrap();
    let source = Source::new(Some(creds.path().to_path_buf()), []);
    let root = tempfile::tempdir().unwrap();
    let store = Store::new(root.path());
    let disk = root.path().join("rootfs.ext4");
    fs::write(&disk, b"rootfs").unwrap();
    store.import("dev", &disk).unwrap();
    let allow = store.set_creds("dev", DEFAULT).unwrap();

    let keyring = handed(&source, &allow);
    let seen = session(
        &keyring,
        root.path(),
        "printf %s \"$CLAUDE_CODE_OAUTH_TOKEN\" | wc -c",
    );
    assert_eq!(seen.trim(), TOKEN.len().to_string());
    assert_eq!(
        files_holding(root.path(), TOKEN.as_bytes()),
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

// An empty or relative $CREDENTIALS_DIRECTORY would read the token from
// wherever the runner happens to be, a repo checkout included.
#[test]
fn only_an_absolute_credentials_directory_counts() {
    let planted = tempfile::tempdir().unwrap();
    fs::write(planted.path().join("CLAUDE_CODE_OAUTH_TOKEN"), "planted").unwrap();
    // The same directory, named relative to where the test runs.
    let cwd = std::env::current_dir().unwrap();
    let up = "../".repeat(cwd.components().count() - 1);
    let relative = Path::new(&up).join(planted.path().strip_prefix("/").unwrap());
    assert_eq!(
        fs::read_to_string(relative.join("CLAUDE_CODE_OAUTH_TOKEN")).unwrap(),
        "planted"
    );
    let pairs = [(
        "CLAUDE_CODE_OAUTH_TOKEN".to_string(),
        "from-env".to_string(),
    )];
    let source = Source::new(Some(relative), pairs);
    let keyring = Keyring::gather(DEFAULT, &source).unwrap();
    assert_eq!(
        keyring.vars().collect::<Vec<_>>(),
        vec![("CLAUDE_CODE_OAUTH_TOKEN", "from-env")]
    );
}

#[test]
fn a_blank_systemd_credential_falls_through_to_the_environment() {
    let creds = tempfile::tempdir().unwrap();
    fs::write(creds.path().join("CLAUDE_CODE_OAUTH_TOKEN"), "\n").unwrap();
    let pairs = [(
        "CLAUDE_CODE_OAUTH_TOKEN".to_string(),
        "from-env".to_string(),
    )];
    let source = Source::new(Some(creds.path().to_path_buf()), pairs);
    let keyring = Keyring::gather(DEFAULT, &source).unwrap();
    assert_eq!(
        keyring.vars().collect::<Vec<_>>(),
        vec![("CLAUDE_CODE_OAUTH_TOKEN", "from-env")]
    );
}

// std::env::vars() panics on a variable that isn't UTF-8 and prints its
// value. Run in a child test process, so the bad variable stays out of ours.
#[test]
fn from_process_survives_a_variable_that_isnt_utf8() {
    if std::env::var_os("RUNNER_CREDS_CHILD").is_some() {
        let keyring = Keyring::gather(&Cred::ALL, &Source::from_process()).unwrap();
        assert_eq!(
            keyring.vars().collect::<Vec<_>>(),
            vec![("GH_TOKEN", "fine")]
        );
        return;
    }
    let out = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "from_process_survives_a_variable_that_isnt_utf8"])
        .env_clear()
        .env("RUNNER_CREDS_CHILD", "1")
        .env("ANTHROPIC_API_KEY", OsStr::from_bytes(b"bad-\xff-value"))
        .env("UNRELATED", OsStr::from_bytes(b"also-\xff-bad"))
        .env("GH_TOKEN", "fine")
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&out.stdout) + String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{said}");
    assert!(
        !said.contains("-value") && !said.contains("also-"),
        "{said}"
    );
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
    let entry = format!("GH_TOKEN={}\0", "x".repeat(MAX_HANDOFF as usize - 9));
    assert_eq!(entry.len(), MAX_HANDOFF as usize + 1);
    let got = Keyring::receive(&mut Cursor::new(frame(entry.as_bytes())));
    assert_eq!(got.unwrap_err().kind(), ErrorKind::InvalidData);
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

    // Framed with its true length, so the entry parser sees every body.
    #[test]
    fn receive_takes_only_well_formed_entries(parts in proptest::collection::vec(
        prop_oneof![
            Just(b"GH_TOKEN".to_vec()),
            Just(b"CLAUDE_CODE_OAUTH_TOKEN".to_vec()),
            Just(b"PATH".to_vec()),
            Just(b"=".to_vec()),
            Just(b"\0".to_vec()),
            proptest::collection::vec(any::<u8>(), 0..6),
        ],
        0..12,
    )) {
        if let Ok(keyring) = Keyring::receive(&mut Cursor::new(frame(&parts.concat()))) {
            let creds = keyring.creds();
            let mut unique = creds.clone();
            unique.dedup();
            prop_assert_eq!(creds, unique);
            for (var, value) in keyring.vars() {
                prop_assert!(var != "PATH" && !value.is_empty() && !value.contains('\0'));
            }
        }
    }

    #[test]
    fn receive_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..64)) {
        let _ = Keyring::receive(&mut Cursor::new(bytes));
    }
}
