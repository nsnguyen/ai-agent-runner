#!/bin/sh
# A milestone's check, from the laptop, in one command:
#
#   client/check.sh           both parts: about 40 minutes with a base rebuild
#   client/check.sh host      copy this checkout to the host and run host/check.sh
#   client/check.sh laptop    the laptop checks alone
#
# host:   copies this checkout to the KVM host, as README.md's After merging
#         a change did, with --delete but leaving target and images/base/out
#         be, then runs host/check.sh there (docs/host.md, Boot tests), its
#         lines shown as they come.
# laptop: what only the laptop's path shows, through this checkout's
#         client/runner, on a scratch VM, client-check, forked from base and
#         deleted at the end, after a FAIL too, unless KEEP=1.
#
# The host is RUNNER_HOST, else the first line of ~/.config/runner/host, as
# client/runner has it, and every laptop check runs with RUNNER_HOST set to
# it. The copy and host/check.sh go in as ubuntu@ that host's name, with
# client/aws.sh's key (RUNNER_AWS_KEY, else ~/.ssh/kvm-host.pem) when it's
# there; RUNNER_ADMIN names another login.
#
# It ends with a summary: every FAIL again, then the counts. Exit 0 when
# nothing failed, 1 on any FAIL, 2 when the host doesn't answer. Ctrl-C ends
# a proxy and leaves the scratch VM for a look, saying how to delete it.
# A new by-hand check goes in here, under Laptop checks, never in a script of
# its own. POSIX sh only, since macOS's /bin/sh runs it.

# pass and fail always return 0, so `test && pass || fail` is an if-else.
# shellcheck disable=SC2015

usage="usage: client/check.sh [host|laptop]
  client/check.sh           copy, host/check.sh on the host, then the laptop checks
  client/check.sh host      copy and host/check.sh only
  client/check.sh laptop    the laptop checks only"

do_host=1
do_laptop=1
case $# in
0) ;;
1)
  case $1 in
  host) do_laptop= ;;
  laptop) do_host= ;;
  -h | --help)
    echo "$usage"
    exit 0
    ;;
  *)
    echo "$usage" >&2
    exit 2
    ;;
  esac
  ;;
*)
  echo "$usage" >&2
  exit 2
  ;;
esac

host=${RUNNER_HOST-}
if [ -z "$host" ] && [ -r "$HOME/.config/runner/host" ]; then
  read -r host <"$HOME/.config/runner/host"
fi
case $host in
'' | -*)
  echo "check.sh: no host. Set RUNNER_HOST, or put it in ~/.config/runner/host (client/install.sh <host> does)." >&2
  exit 2
  ;;
