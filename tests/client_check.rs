// client/check.sh against fakes on the laptop's PATH: ssh, rsync, curl and
// sleep. As in tests/client.rs, the fake ssh runs the remote command through
// bash, here in a home of the login's own, with the host's `runner` first on
// PATH. The admin login's checkout has a host/check.sh that prints what the
// test says. The host runner keeps the scratch VM's state in $FAKE/vm and
// answers every laptop check right, unless a key in $FAKE/wrong makes it
// answer one wrong. The fake sleep returns at once, so no check waits.
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

mod common;
use common::wait_for;

// Linux's sh, and bash in POSIX mode, which is what macOS's /bin/sh is.
const SHELLS: &[&[&str]] = &[&["sh"], &["bash", "--posix"]];

const HOST: &str = "runner@kvm-host";
const ADMIN: &str = "ubuntu@kvm-host";
const VM: &str = "client-check";
const LOG: &str = "/home/ubuntu/ai-agent-runner/target/check/20261103T200000Z-abcd.log";

// Each fake starts with this: whether $FAKE/wrong names a key.
const PRELUDE: &str = r#"wrong() { grep -qx "$1" "$FAKE/wrong" 2>/dev/null; }
"#;

// Every call on a line in ssh-calls: RUNNER_HOST as it was, then the argv,
// split by US. A tunnel (-L) notes what it forwards, and runs its command as
// is, so the proxy's pid is the hold's, as a real ssh's end is the hold's end.
// With -tt, lines end in CRLF, as a terminal's do.
const FAKE_SSH: &str = r#"{ printf 'RUNNER_HOST=%s' "${RUNNER_HOST-}"; for arg; do printf '\037%s' "$arg"; done; printf '\n'; } >>"$FAKE/ssh-calls"
tunnel=
tty=
while [ $# -gt 0 ]; do
  case $1 in
  -L) tunnel=1; echo "$2" >"$FAKE/tunnel"; shift 2 ;;
  -tt) tty=1; shift ;;
  -[ioplF]) shift 2 ;;
  -n) exec </dev/null; shift ;;
  -*) shift ;;
  *) break ;;
  esac
done
dest=$1
shift
cmd=$*
if [ -e "$FAKE/down" ]; then
  echo "ssh: connect to host ${dest#*@} port 22: Operation timed out" >&2
  exit 255
fi
case $dest in *@*) user=${dest%%@*} ;; *) user=runner ;; esac
mkdir -p "$FAKE/home-$user" && cd "$FAKE/home-$user" || exit 255
PATH=$HOST_BIN:$PATH
export PATH
if [ -n "$tunnel" ]; then
  eval "set -- $cmd"
  exec "$@"
fi
[ -n "$tty" ] || exec bash -c "$cmd"
{ bash -c "$cmd"; echo $? >"$FAKE/tty-exit"; } | sed 's/$/\r/'
exit "$(cat "$FAKE/tty-exit")"
"#;

const FAKE_RSYNC: &str = r#"{ printf 'PWD=%s' "$PWD"; for arg; do printf '\037%s' "$arg"; done; printf '\n'; } >>"$FAKE/rsync-calls"
exit "$(cat "$FAKE/rsync-exit" 2>/dev/null || echo 0)"
"#;

// Returns at once, unless its time is block-at's: then it waits to be killed.
const FAKE_SLEEP: &str = r#"echo "$1" >>"$FAKE/sleeps"
if [ "$1" = "$(cat "$FAKE/block-at" 2>/dev/null)" ]; then
  touch "$FAKE/blocked"
  exec tail -f /dev/null
fi
"#;

// localhost:18080 answers while the host's hold is up, as the proxy's
// tunnel to the VM's port 8080 would; the VM's URL answers while it's on, and wakes the VM.
const FAKE_CURL: &str = r#"for url; do :; done
code=000
case $url in
http://localhost:18080/*)
  if wrong port-taken; then
    code=200
  elif [ -e "$FAKE/holding" ] && [ "$(cat "$FAKE/tunnel")" = localhost:18080:172.16.0.6:8080 ] && ! wrong proxy-dead && ! { wrong proxy-drops && grep -qx 150 "$FAKE/sleeps"; }; then
    code=200
  fi
  ;;
http://100.64.0.1:10001/*)
  if [ -d "$FAKE/vm" ] && [ ! -e "$FAKE/vm/url-off" ] && ! wrong url-dead; then
    wrong url-no-wake || { echo running >"$FAKE/vm/state"; rm -f "$FAKE/vm/let-go"; }
    code=200
  fi
  ;;
esac
printf '%s' "$code"
[ "$code" != 000 ] || exit 7
"#;

// The host's runner: one VM, in $FAKE/vm, with output as the real one
// prints it (src/main.rs). Every call goes in host-calls, split by US.
const FAKE_RUNNER: &str = r##"{ printf 'runner'; for arg; do printf '\037%s' "$arg"; done; printf '\n'; } >>"$FAKE/host-calls"
vm=$FAKE/vm
name=${2-}
there() { [ -d "$vm" ] || { echo "runner: no VM named $name" >&2; exit 1; }; }
line() { printf '%s  gen 1  4096 bytes' "$(cat "$vm/name")"; }
# A VM a proxy has let go sleeps unless its idle time is never.
state() {
  s=$(cat "$vm/state")
  if [ "$s" = running ] && [ -e "$vm/let-go" ] && [ "$(cat "$vm/idle")" != never ] && ! wrong never-sleeps; then
    s=sleeping
    echo sleeping >"$vm/state"
  fi
  if [ "$s" = running ] && [ -e "$FAKE/holding" ] && wrong sleeps-under-proxy; then s=sleeping; fi
  echo "$s"
}
up() { [ "$(state)" = running ] || { echo running >"$vm/state"; booted; }; }
booted() { echo $(($(cat "$vm/boot" 2>/dev/null || echo 0) + 1)) >"$vm/boot"; rm -f "$vm/session"; }
row() { printf '%s  2026-11-03T20:00:00Z  %s  "%s"\n' "$1" "$(cat "$FAKE/runs/$1.state")" "$(cat "$FAKE/runs/$1.prompt")"; }
path() { printf '      {\n        "path": "%s",\n        "change": "added"\n      }' "$1"; }
case $1 in
create)
  [ ! -d "$vm" ] || { echo "runner: $name is already there" >&2; exit 1; }
  ! wrong create-fails || { echo "runner: no VM named base" >&2; exit 1; }
  mkdir -p "$vm" "$FAKE/runs"
  echo "$name" >"$vm/name"
  echo stopped >"$vm/state"
  echo 10m >"$vm/idle"
  line; echo ;;
