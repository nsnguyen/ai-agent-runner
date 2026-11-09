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
// RUNNER_MCP_URL on the fake host, on the port README.md funnels.
const MCP_URL: &str = "https://kvm-host.tailnet.ts.net:8443";

// Each fake starts with this: whether $FAKE/wrong names a key.
const PRELUDE: &str = r#"wrong() { grep -qx "$1" "$FAKE/wrong" 2>/dev/null; }
"#;

// Every call on a line in ssh-calls: RUNNER_HOST as it was, then the argv,
// split by US, in one write, since the client asks every host at once. A tunnel (-L) notes what it forwards, and runs its command as
// is, so the proxy's pid is the hold's, as a real ssh's end is the hold's end.
// With -tt, lines end in CRLF, as a terminal's do.
const FAKE_SSH: &str = r#"us=$(printf '\037')
call="RUNNER_HOST=${RUNNER_HOST-}"
for arg; do call="$call$us$arg"; done
printf '%s\n' "$call" >>"$FAKE/ssh-calls"
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
# The host's /etc/runner, for what check.sh reads there, is $FAKE/etc-runner.
case $cmd in "cat /etc/runner/"*) cmd=$(printf '%s' "$cmd" | sed "s|/etc/runner/|$FAKE/etc-runner/|g") ;; esac
if [ -e "$FAKE/down" ]; then
  echo "ssh: connect to host ${dest#*@} port 22: Operation timed out" >&2
  exit 255
fi
case $dest in *@*) user=${dest%%@*} ;; *) user=runner ;; esac
mkdir -p "$FAKE/home-$user" && cd "$FAKE/home-$user" || exit 255
PATH=$HOST_BIN:$PATH
# Which host it is, for the host runner's VM to be on one at a time.
FAKE_DEST=${dest#*@}
export PATH FAKE_DEST
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
// check.sh runs this checkout's client/runner, which asks the aws.sh beside
// it about a host it can't reach: this AWS CLI takes nothing, so no test
// reaches a real account.
const NO_AWS: &str = "echo 'aws: no AWS in tests' >&2\nexit 1\n";

const FAKE_SLEEP: &str = r#"echo "$1" >>"$FAKE/sleeps"
if [ "$1" = "$(cat "$FAKE/block-at" 2>/dev/null)" ]; then
  touch "$FAKE/blocked"
  exec tail -f /dev/null
fi
"#;

// localhost:18080 is the VM's port 8080 while the host's hold is up, as the
// proxy's tunnel there would be: the compose stack's page once it's up, else
// the web service's, else nothing. proxy-drops drops the first hold once it's
// held through check_proxy's 150 seconds. The VM's URL answers while it's on,
// and wakes the VM. RUNNER_MCP_URL answers as the MCP server does with no
// token: through Funnel only when public DNS (--doh-url) names it, since on
// the tailnet MagicDNS takes it to the host, Funnel or not. With -w, the
// status; without, the page. Every call goes in curl-calls, split by US.
const FAKE_CURL: &str = r#"{ printf 'curl'; for arg; do printf '\037%s' "$arg"; done; printf '\n'; } >>"$FAKE/curl-calls"
for url; do :; done
code=000
page=
case $url in
https://kvm-host.tailnet.ts.net:8443/*)
  case " $* " in *" --doh-url "*) public=1 ;; *) public= ;; esac
  if [ -z "$public" ] || ! wrong funnel-off; then
    case $url in
    */mcp) code=401; ! wrong mcp-open || code=200 ;;
    */.well-known/oauth-protected-resource) code=200; ! wrong no-metadata || code=404 ;;
    *) code=404 ;;
    esac
  fi
  ;;
