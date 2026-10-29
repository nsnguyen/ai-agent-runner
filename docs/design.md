# ai-agent-runner design, v0

**Goal.** From my laptop, start an AI agent in its own Firecracker microVM on my KVM box. I drive it from local Claude Code over MCP or step into its shell over SSH, and it keeps its files between runs, like fly.io Sprites.

## ★ Approach: SSH is the control plane, every VM is a fork

```
laptop                                       KVM host (tailnet only)
Claude Code ── MCP over `ssh host runner mcp` ─┐   runner: one Rust binary
runner CLI  ── `ssh -t host runner …` ─────────┴─▶ /var/lib/runner/vms/<vm>/  (state, no database)
                                                   firecracker@<vm>.service  (systemd)
                                                   vsock ─▶ small guest agent in the VM
```

- **No daemon, no API, no open ports.** Local Claude Code registers `ssh -T kvm-host runner mcp` as a stdio MCP server. The CLI is `ssh -t kvm-host runner …`. SSH keys are the auth, and the tailnet is the network. MCP tools and CLI commands call the same functions, so they can't drift apart.
- **The filesystem is the database.** Each VM is a directory: its disk generations, a `history` of committed ones, and later Firecracker's config and sockets. `list` reads directories.
- **systemd runs the VMs.** Each VM is a `firecracker@<vm>.service`: start and stop are systemctl, and the serial console goes to journald, which is `runner logs`.
- **Every VM is a fork.** Disks are reflink copies on XFS or btrfs, with a full copy as fallback. An image is a VM you stopped changing; `base` is the one you fork most. A run clones the current disk into a new generation and commits it on a clean finish. A crash leaves the old generation current, so rollback costs nothing, and `undo` steps back one.
- **Runs are detached.** `run` starts `claude -p` in the VM and returns a run id at once; `output` reads its log by offset. Laptop sleep, an SSH drop or an MCP timeout can't kill a run.
- **Into the VM over vsock.** The host reaches a small guest agent through Firecracker's vsock socket for runs and shells. The guest needs no sshd, no keys, and no IP for control. `runner shell <vm>` is SSH to the host, then a terminal over vsock.

## Milestones

| | What | Done when |
|---|---|---|
| M1 (this PR) | `runner` crate: the VM store (import, create, fork, generations, undo), the CLI, and MCP over stdio. No VM boots yet. | Checks pass here, and on your box local Claude Code lists VMs through `ssh kvm-host runner mcp` |
| M2 | Boot: systemd unit, tap networking with NAT, `start`, `stop`, `logs`, the guest agent on vsock, `shell` | `runner shell dev` lands you in a VM |
| M3 | Agent runs: `run` and `output`, credentials passed in per boot, and a result made of the exit code and changed paths | Local Claude Code starts Claude Code in a VM and reads what it did |
| Later | Snapshots for Sprites-style sleep and wake, best-of-N forks, watching or taking over a run in tmux, more hosts | Each when it hurts |

## Guards

- Only my agents. The host is reachable only over the tailnet.
- Credentials come in per boot over vsock and never land on a VM's disk, so a forked or copied disk holds no keys.
- A run's result keeps what the runner measured (exit code, changed paths) apart from what the agent wrote, which is marked untrusted, so an agent's output can't steer local Claude Code.
- Never rents a host. Tests that boot a VM run only where `/dev/kvm` exists.

## Traps

- Faking persistence by never stopping VMs: one reboot loses everything.
- The agent as the VM's init: it leaves no shell to SSH into.
- Writing our own SSH server: OpenSSH on the host already does it.
- One MCP call that blocks for a whole run: the first closed laptop lid kills it.

## Assumptions (correct these)

- The host is a Linux machine of yours with KVM, on your tailnet, with XFS or btrfs under `/var/lib/runner`.
- Claude Code is the first agent, installed in the base image.

The full brainstorm is in `brainstorm.md`.
