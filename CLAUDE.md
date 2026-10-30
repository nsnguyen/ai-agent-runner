# ai-agent-runner

## Purpose
A cloud runner for AI agents on Firecracker microVMs, like fly.io Sprites. Each agent gets its own microVM. My local Claude Code starts and drives one through an MCP server, or I start one with a CLI command and land in its terminal over SSH.

## Done looks like
- From my local Claude Code, an MCP tool call starts an agent in its own microVM and returns its output.
- One CLI command starts a microVM and drops me into its shell over SSH.

## Not this
- Not a hosted service for other people. It runs my agents.
- Not a model client. The agent inside the VM talks to its own model; the runner starts, connects to, and stops VMs.
- Not a cloud buyer. It never signs up for or rents hosts; it runs on machines I give it.

## Autonomy
- Merging: only forge's review task merges, and only the build PRs it approves with every check passing, so issues that depend on each other build one after another. No other session merges, the builder included. The cloud has no KVM, so those PRs merge without a VM booted: at each milestone, run every `#[ignore]`d test on the KVM host (Commands and Sharp edges say how).
- Deploys: previews only.

## Stack
- Firecracker microVMs, driven through Firecracker's API socket. Everything else follows temper's defaults: one Rust binary on the host, `runner`, that is both the CLI and the MCP server.
- `runner-guest`, a static binary in each VM, runs commands for the host over vsock in place of sshd. The protocol is `src/proto.rs`.
- VM state is a directory per VM, not SQLite (docs/design.md). Disks are copied with GNU `cp --reflink=auto --sparse=always`.

## Commands
- Build: `cargo build --release` (the binary is `target/release/runner`)
- Build the guest agent: `cargo build --release --target x86_64-unknown-linux-musl --bin runner-guest` (static, for the base image)
- Test: `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test && shellcheck host/*.sh client/*`
- Run locally: `RUNNER_ROOT=/tmp/runner cargo run -- list`. No KVM needed until VMs boot.
- Boot tests, on the KVM host only: `cargo test --test kvm -- --ignored` (docs/host.md).

## Sharp edges
- On a filesystem without reflink, like ext4, every fork and run is a full (sparse) copy of the disk: slow, and real space. Put `/var/lib/runner` on XFS or btrfs.
- Look at a failed run's disk before the next commit. That commit's gc keeps undo history first, so the failed disk usually goes then.
- Space used in `list`, and what `gc` says it frees, sum each disk's allocated blocks. A block reflinked into several disks (forks, generations) counts in each, so on XFS or btrfs the real figure can be far less.
- Firecracker needs Linux with `/dev/kvm`. Claude's cloud sessions don't have it, so tests there can't boot a VM. Keep tests that boot one apart from the rest, so they're skippable, and say in each PR which ones didn't run.
- The session tests run the real tmux, as the test's user and on that user's own tmux server, so install tmux first. Each test names its sessions apart and kills them after.
- Making a tap needs CAP_NET_ADMIN and iproute2, so those tests are `#[ignore]`d too: `cargo test --test net --test boot -- --ignored`, as root (docs/host.md says how). Cloud sessions run as root; install iproute2 first.