status) there; echo "$(line)  $(state)  creds claude" ;;
start) there; up; echo "$(line)  running" ;;
stop) there; echo stopped >"$vm/state"; rm -f "$vm/session" "$vm/let-go"; echo "$(line)  stopped" ;;
delete)
  there
  [ "$(state)" = stopped ] || { echo "runner: $name is up: runner stop $name first" >&2; exit 1; }
  echo "deleted $(line)"
  rm -rf "$vm" ;;
idle) there; echo "$3" >"$vm/idle"; echo "$name  idle $3" ;;
ip) there; echo 172.16.0.6 ;;
hold)
  there
  trap 'kill $t; rm -f "$FAKE/holding"; echo "let go" >>"$FAKE/held"; touch "$vm/let-go"; exit 0' TERM
  tail -f /dev/null &
  t=$!
  touch "$FAKE/holding"
  echo holding >>"$FAKE/held"
  wait $t ;;
exec)
  shift 2
  env=
  while [ $# -gt 0 ] && [ "$1" != -- ]; do
    case $1 in -e) env="$env $2"; shift 2 ;; *) shift ;; esac
  done
  shift
  for kv in $env; do
    case ${kv%%=*} in
    GH_TOKEN | CLAUDE_CODE_OAUTH_TOKEN | ANTHROPIC_API_KEY)
      wrong cred-allowed || {
        echo "runner: ${kv%%=*} is a credential: credentials reach a VM only through \`runner creds\`" >&2
        ! wrong cred-exits-0 || exit 0
        exit 255
      } ;;
    esac
  done
  there
  up
  case $* in
  "touch /tmp/client-check-ran") touch "$vm/ran" ;;
  "test ! -e /tmp/client-check-ran") [ ! -e "$vm/ran" ] ;;
  "printenv FOO")
    ! wrong env-dropped || exit 1
    for kv in $env; do [ "${kv%%=*}" != FOO ] || echo "${kv#*=}"; done ;;
  "tmux new-session -d -s in-use sleep 100000") touch "$vm/session" ;;
  "cat /proc/sys/kernel/random/boot_id") wrong boot-id-empty || cat "$vm/boot" ;;
  "timeout 300 sh -c "*/home/agent/work/joined.txt*) [ -e "$vm/joined" ] ;;
  "test -f /home/agent/work/joined.txt") [ -e "$vm/joined" ] ;;
  "test ! -e /home/agent/work/joined.txt") [ ! -e "$vm/joined" ] ;;
  *) echo "fake runner: unexpected exec $*" >&2; exit 99 ;;
  esac ;;
sessions)
  there
  if [ -e "$vm/session" ] && ! wrong session-missing; then
    echo "in-use  sleep  2026-11-03T20:00:00Z  detached"
  else
    echo "no sessions"
  fi ;;
run)
  there
  n=$(($(cat "$vm/runs" 2>/dev/null || echo 0) + 1))
  echo "$n" >"$vm/runs"
  id=$name.$n
  printf '%s' "$3" >"$FAKE/runs/$id.prompt"
  if [ "$(state)" = running ]; then
    echo joined >"$FAKE/runs/$id.kind"
    echo running >"$FAKE/runs/$id.state"
    wrong run-writes-nothing || touch "$vm/joined"
  else
    ! wrong booted-run-fails || { echo "runner: $name: no token for Claude Code" >&2; exit 1; }
    echo booted >"$FAKE/runs/$id.kind"
    if wrong hello-rolled-back; then
      echo "exited 1, rolled back" >"$FAKE/runs/$id.state"
    else
      echo "exited 0, committed" >"$FAKE/runs/$id.state"
    fi
  fi
  echo "$id" ;;
cancel)
  if wrong run-rolled-back; then
    echo "cancelled, rolled back" >"$FAKE/runs/$2.state"
  else
    echo "cancelled, not rolled back, checkpoint 4 undoes it" >"$FAKE/runs/$2.state"
  fi
  ! wrong run-restarts || echo $(($(cat "$vm/boot") + 1)) >"$vm/boot"
  ! wrong session-lost || rm -f "$vm/session"
  ! wrong joined-lost || rm -f "$vm/joined"
  row "$2" ;;
runs)
  there
  for f in "$FAKE"/runs/*.state; do
    [ -e "$f" ] || continue
    id=${f##*/}
    row "${id%.state}"
  done ;;
output) echo "runner: $2 exited 0" >&2 ;;
result)
  ! wrong result-empty || [ "$(cat "$FAKE/runs/$2.kind")" = joined ] || { echo "runner: $2 has no result yet" >&2; exit 1; }
  if [ "$(cat "$FAKE/runs/$2.kind")" = joined ]; then
    outcome=not_rolled_back checkpoint=4 boot_only=0
    ! wrong result-outcome || outcome=committed
    ! wrong result-checkpoint || checkpoint=null
    paths=$(path /home/agent/work/joined.txt)
  else
    outcome=committed checkpoint=null boot_only=12
    ! wrong hello-rolled-back || outcome=rolled_back
    ! wrong boot-only-zero || boot_only=0
    paths=$(path /home/agent/.claude.json)
    wrong hello-missing || paths="$paths,
