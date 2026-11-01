// Services: the unit `runner service add` writes, how it quotes a command for
// systemd, and the scripts it runs as root in the guest. Those run here, on
// a directory of the test's own in place of /etc/systemd/system, with
// systemctl and journalctl faked. The real systemd is the boot test's
// (tests/kvm.rs).
mod common;

use common::{me, script, text};
use proptest::prelude::*;
use runner::exec::{self, Captured};
use runner::services::{self, Error, Guest, Service, Spec};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::TempDir;

fn spec(name: &str, argv: &[&str]) -> Spec {
    Spec {
        name: name.to_string(),
        argv: argv.iter().map(|s| s.to_string()).collect(),
        dir: "/home/agent/work".to_string(),
        user: "agent".to_string(),
    }
}

#[test]
fn renders_a_unit_that_runs_as_the_user_from_every_boot() {
    let unit = services::render_unit(&spec("web", &["python3", "-m", "http.server", "8080"]));
    assert_eq!(
        unit.unwrap(),
        r#"# Made by `runner service add web`; `runner service rm` removes it.
[Unit]
Description=runner service web
After=network.target
StartLimitIntervalSec=0

[Service]
Type=exec
User=agent
WorkingDirectory=/home/agent/work
ExecStart="python3" "-m" "http.server" "8080"
Restart=always
RestartSec=1
TimeoutStopSec=10

[Install]
WantedBy=multi-user.target
"#
    );
    assert_eq!(services::unit_name("web"), "runner-svc-web.service");
}

#[test]
fn quotes_each_word_so_systemd_takes_it_as_it_is() {
    let argv = [
        "sh",
        "-c",
        "echo \"$HOME\" 100% 'done'; exit",
        "a\\b",
        "",
        ";",
        "two\nlines\tand\x07bell",
        "ünï",
    ];
    let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
    assert_eq!(
        services::exec_start(&argv),
        r#""sh" "-c" "echo \"$$HOME\" 100%% 'done'; exit" "a\\b" "" ";" "two\nlines\tand\x07bell" "ünï""#
    );
    assert_eq!(
        services::parse_exec_start(&services::exec_start(&argv)),
        Some(argv)
    );
}

proptest! {
    #[test]
    fn exec_start_reads_back_as_it_was(
        program in "[a-z][a-z0-9_.+-]{0,15}|/[a-z0-9/._-]{1,30}",
        args in proptest::collection::vec(any::<String>(), 0..6),
    ) {
        let argv: Vec<String> = std::iter::once(program).chain(args).collect();
        let line = services::exec_start(&argv);
        prop_assert!(!line.contains('\n') && !line.ends_with('\\'), "{line}");
        prop_assert_eq!(services::parse_exec_start(&line), Some(argv));
    }

    #[test]
    fn a_unit_s_lines_stay_its_own(
        dir in "/[^\\x00-\\x1f\\x7f\"'\\\\]{0,40}",
        args in proptest::collection::vec(any::<String>(), 0..4),
    ) {
        let mut s = spec("web", &["serve"]);
        s.argv.extend(args);
        s.dir = dir;
        // Whatever it's given, a unit renders with exactly these keys, or not at all.
        if let Ok(unit) = services::render_unit(&s) {
            let keys: Vec<&str> = unit
                .lines()
                .filter(|l| !l.starts_with('#') && !l.starts_with('[') && !l.is_empty())
                .map(|l| l.split_once('=').unwrap().0)
                .collect();
            prop_assert_eq!(keys, [
                "Description", "After", "StartLimitIntervalSec", "Type", "User", "WorkingDirectory", "ExecStart",
                "Restart", "RestartSec", "TimeoutStopSec", "WantedBy",
            ]);
        }
    }
}

