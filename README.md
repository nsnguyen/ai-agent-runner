# ai-agent-runner

`runner` runs my AI agents on Firecracker microVMs, one microVM per agent, like fly.io Sprites: local Claude Code starts and drives one over MCP, or I land in its terminal over SSH. Design: [docs/design.md](docs/design.md).

## Set up a host

The host needs:

- Linux on x86_64 or aarch64, with `/dev/kvm` and systemd.
- On the tailnet, with no other way in over SSH (see Tailnet only).
- `/var/lib/runner` on XFS or btrfs. On ext4 every fork and run is a full copy of the disk.

Install, from a checkout:

```sh
git clone https://github.com/nsnguyen/ai-agent-runner && cd ai-agent-runner
cargo build --release
sudo host/install.sh
```

VMs get 172.16.0.0/16; for another range, `sudo RUNNER_NET=10.20.0.0/16 host/install.sh`, which reruns remember (`/etc/runner/net`). VMs reach the internet but not the tailnet, the LAN, cloud metadata (169.254.0.0/16) or each other, except for DNS (port 53), since they use the host's resolvers. Docker and firewalld drop forwarded traffic by default, so on a host running either, allow the VM range there; `runner doctor` warns when one is active. It installs the pinned Firecracker (hash-checked, `host/firecracker.pin`) and `runner` into `/usr/local/bin`, makes the `runner` user, lets it start and stop `firecracker@<vm>` units through polkit, and sets up NAT for the VMs. It's safe to rerun, and rerunning it after a `git pull` and a build is how to upgrade. It ends with `runner doctor`, run as the runner user, which says what's still missing: usually a guest kernel at `/var/lib/runner/kernel/vmlinux`, the base disk (`runner import base <disk>`), and the laptop's key. Run it again any time with `ssh runner@kvm-host runner doctor`.

### Add the laptop's key

From the laptop, through your own sudo account on the host (`admin` here):

```sh
ssh -t admin@kvm-host "echo 'from=\"100.64.0.0/10,fd7a:115c:a1e0::/48\",restrict,pty $(cat ~/.ssh/id_ed25519.pub)' | sudo -u runner tee -a ~runner/.ssh/authorized_keys"
```

- `from=` takes this key only from Tailscale's IPv4 and IPv6 ranges: the tailnet-only rule, held per key.
- `restrict` turns off port, agent and X11 forwarding.
- `pty` gives back the terminal `restrict` takes away, so `ssh -t` works for `runner shell`.

### Tailnet only

The host takes SSH only over the tailnet. Never forward port 22 to it from a router or expose it on a public IP. If sshd does listen elsewhere, `from=` still refuses the runner user's key from outside the tailnet.

This needs the host's own sshd, not Tailscale SSH: with Tailscale SSH on, tailscaled answers port 22 on the tailnet address and never reads `authorized_keys`.

## Build the base image

`base` is the VM every other VM forks from: Ubuntu 24.04 booting systemd, with `runner-guest` as a service, git, curl, build-essential, tmux, Node LTS, Python, Rust (rustup) and Claude Code. You work as `agent`, with passwordless sudo, in `/home/agent`. The image holds no credentials: each boot hands the VM the host's Claude Code token, in memory only, so `claude` there is logged in (docs/host.md, step 6). `runner creds <vm>` says which credentials a VM gets.

On a Linux machine with docker or podman and e2fsprogs:

```sh
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl --bin runner-guest
sudo images/base/build.sh
```

It writes `images/base/out/base.ext4`, a sparse 20 GiB ext4 disk, and `images/base/out/vmlinux`, Firecracker's pinned CI guest kernel. It runs as root so the disk keeps every file's owner. Before it replaces an older disk, it checks the new one with `images/base/check.sh` (runner-guest is enabled, and nothing is under `/home/agent/.claude` or `/root/.claude`) and runs `claude --version` in the image.

Settings go after `sudo`, which drops your environment: `sudo SIZE=40G images/base/build.sh`. The others are `OUT`, `ENGINE` and `RUNNER_GUEST`, described at the top of `build.sh`.

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

## Use it from the laptop