$(path /home/agent/work/hello.txt)"
    paths="$paths,
$(path /tmp)"
    ! wrong boot-noise || paths="$paths,
$(path /var/log/journal/system.journal)"
  fi
  cat <<EOF
{
  "exit_code": 0,
  "duration_ms": 5120,
  "outcome": "$outcome",
  "from_generation": 1,
  "generation": 2,
  "checkpoint": $checkpoint,
  "changed": {
    "paths": [
$paths
    ],
    "more": 0,
    "boot_only": $boot_only
  },
  "agent_says": {
    "untrusted": true,
    "text": "Done."
  }
}
EOF
  ;;
checkpoints)
  there
  kind=run
  ! wrong checkpoint-kind || kind=checkpoint
  echo "* 5  2026-11-03T20:01:00Z  checkpoint  4096 bytes"
  echo "  4  2026-11-03T20:00:00Z  $kind  4096 bytes  \"before run $name.1\"" ;;
gc)
  if [ "$2" = --dry-run ]; then
    echo "dev  nothing to free"
    if [ "$(state)" = stopped ] || wrong gc-sweeps; then
      echo "$(cat "$vm/name")  would free gen 1  up to 4096 bytes"
    else
      echo "$(cat "$vm/name")  skipped: $(state)"
    fi
  else
    there
    ! wrong gc-other-error || { echo "ssh: connect to host kvm-host port 22: Connection reset" >&2; exit 255; }
    if [ "$(state)" != stopped ] && ! wrong gc-runs; then
      echo "runner: $name is busy: it's running or sleeping, or another runner command holds it" >&2
      exit 1
    fi
    echo "$name  nothing to free"
  fi ;;
restore)
  there
  [ "$3" = 4 ] || { echo "runner: $name has no checkpoint $3" >&2; exit 1; }
  wrong restore-keeps || rm -f "$vm/joined"
  echo stopped >"$vm/state"
  up
  line; echo ;;
service)
  name=$3
  there
  ! wrong service-fails || { echo "runner: web didn't start" >&2; exit 1; }
  up
  echo "web  active  /home/agent/work  python3 -m http.server 8080" ;;
url)
  there
  case ${3-} in
  off) wrong url-stays-on || touch "$vm/url-off" ;;
  on) wrong url-stays-off || rm -f "$vm/url-off" ;;
  esac
  if [ -e "$vm/url-off" ]; then
    echo "$name's URL is off: \`runner url $name on\` turns it on"
  else
    echo http://100.64.0.1:10001
  fi ;;
mcp)
  while IFS= read -r req; do
    printf '%s\n' "$req" >>"$FAKE/mcp-in"
    id=$(printf '%s' "$req" | sed -n 's/^.*"id":\([0-9]*\),.*$/\1/p')
    case $req in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"runner","version":"0.1.0"}}}\n' "$id" ;;
    *'"env":{"GH_TOKEN":"x"}'*)
      if wrong cred-allowed; then
        printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"{}"}],"structuredContent":{"exit_code":0,"signal":null,"stdout":"","stdout_truncated":false,"stderr":"","stderr_truncated":false,"timed_out":false},"isError":false}}\n' "$id"
      else
        printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"GH_TOKEN is a credential: credentials reach a VM only through `runner creds`"}],"isError":true}}\n' "$id"
      fi ;;
    *'"argv":["seq","1","40000"]'*)
      head='1\n2\n3\n4\n'
      ! wrong mcp-head-lost || head='17\n18\n'
      cut='[runner cut 196608 bytes here]\n'
      ! wrong mcp-no-cut || cut=
      tail='39999\n40000\n'
      ! wrong mcp-tail-lost || tail='39998\n'
      truncated=true
      ! wrong mcp-not-truncated || truncated=false
      printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"{}"}],"structuredContent":{"exit_code":0,"signal":null,"stdout":"%s%s%s","stdout_truncated":%s,"stderr":"","stderr_truncated":false,"timed_out":false},"isError":false}}\n' "$id" "$head" "$cut" "$tail" "$truncated" ;;
    esac
  done ;;
*) echo "fake runner: unexpected $*" >&2; exit 99 ;;
esac
"##;

// The host's check, in the admin login's checkout: prints host-out, exits host-exit.
const FAKE_HOST_CHECK: &str = r#"pwd >"$FAKE/host-check-ran"
cat "$FAKE/host-out"
exit "$(cat "$FAKE/host-exit" 2>/dev/null || echo 0)"
"#;

fn host_passing() -> String {
    format!(
        "check: log at {LOG}
PASS  install
PASS  guest
PASS  base: unchanged, fingerprint 222222222222
PASS  kernel: unchanged
PASS  doctor: every line ok
PASS  kvm: a_vm_boots_and_answers_exec
check: 6 passed, 0 failed. Log: {LOG}
"
    )
}

fn host_failing() -> String {
    format!(
        "check: log at {LOG}
PASS  install
PASS  guest
PASS  base: unchanged, fingerprint 222222222222
PASS  kernel: unchanged
PASS  doctor: every line ok
FAIL  kvm: a_vm_boots_and_answers_exec
PASS  kvm: sleep_and_wake_keep_a_process
check: 6 passed, 1 failed. Log: {LOG}
"
    )
}

