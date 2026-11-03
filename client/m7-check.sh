#!/bin/sh
# M6 and M7 (#111) checked by hand, from the laptop, through the laptop's
# runner: what the KVM tests can't drive, the way you'd use it. Prints PASS or
# FAIL for each check and a count at the end, and exits 1 if any failed.
#
#   sh client/m7-check.sh
#
# Run it after the host is upgraded, base is rebuilt and imported, and
# client/install.sh has been rerun. It makes a scratch VM, m7-check (VM=name
# for another), from base, and deletes it at the end unless KEEP=1. It takes
# about 15 minutes, most of it waiting out idle times, and makes two short
# headless runs. POSIX sh only, since macOS's /bin/sh runs it.

# pass and fail always return 0, so `test && pass || fail` is an if-else.
# shellcheck disable=SC2015

vm=${VM:-m7-check}
port=${PORT:-18080}
passed=0
failed=0
proxy=

# No stdin, so keys typed while it runs stay in the terminal.
r() { runner "$@" </dev/null; }
step() { printf '\n== %s\n' "$*"; }
pass() {
  passed=$((passed + 1))
  printf 'PASS  %s\n' "$*"
}
fail() {
  failed=$((failed + 1))
  printf 'FAIL  %s\n' "$*"
}
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
# http URL: the HTTP status URL answers with, or 000.
http() { curl -s -o /dev/null -w '%{http_code}' -m "${2:-10}" "$1"; }
# A proxy left by a Ctrl-C would keep the VM awake until sshd gives up.
trap '[ -n "$proxy" ] && kill "$proxy" 2>/dev/null' EXIT
trap 'echo; echo "stopped; $vm is left for a look: runner stop $vm && runner delete $vm"; exit 130' INT TERM

command -v runner >/dev/null || {
  echo "m7-check: no runner on PATH: run client/install.sh first" >&2
  exit 2
}

step "Setup: $vm, a fresh fork of base"
if r status "$vm" >/dev/null 2>&1; then
  echo "$vm is left from an earlier check: deleting it"
  r stop "$vm" >/dev/null 2>&1
  r delete "$vm" >/dev/null || exit 1
fi
r create "$vm" >/dev/null || exit 1
# Never idle-sleeps until the proxy check, so nothing below races a sleep.
r idle "$vm" never >/dev/null || exit 1
r start "$vm" >/dev/null || exit 1
echo "$vm is up"

step "#110: Claude Code doesn't update itself"
got=$(r exec "$vm" -- timeout 60 claude doctor 2>&1 | grep -i 'auto-update')
case $got in
*[Dd]isabled*) pass "claude doctor: $got" ;;
*) fail "claude doctor says: ${got:-nothing about auto-updates}" ;;
esac

step "#109: VMs reach the internet but not the host"
got=$(r exec "$vm" -- curl -sS -m 20 -o /dev/null -w '%{http_code}' https://example.com 2>&1)
[ "$got" = 200 ] && pass "curl https://example.com from the VM: 200" || fail "curl https://example.com from the VM: $got"
# shellcheck disable=SC2016 # expanded in the VM, not here.
got=$(r exec "$vm" -- sh -c 'gw=$(ip route | awk "/default/ {print \$3}"); timeout 5 bash -c "</dev/tcp/$gw/22" 2>/dev/null && echo "reached $gw:22" || echo "blocked $gw:22"')
case $got in
blocked*) pass "the host's sshd from the VM: $got" ;;
*) fail "the host's sshd from the VM: $got" ;;
esac

