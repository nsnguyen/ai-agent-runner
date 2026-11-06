#!/usr/bin/env bash
# Build the base VM: images/base/Dockerfile becomes a sparse ext4 disk, with
# the pinned Firecracker CI guest kernel next to it.
#
#   cargo build --release --target x86_64-unknown-linux-musl --bin runner-guest
#   sudo images/base/build.sh
#   sudo -u runner runner import --replace base - <images/base/out/base.ext4
#
# The disk's /.runner/build says what it was built from: a fingerprint of what
# the image is made from, and the commit. `build.sh --fingerprint` prints the
# fingerprint alone and builds nothing, needing no root, so a check can hold
# it against what `runner doctor` says base was built from.
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
need_guest() {
  [[ -f $guest && -r $guest ]] || die "no runner-guest at $guest: build it first with
  cargo build --release --target x86_64-unknown-linux-musl --bin runner-guest
or point RUNNER_GUEST at one"
}

# What the image is made from, as one sha256: every file and link under
# images/base but the build's output, by path, owner execute bit and
# contents (a link's target), then the runner-guest binary. That takes in
# what it copies in or runs, and the kernel pin, which is in this file. Times
# don't count, nor do files elsewhere. When unsure whether a file counts, it
# does: a needless rebuild costs minutes, a missed one leaves a stale base.
fingerprint() {
  need_guest
  local files sum skip
  # OUT may be under images/base too.
  skip=$(realpath -m -- "$out")
  skip=./${skip#"$here"/}
  files=$(
    cd "$here" || exit
    find . \( -path ./out -o -path "$skip" \) -prune -o \( -type f -o -type l \) -print0 |
      LC_ALL=C sort -z |
      while IFS= read -r -d '' f; do
        if [[ -L $f ]]; then
          sum=$(readlink -- "$f" | sha256sum) && mode=---l || exit
        else
          sum=$(sha256sum <"$f") && mode=$(stat -c %A -- "$f") || exit
        fi
        printf '%s %s %s\n' "${sum%% *}" "${mode:3:1}" "${f#./}"
      done
  ) || die "can't read every file under $here"
  sum=$(sha256sum <"$guest") || die "can't read $guest"
  printf '%s\n%s x runner-guest\n' "$files" "${sum%% *}" | sha256sum | cut -d' ' -f1
  # It's of the binary there is, which a pull doesn't rebuild.
  if [[ -n $(find "$repo/src" "$repo/Cargo.toml" "$repo/Cargo.lock" -newer "$guest" \
    -print -quit 2>/dev/null) ]]; then
    echo "build.sh: $guest is older than the source in $repo: rebuild it, or this" \
      "fingerprint may be of an old one" >&2
  fi
}

case $# in
  0) ;;
  1) [[ $1 == --fingerprint ]] || die "usage: build.sh [--fingerprint]"
    fingerprint
    exit 0 ;;
  *) die "usage: build.sh [--fingerprint]" ;;
esac

[[ $(id -u) -eq 0 ]] || die "run as root (sudo), so the disk keeps file owners"
need_guest
[[ -x $guest ]] || die "$guest isn't executable"
engine=${ENGINE:-$(command -v docker || command -v podman || true)}
[[ -n $engine ]] || die "needs docker or podman"
for tool in mkfs.ext4 debugfs curl sha256sum tar truncate; do
  command -v "$tool" >/dev/null || die "needs $tool"
done

# Taken before anything is built, so it's of what this build is made from.
print=$(fingerprint)
# As root, git refuses a repo someone else owns unless told it's safe, and
# status mustn't write root's index into it.
in_repo() { git -c safe.directory="$repo" -C "$repo" --no-optional-locks "$@" 2>/dev/null; }
commit=$(in_repo rev-parse --short=12 HEAD) || commit=
if [[ -n $commit && -n $(in_repo status --porcelain --untracked-files=no) ]]; then
  commit+=-dirty
fi

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
cp "$here/Dockerfile" "$here/runner-guest.service" "$here/llm.txt" \
  "$here/managed-settings.json" "$here/docker-daemon.json" \
  "$here/docker-no-raw-table.conf" "$work/context/"
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
# What it was built from, for `runner import` to note (src/image.rs).
{
  echo "# What this image was built from: images/base/build.sh wrote it."
  echo "fingerprint=$print"
  [[ -z $commit ]] || echo "commit=$commit"
} >"$work/root/.runner/build"
chmod 0644 "$work/root/.runner/build"

echo "== disk ($size)"
truncate -s "$size" "$work/base.ext4"
mkfs.ext4 -q -F -L runner-base -d "$work/root" "$work/base.ext4"

# Check before the new disk replaces the last good one.
echo "== check"
"$here/check.sh" "$work/base.ext4"
# The tools run on the image's userland, checked in a container with no
# network: docker's clients only, as dockerd isn't running there. The boot
# test checks the guest kernel and runner-guest.
"$engine" run --rm --network none -u agent "$tag" bash -c \
  'claude --version && node --version && python3 --version && rustc --version && cargo --version &&
   docker --version && docker compose version && docker buildx version' |
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

Built $disk ($(du -h "$disk" | cut -f1) used of $size) and $kernel,
fingerprint $print${commit:+ at commit $commit}.
Next, on the runner host:
  sudo -u runner runner import --replace base - <$disk
  sudo install -D -m 0644 $kernel /var/lib/runner/kernel/vmlinux
EOF