fn host_stopped() -> String {
    format!(
        "check: log at {LOG}
FAIL  install: host/install.sh failed
check: 0 passed, 1 failed, stopped. Log: {LOG}
"
    )
}

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn script(path: &Path, body: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, format!("#!/bin/sh\n{PRELUDE}{body}")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

// Each line split by US.
fn calls(path: &Path) -> Vec<Vec<String>> {
    let log = fs::read_to_string(path).unwrap_or_default();
    log.lines()
        .map(|l| l.split('\x1f').map(str::to_string).collect())
        .collect()
}

struct Laptop {
    dir: tempfile::TempDir,
}

impl Laptop {
    fn new() -> Laptop {
        let dir = tempfile::tempdir().unwrap();
        let laptop = Laptop { dir };
        let bin = laptop.path("bin");
        script(&bin.join("ssh"), FAKE_SSH);
        script(&bin.join("rsync"), FAKE_RSYNC);
        script(&bin.join("sleep"), FAKE_SLEEP);
        script(&bin.join("curl"), FAKE_CURL);
        script(&laptop.path("host-bin/runner"), FAKE_RUNNER);
        script(
            &laptop
                .fake()
                .join("home-ubuntu/ai-agent-runner/host/check.sh"),
            FAKE_HOST_CHECK,
        );
        fs::create_dir_all(laptop.home().join(".ssh")).unwrap();
        fs::write(laptop.key(), "not a key\n").unwrap();
        laptop.host_says(&host_passing(), 0);
        laptop
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn fake(&self) -> PathBuf {
        self.path("fake")
    }

    fn home(&self) -> PathBuf {
        self.path("home")
    }

    fn key(&self) -> PathBuf {
        self.home().join(".ssh/kvm-host.pem")
    }

    /// What the host's host/check.sh prints, and its exit.
    fn host_says(&self, out: &str, exit: i32) -> &Laptop {
        fs::write(self.fake().join("host-out"), out).unwrap();
        fs::write(self.fake().join("host-exit"), exit.to_string()).unwrap();
        self
    }

    /// Has the fake host answer one laptop check wrong.
    fn wrong(&self, key: &str) -> &Laptop {
        fs::write(self.fake().join("wrong"), format!("{key}\n")).unwrap();
        self
    }

    fn down(&self) -> &Laptop {
        fs::write(self.fake().join("down"), "").unwrap();
        self
    }

    /// client/check.sh under `shell`, run from elsewhere, with the fakes first
    /// on PATH and RUNNER_HOST set.
    fn check(&self, shell: &[&str], args: &[&str]) -> Command {
        let path = format!(
            "{}:{}",
            self.path("bin").display(),
            std::env::var("PATH").unwrap()
        );
        let mut cmd = Command::new(shell[0]);
        cmd.args(&shell[1..])
            .arg(repo().join("client/check.sh"))
            .args(args)
            .current_dir(self.dir.path())
            .env("PATH", path)
            .env("HOME", self.home())
            .env("FAKE", self.fake())
            .env("HOST_BIN", self.path("host-bin"))
            .env("RUNNER_HOST", HOST)
            .env_remove("RUNNER_ADMIN")
            .env_remove("RUNNER_AWS_KEY")
            .env_remove("RUNNER_AWS_NAME")
            .env_remove("KEEP")
            .stdin(Stdio::null());
        cmd
    }

    fn run(&self, shell: &[&str], args: &[&str]) -> Output {
        self.check(shell, args).output().unwrap()
    }

    fn ssh_calls(&self) -> Vec<Vec<String>> {
        calls(&self.fake().join("ssh-calls"))
    }

    fn rsync_calls(&self) -> Vec<Vec<String>> {
        calls(&self.fake().join("rsync-calls"))
    }

    /// Every command the host's runner got, its arguments joined by spaces.
    fn host_calls(&self) -> Vec<String> {
        calls(&self.fake().join("host-calls"))
            .into_iter()
            .map(|c| c[1..].join(" "))
            .collect()
    }

    fn host_check_ran(&self) -> bool {
        self.fake().join("host-check-ran").exists()
    }

    fn vm_is_there(&self) -> bool {
        self.fake().join("vm").exists()
    }

    fn held(&self) -> Vec<String> {
        let held = fs::read_to_string(self.fake().join("held")).unwrap_or_default();
        held.lines().map(str::to_string).collect()
    }
}

/// The lines after "== Summary": every FAIL, then the counts.
fn summary(out: &Output) -> Vec<String> {
    let stdout = text(&out.stdout);
    let at = stdout
        .find("\n== Summary")
        .unwrap_or_else(|| panic!("no summary in:\n{stdout}"));
    stdout[at + 1..]
        .lines()
        .skip(1)
        .map(str::to_string)
        .collect()
}

fn fails(out: &Output) -> Vec<String> {
    summary(out)
        .into_iter()
        .filter(|l| l.starts_with("FAIL  "))
        .collect()
}

fn last(out: &Output) -> String {
    summary(out).last().cloned().unwrap_or_default()
}

fn says(out: &Output) -> String {
    format!(
        "{:?}\n--- stdout\n{}--- stderr\n{}",
        out.status,
        text(&out.stdout),
        text(&out.stderr)
    )
}

fn git(args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo())
        .args(args)
        .output()
        .unwrap();
    text(&out.stdout).trim().to_string()
}

#[test]
fn all_passing_exits_0_and_the_hosts_lines_count_in_the_summary() {
    for shell in SHELLS {
        let laptop = Laptop::new();
        let out = laptop.run(shell, &[]);
        assert_eq!(out.status.code(), Some(0), "{shell:?}: {}", says(&out));
        let stdout = text(&out.stdout);

        // The checkout it copies, named first.
        let first = stdout.lines().next().unwrap_or_default();
        assert!(
            first.contains(&git(&["rev-parse", "--short", "HEAD"])),
            "{first}"
        );
        assert!(
            first.contains(&git(&["rev-parse", "--abbrev-ref", "HEAD"])),
            "{first}"
        );

        // The host's lines as they came, and every PASS counted.
        assert!(stdout.contains("PASS  kvm: a_vm_boots_and_answers_exec\n"));
        let passes = stdout.lines().filter(|l| l.starts_with("PASS  ")).count();
        assert!(passes > 6 + 20, "{passes}: {stdout}");
        let summary = summary(&out);
        assert!(fails(&out).is_empty(), "{summary:?}");
        assert!(
            summary.contains(&format!("host: 6 passed, 0 failed. Log on the host: {LOG}")),
            "{summary:?}"
        );
        assert!(
            summary.contains(&format!("laptop: {} passed, 0 failed", passes - 6)),
            "{summary:?}"
        );
        assert_eq!(last(&out), format!("all: {passes} passed, 0 failed"));

        // The scratch VM is gone.
        assert!(!laptop.vm_is_there(), "{shell:?}");
        assert!(laptop.host_calls().contains(&format!("delete {VM}")));
    }
}

#[test]
fn it_copies_the_checkout_then_runs_the_host_check_as_the_admin_login() {
    let laptop = Laptop::new();
    let out = laptop.run(SHELLS[0], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", says(&out));

    let rsync = laptop.rsync_calls();
    assert_eq!(rsync.len(), 1, "{rsync:?}");
    let key = laptop.key().display().to_string();
    let rsh = format!(
        "ssh -i '{key}' -o IdentitiesOnly=yes -o BatchMode=yes -o ConnectTimeout=10 -o StrictHostKeyChecking=accept-new -o ServerAliveInterval=30"
    );
    assert_eq!(
        rsync[0],
        [
            format!("PWD={}", repo().display()).as_str(),
            "-az",
            "--delete",
            "--exclude",
            "target",
            "--exclude",
            "images/base/out",
            "-e",
            &rsh,
            "./",
            "ubuntu@kvm-host:ai-agent-runner/",
        ]
    );

    let admin: Vec<_> = laptop
        .ssh_calls()
        .into_iter()
        .filter(|c| c.contains(&ADMIN.to_string()))
        .collect();
    let opts = [
        "-i",
        key.as_str(),
        "-o",
        "IdentitiesOnly=yes",
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=10",
        "-o",
        "StrictHostKeyChecking=accept-new",
        "-o",
        "ServerAliveInterval=30",
    ];
    let call = |rest: &[&str]| -> Vec<String> {
        let mut c = vec!["RUNNER_HOST=runner@kvm-host"];
        c.extend(opts);
        c.extend(rest);
        c.into_iter().map(str::to_string).collect()
    };
    assert_eq!(
        admin,
        [
            call(&["-n", ADMIN, "true"]),
            call(&[
                "-n",
                "-tt",
                "-o",
                "LogLevel=ERROR",
                ADMIN,
                "cd ai-agent-runner && host/check.sh"
            ]),
        ]
    );
    assert!(laptop.host_check_ran());
}

#[test]
fn a_fail_on_the_host_is_listed_at_the_end_with_exit_1() {
    for shell in SHELLS {
        let laptop = Laptop::new();
        laptop.host_says(&host_failing(), 1);
        let out = laptop.run(shell, &[]);
        assert_eq!(out.status.code(), Some(1), "{shell:?}: {}", says(&out));
        assert_eq!(
            fails(&out),
            ["FAIL  host: kvm: a_vm_boots_and_answers_exec"],
            "{shell:?}"
        );
        let summary = summary(&out);
        assert!(
            summary.contains(&format!("host: 6 passed, 1 failed. Log on the host: {LOG}")),
            "{summary:?}"
        );
        assert!(last(&out).ends_with(" passed, 1 failed"), "{summary:?}");
        // The host got to its tests, so the laptop checks ran too.
        assert!(laptop.host_calls().contains(&format!("create {VM}")));
    }
}

#[test]
fn a_laptop_fail_is_listed_at_the_end_with_exit_1_and_the_vm_still_goes() {
    for shell in SHELLS {
        let laptop = Laptop::new();
        laptop.wrong("env-dropped");
        let out = laptop.run(shell, &[]);
        assert_eq!(out.status.code(), Some(1), "{shell:?}: {}", says(&out));
        let fails = fails(&out);
        assert_eq!(fails.len(), 1, "{shell:?}: {fails:?}");
        assert!(
            fails[0].starts_with("FAIL  laptop: exec -e FOO=bar"),
            "{fails:?}"
        );
        assert!(last(&out).ends_with(" passed, 1 failed"));
        assert!(!laptop.vm_is_there(), "{shell:?}");
    }
}

#[test]
fn keep_1_keeps_the_scratch_vm_and_says_how_to_delete_it() {
    for shell in SHELLS {
        let laptop = Laptop::new();
        laptop.wrong("env-dropped");
        let out = laptop
            .check(shell, &["laptop"])
            .env("KEEP", "1")
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(1), "{shell:?}: {}", says(&out));
        assert!(laptop.vm_is_there(), "{shell:?}");
        assert!(!laptop.host_calls().iter().any(|c| c.starts_with("delete")));
        let stdout = text(&out.stdout);
        assert!(
            stdout.contains(&format!(
                "RUNNER_HOST={HOST} client/runner stop {VM} && RUNNER_HOST={HOST} client/runner delete {VM}"
            )),
            "{stdout}"
        );
    }
}

#[test]
fn a_vm_left_by_an_earlier_check_is_deleted_first() {
    let laptop = Laptop::new();
    laptop.wrong("env-dropped");
    let out = laptop
        .check(SHELLS[0], &["laptop"])
        .env("KEEP", "1")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", says(&out));
    fs::write(laptop.fake().join("wrong"), "").unwrap();
    fs::remove_file(laptop.fake().join("host-calls")).unwrap();

    let out = laptop.run(SHELLS[0], &["laptop"]);
    assert_eq!(out.status.code(), Some(0), "{}", says(&out));
    let calls = laptop.host_calls();
    let at = |c: &str| calls.iter().position(|x| x == c).unwrap_or(usize::MAX);
    assert!(
        at(&format!("stop {VM}")) < at(&format!("delete {VM}")),
        "{calls:?}"
    );
    assert!(
        at(&format!("delete {VM}")) < at(&format!("create {VM}")),
        "{calls:?}"
    );
    assert!(!laptop.vm_is_there());
}

#[test]
fn a_host_that_doesnt_answer_exits_2_at_once_and_runs_nothing() {
    for shell in SHELLS {
        for args in [&[][..], &["host"], &["laptop"]] {
            let laptop = Laptop::new();
            laptop.down();
            let out = laptop.run(shell, args);
            assert_eq!(
                out.status.code(),
                Some(2),
                "{shell:?} {args:?}: {}",
                says(&out)
            );
            let err = text(&out.stderr);
            assert!(err.contains("doesn't answer"), "{err}");
            assert!(err.contains("client/aws.sh start"), "{err}");
            assert_eq!(laptop.ssh_calls().len(), 1, "{shell:?} {args:?}");
            assert!(laptop.rsync_calls().is_empty());
            assert!(!laptop.host_check_ran());
            assert!(laptop.host_calls().is_empty(), "{:?}", laptop.host_calls());
        }
    }
}

#[test]
fn host_runs_only_the_host_part_and_laptop_only_the_laptops() {
    for shell in SHELLS {
        let laptop = Laptop::new();
        let out = laptop.run(shell, &["host"]);
        assert_eq!(out.status.code(), Some(0), "{shell:?}: {}", says(&out));
        assert!(laptop.host_check_ran());
        assert!(laptop.host_calls().is_empty(), "{shell:?}");
        assert_eq!(last(&out), "all: 6 passed, 0 failed");
        assert!(!summary(&out).iter().any(|l| l.starts_with("laptop:")));

        let laptop = Laptop::new();
        let out = laptop.run(shell, &["laptop"]);
        assert_eq!(out.status.code(), Some(0), "{shell:?}: {}", says(&out));
        assert!(laptop.rsync_calls().is_empty());
        assert!(!laptop.host_check_ran());
        assert!(
            !laptop
                .ssh_calls()
                .iter()
                .any(|c| c.contains(&ADMIN.to_string()))
        );
        assert!(laptop.host_calls().contains(&format!("create {VM}")));
        assert!(!summary(&out).iter().any(|l| l.starts_with("host:")));
    }
}

#[test]
fn a_bad_argument_says_how_and_runs_nothing() {
    for shell in SHELLS {
        let laptop = Laptop::new();
        let out = laptop.run(shell, &["both"]);
        assert_eq!(out.status.code(), Some(2), "{shell:?}");
        assert!(text(&out.stderr).contains("usage: client/check.sh [host|laptop]"));
        assert!(laptop.ssh_calls().is_empty());

        let out = laptop.run(shell, &["--help"]);
        assert_eq!(out.status.code(), Some(0), "{shell:?}");
        assert!(text(&out.stdout).contains("usage: client/check.sh [host|laptop]"));
        assert!(laptop.ssh_calls().is_empty());
    }
}

#[test]
fn the_laptop_checks_go_to_the_host_being_checked() {
    // No RUNNER_HOST: the first host the laptop knows, set for every check.
    for shell in SHELLS {
        let laptop = Laptop::new();
        let conf = laptop.home().join(".config/runner");
        fs::create_dir_all(&conf).unwrap();
        fs::write(conf.join("host"), "runner@kvm-host-2\nrunner@kvm-host\n").unwrap();
        let out = laptop
            .check(shell, &[])
            .env_remove("RUNNER_HOST")
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(0), "{shell:?}: {}", says(&out));
        let ssh = laptop.ssh_calls();
        assert!(!ssh.is_empty());
        for call in &ssh {
            assert_eq!(call[0], "RUNNER_HOST=runner@kvm-host-2", "{call:?}");
            assert!(
                call.contains(&"runner@kvm-host-2".to_string())
                    || call.contains(&"ubuntu@kvm-host-2".to_string()),
                "{call:?}"
            );
        }
        assert!(
            ssh.iter()
                .any(|c| c.contains(&"runner@kvm-host-2".to_string()))
        );
        assert_eq!(
            laptop.rsync_calls()[0].last().unwrap(),
            "ubuntu@kvm-host-2:ai-agent-runner/"
        );
    }
}

