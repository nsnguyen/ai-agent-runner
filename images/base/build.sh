#!/usr/bin/env bash
# Build the base VM: images/base/Dockerfile becomes a sparse ext4 disk, with
# the pinned Firecracker CI guest kernel next to it.
#
#   cargo build --release --target x86_64-unknown-linux-musl --bin runner-guest
#   sudo images/base/build.sh
#   runner import base images/base/out/base.ext4
#
# Runs as root, so unpacking the image keeps every file's owner and mode.
# Settings, from the environment:
#   OUT           where base.ext4 and vmlinux go (images/base/out)
#   SIZE          the disk's size, as truncate takes it (20G)
#   ENGINE        docker or podman (the first one on PATH)
#   RUNNER_GUEST  a runner-guest binary (the repo's musl release build)
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo=$(cd "$here/../.." && pwd)
out=${OUT:-$here/out}
size=${SIZE:-20G}
guest=${RUNNER_GUEST:-$repo/target/x86_64-unknown-linux-musl/release/runner-guest}
tag=runner-base

# Firecracker's CI kernel for v1.15; it has virtio-vsock and what systemd needs.
kernel_url=https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/v1.15/x86_64/vmlinux-6.1.155
kernel_sha256=e20e46d0c36c55c0d1014eb20576171b3f3d922260d9f792017aeff53af3d4f2

die() { echo "build.sh: $*" >&2; exit 1; }

[[ $(id -u) -eq 0 ]] || die "run as root (sudo), so the disk keeps file owners"
[[ -x $guest ]] || die "no runner-guest at $guest: build it first with
  cargo build --release --target x86_64-unknown-linux-musl --bin runner-guest
or point RUNNER_GUEST at one"
engine=${ENGINE:-$(command -v docker || command -v podman || true)}
[[ -n $engine ]] || die "needs docker or podman"
for tool in mkfs.ext4 debugfs curl sha256sum tar truncate; do
  command -v "$tool" >/dev/null || die "needs $tool"
done

mkdir -p "$out"
disk=$out/base.ext4
work=$(mktemp -d "$out/.build.XXXXXX")
container=
cleanup() {
  [[ -z $container ]] || "$engine" rm -f "$container" >/dev/null
  rm -rf "$work"
}
trap cleanup EXIT

echo "== image ($engine)"
mkdir "$work/context"
cp "$here/Dockerfile" "$here/runner-guest.service" "$work/context/"
cp "$guest" "$work/context/runner-guest"
"$engine" build -t "$tag" "$work/context"

echo "== root filesystem"
container=$("$engine" create "$tag")
mkdir "$work/root"
"$engine" export "$container" |
  tar -x -C "$work/root" --numeric-owner --xattrs --xattrs-include='*'
# The engine leaves its own placeholders for these, with its own modes. The
# kernel's ip= argument sets the address and hostname at boot, and writes DNS
# to /proc/net/pnp.
etc=$work/root/etc
rm -f "$work/root/.dockerenv" "$etc/hostname" "$etc/hosts" "$etc/resolv.conf"
install -m 0644 /dev/null "$etc/hostname"
printf '127.0.0.1 localhost\n::1 localhost ip6-localhost ip6-loopback\n' > "$etc/hosts"
chmod 0644 "$etc/hosts"
ln -s /proc/net/pnp "$etc/resolv.conf"

echo "== disk ($size)"
truncate -s "$size" "$work/base.ext4"
mkfs.ext4 -q -F -L runner-base -d "$work/root" "$work/base.ext4"

# Check before the new disk replaces the last good one.
echo "== check"
"$here/check.sh" "$work/base.ext4"
# The tools run on the image's userland, checked in a container with no
# network. The boot test checks the guest kernel and runner-guest.
"$engine" run --rm --network none -u agent "$tag" bash -c \
  'claude --version && node --version && python3 --version && rustc --version && cargo --version' |
  sed 's/^/ok    /'
mv -f "$work/base.ext4" "$disk"

echo "== kernel"
kernel=$out/vmlinux
if ! echo "$kernel_sha256  $kernel" | sha256sum -c --status 2>/dev/null; then
  curl -fsSL -o "$work/vmlinux" "$kernel_url"
  echo "$kernel_sha256  $work/vmlinux" | sha256sum -c --quiet
  mv "$work/vmlinux" "$kernel"
fi

if [[ -n ${SUDO_UID:-} ]]; then
  chown "$SUDO_UID:${SUDO_GID:-$SUDO_UID}" "$out" "$disk" "$kernel"
fi

cat <<EOF

Built $disk ($(du -h "$disk" | cut -f1) used of $size) and $kernel.
Next, on the runner host:
  runner import base $disk
  install -D -m 0644 $kernel /var/lib/runner/kernel/vmlinux
EOF
