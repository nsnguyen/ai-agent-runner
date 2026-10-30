# ai-agent-runner

Firecracker microVMs for my AI agents, like fly.io Sprites, on a KVM host I run. Design: [docs/design.md](docs/design.md).

## Build the base image

`base` is the VM every other VM forks from: Ubuntu 24.04 booting systemd, with `runner-guest` as a service, git, curl, build-essential, tmux, Node LTS, Python, Rust (rustup) and Claude Code. You work as `agent`, with passwordless sudo, in `/home/agent`. Claude Code isn't logged in, and the image holds no credentials.

On a Linux machine with docker or podman and e2fsprogs:

```sh
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl --bin runner-guest
sudo images/base/build.sh
```

It writes `images/base/out/base.ext4`, a sparse 20 GiB ext4 disk (`SIZE=40G` for another size), and `images/base/out/vmlinux`, Firecracker's pinned CI guest kernel. It runs as root so the disk keeps every file's owner. Then it checks the disk with `images/base/check.sh`: runner-guest is enabled, and nothing is under `/home/agent/.claude` or `/root/.claude`.

On the runner host:

```sh
runner import base images/base/out/base.ext4
sudo install -D -m 0644 images/base/out/vmlinux /var/lib/runner/kernel/vmlinux
```

The boot test needs `/dev/kvm` and `firecracker` on PATH. It boots a copy of the disk and checks that runner-guest starts and accepts on its vsock port:

```sh
RUNNER_GUEST_PORT=<port> cargo test --test base_image -- --ignored
```

To change versions, edit the `ARG`s at the top of `images/base/Dockerfile` (Node major, Claude Code version) and the kernel pin at the top of `build.sh`.