#[test]
fn runner_admin_and_runner_aws_key_override_the_admin_login() {
    let laptop = Laptop::new();
    let key = laptop.path("other.pem");
    fs::write(&key, "not a key\n").unwrap();
    script(
        &laptop
            .fake()
            .join("home-admin/ai-agent-runner/host/check.sh"),
        FAKE_HOST_CHECK,
    );
    let out = laptop
        .check(SHELLS[0], &["host"])
        .env("RUNNER_ADMIN", "admin@box")
        .env("RUNNER_AWS_KEY", &key)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", says(&out));
    let rsync = &laptop.rsync_calls()[0];
    assert_eq!(rsync.last().unwrap(), "admin@box:ai-agent-runner/");
    assert!(
        rsync
            .iter()
            .any(|a| a.starts_with(&format!("ssh -i '{}' ", key.display())))
    );
    let ssh = laptop.ssh_calls();
    assert_eq!(ssh.len(), 2, "{ssh:?}");
    for call in ssh {
        assert!(call.contains(&"admin@box".to_string()), "{call:?}");
        assert!(call.contains(&key.display().to_string()), "{call:?}");
    }
}

#[test]
fn without_aws_shs_key_ssh_uses_its_own() {
    let laptop = Laptop::new();
    fs::remove_file(laptop.key()).unwrap();
    let out = laptop.run(SHELLS[0], &["host"]);
    assert_eq!(out.status.code(), Some(0), "{}", says(&out));
    let rsync = &laptop.rsync_calls()[0];
    assert!(
        rsync.iter().any(|a| a.starts_with("ssh -o BatchMode=yes ")),
        "{rsync:?}"
    );
    for call in laptop.ssh_calls() {
        assert!(!call.contains(&"-i".to_string()), "{call:?}");
        assert!(
            !call.contains(&"IdentitiesOnly=yes".to_string()),
            "{call:?}"
        );
    }
}