http://localhost:18080/*)
  if wrong port-taken; then
    code=200
  elif [ -e "$FAKE/holding" ] && [ "$(cat "$FAKE/tunnel")" = localhost:18080:172.16.0.6:8080 ] && ! wrong proxy-dead && ! { wrong proxy-drops && grep -qx 150 "$FAKE/sleeps" && [ "$(grep -cx holding "$FAKE/held")" = 1 ]; }; then
    if [ -e "$FAKE/vm/stack" ]; then
      code=200
      wrong stack-blank || page=$(cat "$FAKE/vm/stack")
    elif [ -e "$FAKE/vm/web" ]; then
      code=200
      page='<title>Directory listing for /</title>'
    fi
  fi
  ;;
http://100.64.0.1:10001/*)
  if [ -d "$FAKE/vm" ] && [ ! -e "$FAKE/vm/url-off" ] && ! wrong url-dead; then
    wrong url-no-wake || { echo running >"$FAKE/vm/state"; rm -f "$FAKE/vm/let-go"; }
    code=200
  fi
  ;;
esac
case " $* " in
*" -w "*) printf '%s' "$code" ;;
*) [ -z "$page" ] || printf '%s\n' "$page" ;;
esac
[ "$code" != 000 ] || exit 7
"#;

// The host's runner: one VM, in $FAKE/vm, with output as the real one
// prints it (src/main.rs). Every call goes in host-calls, split by US.
const FAKE_RUNNER: &str = r##"{ printf 'runner'; for arg; do printf '\037%s' "$arg"; done; printf '\n'; } >>"$FAKE/host-calls"
vm=$FAKE/vm
name=${2-}
there() { [ -d "$vm" ] || { echo "runner: no VM named $name" >&2; exit 1; }; }
# The VM is on the host that made it, until a move takes it elsewhere.
here() { there; [ "$(cat "$vm/on")" = "$FAKE_DEST" ] || { echo "runner: no VM named $name" >&2; exit 1; }; }
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
  echo "$FAKE_DEST" >"$vm/on"
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
idle)
  there
  # Only the compose check's, after the proxy check's 90s.
  [ "$3" != never ] || [ "$(cat "$vm/idle" 2>/dev/null)" != 90s ] || ! wrong idle-never-fails || { echo "runner: couldn't write $name's config" >&2; exit 1; }
  echo "$3" >"$vm/idle"
  echo "$name  idle $3"
  ;;
ip) here; echo 172.16.0.6 ;;
list) if [ -d "$vm" ] && [ "$(cat "$vm/on")" = "$FAKE_DEST" ]; then echo "$(line)  4096 bytes used"; else echo "no VMs yet"; fi ;;
move)
  [ "$2" != --help ] || { wrong move-missing && exit 2; exit 0; }
  here
  ! wrong move-fails || { echo "runner: $name is here as it was, and running again: ${3##*@}'s import failed: Connection to ${3##*@} closed by remote host" >&2; exit 1; }
  wrong move-stays || echo "${3##*@}" >"$vm/on"
  ! wrong move-loses-file || rm -f "$vm/moved"
  echo "$(line)  $(state)  on ${3##*@}" ;;
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
  here
  up
  case $* in
  "sh -c echo carried >/home/agent/moved") touch "$vm/moved" ;;
  "cat /home/agent/moved") [ -e "$vm/moved" ] && echo carried ;;
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
  # The stack, up in compose-check with the compose.yaml it's given: it
  # publishes 8080, so not while the web service has it. Its page is a uuid.
  "sh -c mkdir -p compose-check && cd compose-check && "*" && docker compose up -d sh services:"*)
    ! wrong compose-up-fails || { echo 'Error response from daemon: Get "https://registry-1.docker.io/v2/": dial tcp: lookup registry-1.docker.io: no such host' >&2; exit 1; }
    [ ! -e "$vm/web" ] || { echo "Error response from daemon: driver failed programming external connectivity on endpoint compose-check-web-1: failed to bind port 0.0.0.0:8080/tcp: Error starting userland proxy: listen tcp4 0.0.0.0:8080: bind: address already in use" >&2; exit 1; }
    wrong compose-dead || echo 6f1c2b9e-4d3a-4e0b-9a51-2c7d8e0f1a23 >"$vm/stack"
    echo " Container compose-check-web-1  Started" >&2 ;;
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
# The web service on the VM's port 8080 is $vm/web while it's there.
service)
  name=$3
  there
  case $2 in
  rm)
    ! wrong service-rm-fails || { echo "runner: services in $name: removing it exited 1" >&2; exit 1; }
    [ -e "$vm/web" ] || { echo "runner: no service $4 in $name" >&2; exit 1; }
    up
    rm "$vm/web"
    echo "removed $4" ;;
  *)
    ! wrong service-fails || { echo "runner: web didn't start" >&2; exit 1; }
    up
    touch "$vm/web"
    echo "web  active/running  /home/agent/work  python3 -m http.server 8080" ;;
  esac ;;
