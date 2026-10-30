#!/usr/bin/env bash
# Turns a fresh Linux box into a runner host. Run it from a checkout, as root,
# after `cargo build --release`:
#
#   sudo host/install.sh [path/to/runner]
#
# RUNNER_NET sets the VMs' address range (default 172.16.0.0/16). Safe to rerun,
# after an upgrade too: every step overwrites or checks, never appends.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
bin=${1:-$(dirname "$here")/target/release/runner}
net=${RUNNER_NET:-172.16.0.0/16}

say() { printf '==> %s\n' "$*"; }
die() { printf 'install.sh: %s\n' "$*" >&2; exit 1; }

[[ $EUID -eq 0 ]] || die "run it as root: sudo $0"
[[ $(uname -s) == Linux ]] || die "runner hosts run Linux"
command -v systemctl >/dev/null || die "runner hosts need systemd, and there's no systemctl"
[[ -f $bin ]] || die "no runner binary at $bin: build it first: cargo build --release"
octet='(25[0-5]|2[0-4][0-9]|1[0-9][0-9]|[1-9]?[0-9])'
cidr="^$octet\.$octet\.$octet\.$octet/(3[0-2]|[12]?[0-9])$"
[[ $net =~ $cidr ]] || die "RUNNER_NET=$net isn't an a.b.c.d/n range"

missing=()
command -v nft >/dev/null || missing+=(nftables)
command -v curl >/dev/null || missing+=(curl)
command -v pkaction >/dev/null || missing+=(polkit)
if ((${#missing[@]})); then
  say "installing ${missing[*]}"
  if command -v apt-get >/dev/null; then
    apt-get update -q
    # Debian and Ubuntu ship the polkit daemon as polkitd.
    DEBIAN_FRONTEND=noninteractive apt-get install -y -q "${missing[@]/#polkit/polkitd}"
  elif command -v dnf >/dev/null; then
    dnf install -y -q "${missing[@]}"
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

say "/var/lib/runner"
install -d -o runner -g runner -m 0750 /var/lib/runner /var/lib/runner/kernel

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
# mkdir, not install -d: some distros keep this directory 0700 for polkitd.
mkdir -p /etc/polkit-1/rules.d
install -m 0644 "$tmp/50-runner.rules" /etc/polkit-1/rules.d/50-runner.rules

say "firecracker@.service"
install -m 0644 "$here/firecracker@.service" /etc/systemd/system/firecracker@.service

say "NAT for $net"
cat >"$tmp/90-runner.conf" <<'EOF'
# Written by runner's host/install.sh: VMs reach the internet through NAT.
net.ipv4.ip_forward = 1
EOF
install -m 0644 "$tmp/90-runner.conf" /etc/sysctl.d/90-runner.conf
sysctl -q -p /etc/sysctl.d/90-runner.conf
# The first two lines make reapplying replace the table instead of adding to it.
cat >"$tmp/nat.nft" <<EOF
table ip runner
delete table ip runner
table ip runner {
	chain postrouting {
		type nat hook postrouting priority srcnat; policy accept;
		ip saddr $net ip daddr != $net masquerade
	}
}
EOF
nft -c -f "$tmp/nat.nft"
install -d -m 0755 /etc/runner
install -m 0644 "$tmp/nat.nft" /etc/runner/nat.nft
install -m 0644 "$here/runner-nat.service" /etc/systemd/system/runner-nat.service
systemctl daemon-reload
systemctl enable runner-nat.service
systemctl restart runner-nat.service

say "runner doctor"
runuser -u runner -- /usr/local/bin/runner doctor || true
cat <<EOF

Next:
  1. Put a guest kernel at /var/lib/runner/kernel/vmlinux.
  2. Add the laptop's SSH key to $home/.ssh/authorized_keys, as README.md says.
  3. Bring in the base disk: sudo -u runner /usr/local/bin/runner import base <disk>
EOF
