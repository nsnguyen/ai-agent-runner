// MCP over HTTP on the host: runner-mcp-http.service as systemd reads it,
// and host/install.sh's part, which keeps RUNNER_MCP_HTTP and
// RUNNER_MCP_URL for reruns, installs the unit only with RUNNER_MCP_HTTP=1,
// and sets the host's idle time to never then, since claude.ai can't start
// a stopped host. That part runs in bash on a fake /etc, with systemctl
// logging its calls.
mod common;

use common::text;
use runner::host;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use tempfile::TempDir;

// systemd 255's own reading of the unit: no errors, and the runner serving
// on loopback as runner, in the unit's root.
#[test]
fn the_unit_passes_systemd_analyze_verify() {
    let dir = tempfile::tempdir().unwrap();
    // verify wants the program there: this build's, where the unit runs it.
    let runner = env!("CARGO_BIN_EXE_runner");
    let unit = host::MCP_HTTP_UNIT.replace("/usr/local/bin/runner", runner);
    assert_ne!(unit, host::MCP_HTTP_UNIT);
    let at = dir.path().join(host::MCP_HTTP_SERVICE);
    fs::write(&at, unit).unwrap();
    let out = Command::new("systemd-analyze")
        .args(["verify", "--man=no"])
        .arg(&at)
        .env("SYSTEMD_LOG_LEVEL", "debug")
        .output()
        .expect("systemd-analyze, from systemd, to be installed");
    let dump = text(&out.stderr) + &text(&out.stdout);
    assert!(out.status.success(), "{dump}");
    for bad in [
        "Unknown key",
        "Unknown section",
        "Failed to parse",
        "is not executable",
    ] {
        assert!(!dump.contains(bad), "{bad}:\n{dump}");
    }
    let serve = format!("Command Line: {runner} mcp --http {}", host::MCP_HTTP_ADDR);
    assert!(dump.contains(&serve), "{dump}");
    assert!(dump.contains("User: runner"), "{dump}");
    assert!(dump.contains("RUNNER_ROOT=/var/lib/runner"), "{dump}");
    assert!(dump.contains("Restart: on-failure"), "{dump}");
    // A stop or restart ends the server alone: the runs it started, which
    // setsid leaves in its cgroup, go on, as they do over SSH.
    assert!(dump.contains("KillMode: process"), "{dump}");
}

#[test]
fn the_unit_listens_on_loopback_only() {
    let addr = host::MCP_HTTP_ADDR;
    assert!(addr.starts_with("127.0.0.1:"), "{addr}");
    let line = format!("ExecStart=/usr/local/bin/runner mcp --http {addr}\n");
    assert!(host::MCP_HTTP_UNIT.contains(&line), "{line}");
}

fn install_sh() -> String {
    fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/host/install.sh")).unwrap()
}

// The if at the start of a line that opens with `opener` and holds
// `inside`, up to its fi, split at its first else or elif.
fn branches<'a>(script: &'a str, opener: &str, inside: &str) -> (&'a str, &'a str) {
    let at = script.find(inside).unwrap();
    let start = script[..at].rfind(&format!("\n{opener}")).unwrap() + 1;
    let end = at + script[at..].find("\nfi\n").unwrap() + 1;
    let whole = &script[start..end];
    let split = whole.find("\nel").map_or(whole.len(), |i| i + 1);
    assert!(start + split > at, "{inside:?} is past the else");
    whole.split_at(split)
}

#[test]
fn install_sh_writes_the_unit_only_with_mcp_over_http_on() {
    let script = install_sh();
    let unit = host::MCP_HTTP_SERVICE;
    let install = format!("install -m 0644 \"$here/{unit}\" /etc/systemd/system/{unit}\n");
    let (on, off) = branches(&script, "if [[ $mcp_http == 1 ]]; then\n", &install);
    let enable = format!("systemctl enable {unit}\n");
    let restart = format!("systemctl restart {unit}\n");
    for line in [&install, &enable, &restart] {
        assert!(on.contains(line.as_str()), "{line} isn't with it on:\n{on}");
        assert_eq!(script.matches(line.as_str()).count(), 1, "{line}");
    }
    for line in [
        format!("systemctl disable --now {unit}\n"),
        format!("rm -f /etc/systemd/system/{unit}\n"),
    ] {
        assert!(off.contains(&line), "{line} isn't with it off:\n{off}");
    }
    // Kept for reruns whether it's on or off, as net is.
    let keep = "\ninstall -m 0644 \"$tmp/mcp-http\" /etc/runner/mcp-http\n";
    assert!(script.contains(keep), "{keep}");
}

