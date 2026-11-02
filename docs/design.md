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
- **Every VM is a fork.** Disks are reflink copies on XFS or btrfs, with a sparse full copy elsewhere. An image is a VM you stopped changing; `base` is the one you fork most. A run clones the current disk into a new generation that only a clean finish commits, so a crash rolls back for free and `undo` steps back one. Every commit then deletes old generations, keeping every checkpoint and five more, undo's way back first; `runner gc` does the same by hand. A running VM's checkpoint syncs the guest, pauses it through Firecracker's API, copies the disk it runs, and resumes it.
- **One run per VM.** The process running a VM holds an flock on it, which the kernel drops if that process dies. In M2 that process is the VM's systemd unit.
- **Runs are detached.** `run` starts `claude -p` in the VM and returns a run id at once; `output` reads its log by offset. A process of its own drives each run, in a session of its own, holding the VM's run lock, so a closed laptop or a dropped SSH connection can't kill a run, and the log is on the host, in the VM's directory. A run on a stopped VM boots it and shuts it down at the end, committing only a clean exit; one on a running VM checkpoints it and joins it, and restores that checkpoint, restarting the VM, unless the run exits cleanly. Once the VM is down, the run's own process measures what changed between the generation it booted and the one it booted from, reading both disks in userspace, and writes that result before it says the run is over.
- **A fixed address per VM.** A slot picked at create gives each VM a /30 of the host's range (`/etc/runner/net`, which `host/install.sh` writes and NATs; default 172.16.0.0/16) and a tap named for it. The guest gets its address, gateway and DNS from the kernel's `ip=` argument; the DNS servers are the host's, and the image links `/etc/resolv.conf` to `/proc/net/pnp` to use them. The runner only adds and removes taps.
- **Into the VM over vsock.** The host reaches a small guest agent through Firecracker's vsock socket, so the guest needs no sshd and no keys. `runner shell <vm>` is SSH to the host, then a terminal over vsock.
- **Services are systemd units in the guest.** `runner service add` writes `/etc/systemd/system/runner-svc-<name>.service` over the guest protocol, as root, and enables it, so it runs from every boot with `Restart=always`. The disk keeps the unit; no state about it is on the host.
- **Sleep keeps the boot.** `runner sleep` has the VM's boot pause it, write a Firecracker snapshot of its memory and devices into its directory, and stop Firecracker; the boot stays up, holding the VM's lock, tap and guest sockets, so `wake` only starts a fresh Firecracker to load the snapshot, then sets the guest's clock (`src/sleep.rs`). The snapshot runs on the boot's own uncommitted disk, so only that boot can wake it: a stop wakes the VM to shut down cleanly, and a restore stops it first.
- **Sleep when idle.** Each command that needs a VM's guest, or would hold the VM as a sleep does (`run`, `checkpoint`, `restore`), holds its use lock, shared, from before it looks at the VM until it's done, and touches the lock's file at each end. Every 10 seconds the boot looks: the lock held or touched, a run going, or a terminal on a tmux session in the guest (asked last, of runner-guest, as root) count as use. Once none has been seen for the VM's idle time, 10 minutes unless `runner idle` says otherwise, the boot puts it to sleep holding the lock exclusive, so a command that comes then waits, sees it sleeping, and wakes it (`src/idle.rs`).
- **A URL per VM, on the tailnet only.** The VM's boot listens on the host's Tailscale address at 10000 plus the VM's slot, the only port the runner opens, and takes connections only from Tailscale's range, so not from the LAN or another VM. Each connection holds the use lock, has the boot's loop wake the VM if it's asleep, waits for the guest to accept on port 8080 (or what `runner url --port` set), and passes TCP both ways (`src/url.rs`). The setting is in the VM's config, which the boot reads every second.
- **Out of the VM, to its own boot.** The agent checkpoints, lists and restores its own VM with `runner-guest checkpoint`, `checkpoints` and `restore`, calls over vsock that the VM's `runner boot` serves (`src/calls.rs`). No call names a VM, so a guest can't reach another. Its checkpoints are capped at 10 an hour, since gc keeps them all. A restore reboots the VM onto the copy: boot commits at the clean shutdown, then exits with a status the unit restarts on. `/.runner/llm.txt` in the base image tells the agent all this.

## Milestones

| | What | Done when |
|---|---|---|
| M1 (this PR) | The VM store (import, create, fork, generations, undo, delete), the CLI, and MCP over stdio. No VM boots. | Checks pass here, and local Claude Code lists VMs through `ssh kvm-host runner mcp` |
| M2 | Boot: systemd unit, tap networking with NAT, `start`, `stop`, `logs`, the guest agent, `shell` | `runner shell dev` lands you in a VM |
| M3 | Agent runs: `run` and `output`, credentials per boot, a result of exit code and changed paths | Local Claude Code runs Claude Code in a VM and reads what it did |
| M5 | Sleep and wake: Firecracker snapshots, then sleep when idle and wake on use | An idle VM sleeps, and `runner shell dev` wakes it in under a second with its processes running |
| Later | Best-of-N forks, taking over a run, more hosts | Each when it hurts |

## Guards

- Only my agents; the host is reachable only over the tailnet. It never rents a host.
- Credentials come in per boot and never land on a VM's disk, so a forked disk holds no keys.
- A run's result keeps what the runner measured apart from what the agent wrote, which is marked untrusted.

## Credentials

- The host holds `CLAUDE_CODE_OAUTH_TOKEN` (from `claude setup-token`) or `ANTHROPIC_API_KEY`, and optionally `GH_TOKEN`. Each comes from a systemd credential of the same name (`LoadCredential=` in `firecracker@.service`, read from `/etc/credstore/<name>`), or else the environment, never from a file in the repo or a VM's directory.
- Each VM has an allowlist of credential names in its directory: `claude` by default, `github` only when `runner creds <vm> claude github` adds it. A fork starts from the default. No MCP tool sets it.
- `runner-guest` asks for them each time it starts, before it takes a command: it connects to the host (CID 2) on vsock port 10023, which Firecracker passes on to `vsock.sock_10023` in the VM's directory, where `runner boot` hands it the allowed ones (`creds::Keyring`). So no command runs without them, and a restarted guest gets them again. A guest the host can't answer runs without.
- The guest keeps them in memory, takes only those variables, and sets exactly them in each command's environment, sessions and runs included (`Keyring::apply`), last, so a request can't add or change one. Its unit sets `LimitCORE=0`, so no core dump writes one to the disk, and the base image sets up no swap.
- A sleeping VM's snapshot is its memory, so it holds them: in `<vm>/snapshot/`, which only the runner user can read, until the VM wakes, or, if its boot was killed asleep, until it next boots or is deleted. It's no disk's, so forks and checkpoints still hold none.
- Boot notes which ones the guest took in `<vm>/handed`, names only, and `runner status` shows them while the VM is up: `running  creds claude`.

## Assumptions (correct these)

- The host is a Linux machine of yours with KVM, on your tailnet, with XFS or btrfs under `/var/lib/runner`.
- Claude Code is the first agent, installed in the base image.

Traps and the full brainstorm are in `brainstorm.md`.
