#!/usr/bin/env bash
# Turns a fresh Linux box into a runner host. Run it from a checkout, as root,
# after `cargo build --release`:
#
#   sudo host/install.sh [path/to/runner]
#
# RUNNER_NET sets the VMs' address range (default 172.16.0.0/16), kept in
# /etc/runner/net for reruns. RUNNER_HOST_IDLE sets how long the host goes
# unused before it powers itself off, in `runner idle`'s syntax (90s, 60m,
# 2h), or never, the default; it's kept in /etc/runner/host-idle for reruns
# (src/power.rs). RUNNER_HOST_RESERVE sets how much memory the host keeps for
# itself, like 2G (default 1G): VMs boot only into the rest, kept in
# /etc/runner/reserve for reruns (src/room.rs). Safe to rerun, after an
# upgrade too: every step overwrites or checks, never appends.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
bin=${1:-$(dirname "$here")/target/release/runner}
net=${RUNNER_NET:-$(cat /etc/runner/net 2>/dev/null || echo 172.16.0.0/16)}
idle=${RUNNER_HOST_IDLE:-$(cat /etc/runner/host-idle 2>/dev/null || echo never)}
reserve=${RUNNER_HOST_RESERVE:-$(cat /etc/runner/reserve 2>/dev/null || echo 1G)}

say() { printf '==> %s\n' "$*"; }
die() { printf 'install.sh: %s\n' "$*" >&2; exit 1; }