// A fake host for install.sh's MCP part: its /etc, a temp dir for $tmp, and
// a log of systemctl's calls.
struct Fake {
    dir: TempDir,
}

impl Fake {
    fn new() -> Self {
        let fake = Fake {
            dir: tempfile::tempdir().unwrap(),
        };
        fs::create_dir_all(fake.etc("runner")).unwrap();
        fs::create_dir_all(fake.etc("systemd/system")).unwrap();
        fs::create_dir_all(fake.dir.path().join("tmp")).unwrap();
        fake
    }

    fn etc(&self, path: &str) -> PathBuf {
        self.dir.path().join("etc").join(path)
    }

    fn keep(&self, name: &str, value: &str) {
        fs::write(self.etc("runner").join(name), format!("{value}\n")).unwrap();
    }

    // What /etc/runner/<name> holds, or None.
    fn kept(&self, name: &str) -> Option<String> {
        let got = fs::read_to_string(self.etc("runner").join(name)).ok()?;
        Some(got.strip_suffix('\n').unwrap_or(&got).to_string())
    }

    fn unit(&self) -> PathBuf {
        self.etc("systemd/system").join(host::MCP_HTTP_SERVICE)
    }

    fn calls(&self) -> Vec<String> {
        let log = fs::read_to_string(self.dir.path().join("calls")).unwrap_or_default();
        log.lines().map(str::to_string).collect()
    }

    // How many times systemctl was called so.
    fn count(&self, call: &str) -> usize {
        self.calls().iter().filter(|c| *c == call).count()
    }

    // install.sh with only `env` set of its settings: its say and die, the
    // lines that read the settings, its checks of them, and everything from
    // the host idle section to the end, with runuser doing nothing.
    fn install(&self, env: &[(&str, &str)]) -> Output {
        let script = install_sh();
        let at = |text: &str| script.find(text).unwrap();
        let lines = |starts: &[&str]| -> String {
            let picked = (script.lines()).filter(|l| starts.iter().any(|s| l.starts_with(s)));
            picked.map(|l| format!("{l}\n")).collect()
        };
        let checks = &script[at("bad_idle=")..at("\nmissing=()")];
        let rest = &script[at("say \"host idle: $idle\"")..];
        let d = self.dir.path().display();
        let said = lines(&["say() ", "die() "]);
        let stubs = format!(
            "systemctl() {{ echo \"systemctl $*\" >>\"{d}/calls\"; }}\nrunuser() {{ :; }}\n"
        );
        let host = concat!(env!("CARGO_MANIFEST_DIR"), "/host");
        let vars = format!("here='{host}'\ntmp={d}/tmp\nhome=/home/runner\n");
        let settings = lines(&["idle=", "reserve=", "mcp_http=", "mcp_url="]);
        let body = format!("set -euo pipefail\n{said}{stubs}{vars}{settings}{checks}\n{rest}");
        let body = body.replace("/etc/", &format!("{d}/etc/"));
        let mut bash = Command::new("bash");
        bash.arg("-c").arg(body);
        for var in [
            "RUNNER_HOST_IDLE",
            "RUNNER_HOST_RESERVE",
            "RUNNER_MCP_HTTP",
            "RUNNER_MCP_URL",
        ] {
            bash.env_remove(var);
        }
        bash.envs(env.iter().copied()).output().unwrap()
    }

    #[track_caller]
    fn ok(&self, env: &[(&str, &str)]) -> String {
        let out = self.install(env);
        let said = text(&out.stdout);
        assert!(out.status.success(), "{env:?}: {said}{}", text(&out.stderr));
        said
    }

    // install.sh died before it changed anything, saying why.
    #[track_caller]
    fn dies(&self, env: &[(&str, &str)]) -> String {
        let (calls, unit) = (self.calls(), self.unit().exists());
        let out = self.install(env);
        let err = text(&out.stderr);
        assert!(!out.status.success(), "{env:?}: {}", text(&out.stdout));
        assert!(err.starts_with("install.sh: "), "{env:?}: {err}");
        assert_eq!(self.calls(), calls, "{env:?}");
        assert_eq!(self.unit().exists(), unit, "{env:?}");
        err
    }
}