# Sleep and wake keep the compose stack, $vm/stack, unless told otherwise.
sleep)
  there
  ! wrong sleep-fails || { echo "runner: $name has no boot to ask: it's stopped, or it booted under a runner from before sleep and wake, which a stop and start brings up to date" >&2; exit 1; }
  case $(state) in
  running | sleeping) echo sleeping >"$vm/state" ;;
  *) echo "runner: $name is $(state): only a running VM sleeps" >&2; exit 1 ;;
  esac
  echo "$(line)  sleeping  creds claude" ;;
wake)
  there
  ! wrong wake-fails || { echo "runner: $name couldn't wake, and sleeps on: snapshot load failed. \`runner stop $name\` drops its snapshot and rolls its disk back" >&2; exit 1; }
  case $(state) in
  running | sleeping) echo running >"$vm/state" ;;
  *) echo "runner: $name is $(state), not sleeping" >&2; exit 1 ;;
  esac
  ! wrong stack-dies || rm -f "$vm/stack"
  ! wrong stack-restarts || [ ! -e "$vm/stack" ] || echo 0b7e4c1d-9f2a-4c3e-8d6b-5a1f2e3d4c5b >"$vm/stack"
  echo "$(line)  running  creds claude" ;;
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
  # Its peers, by the names MCP gives them, and a second VM, $FAKE/vm2, made
  # on the first of them. Objects' keys come sorted, as serde_json's do.
  peers=
  while [ $# -gt 0 ]; do
    [ "$1" != --peer ] || peers="$peers ${2##*@}"
    shift
  done
  first=${peers# }
  first=${first%% *}
  ok() { printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"{}"}],"structuredContent":%s,"isError":false}}\n' "$id" "$1"; }
  err() { printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"%s"}],"isError":true}}\n' "$id" "$1"; }
  vm2='{"bytes":4096,"generation":0,"name":"client-check-2","used":4096}'
  while IFS= read -r req; do
    printf '%s\n' "$req" >>"$FAKE/mcp-in"
    id=$(printf '%s' "$req" | sed -n 's/^.*"id":\([0-9]*\),.*$/\1/p')
    case $req in
    *'"name":"list_hosts"'*)
      rows='{"mem_available":1,"name":"kvm-host","up":1,"vms":2}'
      down=
      for p in $peers; do
        if wrong mcp-peer-down; then down=$p; continue; fi
        rows="$rows,{\"mem_available\":1,\"name\":\"$p\",\"up\":0,\"vms\":1}"
      done
      [ -z "$down" ] || rows="$rows],\"unreachable\":[{\"error\":\"ssh: connect to host $down port 22: Connection timed out\",\"host\":\"$down\"}"
      ok "{\"hosts\":[$rows]}" ;;
    *'"name":"create_vm"'*'"name":"client-check-2","host":"'"$first"'"'*)
      if wrong mcp-create-fails || [ -d "$FAKE/vm2" ]; then
        err "$first didn't answer: Transport closed"
      else
        mkdir "$FAKE/vm2"
        ok "$vm2"
      fi ;;
    *'"name":"list_vms"'*)
      on=$first
      ! wrong mcp-wrong-host || on=kvm-host
      vms=
      [ ! -d "$vm" ] || vms="{\"bytes\":4096,\"generation\":1,\"host\":\"kvm-host\",\"name\":\"client-check\",\"used\":4096}"
      [ ! -d "$FAKE/vm2" ] || vms="${vms:+$vms,}{\"bytes\":4096,\"generation\":0,\"host\":\"$on\",\"name\":\"client-check-2\",\"used\":4096}"
      ok "{\"vms\":[$vms]}" ;;
    *'"name":"vm_status"'*'"name":"client-check-2"'*)
      if [ -d "$FAKE/vm2" ]; then ok '{"address":"172.16.0.10","bytes":4096,"generation":0,"name":"client-check-2","state":"stopped","used":4096}'; else err "no VM named client-check-2"; fi ;;
    *'"name":"vm_status"'*'"name":"client-check"'*)
      ok "{\"address\":\"172.16.0.6\",\"bytes\":4096,\"generation\":1,\"name\":\"client-check\",\"state\":\"$(state)\",\"used\":4096}" ;;
    *'"name":"move_vm"'*'"name":"client-check"'*)
      to=$(printf '%s' "$req" | sed -n 's/^.*"host":"\([^"]*\)".*$/\1/p')
      if wrong mcp-move-fails; then
        err "client-check is on kvm-host-2 now, but didn't start there: kvm-host-2 didn't answer: Transport closed"
      else
        echo "$to" >"$vm/on"
        ok "{\"address\":\"172.16.0.6\",\"bytes\":4096,\"checkpoints\":0,\"generation\":1,\"host\":\"$to\",\"name\":\"client-check\",\"state\":\"$(state)\",\"used\":4096}"
      fi ;;
    *'"name":"delete_vm"'*'"name":"client-check-2"'*)
      if [ -d "$FAKE/vm2" ] && ! wrong mcp-delete-fails; then rm -r "$FAKE/vm2"; ok "$vm2"; else err "no VM named client-check-2"; fi ;;
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
        script(&bin.join("aws"), NO_AWS);
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
        laptop.mcp_http(Some("1"), Some(MCP_URL));
        laptop
    }

    /// The host's /etc/runner/mcp-http and mcp-url, as install.sh keeps
    /// them; None for one it hasn't written. On, with a URL, unless a test
    /// says otherwise.
    fn mcp_http(&self, on: Option<&str>, url: Option<&str>) -> &Laptop {
        let etc = self.fake().join("etc-runner");
        fs::create_dir_all(&etc).unwrap();
        for (file, got) in [("mcp-http", on), ("mcp-url", url)] {
            match got {
                Some(got) => fs::write(etc.join(file), format!("{got}\n")).unwrap(),
                None => drop(fs::remove_file(etc.join(file))),
            }
        }
        self
    }

    /// Every URL curl was asked for, with --doh-url's server if it had one.
    fn curl_calls(&self) -> Vec<Vec<String>> {
        calls(&self.fake().join("curl-calls"))
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

    /// ~/.config/runner/hosts with the host being checked and another.
    fn two_hosts(&self) -> &Laptop {
        let conf = self.home().join(".config/runner");
        fs::create_dir_all(&conf).unwrap();
        fs::write(conf.join("hosts"), "runner@kvm-host\nrunner@kvm-host-2\n").unwrap();
        self
    }

    fn vm2_is_there(&self) -> bool {
        self.fake().join("vm2").exists()
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
            let last = call.last().unwrap();
            // The client asks a host whether it answers (`true`, as runner)
            // before a command goes there, so a stopped one starts first.
            if last == "true" && call[call.len() - 2].starts_with("runner@") {
                continue;
            }
            // The move goes through the hosts file, finding the VM's host,
            // which `runner ip` asks as the checks do.
            let moving = last == "runner list" || last.starts_with("runner move client-check ");
            if moving || (last == "runner ip client-check" && call[0] == "RUNNER_HOST=") {
                assert_eq!(call[0], "RUNNER_HOST=", "{call:?}");
            } else if [
                "runner exec client-check -- cat /home/agent/moved",
                "runner move --help",
            ]
            .contains(&last.as_str())
            {
                // Then on the host it went to.
                assert_eq!(call[0], "RUNNER_HOST=runner@kvm-host", "{call:?}");
            } else if last.contains(" --peer ") {
                // MCP across hosts: the hosts file's, to the host checked.
                assert_eq!(call[0], "RUNNER_HOST=", "{call:?}");
                assert_eq!(call[call.len() - 2], "runner@kvm-host-2", "{call:?}");
                let want = "runner mcp --name runner@kvm-host-2 --peer runner@kvm-host";
                assert_eq!(last, want, "{call:?}");
            } else {
                assert_eq!(call[0], "RUNNER_HOST=runner@kvm-host-2", "{call:?}");
            }
        }
        let says = text(&out.stdout);
        assert!(
            says.contains("PASS  MCP list_hosts: kvm-host answers"),
            "{says}"
        );
    }
}