step "#106: credential variables are refused"
got=$(r exec "$vm" -e GH_TOKEN=x -- touch /tmp/m7-ran 2>&1)
code=$?
case $got in
*"GH_TOKEN is a credential"*"runner creds"*) [ "$code" -ne 0 ] && pass "exec -e GH_TOKEN: refused, exit $code" || fail "exec -e GH_TOKEN: said '$got' but exited 0" ;;
*) fail "exec -e GH_TOKEN: exit $code, '$got'" ;;
esac
r exec "$vm" -- test ! -e /tmp/m7-ran && pass "exec -e GH_TOKEN: nothing ran" || fail "exec -e GH_TOKEN: the command ran"
got=$(r exec "$vm" -e FOO=bar -- printenv FOO 2>&1)
[ "$got" = bar ] && pass "exec -e FOO=bar: passes through" || fail "exec -e FOO=bar: got '$got'"

step "#106: MCP exec keeps both ends of long output"
init='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"m7-check","version":"1"}}}'
ready='{"jsonrpc":"2.0","method":"notifications/initialized"}'
long='{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"exec","arguments":{"name":"'$vm'","argv":["seq","1","40000"]}}}'
cred='{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"exec","arguments":{"name":"'$vm'","argv":["true"],"env":{"GH_TOKEN":"x"}}}}'
# runner mcp ends at the end of its stdin, so that's held open while it answers.
echo "asking runner mcp; about 30 seconds"
mcp=$({
  printf '%s\n' "$init" "$ready" "$long" "$cred"
  sleep 30
} | runner mcp 2>/dev/null)
got=$(printf '%s\n' "$mcp" | grep '"id":2,')
printf '%s' "$got" | grep -q '"stdout":"1\\n2\\n3\\n' && pass "MCP exec: stdout starts at 1" || fail "MCP exec: stdout doesn't start at 1"
printf '%s' "$got" | grep -q '39999\\n40000\\n"' && pass "MCP exec: stdout ends at 40000" || fail "MCP exec: stdout doesn't end at 40000"
cut=$(printf '%s' "$got" | grep -o '\[runner cut [0-9]* bytes here\]' | head -1)
[ -n "$cut" ] && pass "MCP exec: marks the cut: $cut" || fail "MCP exec: no cut marked"
printf '%s' "$got" | grep -q '"stdout_truncated":true' && pass "MCP exec: stdout_truncated is true" || fail "MCP exec: stdout_truncated isn't true"
got=$(printf '%s\n' "$mcp" | grep '"id":3,')
printf '%s' "$got" | grep -q '"isError":true' && printf '%s' "$got" | grep -q 'GH_TOKEN is a credential' &&
  pass "MCP exec with env GH_TOKEN: refused" || fail "MCP exec with env GH_TOKEN: ${got:-no reply}"

step "#105: a run that joins a VM in use and fails leaves it running"
# What "in use" looks like from inside: a tmux session, and the boot it's on.
r exec "$vm" -- tmux new-session -d -s in-use sleep 100000
session=no
r sessions "$vm" | grep -q '^in-use  ' && session=yes
boot=$(r exec "$vm" -- cat /proc/sys/kernel/random/boot_id)
run=$(r run "$vm" "Write hi to /home/agent/work/joined.txt, then run sleep 600 in the shell.")
echo "${run:-no run} started; waiting for its agent to write joined.txt"
if [ -n "$run" ] && r exec "$vm" -- timeout 300 sh -c 'until [ -f /home/agent/work/joined.txt ]; do sleep 2; done'; then
  r cancel "$run" >/dev/null
  waited=0
  while row=$(r runs "$vm" | grep "^$run  ") && printf '%s' "$row" | grep -q 'Z  running  ' && [ "$waited" -lt 120 ]; do
    sleep 5
    waited=$((waited + 5))
  done
  echo "runner runs: $row"
  n=$(printf '%s' "$row" | sed -n 's/.*not rolled back, checkpoint \([0-9]*\) undoes it.*/\1/p')
  [ -n "$n" ] && pass "cancelled: not rolled back, checkpoint $n undoes it" || fail "cancelled, but runs says: $row"
  [ "$(r exec "$vm" -- cat /proc/sys/kernel/random/boot_id)" = "$boot" ] && pass "the VM wasn't restarted" || fail "the VM was restarted"
  if [ $session = yes ]; then
    r sessions "$vm" | grep -q '^in-use  ' && pass "its tmux session carried on" || fail "its tmux session is gone"
  fi
  r exec "$vm" -- test -f /home/agent/work/joined.txt && pass "what the run wrote is still there" || fail "joined.txt is gone"
  got=$(r result "$run")
  printf '%s' "$got" | grep -q '"outcome": "not_rolled_back"' && pass "runner result: outcome not_rolled_back" || fail "runner result: $(printf '%s' "$got" | grep '"outcome"')"
  [ -n "$n" ] && { printf '%s' "$got" | grep -q "\"checkpoint\": $n" && pass "runner result: checkpoint $n" || fail "runner result doesn't name checkpoint $n"; }

  step "#104: run checkpoints, and gc skips a running VM"
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
else
  fail "the run never wrote joined.txt: runner output ${run:-?}"
  [ -n "$run" ] && r cancel "$run" >/dev/null 2>&1