// systemd 255's own reading of a unit, as `systemd-analyze verify` dumps it:
// every word as given. `$$` stays doubled there, since systemd replaces
// variables only as it runs the command.
#[test]
fn systemd_reads_each_word_as_given() {
    let dir = tempfile::tempdir().unwrap();
    let argv = [
        "sh",
        "-c",
        "echo \"$HOME\" 100% 'done'; exit",
        "a\\b",
        "",
        ";",
        "two\nlines\tand\x07bell",
        "ünï",
    ];
    let s = Spec {
        dir: "/srv/100% x".to_string(),
        user: me(),
        ..spec("web", &argv)
    };
    let unit = dir.path().join(services::unit_name("web"));
    fs::write(&unit, services::render_unit(&s).unwrap()).unwrap();
    let out = Command::new("systemd-analyze")
        .args(["verify", "--man=no"])
        .arg(&unit)
        .env("SYSTEMD_LOG_LEVEL", "debug")
        .output()
        .expect("systemd-analyze, from systemd, to be installed");
    let dump = text(&out.stderr) + &text(&out.stdout);
    let field = |key: &str| {
        let line = dump.lines().find_map(|l| l.trim().strip_prefix(key));
        line.unwrap_or_else(|| panic!("no {key} in:\n{dump}"))
            .to_string()
    };
    assert_eq!(
        field("Command Line: "),
        r#"sh -c "echo \"\$\$HOME\" 100% 'done'; exit" "a\\b" "" ";" "two\nlines\tand\abell" ünï"#
    );
    assert_eq!(field("WorkingDirectory: "), "/srv/100% x");
}

#[test]
fn a_relative_program_runs_from_the_dir() {
    let exec_line = |argv: &[&str], dir: &str| {
        let mut s = spec("web", argv);
        s.dir = dir.to_string();
        let unit = services::render_unit(&s).unwrap();
        let line = unit.lines().find(|l| l.starts_with("ExecStart=")).unwrap();
        line.to_string()
    };
    assert_eq!(
        exec_line(&["./serve", "--port", "3000"], "/home/agent/work/app"),
        r#"ExecStart="/home/agent/work/app/serve" "--port" "3000""#
    );
    assert_eq!(
        exec_line(&["node_modules/.bin/vite"], "/srv/./app/"),
        r#"ExecStart="/srv/app/node_modules/.bin/vite""#
    );
    assert_eq!(
        exec_line(&["/usr/bin/python3"], "/"),
        r#"ExecStart="/usr/bin/python3""#
    );
    // WorkingDirectory keeps a % as systemd reads it.
    let mut s = spec("web", &["serve"]);
    s.dir = "/home/agent/100% done".to_string();
    let unit = services::render_unit(&s).unwrap();
    assert!(
        unit.contains("\nWorkingDirectory=/home/agent/100%% done\n"),
        "{unit}"
    );
}

#[test]
fn refuses_what_systemd_would_read_otherwise() {
    let bad = |s: Spec| services::render_unit(&s).unwrap_err().to_string();
    for name in ["", "-web", "a.b", "a b", "web/1", &"x".repeat(33)] {
        let why = bad(spec(name, &["serve"]));
        assert!(why.contains("isn't a service name"), "{name:?}: {why}");
    }
    assert!(bad(spec("web", &[])).contains("no command"));
    // systemd reads these as prefixes, variables, specifiers or quotes in a program.
    for program in [
        "", "-serve", "@serve", "+serve", "!serve", ":serve", ";", "a$b", "100%", "it's", "a\\b",
        "a\nb", ".", "..",
    ] {
        let why = bad(spec("web", &[program, "x"]));
        assert!(why.contains("program"), "{program:?}: {why}");
    }
    // A relative program's path takes the directory's $ or % in with it.
    for dir in ["/home/agent/a$b", "/home/agent/100%"] {
        let mut s = spec("web", &["./serve"]);
        s.dir = dir.to_string();
        let why = bad(s.clone());
        assert!(why.contains("program"), "{dir:?}: {why}");
        s.argv = vec!["serve".to_string()];
        assert!(services::render_unit(&s).is_ok(), "{dir:?}");
    }
    for dir in [
        "work",
        "",
        "/home/../etc",
        "/a\nb",
        "/trailing ",
        "/back\\slash",
        "/q\"uote",
    ] {
        let mut s = spec("web", &["serve"]);
        s.dir = dir.to_string();
        let why = bad(s);
        assert!(why.contains("directory"), "{dir:?}: {why}");
    }
    for user in ["", "-agent", "a b", "a:b", "a\nb", &"u".repeat(33)] {
        let mut s = spec("web", &["serve"]);
        s.user = user.to_string();
        let why = bad(s);
        assert!(why.contains("user"), "{user:?}: {why}");
    }
}

// The guest, here: a unit directory of its own, and systemctl and journalctl
// that keep what's asked of them in a log. systemctl keeps each unit's state
// in a file of its own; `fail` makes the next start fail, with what it holds,
// if anything, as ExecMainStatus. Like systemd 255's, `enable --now` says a
// start failed but exits 0; only `start` exits 1. journalctl says what
// systemd 255 does when a start fails, which never names the program.
struct Here {
    dir: TempDir,
}