// With several hosts, the first in ~/.config/runner/hosts, as for MCP.
#[test]
fn with_a_hosts_file_the_checks_go_to_its_first_host() {
    for shell in SHELLS {
        let laptop = Laptop::new();
        let conf = laptop.home().join(".config/runner");
        fs::create_dir_all(&conf).unwrap();
        fs::write(conf.join("host"), "runner@kvm-host\n").unwrap();
        fs::write(
            conf.join("hosts"),
            "# the first is MCP's\nrunner@kvm-host-2\nrunner@kvm-host\n",
        )
        .unwrap();
        let out = laptop
            .check(shell, &[])
            .env_remove("RUNNER_HOST")
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(0), "{shell:?}: {}", says(&out));
        let ssh = laptop.ssh_calls();
        assert!(!ssh.is_empty());
        for call in &ssh {
            assert_eq!(call[0], "RUNNER_HOST=runner@kvm-host-2", "{call:?}");
        }
    }
}

#[test]
fn without_a_host_it_says_where_to_set_one_and_runs_nothing() {
    let laptop = Laptop::new();
    let out = laptop
        .check(SHELLS[0], &[])
        .env_remove("RUNNER_HOST")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "{}", says(&out));
    let err = text(&out.stderr);
    assert!(err.contains("RUNNER_HOST"), "{err}");
    assert!(err.contains("~/.config/runner/host"), "{err}");
    assert!(laptop.ssh_calls().is_empty());
}