// MCP across hosts, when the hosts file lists the host checked and another:
// the checked host's MCP reaches the other, and a VM made there through it
// is listed, reached and deleted there.
#[test]
fn with_two_hosts_mcp_reaches_the_other_through_the_one_checked() {
    for shell in SHELLS {
        let laptop = Laptop::new();
        laptop.two_hosts();
        let out = laptop.run(shell, &["laptop"]);
        assert_eq!(out.status.code(), Some(0), "{shell:?}: {}", says(&out));
        let stdout = text(&out.stdout);
        for want in [
            "PASS  MCP list_hosts: kvm-host-2 answers",
            "PASS  MCP list_hosts: no host unreachable",
            "PASS  MCP create_vm: client-check-2 made on kvm-host-2",
            "PASS  MCP list_vms: client-check-2 is on kvm-host-2",
            "PASS  MCP vm_status: client-check-2 answers from kvm-host-2",
            "PASS  MCP vm_status: client-check still answers from runner@kvm-host",
            "PASS  MCP delete_vm: client-check-2 is gone from kvm-host-2",
            "PASS  move: client-check is running on kvm-host-2",
            "PASS  move: client-check's file came along",
            "PASS  move: client-check is gone from kvm-host",
            "PASS  MCP move_vm: client-check is back on kvm-host, running",
        ] {
            assert!(stdout.contains(want), "{shell:?} {want}: {stdout}");
        }
        let mcp = laptop.ssh_calls();
        let mcp: Vec<_> = mcp
            .iter()
            .filter(|c| c.last().unwrap().contains("--peer"))
            .collect();
        assert_eq!(mcp.len(), 2, "{shell:?} {mcp:?}");
        for call in mcp {
            assert_eq!(call[call.len() - 2], HOST, "{shell:?}");
        }
        // The move went to the VM's host, with the other as listed.
        let moved: Vec<_> = (laptop.host_calls().into_iter())
            .filter(|c| c.starts_with("move "))
            .collect();
        // Once the other host says it has move.
        let want = ["move --help", "move client-check runner@kvm-host-2"];
        assert_eq!(moved, want, "{shell:?}");
        assert!(!laptop.vm2_is_there(), "{shell:?}");
        assert!(!laptop.vm_is_there(), "{shell:?}");
    }
}

