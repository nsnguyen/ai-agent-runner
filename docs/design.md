# ai-agent-runner design, v0

**Goal.** From my laptop, start an AI agent in its own Firecracker microVM on my KVM box. I drive it from local Claude Code over MCP or step into its shell over SSH, and it keeps its files between runs, like fly.io Sprites.

## ★ Approach: SSH is the control plane, every VM is a fork

```
laptop                                        KVM host (tailnet only)
Claude Code ── ssh kvm-host runner mcp ──┐    runner: one Rust binary
runner CLI  ── ssh -t kvm-host runner … ─┴──▶ /var/lib/runner/vms/<vm>/  state, no database
                                              firecracker@<vm>.service   systemd
                                              vsock ─▶ small guest agent in the VM
```

- **No daemon, no API, no open ports.** Local Claude Code registers `ssh kvm-host runner mcp` as a stdio MCP server, and the CLI is `ssh -t kvm-host runner …`. SSH keys are the auth and the tailnet is the network. MCP tools and CLI commands call the same functions.
- **The filesystem is the database.** Each VM is a directory with its disk generations and a `history` whose last line is the current one.
- **Every VM is a fork.** Disks are reflink copies on XFS or btrfs, with a sparse full copy elsewhere. An image is a VM you stopped changing; `base` is the one you fork most. A run clones the current disk into a new generation that only a clean finish commits, so a crash rolls back for free and `undo` steps back one.
- **One run per VM.** The process running a VM holds an flock on it, which the kernel drops if that process dies. In M2 that process is the VM's systemd unit.
- **Runs are detached.** `run` starts `claude -p` in the VM and returns a run id at once; `output` reads its log by offset. A closed laptop or a dropped SSH connection can't kill a run.
- **A fixed address per VM.** A slot picked at create gives each VM a tap and a /30 of the host's subnet (`<root>/subnet`, default 172.16.0.0/16); the guest gets its address, gateway and DNS from the kernel's `ip=` argument. The host's install sets up NAT once; the runner only adds and removes taps.
- **Into the VM over vsock.** The host reaches a small guest agent through Firecracker's vsock socket, so the guest needs no sshd and no keys. `runner shell <vm>` is SSH to the host, then a terminal over vsock.

## Milestones

| | What | Done when |
|---|---|---|
| M1 (this PR) | The VM store (import, create, fork, generations, undo, delete), the CLI, and MCP over stdio. No VM boots. | Checks pass here, and local Claude Code lists VMs through `ssh kvm-host runner mcp` |
| M2 | Boot: systemd unit, tap networking with NAT, `start`, `stop`, `logs`, the guest agent, `shell` | `runner shell dev` lands you in a VM |
| M3 | Agent runs: `run` and `output`, credentials per boot, a result of exit code and changed paths | Local Claude Code runs Claude Code in a VM and reads what it did |
| Later | Snapshots for sleep and wake, best-of-N forks, taking over a run, cleaning up old generations of a VM that stays, more hosts | Each when it hurts |

## Guards

- Only my agents; the host is reachable only over the tailnet. It never rents a host.
- Credentials come in per boot and never land on a VM's disk, so a forked disk holds no keys.
- A run's result keeps what the runner measured apart from what the agent wrote, which is marked untrusted.

## Assumptions (correct these)

- The host is a Linux machine of yours with KVM, on your tailnet, with XFS or btrfs under `/var/lib/runner`.
- Claude Code is the first agent, installed in the base image.

Traps and the full brainstorm are in `brainstorm.md`.