fi

step "#107: a run's result leaves out boot noise"
r stop "$vm" >/dev/null
run=$(r run "$vm" "Write hello to /home/agent/work/hello.txt. Do nothing else.")
echo "$run started on the stopped VM; waiting for it to end and be measured"
r output "$run" --follow >/dev/null 2>&1
got=$(r result "$run")
printf '%s' "$got" | grep -q '"outcome": "committed"' && pass "runner result: committed" || fail "runner result: $(printf '%s' "$got" | grep '"outcome"')"
printf '%s' "$got" | grep -q '"path": "/home/agent/work/hello.txt"' && pass "changed lists /home/agent/work/hello.txt" || fail "changed doesn't list hello.txt"
boot_only=$(printf '%s' "$got" | sed -n 's/.*"boot_only": \([0-9]*\).*/\1/p')
[ "${boot_only:-0}" -gt 0 ] && pass "boot_only: $boot_only paths left out" || fail "boot_only: ${boot_only:-missing}"
others=$(printf '%s\n' "$got" | sed -n 's/^ *"path": "\(.*\)",*$/\1/p' | grep -v -e '^/home/' -e '^/tmp/' -e '^/tmp$')
[ -z "$others" ] && pass "changed lists nothing outside /home and /tmp" || fail "changed lists, outside /home and /tmp: $(echo "$others" | tr '\n' ' ')"

step "M6 and #108: runner proxy, and an open proxy keeps the VM awake"
r idle "$vm" 90s >/dev/null
r service add "$vm" web -- python3 -m http.server 8080 >/dev/null || fail "service add web"
runner proxy "$vm" "$port:8080" </dev/null 2>/dev/null &
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

step "M6: the VM's URL"
url=$(r url "$vm")
case $url in
http://*)
  before=$(state)
  got=$(http "$url/" 45)
  [ "$got" = 200 ] && pass "$url answers, with $vm $before before" || fail "$url: HTTP $got, with $vm $before before"
  [ "$(state)" = running ] && pass "the URL left $vm running" || fail "after the URL, $vm is $(state)"
  r url "$vm" off >/dev/null
  sleep 3
  got=$(http "$url/" 5)
  [ "$got" = 000 ] && pass "url off: $url refuses" || fail "url off: $url still gives HTTP $got"
  r url "$vm" on >/dev/null
  sleep 3
  got=$(http "$url/" 45)
  [ "$got" = 200 ] && pass "url on: $url answers again" || fail "url on: $url gives HTTP $got"
  ;;
*) fail "runner url $vm: $url" ;;
esac

if [ "${KEEP-}" = 1 ]; then
  echo
  echo "KEEP=1: $vm stays"
else
  r stop "$vm" >/dev/null 2>&1
  r delete "$vm" >/dev/null && printf '\ndeleted %s\n' "$vm"
fi
printf '\n%s passed, %s failed\n' "$passed" "$failed"
[ "$failed" -eq 0 ]