#[test]
fn a_host_check_that_stops_before_its_tests_skips_the_laptop_checks() {
    for shell in SHELLS {
        let laptop = Laptop::new();
        laptop.host_says(&host_stopped(), 1);
        let out = laptop.run(shell, &[]);
        assert_eq!(out.status.code(), Some(1), "{shell:?}: {}", says(&out));
        let fails = fails(&out);
        assert_eq!(fails[0], "FAIL  host: install: host/install.sh failed");
        assert!(fails[1].starts_with("FAIL  laptop: not run"), "{fails:?}");
        assert_eq!(fails.len(), 2, "{fails:?}");
        assert!(laptop.host_calls().is_empty(), "{shell:?}");
    }
}

#[test]
fn a_dropped_connection_or_a_failed_copy_is_a_fail_and_skips_the_laptop_checks() {
    for shell in SHELLS {
        // ssh drops partway: no count line, and ssh's 255.
        let laptop = Laptop::new();
        laptop.host_says(&format!("check: log at {LOG}\nPASS  install\n"), 255);
        let out = laptop.run(shell, &[]);
        assert_eq!(out.status.code(), Some(1), "{shell:?}: {}", says(&out));
        let dropped = fails(&out);
        assert_eq!(dropped.len(), 2, "{dropped:?}");
        assert!(
            dropped[0].starts_with("FAIL  host: host/check.sh exited 255"),
            "{dropped:?}"
        );
        assert!(
            dropped[1].starts_with("FAIL  laptop: not run"),
            "{dropped:?}"
        );
        assert!(laptop.host_calls().is_empty());

        let laptop = Laptop::new();
        fs::write(laptop.fake().join("rsync-exit"), "23").unwrap();
        let out = laptop.run(shell, &[]);
        assert_eq!(out.status.code(), Some(1), "{shell:?}: {}", says(&out));
        let failed = fails(&out);
        assert_eq!(failed.len(), 2, "{failed:?}");
        assert!(failed[0].starts_with("FAIL  host: copy"), "{failed:?}");
        assert!(failed[1].starts_with("FAIL  laptop: not run"), "{failed:?}");
        assert!(!laptop.host_check_ran());
        assert!(laptop.host_calls().is_empty());
    }
}

#[test]
fn a_vm_that_cant_be_made_is_a_fail_and_runs_no_check() {
    for shell in SHELLS {
        let laptop = Laptop::new();
        laptop.wrong("create-fails");
        let out = laptop.run(shell, &["laptop"]);
        assert_eq!(out.status.code(), Some(1), "{shell:?}: {}", says(&out));
        let fails = fails(&out);
        assert_eq!(fails.len(), 1, "{fails:?}");
        assert!(fails[0].starts_with("FAIL  laptop: setup"), "{fails:?}");
        assert!(!laptop.host_calls().iter().any(|c| c.starts_with("exec")));
    }
}