On the laptop, `runner` is a short sh script that runs each command on the host over SSH: `runner shell dev` is `ssh -t runner@kvm-host runner shell dev`. Nothing else goes on the laptop.

```sh
git clone https://github.com/nsnguyen/ai-agent-runner && cd ai-agent-runner
client/install.sh runner@kvm-host
```

It installs `runner` to `~/.local/bin` and saves the host to `~/.config/runner/host`; `RUNNER_HOST` overrides that. The host is anything `ssh` takes, so an alias from `~/.ssh/config` works. Arguments reach the host as typed, spaces and quotes included, and the host's exit code comes back. It asks for a terminal only when stdin and stdout are both one, so `runner list | grep dev` gets plain lines and `echo hi | runner exec dev -- cat` works. Rerun `client/install.sh` after a `git pull` to upgrade.

### Claude Code in the cloud

```sh
runner claude dev
```

lands you in Claude Code running in dev, logged in with the host's token (docs/host.md, step 6). If dev doesn't exist it's made from `base` first (`--from` picks another VM to fork), and if it's stopped it's started. Claude Code runs in dev's tmux session named `claude`, started in `/home/agent/work` (`--dir` picks another, which has to exist). Detach with `Ctrl-b d`, or just close the laptop: Claude Code keeps working. `runner claude dev` again reattaches to the same one; a VM never runs two. Anything after `--` goes to `claude` when it starts, as in `runner claude dev -- --continue`; with Claude Code already running, it's left out and `runner` says so. The session is an ordinary one, so `runner sessions dev`, `attach` and `kill` work on it too. There's no MCP tool for this, since it's interactive.

### Shell and exec

`runner shell dev` logs in to dev as `agent`, in `/home/agent`, on a terminal of its own, which follows your window's size. `runner exec dev -- cmd args` runs one command there: no terminal unless `--tty`, `--env K=V` and `--dir` for its environment and directory, stdin passed through, and the command's exit code back (128 plus the signal for one a signal killed, 255 when it couldn't run). Both start a stopped VM first and wait up to a minute for its guest agent. `--user` runs as someone else.

### Sessions

`runner shell dev --session work` (or `-s work`) lands you in tmux session `work` in dev, making it if it's new. Detach with tmux's `Ctrl-b d`, or just close the laptop: a dropped connection detaches the session and never kills it. `runner attach dev work` gets back in, `runner sessions dev` lists each session's name, the command in its active pane, when it started and whether anyone is attached, and `runner kill dev work` ends one. They're `agent`'s own tmux sessions, so `tmux ls` inside the VM shows the same ones. Only `shell --session` starts a stopped VM; a stopped one has no sessions.

### MCP clients

`runner mcp` is `ssh -T -o BatchMode=yes runner@kvm-host runner mcp`: no terminal, so stdout carries only JSON-RPC, and no prompts, so the SSH key has to load without one (from the agent, or with no passphrase). Run `runner list` once from a terminal first, so ssh can ask you to accept the host's key.

Claude Code, in every project:

```sh
claude mcp add --scope user runner -- runner mcp
```

Claude Desktop (`~/Library/Application Support/Claude/claude_desktop_config.json`) and Cursor (`~/.cursor/mcp.json`) take the same entry. They don't get your shell's PATH or `RUNNER_HOST`, so give the absolute path, which `client/install.sh` prints, and keep the host in `~/.config/runner/host`:

```json
{
  "mcpServers": {
    "runner": {
      "command": "/Users/you/.local/bin/runner",
      "args": ["mcp"]
    }
  }
}
```

Restart the app after editing its config.

The `exec` tool runs a command in a VM, starting it first if it's stopped: `argv`, optional `stdin` text, and `timeout_secs` (120 unless given), after which the command is killed. It returns the exit code or signal, and stdout and stderr, each cut at 32 KiB with a truncated flag. It runs as `agent`, or as `RUNNER_USER` if the host's environment sets one.

`list_sessions`, `read_session` and `kill_session` do for an agent what `runner sessions`, `attach` and `kill` do for you. `read_session` returns the last `lines` (100 unless given, at most 2000) of a session's active pane as plain text, history included, cut at 32 KiB from the end.
