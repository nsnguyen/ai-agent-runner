#!/usr/bin/env bash
# Check a built base disk without mounting it (debugfs, no root): the guest
# agent is installed and enabled at boot, Claude Code is on PATH, the agent's
# /.runner/llm.txt is there, and no Claude Code login or state is baked in for
# any fork to inherit.
#
#   images/base/check.sh images/base/out/base.ext4
set -euo pipefail

disk=${1:?usage: check.sh <disk.ext4>}
[[ -r $disk ]] || { echo "check.sh: can't read $disk" >&2; exit 2; }
failed=0

ok() { printf 'ok    %s\n' "$1"; }
bad() { printf 'FAIL  %s\n' "$1"; failed=1; }

# debugfs exits 0 even when a path is missing; the answer is in its output.
stat_of() { debugfs -R "stat $1" "$disk" 2>/dev/null; }
type_of() { stat_of "$1" | sed -n 's/^Inode: [0-9]* *Type: \([a-z]*\).*/\1/p'; }

guest=/usr/local/bin/runner-guest
mode=$(stat_of "$guest" | sed -n 's/.*Mode: *0*\([0-7]*\) .*/\1/p')
if [[ $(type_of "$guest") == regular && $(( 8#${mode:-0} & 8#111 )) -ne 0 ]]; then
  ok "$guest is installed"
else
  bad "$guest is missing or not executable"
fi

unit=/etc/systemd/system/runner-guest.service
wants=/etc/systemd/system/multi-user.target.wants/runner-guest.service
dest=$(stat_of "$wants" | sed -n 's/^Fast link dest: "\(.*\)"$/\1/p')
if [[ $(type_of "$unit") == regular && $(type_of "$wants") == symlink &&
      ( -z $dest || $dest == "$unit" || $dest == ../runner-guest.service ) ]]; then
  ok "runner-guest.service is enabled"
else
  bad "runner-guest.service is not enabled: want $wants -> $unit"
fi

if [[ $(type_of /usr/local/bin/claude) == symlink || $(type_of /usr/local/bin/claude) == regular ]]; then
  ok "claude is on PATH"
else
  bad "/usr/local/bin/claude is missing"
fi

if [[ $(type_of /.runner/llm.txt) == regular ]]; then
  ok "/.runner/llm.txt is there"
else
  bad "/.runner/llm.txt is missing"
fi

# Claude Code keeps its login in ~/.claude and its account in ~/.claude.json.
for home in /home/agent /root; do
  state=$home/.claude
  entries=$(debugfs -R "ls -p $state" "$disk" 2>/dev/null |
    awk -F/ '$6 != "." && $6 != ".." { print $6 }')
  if [[ -n $entries ]]; then
    bad "$state is not empty: $(echo "$entries" | paste -sd ' ' -)"
  else
    ok "nothing under $state"
  fi
  if [[ -n $(type_of "$home/.claude.json") ]]; then
    bad "$home/.claude.json exists"
  else
    ok "no $home/.claude.json"
  fi
done

exit "$failed"