// Each key makes the fake host answer one check wrong; the check must say
// so. What the FAIL lines start with, after "FAIL  laptop: ".
const WRONG: &[(&str, &[&str])] = &[
    // exec
    (
        "cred-allowed",
        &[
            "exec -e GH_TOKEN",
            "exec -e GH_TOKEN",
            "MCP exec with env GH_TOKEN",
        ],
    ),
    ("cred-exits-0", &["exec -e GH_TOKEN: said"]),
    ("env-dropped", &["exec -e FOO=bar"]),
    // MCP
    ("mcp-head-lost", &["MCP exec: stdout doesn't start at 1"]),
    ("mcp-tail-lost", &["MCP exec: stdout doesn't end at 40000"]),
    ("mcp-no-cut", &["MCP exec: no cut marked"]),
    ("mcp-not-truncated", &["MCP exec: stdout_truncated"]),
    // A run that joins a VM in use, and gc
    ("session-missing", &["runner sessions doesn't list"]),
    ("run-writes-nothing", &["the run never wrote joined.txt"]),
    ("run-rolled-back", &["cancelled, but runs says"]),
    ("run-restarts", &["the VM was restarted"]),
    ("boot-id-empty", &["the VM's boot_id reads empty"]),
    ("session-lost", &["its tmux session is gone"]),
    ("joined-lost", &["joined.txt is gone"]),
    ("result-outcome", &["runner result:"]),
    (
        "result-checkpoint",
        &["runner result doesn't name checkpoint 4"],
    ),
    ("checkpoint-kind", &["checkpoint 4:"]),
    ("gc-sweeps", &["gc --dry-run"]),
    ("gc-runs", &["gc client-check: ran on a running VM"]),
    ("gc-other-error", &["gc client-check: exit 255"]),
    ("restore-keeps", &["restore 4 didn't roll the run back"]),
    // A run's result without boot noise
    (
        "booted-run-fails",
        &["runner run on the stopped client-check printed no run"],
    ),
    ("hello-rolled-back", &["runner result:"]),
    (
        "result-empty",
        &[
            "runner result:",
            "changed doesn't list hello.txt",
            "boot_only: missing",
            "changed lists no paths",
        ],
    ),
    ("hello-missing", &["changed doesn't list hello.txt"]),
    ("boot-only-zero", &["boot_only: 0"]),
    (
        "boot-noise",
        &["changed lists, outside /home and /tmp: /var/log/journal/system.journal"],
    ),
    // The proxy
    ("service-fails", &["service add web"]),
    ("port-taken", &["localhost:18080 answers before the proxy"]),
    (
        "proxy-dead",
        &[
            "localhost:18080 doesn't answer",
            "localhost:18080 stopped answering",
        ],
    ),
    ("proxy-drops", &["localhost:18080 stopped answering"]),
    (
        "sleeps-under-proxy",
        &["with the proxy open, client-check is sleeping"],
    ),
    (
        "never-sleeps",
        &["client-check is running 3 minutes after the proxy ended"],
    ),
    // The URL
    (
        "url-dead",
        &[
            "http://100.64.0.1:10001: HTTP 000",
            "after the URL, client-check is sleeping",
            "url on: http://100.64.0.1:10001",
        ],
    ),
    ("url-no-wake", &["after the URL, client-check is sleeping"]),
    (
        "url-stays-on",
        &["url off: http://100.64.0.1:10001 still gives HTTP 200"],
    ),
    (
        "url-stays-off",
        &["url on: http://100.64.0.1:10001 gives HTTP 000"],
    ),
];

#[test]
fn each_laptop_check_fails_when_the_host_answers_it_wrong() {
    for shell in SHELLS {
        // All at once: each is a few hundred processes.
        let runs: Vec<_> = WRONG
            .iter()
            .map(|(key, want)| {
                let laptop = Laptop::new();
                laptop.wrong(key);
                let child = laptop
                    .check(shell, &["laptop"])
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .unwrap();
                (key, want, laptop, child)
            })
            .collect();
        for (key, want, laptop, child) in runs {
            let out = child.wait_with_output().unwrap();
            assert_eq!(
                out.status.code(),
                Some(1),
                "{shell:?} {key}: {}",
                says(&out)
            );
            // Each FAIL on a line of its own, however many lines it quotes.
            let summary = summary(&out);
            let listed = summary.iter().take_while(|l| !l.starts_with("laptop: "));
            assert!(
                listed.clone().all(|l| l.starts_with("FAIL  ")),
                "{shell:?} {key}: {summary:?}"
            );
            let fails = fails(&out);
            assert_eq!(fails.len(), want.len(), "{shell:?} {key}: {fails:?}");
            for (got, want) in fails.iter().zip(want.iter()) {
                assert!(
                    got.starts_with(&format!("FAIL  laptop: {want}")),
                    "{shell:?} {key}: {got:?} isn't {want:?}"
                );
            }
            assert!(!laptop.vm_is_there(), "{shell:?} {key}");
        }
    }
}

#[test]
fn every_check_ran_against_the_fake() {
    // The fake host answered every call: none was one it didn't expect.
    let laptop = Laptop::new();
    let out = laptop.run(SHELLS[0], &["laptop"]);
    assert_eq!(out.status.code(), Some(0), "{}", says(&out));
    assert!(
        !text(&out.stderr).contains("fake runner: unexpected"),
        "{}",
        says(&out)
    );
    let mcp = fs::read_to_string(laptop.fake().join("mcp-in")).unwrap();
    assert!(mcp.contains(r#""method":"initialize""#), "{mcp}");
    assert!(
        mcp.contains(r#""method":"notifications/initialized""#),
        "{mcp}"
    );
    // Through this checkout's client/runner over ssh: no runner on the laptop.
    let ssh = laptop.ssh_calls();
    assert!(
        ssh.iter().any(|c| c.last().unwrap() == "runner mcp"),
        "{ssh:?}"
    );
    assert!(
        laptop.held() == ["holding", "let go"],
        "{:?}",
        laptop.held()
    );
}

// Ctrl-C while the proxy is open: the proxy ends, and with it the host's hold,
// and the VM is left with how to delete it.
#[test]
fn ctrl_c_ends_the_proxy_and_leaves_the_vm() {
    for shell in SHELLS {
        let laptop = Laptop::new();
        fs::write(laptop.fake().join("block-at"), "150").unwrap();
        let child = laptop
            .check(shell, &["laptop"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()
            .unwrap();
        let group = Pid::from_raw(child.id() as i32);
        wait_for("the proxy hold", || laptop.fake().join("blocked").exists());
        assert_eq!(laptop.held(), ["holding"], "{shell:?}");
        killpg(group, Signal::SIGINT).unwrap();
        let out = child.wait_with_output().unwrap();
        assert_eq!(out.status.code(), Some(130), "{shell:?}: {}", says(&out));
        wait_for("the hold to let go", || laptop.held().len() == 2);
        assert_eq!(laptop.held(), ["holding", "let go"], "{shell:?}");
        assert!(laptop.vm_is_there(), "{shell:?}");
        assert!(!laptop.host_calls().iter().any(|c| c.starts_with("delete")));
        let said = format!("{}{}", text(&out.stdout), text(&out.stderr));
        assert!(
            said.contains(&format!(
                "RUNNER_HOST={HOST} client/runner stop {VM} && RUNNER_HOST={HOST} client/runner delete {VM}"
            )),
            "{said}"
        );
    }
}