fn mcp(verb: &str) -> String {
    format!("systemctl {verb} {}", host::MCP_HTTP_SERVICE)
}

#[test]
fn mcp_over_http_installs_the_unit_and_sets_the_idle_time_to_never() {
    let h = Fake::new();
    // As aws.sh's setup leaves a host, with an idle time of an hour.
    h.keep("host-idle", "1h");
    let said = h.ok(&[("RUNNER_MCP_HTTP", "1")]);
    assert!(said.contains("never, not 1h"), "{said}");
    assert!(said.contains("claude.ai"), "{said}");
    assert_eq!(h.kept("host-idle").as_deref(), Some("never"));
    assert_eq!(h.kept("mcp-http").as_deref(), Some("1"));
    assert_eq!(fs::read_to_string(h.unit()).unwrap(), host::MCP_HTTP_UNIT);
    let calls = h.calls();
    let timer = format!("systemctl disable --now {}", host::IDLE_TIMER);
    assert!(calls.contains(&timer), "{calls:?}");
    let enable = calls.iter().position(|c| *c == mcp("enable")).unwrap();
    let restart = calls.iter().position(|c| *c == mcp("restart")).unwrap();
    assert!(enable < restart, "{calls:?}");
    let reload = calls.iter().rposition(|c| c == "systemctl daemon-reload");
    assert!(reload.unwrap() < enable, "{calls:?}");
    // Never before the polkit rule, which then leaves out the power-off.
    let script = install_sh();
    let polkit = script.find("say \"polkit rule\"").unwrap();
    assert!(script.find("  idle=never\n").unwrap() < polkit);
    let next = said.split("Next:").nth(1).unwrap();
    assert!(next.contains("runner mcp-http token"), "{said}");

    // A rerun keeps it on, and an idle time asked for, as aws.sh's setup
    // asks on a host with none, is never too.
    let said = h.ok(&[("RUNNER_HOST_IDLE", "2h")]);
    assert!(said.contains("never, not 2h"), "{said}");
    assert_eq!(h.kept("host-idle").as_deref(), Some("never"));
    assert_eq!(h.kept("mcp-http").as_deref(), Some("1"));
    assert_eq!(h.count(&mcp("restart")), 2);
}

#[test]
fn mcp_over_http_is_off_unless_set() {
    let h = Fake::new();
    h.keep("host-idle", "1h");
    let said = h.ok(&[]);
    assert_eq!(h.kept("host-idle").as_deref(), Some("1h"));
    assert_eq!(h.kept("mcp-http").as_deref(), Some("0"));
    assert_eq!(h.kept("mcp-url"), None);
    assert!(!h.unit().exists());
    let calls = h.calls();
    let unit = host::MCP_HTTP_SERVICE;
    assert!(!calls.iter().any(|c| c.contains(unit)), "{calls:?}");
    let timer = format!("systemctl enable {}", host::IDLE_TIMER);
    assert!(calls.contains(&timer), "{calls:?}");
    assert!(!said.contains("mcp-http token"), "{said}");

    // Off, as asked, keeps an idle time too.
    let h = Fake::new();
    h.ok(&[("RUNNER_MCP_HTTP", "0"), ("RUNNER_HOST_IDLE", "90s")]);
    assert_eq!(h.kept("host-idle").as_deref(), Some("90s"));
    assert_eq!(h.kept("mcp-http").as_deref(), Some("0"));
    assert!(!h.unit().exists());

    // A host with no idle time gets none written: aws.sh's setup tells a
    // host that has none by the file.
    let h = Fake::new();
    h.ok(&[("RUNNER_MCP_HTTP", "1")]);
    assert_eq!(h.kept("host-idle"), None);
    assert!(h.unit().exists());
}