#[test]
fn with_one_host_mcp_across_hosts_says_it_isn_t_checked() {
    let laptop = Laptop::new();
    let out = laptop.run(SHELLS[0], &["laptop"]);
    assert_eq!(out.status.code(), Some(0), "{}", says(&out));
    let stdout = text(&out.stdout);
    assert!(stdout.contains("MCP across hosts: not checked"), "{stdout}");
    assert!(!stdout.contains("PASS  MCP list_hosts"), "{stdout}");
}

#[test]
fn mcp_across_hosts_fails_when_a_host_answers_it_wrong() {
    let cases: &[(&str, &[&str])] = &[
        (
            "mcp-peer-down",
            &[
                "MCP list_hosts: no kvm-host-2",
                "MCP list_hosts: unreachable",
            ],
        ),
        (
            "mcp-create-fails",
            &[
                "MCP create_vm: client-check-2 on kvm-host-2",
                "MCP list_vms: client-check-2 isn't listed",
                "MCP vm_status: client-check-2",
                "MCP delete_vm: client-check-2",
            ],
        ),
        (
            "mcp-wrong-host",
            &["MCP list_vms: client-check-2 isn't listed on kvm-host-2"],
        ),
        ("mcp-delete-fails", &["MCP delete_vm: client-check-2"]),
    ];
    for shell in SHELLS {
        for (key, want) in cases {
            let laptop = Laptop::new();
            laptop.two_hosts().wrong(key);
            let out = laptop.run(shell, &["laptop"]);
            assert_eq!(
                out.status.code(),
                Some(1),
                "{shell:?} {key}: {}",
                says(&out)
            );
            let fails = fails(&out);
            assert_eq!(fails.len(), want.len(), "{shell:?} {key}: {fails:?}");
            for (got, want) in fails.iter().zip(want.iter()) {
                assert!(
                    got.starts_with(&format!("FAIL  laptop: {want}")),
                    "{shell:?} {key}: {got:?} isn't {want:?}"
                );
            }
        }
    }
}

