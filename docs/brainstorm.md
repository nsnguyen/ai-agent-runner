# Brainstorm: ai-agent-runner's architecture

Run with temper's adhd skill: 5 isolated branches (competitor, 3am on-call, $0 and one hour, logistics, remove the load-bearing assumption), 6 ideas each, then scored and clustered, with the top 3 deepened.

**Problem.** A personal runner for AI agents on Firecracker microVMs, like fly.io Sprites. Local Claude Code drives an agent in a VM over MCP, or a CLI drops me into its shell over SSH. VMs keep their files. It runs on KVM hosts I provide, in Rust, and never calls models itself.

Scores are `[N novelty, V viability, F fit]`, out of 10. ⚠ marks a trap.

## Wide set

### No control plane
- Stdio MCP over SSH: Claude Code runs `ssh host runner mcp`, so SSH is transport, auth and discovery `[N8 V9 F10]`
- No daemon: each VM is a systemd unit the CLI starts over SSH `[N7 V8 F9]`
- systemd as the control plane: a `firecracker@` template unit, with journald holding each run's output `[N7 V8 F9]`
- The filesystem is the database: one directory per VM, and state is what `ls` says `[N6 V9 F9]`
- Tailnet and SSH keys only: no public ports, certificates or DNS `[N4 V9 F8]`
- A run is a manifest dropped into a spool directory over scp `[N7 V7 F7]`
- Name-hashed networking: a VM's name picks its subnet and tap, and one ProxyJump line reaches it `[N8 V7 F8]`

### Every VM is a fork
- No image registry: a VM's disk is a reflink fork of another's, and an image is a VM you stopped changing `[N8 V8 F9]`
- A run is a transaction over disk generations: commit on clean exit, roll back on crash `[N6 V7 F8]`
- A return label per run: the generation id comes back with the output and rolls the VM back `[N6 V7 F8]`
- A read-only system layer plus a writable home, so persistence can't change what runs at boot `[N5 V8 F8]`
- Dead-letter dock: a failed run's VM is frozen so I can SSH into the exact crashed state `[N8 V6 F8]`
- Demurrage: idle VMs drop to cheaper storage tiers over time `[N7 V6 F6]`
- A running VM is just a snapshot file that each call resumes and re-pauses `[N8 V6 F8]`

### One session, two doors
- tmux as the drive protocol: MCP sends keys and reads the pane, and SSH attaches to the same session `[N8 V7 F9]`
- One long-lived agent session per VM that both I and local Claude Code step into `[N7 V7 F9]`
- Parcel-locker SSH: only the host runs sshd, and each VM session is bridged in over vsock `[N8 V7 F9]`

### Distrust the agent
- Credentials never touch the VM's disk: handed in per run over vsock, revoked after `[N7 V6 F8]`
- The MCP result is what the runner measured; the agent's own text is marked untrusted `[N8 V7 F8]`
- Idempotency key and a single-writer lease per VM; start returns a handle, output is read by cursor `[N5 V8 F9]`
- A lease the caller renews, or the VM snapshots itself and stops `[N7 V7 F8]`

### Wild
- Runs are git pushes: push a TASK.md, results come back as commits `[N9 V6 F7]`
- A weekly reboot proves everything comes back from disk `[N7 V6 F5]`
- Prompt in and result out on tiny "mailbox" block devices `[N8 V5 F6]` ⚠
- The agent run is the VM's PID 1 `[N8 V6 F6]` ⚠
- The MCP server lives inside each VM `[N8 V6 F6]` ⚠
- Persistence by never stopping VMs `[N6 V7 F6]` ⚠
- KVM hosts dial out to the laptop over a reverse tunnel `[N7 V5 F5]` ⚠
- The host terminates SSH itself with a Rust SSH server and splices into the guest `[N7 V6 F8]` ⚠

## Traps

- **Mailbox drives:** no streaming output, and results only after power-off.
- **Agent as PID 1:** leaves no shell to SSH into and no persistent session.
- **MCP inside each VM:** local Claude Code couldn't create a VM, and every VM needs its own MCP entry.
- **Never stopping VMs:** one reboot or OOM loses every VM's state.
- **Reverse tunnel to the laptop:** a sleeping laptop stops all work.
- **Our own SSH server:** a security surface OpenSSH already covers.