#[test]
fn turning_mcp_over_http_off_stops_and_removes_the_unit() {
    let h = Fake::new();
    h.ok(&[("RUNNER_MCP_HTTP", "1")]);
    assert!(h.unit().exists());
    let said = h.ok(&[("RUNNER_MCP_HTTP", "0")]);
    assert!(!h.unit().exists());
    assert_eq!(h.kept("mcp-http").as_deref(), Some("0"));
    let calls = h.calls();
    let disable = calls.iter().position(|c| *c == mcp("disable --now"));
    let reload = calls.iter().rposition(|c| c == "systemctl daemon-reload");
    assert!(disable.unwrap() < reload.unwrap(), "{calls:?}");
    assert!(!said.contains("mcp-http token"), "{said}");
    // Its never stays, so the host stays up until told otherwise: it says how.
    assert_eq!(h.kept("host-idle"), None);
    assert!(said.contains("RUNNER_HOST_IDLE=1h"), "{said}");
    // And a rerun leaves it off, with nothing to stop.
    h.ok(&[]);
    assert_eq!(h.count(&mcp("disable --now")), 1);
}

#[test]
fn install_sh_takes_only_0_or_1() {
    for bad in ["2", "yes", "true", "on", "01", "1 ", " 1", "-1"] {
        let h = Fake::new();
        let err = h.dies(&[("RUNNER_MCP_HTTP", bad)]);
        assert!(err.contains(&format!("RUNNER_MCP_HTTP={bad}")), "{err}");
        assert_eq!(h.kept("mcp-http"), None, "{bad:?}");
    }
    // One kept from before is checked the same.
    let h = Fake::new();
    h.keep("mcp-http", "yes");
    h.dies(&[]);
    // Empty is unset, as with RUNNER_NET.
    Fake::new().ok(&[("RUNNER_MCP_HTTP", "")]);
}

#[test]
fn install_sh_keeps_a_plain_http_url_and_refuses_the_rest() {
    for good in [
        "https://kvm-host.tailnet.ts.net",
        "http://127.0.0.1:8740",
        "https://kvm-host.tailnet.ts.net:8443/runner",
    ] {
        let h = Fake::new();
        h.ok(&[("RUNNER_MCP_HTTP", "1"), ("RUNNER_MCP_URL", good)]);
        assert_eq!(h.kept("mcp-url").as_deref(), Some(good));
    }
    for bad in [
        "kvm-host.tailnet.ts.net",
        "ftp://kvm-host.tailnet.ts.net",
        "https://",
        "https://kvm host.tailnet.ts.net",
        "https://kvm-host.tailnet.ts.net/\"; rm -rf /",
        "https://kvm-host.tailnet.ts.net'",
        "https://kvm-host.tailnet.ts.net/$(id)",
        "https://kvm-host.tailnet.ts.net\n",
        "https://user@kvm-host.tailnet.ts.net",
        "https://kvm-host.tailnet.ts.net?x=1",
        "https://kvm-host.tailnet.ts.net:port",
    ] {
        let h = Fake::new();
        let err = h.dies(&[("RUNNER_MCP_HTTP", "1"), ("RUNNER_MCP_URL", bad)]);
        assert!(err.contains("RUNNER_MCP_URL"), "{bad:?}: {err}");
        assert_eq!(h.kept("mcp-url"), None, "{bad:?}");
    }
}

#[test]
fn install_sh_keeps_the_url_for_reruns_and_an_empty_one_drops_it() {
    let h = Fake::new();
    let url = "https://kvm-host.tailnet.ts.net";
    h.ok(&[("RUNNER_MCP_HTTP", "1"), ("RUNNER_MCP_URL", url)]);
    h.ok(&[]);
    assert_eq!(h.kept("mcp-url").as_deref(), Some(url));
    // A bad one kept from before is checked the same.
    h.keep("mcp-url", "ftp://kvm-host");
    h.dies(&[]);
    h.ok(&[("RUNNER_MCP_URL", "")]);
    assert_eq!(h.kept("mcp-url"), None);
    h.ok(&[]);
    assert_eq!(h.kept("mcp-url"), None);
}

// Behind Funnel or `tailscale serve`, a request's Host is the public one,
// which the runner lets in only once RUNNER_MCP_URL names it.
#[test]
fn mcp_over_http_without_a_url_says_only_loopback_reaches_it() {
    let h = Fake::new();
    let said = h.ok(&[("RUNNER_MCP_HTTP", "1")]);
    assert!(said.contains("no RUNNER_MCP_URL"), "{said}");
    assert!(said.contains("403"), "{said}");
    let h = Fake::new();
    let url = "https://kvm-host.tailnet.ts.net";
    let said = h.ok(&[("RUNNER_MCP_HTTP", "1"), ("RUNNER_MCP_URL", url)]);
    assert!(!said.contains("no RUNNER_MCP_URL"), "{said}");
}