esac
RUNNER_HOST=$host
export RUNNER_HOST
admin=${RUNNER_ADMIN:-ubuntu@${host#*@}}
case $admin in -*)
  echo "check.sh: RUNNER_ADMIN '$admin' isn't a login: give it as ssh takes it, like ubuntu@kvm-host" >&2
  exit 2
  ;;
esac
key=${RUNNER_AWS_KEY:-$HOME/.ssh/${RUNNER_AWS_NAME:-kvm-host}.pem}

# With CDPATH set, cd prints where it went, which would land in $repo.
unset CDPATH
repo=$(cd "$(dirname "$0")/.." && pwd -P) || exit 2
cd "$repo" || exit 2
runner=$repo/client/runner

# No prompts, so a refused key fails at once, and a dropped tailnet ends the
# copy or the host check in a minute and a half rather than hanging it.
opts="-o BatchMode=yes -o ConnectTimeout=10 -o StrictHostKeyChecking=accept-new -o ServerAliveInterval=30"
# IdentitiesOnly: an agent offering six other keys first would use up sshd's
# tries. rsync's -e takes single quotes around an argument.
if [ -r "$key" ]; then
  rsh="ssh -i '$key' -o IdentitiesOnly=yes $opts"
else
  rsh="ssh $opts"
fi
# $opts is the options, split, and the remote commands are this script's own.
# shellcheck disable=SC2086,SC2029
admin_ssh() {
  if [ -r "$key" ]; then
    ssh -i "$key" -o IdentitiesOnly=yes $opts "$@"
  else
    ssh $opts "$@"
  fi
}

branch=$(git rev-parse --abbrev-ref HEAD 2>/dev/null || echo "no branch")
commit=$(git rev-parse --short HEAD 2>/dev/null || echo "no commit")
dirty=
[ -z "$(git status --porcelain 2>/dev/null)" ] || dirty=", with uncommitted changes"
where="$branch at $commit$dirty"
echo "check: $where, on $host"

tmp=$(mktemp -d "${TMPDIR:-/tmp}/client-check.XXXXXX") || exit 2
: >"$tmp/fails"
part=host
hp=0
hf=0
lp=0
lf=0
host_log=
vm=client-check
port=18080
proxy=
made=

step() { printf '\n== %s\n' "$*"; }
pass() {
  printf 'PASS  %s\n' "$*"
  if [ "$part" = host ]; then hp=$((hp + 1)); else lp=$((lp + 1)); fi
}
fail() {
  printf 'FAIL  %s\n' "$*"
  printf 'FAIL  %s: %s\n' "$part" "$*" >>"$tmp/fails"
  if [ "$part" = host ]; then hf=$((hf + 1)); else lf=$((lf + 1)); fi
}
delete_how() {
  echo "RUNNER_HOST=$host client/runner stop $vm && RUNNER_HOST=$host client/runner delete $vm"
}

# A proxy left by a Ctrl-C would keep the VM awake until sshd gives up.
trap '[ -z "$proxy" ] || kill "$proxy" 2>/dev/null; rm -rf "$tmp"' EXIT
stopped() {
  echo
  if [ -n "$made" ]; then
    echo "check: stopped. $vm is left for a look; delete it with: $(delete_how)"
  else
    echo "check: stopped."
  fi
  exit 130
}
trap stopped INT TERM

# A host that doesn't answer ends it here, before anything runs.
down() {
  echo "check.sh: $1 doesn't answer over ssh, or won't take this laptop's key. If the host is stopped, start it: client/aws.sh start. Otherwise check that Tailscale is up on this laptop." >&2
  exit 2
}
if [ -n "$do_host" ]; then
  admin_ssh -n "$admin" true || down "$admin"
fi
if [ -n "$do_laptop" ]; then
  # shellcheck disable=SC2086
  ssh -n $opts "$host" true || down "$host"
fi

# Host: copy, then host/check.sh. Fails when the host never got to its tests,
# so the laptop checks would run against the old build.
host_part() {
  part=host
  step "Host: copying this checkout to $admin:ai-agent-runner"
  rsync -az --delete --exclude target --exclude images/base/out -e "$rsh" ./ "$admin:ai-agent-runner/"
  code=$?
  if [ "$code" -ne 0 ]; then
    fail "copy: rsync to $admin exited $code"
    return 1
  fi
  step "Host: host/check.sh, about 12 minutes, and 10 more when base is rebuilt"
  {
    admin_ssh -n "$admin" 'cd ai-agent-runner && host/check.sh'
    echo $? >"$tmp/host-exit"
  } | tee "$tmp/host"
  code=$(cat "$tmp/host-exit")
  hp=$((hp + $(grep -c '^PASS  ' "$tmp/host")))
  hf=$((hf + $(grep -c '^FAIL  ' "$tmp/host")))
  sed -n 's/^FAIL  /FAIL  host: /p' "$tmp/host" >>"$tmp/fails"
  count=$(grep '^check: [0-9]* passed, [0-9]* failed' "$tmp/host" | tail -n 1)
  host_log=${count##*Log: }
  [ "$host_log" != "$count" ] || host_log=
  if [ "$code" -ne 0 ] && ! grep -q '^FAIL  ' "$tmp/host"; then
    fail "host/check.sh exited $code${count:+: $count}"
  fi
  case $count in '' | *", stopped."*) return 1 ;; esac
}

# --- Laptop checks ---------------------------------------------------------

# No stdin, so keys typed while it runs stay in the terminal.
r() { "$runner" "$@" </dev/null; }
# state: the VM's state, the last word of `runner status` before its creds.
state() { r status "$vm" 2>/dev/null | sed 's/  creds .*//' | awk '{print $NF}'; }
# wait_state STATE SECONDS: whether the VM gets to STATE within SECONDS.
wait_state() {
  waited=0
  while [ "$(state)" != "$1" ]; do
    [ "$waited" -ge "$2" ] && return 1
    sleep 10
    waited=$((waited + 10))
  done
}
# http URL [SECONDS]: the HTTP status URL answers with, or 000.
http() { curl -s -o /dev/null -w '%{http_code}' -m "${2:-10}" "$1"; }
# run_row RUN: RUN's line in `runner runs`.
run_row() { r runs "$vm" | awk -v id="$1" '$1 == id'; }

check_exec() {
  step "exec: a credential's variable is refused, others pass through"
  got=$(r exec "$vm" -e GH_TOKEN=x -- touch /tmp/client-check-ran 2>&1)
  code=$?
  case $got in
  *"GH_TOKEN is a credential"*"runner creds"*) [ "$code" -ne 0 ] && pass "exec -e GH_TOKEN: refused, exit $code" || fail "exec -e GH_TOKEN: said '$got' but exited 0" ;;
  *) fail "exec -e GH_TOKEN: exit $code, '$got'" ;;
  esac
  r exec "$vm" -- test ! -e /tmp/client-check-ran && pass "exec -e GH_TOKEN: nothing ran" || fail "exec -e GH_TOKEN: the command ran"
  got=$(r exec "$vm" -e FOO=bar -- printenv FOO 2>&1)
  [ "$got" = bar ] && pass "exec -e FOO=bar: passes through" || fail "exec -e FOO=bar: got '$got'"
}