## Converge

1. ★ **SSH is the control plane.** One host binary, MCP over `ssh host runner mcp`, systemd for lifecycle, a directory per VM. No daemon, API or open port.
2. **Every VM is a fork.** Reflink disks with generations: persistence, images and rollback are one mechanism.
3. **One session, two doors.** Detached runs that both local Claude Code and I can attach to.

## Focus

### ★ SSH is the control plane
**How it works.** Locally, `claude mcp add runner -- ssh -T -o BatchMode=yes kvm-host runner mcp` registers the MCP server, and a shell function `runner() { ssh -t kvm-host runner "$@"; }` is the CLI, so nothing ships to the laptop. On the host, one Rust binary has subcommands, and `runner mcp` exposes the same functions as MCP tools over stdio, logging only to stderr. `create` makes the VM's directory and reflink-copies a base disk. `start` and `stop` are systemctl on a template unit that runs Firecracker with the VM's config. The serial console goes to journald. Exec and shell use Firecracker's vsock socket, so control never needs a guest IP. Auth is a dedicated `runner` user with a restricted authorized_keys line and a polkit rule limited to `firecracker@*` units.

**Load-bearing risk.** The SSH session's lifetime limits anything the MCP process holds in memory. So every agent run must be detached from day one: it runs as its own process in the VM, writes its output to a file, and the MCP tools only start, poll and read by offset. A smaller trap on the same path: anything the `runner` user's shell prints to stdout corrupts the JSON-RPC stream.

**First step.** The crate with `runner list` and `runner mcp` exposing `list_vms`, tested against a temp directory, then `claude mcp add` on the real host to prove the path before any VM boots.

**Branches.** Stop as snapshot, for sub-second resume. Detached runs as the MCP shape. Fork by reflink to try two approaches. The KVM host as the only integration tester. Several hosts with no scheduler, just SSH config entries.

### Every VM is a fork
**How it works.** Each VM's directory holds disk generations and a list of committed ones. Forking is one reflink clone, constant time and no new space until a side writes. A run clones the current generation into a new one and boots from it. A clean power-off commits it; a panic, kill or host crash leaves the old one current and keeps the failed one for a look. Firecracker snapshots memory but not disks, so in v1 every run goes from boot to power-off and clones are taken from a stopped disk. Per-boot identity and secrets come in at boot and never land on disk, so forks share no keys. The first base comes from importing an ext4 image.

**Load-bearing risk.** Consistency at clone time: cloning a running VM's disk catches unflushed guest cache, so runs must start from a stopped disk, and two runs on one VM must never overlap. Reflink needs XFS or btrfs, so a full-copy fallback has to exist for the cloud containers the code is written in.

**First step.** A store module with no KVM dependency: import, fork, begin a run, commit or abort, undo. A failing test first: abort a run and check the current generation is unchanged.

**Branches.** Failed generations stay bootable for a post-mortem. Best-of-N: fork N VMs, run the same prompt, keep the one whose tests pass. A shared read-only tools drive, so upgrading the agent doesn't mean re-forking. Diff two generations to show what a run changed. Warm forks from memory snapshots later.

### One session, two doors
**How it works.** A small guest agent on vsock owns one tmux session per VM. An MCP run starts `claude -p --resume <session> --output-format stream-json` there and copies the JSON stream to a log on the VM's disk. `run` returns a run id at once, and `wait(run, cursor)` returns new events within about 45 seconds, so no call blocks for a whole run. SSH ends at the host's sshd, which bridges into the guest over vsock. I can watch read-only while a run holds the VM, or take over: the run is interrupted and resumed interactively, then handed back.

**Load-bearing risk.** The two doors need different agent modes, so taking over depends on how Claude Code's `--resume` behaves between headless and interactive runs, which the runner doesn't control.

**First step.** A shell spike in any container with Claude Code: a headless run inside tmux, then an interactive `--resume` of the same session, checking that the transcript continues under the same id.

**Branches.** Preemption as an event the MCP side sees. Forward my ssh-agent only while I'm attached. A phone SSH app as the mobile client. A flight recorder of each unattended run. Snapshot when idle for scale-to-zero.

## Provocation
What if the first VM this runs is the agent that builds the runner?
