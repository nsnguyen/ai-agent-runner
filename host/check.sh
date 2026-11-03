#!/usr/bin/env bash
# The host's side of a milestone check: upgrades the host to this checkout,
# then runs every boot test. From the checkout, as the sudo user (ubuntu on
# AWS), not root:
#
#   host/check.sh [filter]
#
# 1. install  cargo build --release, then sudo host/install.sh.
# 2. base     Builds runner-guest for musl. Rebuilds base (sudo ENGINE=podman
#             images/base/build.sh) only when images/base/out has no disk, or
#             one built from something other than this checkout, and imports
#             it as base when the host's base wasn't built from this checkout
#             either. Installs the kernel where it differs.
# 3. doctor   runner doctor, as the runner user: every line ok, warn included,
#             and base built from this checkout.
# 4. tests    Every #[ignore]d test in every test file, one at a time, as root,
#             or only those matching the filter. About 12 minutes.
#
# A PASS or FAIL line for each, then a count. Exits 1 on any FAIL; a failed
# install or base stops it there. Each step's full output goes to a log under
# target/check, which the count names. FORCE_BASE=1 rebuilds base anyway, and
# ENGINE picks docker over podman.
set -uo pipefail
shopt -s lastpipe

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo=$(dirname "$here")
out=$repo/images/base/out
disk=$out/base.ext4
kernel=$out/vmlinux
# RUNNER_ROOT is for tests: sudo drops it, so doctor never sees it.
root=${RUNNER_ROOT:-/var/lib/runner}
port=10022 # runner-guest's vsock port (src/proto.rs)