check_mcp() {
  step "MCP exec over SSH keeps both ends of long output"
  init='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"client-check","version":"1"}}}'
  ready='{"jsonrpc":"2.0","method":"notifications/initialized"}'
  long='{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"exec","arguments":{"name":"'$vm'","argv":["seq","1","40000"]}}}'
  cred='{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"exec","arguments":{"name":"'$vm'","argv":["true"],"env":{"GH_TOKEN":"x"}}}}'
  # runner mcp ends at the end of its stdin, so that's held open while it answers.
  echo "asking runner mcp; about 30 seconds"
  mcp=$({
    printf '%s\n' "$init" "$ready" "$long" "$cred"
    sleep 30
  } | "$runner" mcp 2>/dev/null)
  got=$(printf '%s\n' "$mcp" | grep '"id":2,')
  printf '%s' "$got" | grep -q '"stdout":"1\\n2\\n3\\n' && pass "MCP exec: stdout starts at 1" || fail "MCP exec: stdout doesn't start at 1"
  printf '%s' "$got" | grep -q '39999\\n40000\\n"' && pass "MCP exec: stdout ends at 40000" || fail "MCP exec: stdout doesn't end at 40000"
  cut=$(printf '%s' "$got" | grep -o '\[runner cut [0-9]* bytes here\]' | head -n 1)
  [ -n "$cut" ] && pass "MCP exec: marks the cut: $cut" || fail "MCP exec: no cut marked"
  printf '%s' "$got" | grep -q '"stdout_truncated":true' && pass "MCP exec: stdout_truncated is true" || fail "MCP exec: stdout_truncated isn't true"
  got=$(printf '%s\n' "$mcp" | grep '"id":3,')
  printf '%s' "$got" | grep -q '"isError":true' && printf '%s' "$got" | grep -q 'GH_TOKEN is a credential' &&
    pass "MCP exec with env GH_TOKEN: refused" || fail "MCP exec with env GH_TOKEN: ${got:-no reply}"
}