impl Here {
    fn new() -> Here {
        let dir = tempfile::tempdir().unwrap();
        for d in ["units", "bin", "state"] {
            fs::create_dir(dir.path().join(d)).unwrap();
        }
        script(
            &dir.path().join("bin/systemctl"),
            r#"echo "systemctl $*" >> "$LOG"
for unit; do :; done
case "$1" in
  show)
    if [ "$3" = ExecMainStatus ]; then
      if [ -s "$STATE/fail" ]; then cat "$STATE/fail"; else echo 0; fi
    elif [ -f "$STATE/$unit" ]; then cat "$STATE/$unit"
    else printf 'ActiveState=inactive\nSubState=dead\n'; fi ;;
  start|enable)
    [ "$1" = enable ] && [ "$2" != --now ] && exit 0
    if [ -e "$STATE/fail" ]; then
      echo "Job for $unit failed." >&2
      [ "$1" = start ] && exit 1
      exit 0
    fi
    printf 'ActiveState=active\nSubState=running\n' > "$STATE/$unit" ;;
  disable) rm -f "$STATE/$unit" ;;
esac
"#,
        );
        script(
            &dir.path().join("bin/journalctl"),
            r#"echo "journalctl $*" >> "$LOG"
echo "$2: Failed with result 'exit-code'."
echo "Failed to start $2 - runner service."
"#,
        );
        Here { dir }
    }

    fn path(&self, p: &str) -> PathBuf {
        self.dir.path().join(p)
    }

    fn calls(&self) -> Vec<String> {
        let log = fs::read_to_string(self.path("log")).unwrap_or_default();
        log.lines().map(String::from).collect()
    }

    fn set(&self, unit: &str, state: &str, sub: &str) {
        let line = format!("ActiveState={state}\nSubState={sub}\n");
        fs::write(self.path("state").join(unit), line).unwrap();
    }

    fn spec(&self, name: &str, argv: &[&str]) -> Spec {
        Spec {
            dir: self.dir.path().to_str().unwrap().to_string(),
            user: me(),
            ..spec(name, argv)
        }
    }

    // As the guest would: argv as it is, PATH its own, stdin given, then closed.
    fn sh(&self, argv: &[String], stdin: &[u8]) -> std::process::Output {
        let path = format!("{}:/usr/bin:/bin", self.path("bin").display());
        let mut child = Command::new(&argv[0])
            .args(&argv[1..])
            .env_clear()
            .env("PATH", path)
            .env("LOG", self.path("log"))
            .env("STATE", self.path("state"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        // A script that exits first doesn't read it, as the guest allows.
        let _ = child.stdin.take().unwrap().write_all(stdin);
        child.wait_with_output().unwrap()
    }
}

impl Guest for Here {
    fn vm(&self) -> &str {
        "dev"
    }

    fn unit_dir(&self) -> String {
        self.path("units").to_str().unwrap().to_string()
    }

    fn run(&self, argv: &[String], stdin: &[u8]) -> exec::Result<Captured> {
        let out = self.sh(argv, stdin);
        Ok(Captured {
            exit_code: out.status.code(),
            signal: None,
            stdout: text(&out.stdout),
            stdout_truncated: false,
            stderr: text(&out.stderr),
            stderr_truncated: false,
            timed_out: false,
        })
    }
}

fn unit_file(here: &Here, name: &str) -> PathBuf {
    Path::new(&here.unit_dir()).join(services::unit_name(name))
}

#[test]
fn add_writes_the_unit_starts_it_and_lists_it() {
    let here = Here::new();
    let s = here.spec("web", &["python3", "-m", "http.server", "8080"]);
    let added = services::add(&here, &s).unwrap();
    assert_eq!(
        fs::read_to_string(unit_file(&here, "web")).unwrap(),
        services::render_unit(&s).unwrap()
    );
    let unit = "runner-svc-web.service";
    assert_eq!(
        here.calls(),
        [
            "systemctl daemon-reload".to_string(),
            format!("systemctl enable {unit}"),
            format!("systemctl start {unit}"),
            format!("systemctl show -p ActiveState -p SubState {unit}"),
        ]
    );
    assert_eq!(
        added,
        Service {
            name: "web".to_string(),
            unit: unit.to_string(),
            argv: s.argv.clone(),
            dir: s.dir.clone(),
            user: me(),
            state: "active".to_string(),
            sub_state: "running".to_string(),
        }
    );
    assert_eq!(services::list(&here).unwrap(), [added]);
}

#[test]
fn add_refuses_a_name_that_s_taken() {
    let here = Here::new();
    services::add(&here, &here.spec("web", &["serve"])).unwrap();
    let before = fs::read_to_string(unit_file(&here, "web")).unwrap();
    let again = services::add(&here, &here.spec("web", &["other"]));
    assert!(matches!(again, Err(Error::Exists { .. })), "{again:?}");
    assert_eq!(
        again.unwrap_err().to_string(),
        "dev already has a service named web: remove it first"
    );
    assert_eq!(fs::read_to_string(unit_file(&here, "web")).unwrap(), before);
}

#[test]
fn a_service_that_won_t_start_leaves_nothing_behind() {
    let here = Here::new();
    fs::write(here.path("state/fail"), "").unwrap();
    let got = services::add(&here, &here.spec("web", &["serve", "--port", "8 0"]));
    let Err(err @ Error::NotStarted { .. }) = got else {
        panic!("{got:?}")
    };
    let journal = "runner-svc-web.service: Failed with result 'exit-code'.\n\
        Failed to start runner-svc-web.service - runner service.";
    assert_eq!(
        err.to_string(),
        format!(
            "web didn't start in dev, so it was removed. \
            systemd couldn't start serve --port '8 0':\n{journal}"
        )
    );
    assert!(!unit_file(&here, "web").exists());
    let calls = here.calls();
    assert!(
        calls
            .iter()
            .any(|c| c.starts_with("journalctl -u runner-svc-web.service")),
        "{calls:?}"
    );
    assert!(
        calls.contains(&"systemctl disable --now runner-svc-web.service".to_string()),
        "{calls:?}"
    );
    assert_eq!(services::list(&here).unwrap(), []);
}

// Without the journal's say, systemctl's.
#[test]
fn a_start_that_fails_says_why_even_with_the_journal_quiet() {
    let here = Here::new();
    fs::write(here.path("state/fail"), "").unwrap();
    script(&here.path("bin/journalctl"), "exit 0\n");
    let got = services::add(&here, &here.spec("web", &["nope"]));
    let Err(Error::NotStarted { why, .. }) = got else {
        panic!("{got:?}")
    };
    assert_eq!(why, "Job for runner-svc-web.service failed.");
    assert!(!unit_file(&here, "web").exists());
}

// systemd's 203/EXEC: its journal says only that, so the error names the
// program, and where it was looked for.
#[test]
fn a_program_that_can_t_be_run_is_named() {
    let here = Here::new();
    fs::write(here.path("state/fail"), "203\n").unwrap();
    let got = services::add(&here, &here.spec("nope", &["no-such-program", "-v"]));
    let Err(err @ Error::CantRun { .. }) = got else {
        panic!("{got:?}")
    };
    let journal = "runner-svc-nope.service: Failed with result 'exit-code'.\n\
        Failed to start runner-svc-nope.service - runner service.";
    assert_eq!(
        err.to_string(),
        format!(
            "nope didn't start in dev, so it was removed. \
            no-such-program couldn't be run: it isn't in PATH, or isn't executable.\n{journal}"
        )
    );
    assert!(!unit_file(&here, "nope").exists());
    assert_eq!(services::list(&here).unwrap(), []);

    // A relative program is named as it was run: from the directory.
    let got = services::add(&here, &here.spec("nope", &["./serve"]));
    let why = got.unwrap_err().to_string();
    let serve = here.path("serve");
    let named = format!(
        "{} couldn't be run: it isn't there, or isn't executable.",
        serve.display()
    );
    assert!(why.contains(&named), "{why}");
}

#[test]
fn add_refuses_a_dir_or_user_that_isn_t_there() {
    let here = Here::new();
    let mut s = here.spec("web", &["serve"]);
    s.dir = here.path("nowhere").to_str().unwrap().to_string();
    let why = services::add(&here, &s).unwrap_err().to_string();
    assert_eq!(why, format!("no directory {} in dev", s.dir));
    let mut s = here.spec("web", &["serve"]);
    s.user = "no-such-user-here".to_string();
    let why = services::add(&here, &s).unwrap_err().to_string();
    assert_eq!(why, "no user no-such-user-here in dev");
    assert!(!unit_file(&here, "web").exists());
    assert_eq!(here.calls(), Vec::<String>::new());
}

#[test]
fn list_gives_each_service_s_state_by_name() {
    let here = Here::new();
    assert_eq!(services::list(&here).unwrap(), []);
    services::add(&here, &here.spec("web", &["serve", "--port", "80 80"])).unwrap();
    services::add(&here, &here.spec("api", &["sh", "-c", "exec api"])).unwrap();
    here.set("runner-svc-api.service", "activating", "auto-restart");
    // A unit that isn't the runner's isn't listed.
    fs::write(
        Path::new(&here.unit_dir()).join("other.service"),
        "[Service]\n",
    )
    .unwrap();
    let got = services::list(&here).unwrap();
    let rows: Vec<_> = got
        .iter()
        .map(|s| {
            (
                s.name.as_str(),
                s.state.as_str(),
                s.sub_state.as_str(),
                s.argv.clone(),
            )
        })
        .collect();
    assert_eq!(
        rows,
        [
            (
                "api",
                "activating",
                "auto-restart",
                vec!["sh".to_string(), "-c".into(), "exec api".into()]
            ),
            (
                "web",
                "active",
                "running",
                vec!["serve".to_string(), "--port".into(), "80 80".into()]
            ),
        ]
    );
}

#[test]
fn rm_stops_the_service_and_removes_its_unit() {
    let here = Here::new();
    services::add(&here, &here.spec("web", &["serve"])).unwrap();
    let added = here.calls().len();
    services::remove(&here, "web").unwrap();
    assert!(!unit_file(&here, "web").exists());
    let unit = "runner-svc-web.service";
    assert_eq!(
        here.calls()[added..],
        [
            format!("systemctl disable --now {unit}"),
            "systemctl daemon-reload".to_string(),
            format!("systemctl reset-failed {unit}"),
        ]
    );
    assert_eq!(services::list(&here).unwrap(), []);
    let again = services::remove(&here, "web");
    assert!(matches!(again, Err(Error::NoService { .. })), "{again:?}");
    assert_eq!(again.unwrap_err().to_string(), "no service web in dev");
}

#[test]
fn logs_read_the_unit_s_journal() {
    let here = Here::new();
    services::add(&here, &here.spec("web", &["serve"])).unwrap();
    let argv = services::logs_argv(&here.unit_dir(), "dev", "web", true, Some(5)).unwrap();
    let out = here.sh(&argv, b"");
    assert!(out.status.success(), "{}", text(&out.stderr));
    let calls = here.calls();
    assert_eq!(
        calls.last().unwrap(),
        "journalctl -u runner-svc-web.service --no-pager --lines=5 --follow"
    );
    let argv = services::logs_argv(&here.unit_dir(), "dev", "api", false, None).unwrap();
    let out = here.sh(&argv, b"");
    assert_eq!(out.status.code(), Some(3));
    assert_eq!(text(&out.stderr), "runner: no service api in dev\n");
    let bad = services::logs_argv(&here.unit_dir(), "dev", "a b", false, None);
    assert!(
        bad.unwrap_err()
            .to_string()
            .contains("isn't a service name")
    );
}

#[test]
fn the_cli_refuses_a_bad_service_before_touching_the_vm() {
    let dir = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_runner"))
        .env("RUNNER_ROOT", dir.path())
        .args(["service", "add", "nope", "a b", "--", "serve"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = text(&out.stderr);
    assert!(err.contains("\"a b\" isn't a service name"), "{err}");
    let out = Command::new(env!("CARGO_BIN_EXE_runner"))
        .env("RUNNER_ROOT", dir.path())
        .args(["service", "list", "nope"])
        .output()
        .unwrap();
    assert_eq!(text(&out.stderr), "runner: no VM named nope\n");
}

#[test]
fn rows_show_the_command_as_a_shell_would_take_it() {
    let s = Service {
        name: "web".to_string(),
        unit: "runner-svc-web.service".to_string(),
        argv: ["sh", "-c", "npm run dev", "it's"]
            .map(String::from)
            .to_vec(),
        dir: "/home/agent/work".to_string(),
        user: "agent".to_string(),
        state: "active".to_string(),
        sub_state: "running".to_string(),
    };
    assert_eq!(
        services::row(&s),
        r#"web  active/running  /home/agent/work  sh -c 'npm run dev' 'it'\''s'"#
    );
}