if (($# > 1)); then
  echo "usage: host/check.sh [filter]" >&2
  exit 2
fi
cd "$repo" || exit 2
# A non-interactive ssh doesn't read the profile that puts cargo on PATH.
if ! command -v cargo >/dev/null && [[ -f $HOME/.cargo/env ]]; then
  # shellcheck source=/dev/null
  . "$HOME/.cargo/env"
fi

mkdir -p target/check || exit 2
log=$(mktemp "$repo/target/check/$(date -u +%Y%m%dT%H%M%SZ)-XXXX.log") || exit 2
echo "check: log at $log"

passed=0
failed=0
pass() {
  passed=$((passed + 1))
  printf 'PASS  %s\n' "$*" | tee -a "$log"
}
fail() {
  failed=$((failed + 1))
  printf 'FAIL  %s\n' "$*" | tee -a "$log"
}
summary() {
  printf 'check: %d passed, %d failed%s. Log: %s\n' "$passed" "$failed" "${1:+, $1}" "$log"
  ((failed == 0))
  exit
}
# Runs a command with its output in the log, under a header.
logged() {
  printf '\n== %s\n' "$*" >>"$log"
  "$@" >>"$log" 2>&1
}
short() { printf '%.12s' "$1"; }

# The fingerprint in a base doctor line, or in a disk's /.runner/build.
doctor_fp() { sed -n 's/^[a-z]* *base .*built from \([0-9a-f]\{64\}\) .*/\1/p' <<<"$1"; }
disk_fp() {
  [[ -f $1 ]] || return 0
  debugfs -R 'cat /.runner/build' "$1" 2>/dev/null | sed -n 's/^fingerprint=\([0-9a-f]\{64\}\)$/\1/p'
}
doctor() { sudo -u runner runner doctor 2>&1; }

# 1. install
if ! logged cargo build --release; then
  fail "install: cargo build --release failed"
  summary "stopped"
fi
if ! logged sudo "$here/install.sh"; then
  fail "install: host/install.sh failed"
  summary "stopped"
fi
pass "install"

# 2. base
if ! logged cargo build --release --target x86_64-unknown-linux-musl --bin runner-guest; then
  fail "guest: cargo build of runner-guest for musl failed"
  summary "stopped"
fi
pass "guest"
if ! want=$(images/base/build.sh --fingerprint 2>>"$log") || [[ -z $want ]]; then
  fail "base: images/base/build.sh --fingerprint failed"
  summary "stopped"
fi
printf '\n== base: this checkout is %s\n' "$want" >>"$log"
before=$(doctor)
printf '%s\n' "$before" >>"$log"
on_host=$(doctor_fp "$before")
on_disk=$(disk_fp "$disk")
built=
if [[ ${FORCE_BASE:-} == 1 || $on_disk != "$want" || ! -f $kernel ]]; then
  echo "check: building base, about 10 minutes"
  if ! logged sudo ENGINE="${ENGINE:-podman}" "$repo/images/base/build.sh"; then
    fail "base: images/base/build.sh failed"
    summary "stopped"
  fi
  built=1
fi
if [[ -n $built || $on_host != "$want" ]]; then
  printf '\n== sudo -u runner runner import --replace base - <%s\n' "$disk" >>"$log"
  # The redirects are this user's on purpose: the runner user can't read
  # the checkout, so the disk comes in on stdin.
  # shellcheck disable=SC2024
  if ! sudo -u runner runner import --replace base - <"$disk" >>"$log" 2>&1; then
    fail "base: runner import --replace base failed"
    summary "stopped"
  fi
  pass "base: ${built:+built and }imported, fingerprint $(short "$want")"
else
  pass "base: unchanged, fingerprint $(short "$want")"
fi
if sudo cmp -s "$kernel" "$root/kernel/vmlinux"; then
  pass "kernel: unchanged"
elif logged sudo install -D -m 0644 "$kernel" "$root/kernel/vmlinux"; then
  pass "kernel: installed"
else
  fail "kernel: installing $kernel failed"
  summary "stopped"
fi

# 3. doctor
after=$(doctor)
printf '\n== runner doctor\n%s\n' "$after" >>"$log"
was=$failed
while read -r level name detail; do
  [[ -z $level || $level == ok ]] || fail "doctor $name: $level, $detail"
done <<<"$after"
now=$(doctor_fp "$after")
if [[ -n $now && $now != "$want" ]]; then
  fail "doctor base: built from $(short "$now"), but this checkout is $(short "$want")"
fi
if [[ -z $after ]]; then
  fail "doctor: runner doctor printed nothing"
elif ((failed == was)); then
  pass "doctor: every line ok"
fi

# 4. tests, a line each as it ends. One that ends with no verdict on its line
# (it crashed, or a child wrote to its stdout) is settled by its binary's
# counts.
bin=
pending=
bin_failed=0
ran=0
was=$failed
verdict() { # ok|FAILED NAME [why]
  ran=$((ran + 1))
  if [[ $1 == ok ]]; then
    pass "$bin: $2"
  else
    bin_failed=$((bin_failed + 1))
    fail "$bin: $2${3:+ ($3)}"
  fi
}
settle() { # [how many of the binary's tests failed]
  [[ -n $pending ]] || return 0
  if ((${1:-1} > bin_failed)); then verdict FAILED "$pending" "no result: see the log"; else verdict ok "$pending"; fi
  pending=
}
printf '\n== tests\n' >>"$log"
CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER='sudo -E' RUNNER_TEST_DISK=$disk \
  RUNNER_TEST_KERNEL=$kernel RUNNER_GUEST_PORT=$port \
  cargo test --no-fail-fast -- --ignored --test-threads=1 "$@" 2>&1 |
  tee -a "$log" |
  while IFS= read -r line; do
    if [[ $line =~ ^\ +Running\ (unittests\ )?([^ ]+) ]]; then
      settle
      bin=$(basename "${BASH_REMATCH[2]}" .rs)
      bin_failed=0
    elif [[ $line =~ ^\ +Doc-tests\  ]]; then
      settle
      bin=doc
      bin_failed=0
    elif [[ $line =~ ^test\ (.+)\ \.\.\.\ (ok|FAILED|ignored)$ ]]; then
      [[ ${BASH_REMATCH[2]} == ignored ]] || verdict "${BASH_REMATCH[2]}" "${BASH_REMATCH[1]}"
    elif [[ $line =~ ^test\ ([^ ]+)\ \.\.\.\ (.*)$ ]]; then
      pending=${BASH_REMATCH[1]}
      [[ ${BASH_REMATCH[2]} != *"error: test failed"* ]] || settle
    elif [[ -n $pending && ($line == ok || $line == FAILED) ]]; then
      verdict "$line" "$pending"
      pending=
    elif [[ $line =~ ^test\ result:\ [A-Za-z]+\.\ [0-9]+\ passed\;\ ([0-9]+)\ failed ]]; then
      settle "${BASH_REMATCH[1]}"
      if ((BASH_REMATCH[1] > bin_failed)); then
        fail "$bin: $((BASH_REMATCH[1] - bin_failed)) more failed: see the log"
      fi
    elif [[ $line =~ ^error:\ test\ failed ]]; then
      settle
      ((bin_failed > 0)) || fail "$bin: the test binary failed: see the log"
    elif [[ $line =~ ^error:\ could\ not\ compile\ .*\((.+)\) ]]; then
      what=${BASH_REMATCH[1]}
      [[ ! $what =~ ^test\ \"(.+)\"( |$) ]] || what=${BASH_REMATCH[1]}
      fail "$what: doesn't build"
    fi
  done
status=${PIPESTATUS[0]}
settle
if ((failed == was)); then
  if ((status != 0)); then
    fail "tests: cargo test exited $status: see the log"
  elif ((ran == 0)); then
    fail "tests: none ran${1:+ matching $1}"
  fi
fi
summary