[[ $EUID -eq 0 ]] || die "run it as root: sudo $0"
[[ $(uname -s) == Linux ]] || die "runner hosts run Linux"
command -v systemctl >/dev/null || die "runner hosts need systemd, and there's no systemctl"
[[ -f $bin ]] || die "no runner binary at $bin: build it first: cargo build --release"
octet='(25[0-5]|2[0-4][0-9]|1[0-9][0-9]|[1-9]?[0-9])'
# Up to /30: each VM takes a /30 of it.
cidr="^$octet\.$octet\.$octet\.$octet/(30|[12]?[0-9])$"
[[ $net =~ $cidr ]] || die "RUNNER_NET=$net isn't an a.b.c.d/n range of /30 or wider"
# The runner refuses a range with host bits set, so every create would fail.
IFS=./ read -r a b c d len <<<"$net"
addr=$(((a << 24) | (b << 16) | (c << 8) | d))
mask=$(((0xffffffff << (32 - len)) & 0xffffffff))
((addr == (addr & mask))) || die "RUNNER_NET=$net has host bits set"
# What the runner reads (src/idle.rs), short of a count too big for it.
bad_idle="RUNNER_HOST_IDLE=$idle isn't an idle time: use minutes, like 60, or 90s, 60m or 2h, or never"
if [[ $idle != never ]]; then
  [[ $idle =~ ^[0-9]{1,6}[smh]?$ ]] || die "$bad_idle"
  ((10#${idle%[smh]} > 0)) || die "$bad_idle"
fi
# What the runner reads (store::parse_mem), short of T and of a count too
# big for it.
bad_reserve="RUNNER_HOST_RESERVE=$reserve isn't a memory size: use MiB, or a whole number with M or G, like 2G"
[[ $reserve =~ ^[0-9]{1,6}([MmGg]([Ii][Bb])?)?$ ]] || die "$bad_reserve"
((10#${reserve%%[!0-9]*} > 0)) || die "$bad_reserve"

missing=()
command -v nft >/dev/null || missing+=(nftables)
# The runner adds VMs' taps with it, by this path.
[[ -x /usr/sbin/ip ]] || missing+=(iproute2)
command -v curl >/dev/null || missing+=(curl)
command -v pkaction >/dev/null || missing+=(polkit)
# It makes runner's SSH key.
command -v ssh-keygen >/dev/null || missing+=(openssh-client)
# The runner copies trees into a disk with debugfs (src/graft.rs), and grows
# one with e2fsck and resize2fs (runner resize), by these paths.
[[ -x /usr/sbin/debugfs && -x /usr/sbin/e2fsck && -x /usr/sbin/resize2fs ]] || missing+=(e2fsprogs)
if ((${#missing[@]})); then
  say "installing ${missing[*]}"
  if command -v apt-get >/dev/null; then
    apt-get update -q
    # Debian and Ubuntu ship the polkit daemon as polkitd.
    DEBIAN_FRONTEND=noninteractive apt-get install -y -q "${missing[@]/#polkit/polkitd}"
  elif command -v dnf >/dev/null; then
    # Fedora calls them iproute and openssh-clients.
    pkgs=("${missing[@]/#iproute2/iproute}")
    dnf install -y -q "${pkgs[@]/#openssh-client/openssh-clients}"
  else
    die "install ${missing[*]} first: there's no apt-get or dnf here"
  fi
fi
# polkit before 0.106 ignores rules.d, so runner couldn't start its own VMs.
pk=$(pkaction --version | awk '{print $NF}')
[[ $pk != 0.* || ${pk#0.} -ge 106 ]] || die "polkit $pk ignores rules.d (it needs 0.106 or later, as in Ubuntu 24.04 or Debian 12)"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# The pin: shellcheck follows it only with -x, and :? stops on a pin missing a value.
# shellcheck source=SCRIPTDIR/firecracker.pin disable=SC1091
. "$here/firecracker.pin"
v=${FIRECRACKER_VERSION:?}
case $(uname -m) in
  x86_64) arch=x86_64 sha=${FIRECRACKER_SHA256_x86_64:?} ;;
  aarch64) arch=aarch64 sha=${FIRECRACKER_SHA256_aarch64:?} ;;
  *) die "Firecracker has no build for $(uname -m)" ;;
esac
# $(...) drops the blank line Firecracker prints after its version.
have=$(/usr/local/bin/firecracker --version 2>/dev/null) || true
if [[ $have == "Firecracker $v" ]]; then
  say "firecracker $v already installed"
else
  say "installing firecracker $v"
  tgz=firecracker-$v-$arch.tgz
  curl -fsSL -o "$tmp/$tgz" "https://github.com/firecracker-microvm/firecracker/releases/download/$v/$tgz"
  (cd "$tmp" && echo "$sha  $tgz" | sha256sum -c --quiet -) || die "$tgz doesn't match the sha256 in host/firecracker.pin"
  tar -xzf "$tmp/$tgz" -C "$tmp"
  install -m 0755 "$tmp/release-$v-$arch/firecracker-$v-$arch" /usr/local/bin/firecracker
fi

say "installing runner from $bin"
install -m 0755 "$bin" /usr/local/bin/runner

say "user runner"
getent group kvm >/dev/null || groupadd --system kvm
if ! id runner >/dev/null 2>&1; then
  # A login shell, since SSH runs every command through it.
  useradd --system --user-group --create-home --home-dir /home/runner --shell /bin/bash runner
fi
# systemd-journal lets `runner logs` read journalctl -u firecracker@<vm>.
groups=kvm
if getent group systemd-journal >/dev/null; then groups+=,systemd-journal; fi
usermod -aG "$groups" runner
# No password logs in as runner, but * doesn't lock the account the way ! does,
# and sshd without PAM refuses a locked account even with a good key.
usermod -p '*' runner
home=$(getent passwd runner | cut -d: -f6)
install -d -o runner -g runner -m 0700 "$home/.ssh"
if [[ ! -e $home/.ssh/authorized_keys ]]; then
  install -o runner -g runner -m 0600 /dev/null "$home/.ssh/authorized_keys"
fi
# Hosts drive each other's runners with this key, which `runner hosts add` on
# the laptop shares with every other host. Made as runner, so runner owns it.
key=$home/.ssh/id_ed25519
if [[ ! -s $key ]]; then
  say "making runner's SSH key, $key"
  rm -f "$key" "$key.pub"
  runuser -u runner -- ssh-keygen -q -t ed25519 -N '' -C "runner@$(hostname -s)" -f "$key"
elif [[ ! -s $key.pub ]]; then
  say "rewriting $key.pub from $key"
  runuser -u runner -- ssh-keygen -y -f "$key" >"$tmp/id_ed25519.pub"
  install -o runner -g runner -m 0644 "$tmp/id_ed25519.pub" "$key.pub"
fi

say "/var/lib/runner"
install -d -o runner -g runner -m 0750 /var/lib/runner /var/lib/runner/kernel
# Every runner command holds it while it works, so the host stays up. runner
# has to own it: one a command run as root left root's goes, and so does
# anything but a plain file, a link above all, which runner could point
# anywhere. runner makes it anew, with only runner's rights.
busy=/var/lib/runner/busy.lock
if [[ -L $busy || -e $busy ]]; then
  if [[ -L $busy || ! -f $busy ]] || [[ $(stat -c %U "$busy") != runner ]]; then
    rm -rf -- "$busy"
  fi
fi
runuser -u runner -- touch "$busy"

say "polkit rule"
cat >"$tmp/50-runner.rules" <<'EOF'
// Written by runner's host/install.sh, which rewrites it on every run.
// The runner user may start and stop firecracker@<vm>.service, and nothing else.
polkit.addRule(function (action, subject) {
  if (action.id == "org.freedesktop.systemd1.manage-units" &&
      subject.user == "runner" &&
      /^firecracker@.*\.service$/.test(action.lookup("unit")) &&
      (action.lookup("verb") == "start" || action.lookup("verb") == "stop")) {
    return polkit.Result.YES;
  }
});
EOF
if [[ $idle != never ]]; then
  cat >>"$tmp/50-runner.rules" <<'EOF'
// It may power the host off once it's idle (runner-host-idle.service), and
// only that: not with anyone else logged in, nor past an inhibitor, nor a
// reboot. With RUNNER_HOST_IDLE=never, install.sh leaves this rule out.
polkit.addRule(function (action, subject) {
  if (action.id == "org.freedesktop.login1.power-off" && subject.user == "runner") {
    return polkit.Result.YES;
  }
});
EOF
fi
# mkdir, not install -d: some distros keep this directory 0700 for polkitd.
mkdir -p /etc/polkit-1/rules.d
install -m 0644 "$tmp/50-runner.rules" /etc/polkit-1/rules.d/50-runner.rules

say "firecracker@.service"
install -m 0644 "$here/firecracker@.service" /etc/systemd/system/firecracker@.service
# At a host shutdown, it parks each sleeping VM before the VMs' units stop.
install -m 0644 "$here/runner-park.service" /etc/systemd/system/runner-park.service

say "NAT for $net"
cat >"$tmp/90-runner.conf" <<'EOF'
# Written by runner's host/install.sh: VMs reach the internet through NAT.
net.ipv4.ip_forward = 1
EOF
install -m 0644 "$tmp/90-runner.conf" /etc/sysctl.d/90-runner.conf
sysctl -q -p /etc/sysctl.d/90-runner.conf
# host/nat.nft says what the rules do. $net passed the check above, so it
# holds nothing sed would read as part of the expression.
sed "s|@NET@|$net|g" "$here/nat.nft" >"$tmp/nat.nft"
nft -c -f "$tmp/nat.nft"
install -d -m 0755 /etc/runner
install -m 0644 "$tmp/nat.nft" /etc/runner/nat.nft
echo "$net" >"$tmp/net"
install -m 0644 "$tmp/net" /etc/runner/net
say "reserve for the host: $reserve"
echo "$reserve" >"$tmp/reserve"
install -m 0644 "$tmp/reserve" /etc/runner/reserve
install -m 0644 "$here/runner-nat.service" /etc/systemd/system/runner-nat.service
systemctl daemon-reload
systemctl enable runner-nat.service
systemctl restart runner-nat.service
# Started, never restarted: its stop parks every sleeping VM.
systemctl enable runner-park.service
systemctl start runner-park.service

say "host idle: $idle"
# Only one that was set, so client/aws.sh's setup can tell a host with none.
if [[ -n ${RUNNER_HOST_IDLE:-} || -e /etc/runner/host-idle ]]; then
  echo "$idle" >"$tmp/host-idle"
  install -m 0644 "$tmp/host-idle" /etc/runner/host-idle
fi
install -m 0644 "$here/runner-host-idle.service" /etc/systemd/system/runner-host-idle.service
install -m 0644 "$here/runner-host-idle.timer" /etc/systemd/system/runner-host-idle.timer
systemctl daemon-reload
if [[ $idle == never ]]; then
  systemctl disable --now runner-host-idle.timer
else
  systemctl enable runner-host-idle.timer
  # The clock starts at the host's boot or its last use, not here.
  systemctl restart runner-host-idle.timer
fi

say "runner doctor"
runuser -u runner -- /usr/local/bin/runner doctor || true
cat <<EOF

Next:
  1. Put a guest kernel at /var/lib/runner/kernel/vmlinux.
  2. Add the laptop's SSH key to $home/.ssh/authorized_keys, as README.md says.
  3. Bring in the base disk, or a new one: sudo -u runner /usr/local/bin/runner import --replace base - <path/to/base.ext4>
EOF