# A run that joins a VM in use and is cancelled leaves it running, its
# writes and all, with a checkpoint that undoes it; gc skips the VM meanwhile.
check_joined() {
  step "A run that joins a VM in use and is cancelled leaves it running"
  # What "in use" looks like from inside: a tmux session, and the boot it's on.
  r exec "$vm" -- tmux new-session -d -s in-use sleep 100000
  session=yes
  r sessions "$vm" | grep -q '^in-use  ' || {
    session=no
    fail "runner sessions doesn't list the in-use session tmux started"
  }
  boot=$(r exec "$vm" -- cat /proc/sys/kernel/random/boot_id)
  run=$(r run "$vm" "Write hi to /home/agent/work/joined.txt, then run sleep 600 in the shell.")
  echo "${run:-no run} started; waiting for its agent to write joined.txt"
  if [ -z "$run" ] || ! r exec "$vm" -- timeout 300 sh -c 'until [ -f /home/agent/work/joined.txt ]; do sleep 2; done'; then
    fail "the run never wrote joined.txt: runner run printed '${run:-nothing}'"
    [ -z "$run" ] || r cancel "$run" >/dev/null 2>&1
    return
  fi
  r cancel "$run" >/dev/null
  waited=0
  while :; do
    row=$(run_row "$run")
    case $row in *"Z  running  "*) ;; *) break ;; esac
    [ "$waited" -ge 120 ] && break
    sleep 5
    waited=$((waited + 5))
  done
  echo "runner runs: $row"
  n=$(printf '%s' "$row" | sed -n 's/.*not rolled back, checkpoint \([0-9]*\) undoes it.*/\1/p')
  [ -n "$n" ] && pass "cancelled: not rolled back, checkpoint $n undoes it" || fail "cancelled, but runs says: $row"
  [ "$(r exec "$vm" -- cat /proc/sys/kernel/random/boot_id)" = "$boot" ] && pass "the VM wasn't restarted" || fail "the VM was restarted"
  if [ "$session" = yes ]; then
    r sessions "$vm" | grep -q '^in-use  ' && pass "its tmux session carried on" || fail "its tmux session is gone"
  fi
  r exec "$vm" -- test -f /home/agent/work/joined.txt && pass "what the run wrote is still there" || fail "joined.txt is gone"
  got=$(r result "$run")
  printf '%s' "$got" | grep -q '"outcome": "not_rolled_back"' && pass "runner result: outcome not_rolled_back" || fail "runner result: $(printf '%s' "$got" | grep '"outcome"')"
  if [ -n "$n" ]; then
    printf '%s' "$got" | grep -q "\"checkpoint\": $n," && pass "runner result: checkpoint $n" || fail "runner result doesn't name checkpoint $n"
  fi

  step "The run's checkpoint, and gc skips a running VM"
  if [ -n "$n" ]; then
    r checkpoints "$vm" | grep -q "^. $n  [^ ]*  run  " && pass "checkpoint $n is listed as kind run" || fail "checkpoint $n: $(r checkpoints "$vm" | grep "^. $n ")"
  fi
  got=$(r gc --dry-run 2>&1)
  code=$?
  printf '%s\n' "$got" | grep -q "^$vm  skipped: running" && [ "$code" -eq 0 ] && pass "gc --dry-run: skips $vm, exit 0" || fail "gc --dry-run: exit $code, '$got'"
  r gc "$vm" >/dev/null 2>&1 && fail "gc $vm: ran on a running VM" || pass "gc $vm: refused while it runs"
  if [ -n "$n" ]; then
    r restore "$vm" "$n" >/dev/null && r exec "$vm" -- test ! -e /home/agent/work/joined.txt && pass "restore $n rolled the run back by hand" || fail "restore $n didn't roll the run back"
  fi
}

check_result() {
  step "A run's result leaves out boot noise"
  r stop "$vm" >/dev/null
  run=$(r run "$vm" "Write hello to /home/agent/work/hello.txt. Do nothing else.")
  if [ -z "$run" ]; then
    fail "runner run on the stopped $vm printed no run"
    return
  fi
  echo "$run started on the stopped VM; waiting for it to end and be measured"
  r output "$run" --follow >/dev/null 2>&1
  got=$(r result "$run")
  printf '%s' "$got" | grep -q '"outcome": "committed"' && pass "runner result: committed" || fail "runner result: $(printf '%s' "$got" | grep '"outcome"')"
  printf '%s' "$got" | grep -q '"path": "/home/agent/work/hello.txt"' && pass "changed lists /home/agent/work/hello.txt" || fail "changed doesn't list hello.txt"
  boot_only=$(printf '%s' "$got" | sed -n 's/.*"boot_only": \([0-9]*\).*/\1/p')
  [ "${boot_only:-0}" -gt 0 ] && pass "boot_only: $boot_only paths left out" || fail "boot_only: ${boot_only:-missing}"
  others=$(printf '%s\n' "$got" | sed -n 's/^ *"path": "\(.*\)",*$/\1/p' | grep -v -e '^/home/' -e '^/tmp/' -e '^/tmp$')
  [ -z "$others" ] && pass "changed lists nothing outside /home and /tmp" || fail "changed lists, outside /home and /tmp: $(echo "$others" | tr '\n' ' ')"
}

