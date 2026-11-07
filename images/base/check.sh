#!/usr/bin/env bash
# Check a built base disk without mounting it (debugfs, no root): the guest
# agent is installed and enabled at boot, Claude Code is on PATH with its
# updater off, Docker starts at boot for agent on what the guest kernel has,
# the agent's /.runner/llm.txt is there, /.runner/build says what the image
# was built from, and no Claude Code login or state is baked in for any fork
# to inherit.
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

# Every claude reads its managed settings, however it starts, and these keep
# it at the version the image installed. The variable counts only in env.
settings=/etc/claude-code/managed-settings.json
mode_of() { stat_of "$1" | sed -n 's/.*Mode: *0*\([0-7]*\) .*/\1/p'; }
file_mode=$(mode_of "$settings")
dir_mode=$(mode_of "${settings%/*}")
json=$(debugfs -R "cat $settings" "$disk" 2>/dev/null | tr -d ' \t\r\n')
off='"env":\{[^}]*"DISABLE_AUTOUPDATER":"1"'
if [[ $(type_of "$settings") != regular ]]; then
  bad "$settings is missing, so Claude Code updates itself"
elif (( (8#${file_mode:-0} & 8#004) == 0 || (8#${dir_mode:-0} & 8#001) == 0 )); then
  bad "$settings isn't readable by agent"
elif ! [[ $json =~ $off ]]; then
  bad "Claude Code's updater is on: $settings doesn't set env.DISABLE_AUTOUPDATER to \"1\""
else
  ok "Claude Code's updater is off"
fi

# Docker: dockerd starts at boot, agent reaches it without sudo, and it runs
# on what Firecracker's CI kernel has, which is no nf_tables and no iptables
# raw table.
link_dest() { stat_of "$1" | sed -n 's/^Fast link dest: "\(.*\)"$/\1/p'; }
wants=/etc/systemd/system/multi-user.target.wants/docker.service
dest=$(link_dest "$wants")
[[ -z $dest || $dest == /* ]] || dest=${wants%/*}/$dest
if [[ $(type_of "$wants") == symlink && $dest == */docker.service &&
      $(type_of "$dest") == regular ]]; then
  ok "docker.service is enabled"
else
  bad "docker.service is not enabled: want $wants -> its unit"
fi

groups=$(debugfs -R "cat /etc/group" "$disk" 2>/dev/null)
if grep -Eq '^docker:[^:]*:[0-9]+:([^:]*,)?agent(,[^:]*)?$' <<<"$groups"; then
  ok "agent is in the docker group"
else
  bad "agent is not in the docker group, so docker needs sudo"
fi

for cmd in iptables ip6tables; do
  dest=$(link_dest "/etc/alternatives/$cmd")
  if [[ $dest == "/usr/sbin/$cmd-legacy" ]]; then
    ok "$cmd is $cmd-legacy"
  else
    bad "$cmd is ${dest:-missing}, not $cmd-legacy: the guest kernel has no nf_tables"
  fi
done

conf=/etc/systemd/system/docker.service.d/no-raw-table.conf
# Set in [Service] by an Environment= line, quoted or not, among others.
if debugfs -R "cat $conf" "$disk" 2>/dev/null | awk '
    /^[[:space:]]*\[/ { service = ($1 == "[Service]") }
    service && /^Environment=/ &&
      /[="[:space:]]DOCKER_INSECURE_NO_IPTABLES_RAW=1("|[[:space:]]|$)/ { found = 1 }
    END { exit !found }'; then
  ok "dockerd leaves the iptables raw table alone"
else
  bad "dockerd writes to the iptables raw table, which the guest kernel lacks: $conf doesn't set DOCKER_INSECURE_NO_IPTABLES_RAW=1"
fi

if [[ $(type_of /.runner/llm.txt) == regular ]]; then
  ok "/.runner/llm.txt is there"
else
  bad "/.runner/llm.txt is missing"
fi

# What `runner import` notes, with the fingerprint build.sh --fingerprint gives.
record=$(debugfs -R "cat /.runner/build" "$disk" 2>/dev/null)
if [[ $(type_of /.runner/build) != regular ]]; then
  bad "/.runner/build is missing"
elif ! grep -Eq '^fingerprint=[0-9a-f]{64}$' <<<"$record"; then
  bad "/.runner/build has no fingerprint"
else
  ok "/.runner/build says what the image was built from"
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
