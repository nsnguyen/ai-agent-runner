#!/usr/bin/env bash
# Boot test for the base image, on the KVM host. Boots a throwaway copy of the
# disk in Firecracker and checks that systemd starts runner-guest and that the
# guest agent takes a connection on its vsock port. Exits 77 when this machine
# can't boot a VM.
#
#   RUNNER_GUEST_PORT=<port> images/base/boot-test.sh \
#     images/base/out/base.ext4 images/base/out/vmlinux
set -euo pipefail

usage="usage: boot-test.sh <disk.ext4> <vmlinux>"
disk=${1:?$usage}
kernel=${2:?$usage}
wait_s=${BOOT_TIMEOUT:-120}

skip() { echo "boot-test.sh: skipped: $*"; exit 77; }
die() { echo "boot-test.sh: FAIL: $*" >&2; exit 1; }

[[ -r /dev/kvm && -w /dev/kvm ]] || skip "no access to /dev/kvm"
command -v firecracker >/dev/null || skip "no firecracker on PATH"
[[ -r $disk && -r $kernel ]] || die "no $disk or $kernel: run images/base/build.sh"
port=${RUNNER_GUEST_PORT:?set RUNNER_GUEST_PORT to the vsock port runner-guest listens on}

# Next to the disk, so the copy below can be a reflink and isn't in a tmpfs.
work=$(mktemp -d "$(dirname "$disk")/.boot.XXXXXX")
fc=
cleanup() {
  [[ -z $fc ]] || kill "$fc" 2>/dev/null || true
  rm -rf "$work"
}
trap cleanup EXIT

# Booting changes a disk, so boot a copy and keep the built image as built.
cp --reflink=auto --sparse=always "$disk" "$work/disk.ext4"
cat > "$work/vm.json" <<EOF
{
  "boot-source": {
    "kernel_image_path": "$kernel",
    "boot_args": "console=ttyS0 reboot=k panic=1 pci=off"
  },
  "drives": [{
    "drive_id": "rootfs",
    "path_on_host": "$work/disk.ext4",
    "is_root_device": true,
    "is_read_only": false
  }],
  "machine-config": { "vcpu_count": 2, "mem_size_mib": 2048 },
  "vsock": { "guest_cid": 3, "uds_path": "$work/vsock" }
}
EOF

console=$work/console.log
firecracker --no-api --config-file "$work/vm.json" < /dev/null > "$console" 2>&1 &
fc=$!

started=
for _ in $(seq "$wait_s"); do
  # systemd colours the unit name on the console, so match around it.
  if grep -aqE 'Started .*runner-guest\.service' "$console"; then started=1; break; fi
  kill -0 "$fc" 2>/dev/null || break
  sleep 1
done
if [[ -z $started ]]; then
  tail -40 "$console" >&2
  die "systemd didn't start runner-guest.service within ${wait_s}s"
fi
echo "ok    the VM boots and systemd starts runner-guest"

# Firecracker's host side of vsock: write "CONNECT <port>", and it answers
# "OK <n>" once a listener in the guest accepts.
answered=
for _ in $(seq 10); do
  if python3 - "$work/vsock" "$port" <<'EOF'
import socket, sys
s = socket.socket(socket.AF_UNIX)
s.settimeout(5)
s.connect(sys.argv[1])
s.sendall(f"CONNECT {sys.argv[2]}\n".encode())
sys.exit(0 if s.makefile().readline().startswith("OK ") else 1)
EOF
  then answered=1; break; fi
  sleep 1
done
[[ -n $answered ]] || die "nothing in the guest accepted on vsock port $port"
echo "ok    runner-guest accepts on vsock port $port"