check_proxy() {
  step "runner proxy, and an open proxy keeps the VM awake"
  r idle "$vm" 90s >/dev/null
  r service add "$vm" web -- python3 -m http.server 8080 >/dev/null || fail "service add web"
  # Else what answers is that, not the VM.
  if [ "$(http "http://localhost:$port/" 5)" != 000 ]; then
    fail "localhost:$port answers before the proxy opens: stop what's on it"
    return
  fi
  "$runner" proxy "$vm" "$port:8080" </dev/null 2>/dev/null &
  proxy=$!
  waited=0
  until [ "$(http "http://localhost:$port/" 5)" = 200 ] || [ "$waited" -ge 60 ]; do
    sleep 2
    waited=$((waited + 2))
  done
  [ "$(http "http://localhost:$port/")" = 200 ] && pass "localhost:$port answers from the VM's port 8080" || fail "localhost:$port doesn't answer"
  echo "holding the proxy open for 2.5 minutes, past the 90s idle time"
  sleep 150
  got=$(state)
  [ "$got" = running ] && pass "with the proxy open past its idle time, $vm is running" || fail "with the proxy open, $vm is $got"
  [ "$(http "http://localhost:$port/")" = 200 ] && pass "localhost:$port still answers" || fail "localhost:$port stopped answering"
  kill "$proxy" 2>/dev/null
  wait "$proxy" 2>/dev/null
  proxy=
  echo "proxy closed; waiting up to 3 minutes for $vm to sleep"
  wait_state sleeping 180 && pass "$vm sleeps once the proxy ends" || fail "$vm is $(state) 3 minutes after the proxy ended"
}

check_url() {
  step "The VM's URL, on and off"
  url=$(r url "$vm")
  case $url in
  http://*) ;;
  *)
    fail "runner url $vm: $url"
    return
    ;;
  esac
  before=$(state)
  got=$(http "$url/" 45)
  [ "$got" = 200 ] && pass "$url answers, with $vm $before before" || fail "$url: HTTP $got, with $vm $before before"
  got=$(state)
  [ "$got" = running ] && pass "the URL left $vm running" || fail "after the URL, $vm is $got"
  r url "$vm" off >/dev/null
  sleep 3
  got=$(http "$url/" 5)
  [ "$got" = 000 ] && pass "url off: $url refuses" || fail "url off: $url still gives HTTP $got"
  r url "$vm" on >/dev/null
  sleep 3
  got=$(http "$url/" 45)
  [ "$got" = 200 ] && pass "url on: $url answers again" || fail "url on: $url gives HTTP $got"
}

# The checks in order: each leaves the VM as the next expects. About 15
# minutes, most of it waiting out idle times, with two short headless runs.
laptop_checks() {
  check_exec
  check_mcp
  check_joined
  check_result
  check_proxy
  check_url
}

laptop_part() {
  part=laptop
  step "Laptop: $vm, a fresh fork of base, through client/runner"
  if r status "$vm" >/dev/null 2>&1; then
    echo "$vm is left from an earlier check: deleting it"
    r stop "$vm" >/dev/null 2>&1
    r delete "$vm" >/dev/null || {
      fail "setup: couldn't delete the $vm an earlier check left"
      return
    }
  fi
  r create "$vm" >/dev/null || {
    fail "setup: runner create $vm failed"
    return
  }
  made=1
  # Never idle-sleeps until the proxy check, so nothing before it races a sleep.
  if ! r idle "$vm" never >/dev/null || ! r start "$vm" >/dev/null; then
    fail "setup: $vm didn't start"
  else
    echo "$vm is up"
    laptop_checks
  fi
  if [ "${KEEP-}" = 1 ]; then
    printf '\nKEEP=1: %s stays. Delete it with: %s\n' "$vm" "$(delete_how)"
  else
    r stop "$vm" >/dev/null 2>&1
    if r delete "$vm" >/dev/null; then
      printf '\ndeleted %s\n' "$vm"
    else
      printf "\ncouldn't delete %s: the next check deletes it first, or: %s\n" "$vm" "$(delete_how)"
    fi
  fi
}

ran_host=yes
if [ -n "$do_host" ]; then
  host_part || ran_host=
fi
if [ -n "$do_laptop" ]; then
  if [ -n "$ran_host" ]; then
    laptop_part
  else
    part=laptop
    fail "not run: the host check stopped before its tests, so the host isn't this checkout"
  fi
fi

step "Summary: $where, on $host"
cat "$tmp/fails"
if [ -n "$do_host" ]; then
  echo "host: $hp passed, $hf failed${host_log:+. Log on the host: $host_log}"
fi
if [ -n "$do_laptop" ]; then
  echo "laptop: $lp passed, $lf failed"
fi
echo "all: $((hp + lp)) passed, $((hf + lf)) failed"
[ $((hf + lf)) -eq 0 ]