#[test]
fn a_move_between_hosts_fails_when_a_host_answers_it_wrong() {
    let cases: &[(&str, &[&str])] = &[
        ("move-fails", &["move: client-check to kvm-host-2"]),
        (
            "move-stays",
            &[
                "move: client-check's file on kvm-host-2",
                "move: client-check is still on kvm-host",
            ],
        ),
        (
            "move-loses-file",
            &["move: client-check's file on kvm-host-2"],
        ),
        ("mcp-move-fails", &["MCP move_vm: client-check to kvm-host"]),
        ("move-missing", &["move: kvm-host-2's runner has no move"]),
    ];
    for shell in SHELLS {
        for (key, want) in cases {
            let laptop = Laptop::new();
            laptop.two_hosts().wrong(key);
            let out = laptop.run(shell, &["laptop"]);
            assert_eq!(
                out.status.code(),
                Some(1),
                "{shell:?} {key}: {}",
                says(&out)
            );
            let fails = fails(&out);
            assert_eq!(fails.len(), want.len(), "{shell:?} {key}: {fails:?}");
            for (got, want) in fails.iter().zip(want.iter()) {
                assert!(
                    got.starts_with(&format!("FAIL  laptop: {want}")),
                    "{shell:?} {key}: {got:?} isn't {want:?}"
                );
            }
            // Back where the checks after look, and deleted at the end.
            assert!(!laptop.vm_is_there(), "{shell:?} {key}");
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
    // MCP over HTTP, through Funnel
    (
        "mcp-open",
        &[
            "MCP over HTTP: https://kvm-host.tailnet.ts.net:8443/mcp with no token gets HTTP 200, not 401",
        ],
    ),
    (
        "no-metadata",
        &[
            "MCP over HTTP: https://kvm-host.tailnet.ts.net:8443/.well-known/oauth-protected-resource gets HTTP 404, not 200",
        ],
    ),
    (
        "funnel-off",
        &[
            "MCP over HTTP: https://kvm-host.tailnet.ts.net:8443/mcp with no token gets HTTP 000, not 401",
            "MCP over HTTP: https://kvm-host.tailnet.ts.net:8443/.well-known/oauth-protected-resource gets HTTP 000, not 200",
        ],
    ),
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
    // The proxy. Nothing on the VM's port 8080 answers it, and the compose
    // check takes a missing service as a free port.
    (
        "service-fails",
        &[
            "service add web",
            "localhost:18080 doesn't answer",
            "localhost:18080 stopped answering",
        ],
    ),
    (
        "port-taken",
        &[
            "localhost:18080 answers before the proxy",
            "compose: localhost:18080 answers before the proxy",
        ],
    ),
    (
        "proxy-dead",
        &[
            "localhost:18080 doesn't answer",
            "localhost:18080 stopped answering",
            "compose: localhost:18080 doesn't answer from the stack: HTTP 000",
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
    // A compose stack through the proxy, and after a sleep and a wake
    (
        "idle-never-fails",
        &["compose: idle never: runner: couldn't write client-check's config"],
    ),
    (
        "stack-blank",
        &["compose: localhost:18080 doesn't answer from the stack: HTTP 200, ''"],
    ),
    (
        "service-rm-fails",
        &["compose: service rm web, which has port 8080: exit 1"],
    ),
    (
        "compose-up-fails",
        &["compose up: exit 1, Error response from daemon: Get"],
    ),
    (
        "compose-dead",
        &["compose: localhost:18080 doesn't answer from the stack: HTTP 000"],
    ),
    (
        "sleep-fails",
        &["compose: after runner sleep, client-check is running: runner: client-check has no boot"],
    ),
    (
        "wake-fails",
        &[
            "compose: after runner wake, client-check is sleeping: runner: client-check couldn't wake",
        ],
    ),
    (
        "stack-dies",
        &["compose: after the wake, localhost:18080 gives HTTP 000"],
    ),
    (
        "stack-restarts",
        &[
            "compose: after the wake, the page is '0b7e4c1d-9f2a-4c3e-8d6b-5a1f2e3d4c5b', not '6f1c2b9e-4d3a-4e0b-9a51-2c7d8e0f1a23'",
        ],
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
    // check_proxy's proxy, then the compose check's, before and after the
    // sleep: each let go.
    assert!(
        laptop.held() == ["holding", "let go"].repeat(3),
        "{:?}",
        laptop.held()
    );
    // The compose check frees port 8080 and stops idle sleeps first, then
    // brings the stack up, and sleeps and wakes the VM itself.
    let calls = laptop.host_calls();
    let at = |c: &str| {
        (calls.iter().rposition(|x| x.starts_with(c)))
            .unwrap_or_else(|| panic!("no {c:?} in {calls:?}"))
    };
    let up = format!("exec {VM} -d /home/agent/work -- sh -c mkdir -p compose-check");
    assert!(at(&format!("idle {VM} 90s")) < at(&format!("idle {VM} never")));
    assert!(at(&format!("idle {VM} never")) < at(&format!("service rm {VM} web")));
    assert!(at(&format!("service rm {VM} web")) < at(&up));
    assert!(at(&up) < at(&format!("sleep {VM}")));
    assert!(at(&format!("sleep {VM}")) < at(&format!("wake {VM}")));
    let up = &calls[at(&up)];
    for want in [
        "image: busybox:1.37",
        r#"ports: ["8080:8080"]"#,
        "restart: unless-stopped",
    ] {
        assert!(up.contains(want), "{want}: {up}");
    }
    let stdout = text(&out.stdout);
    assert!(
        stdout.contains("PASS  compose: the same container answers, not restarted"),
        "{stdout}"
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

// With MCP over HTTP on, the laptop reaches RUNNER_MCP_URL as claude.ai
// does, through Funnel: its name looked up in public DNS, which MagicDNS
// would otherwise answer with the host's tailnet address. /mcp wants a
// token, and the metadata a client finds the sign-in by answers.
#[test]
fn mcp_over_http_is_reached_through_funnel_by_public_dns() {
    for shell in SHELLS {
        let laptop = Laptop::new();
        let out = laptop.run(shell, &["laptop"]);
        assert_eq!(out.status.code(), Some(0), "{shell:?}: {}", says(&out));
        let stdout = text(&out.stdout);
        for want in [
            format!("PASS  MCP over HTTP: {MCP_URL}/mcp with no token gets 401"),
            format!("PASS  MCP over HTTP: {MCP_URL}/.well-known/oauth-protected-resource gets 200"),
        ] {
            assert!(stdout.contains(&want), "{shell:?}: {want}: {stdout}");
        }
        let ours: Vec<_> = (laptop.curl_calls().into_iter())
            .filter(|c| c.last().unwrap().starts_with(MCP_URL))
            .collect();
        assert_eq!(ours.len(), 2, "{ours:?}");
        for call in ours {
            let doh = call.iter().position(|a| a == "--doh-url");
            let server = doh.map(|i| call[i + 1].as_str());
            assert_eq!(server, Some("https://1.1.1.1/dns-query"), "{call:?}");
        }
        // Read on the host checked, as runner.
        let read = laptop.ssh_calls();
        let read = read.iter().find(|c| c.last().unwrap().starts_with("cat "));
        let read = read.unwrap_or_else(|| panic!("{:?}", laptop.ssh_calls()));
        assert_eq!(read[read.len() - 2], HOST, "{read:?}");
        assert_eq!(
            read.last().unwrap(),
            "cat /etc/runner/mcp-http /etc/runner/mcp-url"
        );
    }
}

#[test]
fn with_mcp_over_http_off_it_isn_t_checked() {
    for off in [Some("0"), None] {
        let laptop = Laptop::new();
        laptop.mcp_http(off, Some(MCP_URL));
        let out = laptop.run(SHELLS[0], &["laptop"]);
        assert_eq!(out.status.code(), Some(0), "{off:?}: {}", says(&out));
        let stdout = text(&out.stdout);
        let want = format!("== MCP over HTTP: not checked, since {HOST} has it off");
        assert!(stdout.contains(&want), "{off:?}: {stdout}");
        let calls = laptop.curl_calls();
        assert!(
            !calls.iter().flatten().any(|a| a.contains("ts.net")),
            "{calls:?}"
        );
    }
}

// On, with no URL, or one Funnel can't serve: nothing to reach, so a FAIL
// saying how to set it.
#[test]
fn mcp_over_http_without_an_https_url_fails_saying_how_to_set_one() {
    for url in [None, Some("http://kvm-host.tailnet.ts.net:8443")] {
        let laptop = Laptop::new();
        laptop.mcp_http(Some("1"), url);
        let out = laptop.run(SHELLS[0], &["laptop"]);
        assert_eq!(out.status.code(), Some(1), "{url:?}: {}", says(&out));
        let fails = fails(&out);
        assert_eq!(fails.len(), 1, "{url:?}: {fails:?}");
        let want = format!("FAIL  laptop: MCP over HTTP: {HOST} has no https RUNNER_MCP_URL");
        assert!(fails[0].starts_with(&want), "{url:?}: {fails:?}");
        assert!(fails[0].contains("RUNNER_MCP_URL=https://"), "{fails:?}");
    }
}

// Nothing answering through public DNS is Funnel off, or its name not there
// yet: the FAIL says so.
#[test]
fn mcp_over_http_with_funnel_off_says_to_look_at_funnel() {
    let laptop = Laptop::new();
    laptop.wrong("funnel-off");
    let out = laptop.run(SHELLS[0], &["laptop"]);
    assert_eq!(out.status.code(), Some(1), "{}", says(&out));
    let fails = fails(&out);
    assert_eq!(fails.len(), 2, "{fails:?}");
    for fail in &fails {
        assert!(fail.contains("public DNS. Is Funnel on?"), "{fail}");
    }
}
