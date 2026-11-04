# ai-agent-runner

`runner` runs my AI agents on Firecracker microVMs, one microVM per agent, like fly.io Sprites: local Claude Code starts and drives one over MCP, or I land in its terminal over SSH. Design: [docs/design.md](docs/design.md). New to it? [EXAMPLES.md](EXAMPLES.md) shows each command with an example.

## Day to day, on the AWS host

For a host already set up as Set up a host and On AWS below say: `kvm-host` on the tailnet, the laptop's key in, `runner` installed on the laptop. Everything here runs on the laptop, from this checkout, with Tailscale connected.

### 1. Start the host

```sh
aws login --profile runner      # only if aws.sh says AWS won't take the credentials: a login lasts 12 hours
export AWS_PROFILE=runner       # or put it in ~/.zshrc once
client/aws.sh start             # a few minutes: it waits until the host has booted
runner list                     # over the tailnet; base and your VMs
```

The host comes back at a new public address, which doesn't matter: Tailscale reaches it as `kvm-host`.

### 2. Claude Code in a VM

```sh
runner claude dev
```

Makes `dev` from `base` if it's new, boots it, and lands you in Claude Code running there, logged in with the host's token. Detach with Ctrl-b, then d: Claude Code keeps working. `runner claude dev` again comes back to it. To check where you are, type `! hostname; whoami` in Claude Code: `dev` and `agent`.

### 3. Other ways in

```sh
runner shell dev                          # a shell in dev, as agent
runner exec dev -- ls -la /home/agent/work
runner run dev "Add a README to work/"    # headless: prints a run id, like dev.1
runner output dev.1 --follow              # its stream, until it ends
runner proxy dev 3000                     # localhost:3000 here is port 3000 in dev, until Ctrl-C
runner result dev.1                       # what the runner measured, apart from what the agent says
```

A run on a stopped VM boots it, and saves what it wrote only if Claude Code exits 0. A run on a running VM checkpoints it, then joins it as it is, and leaves it running however it ends: a failed or cancelled run's result names that checkpoint, and `runner restore dev <id>` rolls it back. It can't say what changed.

### 4. From Claude Code on the laptop

Once:

```sh
claude mcp add --scope user runner -- ~/.local/bin/runner mcp
```

Then, in `claude` on the laptop, `/mcp` shows `runner` connected, and you can ask it things like: "use the runner MCP to run an agent in dev that adds a test for main.py, then show me run_output's result". Its tools are `run_agent`, `run_output`, `cancel_run`, `exec`, `start_vm`, `stop_vm` and the session ones.

### 5. When you're done: stop it all

```sh
runner stop dev           # a clean shutdown saves the VM's disk: list shows its gen go up
client/aws.sh close       # SSH from the internet off again, if `open` let you in; says so if it's off already
client/aws.sh stop        # the host: about $0.10 an hour while it runs, only its disks while stopped
```

`client/aws.sh stop` waits until the host has stopped. Next time, start again at step 1.

### After merging a change

The host runs whatever was last copied to it. One command copies this checkout over and checks it end to end:

```sh
git checkout main && git pull
client/check.sh          # about 30 minutes, 40 when base is rebuilt
client/check.sh host     # only the copy and host/check.sh
client/check.sh laptop   # only the laptop checks
```

It names the branch and commit, then copies the checkout to `ubuntu@kvm-host:ai-agent-runner/` with `--delete`, leaving the host's `target` and `images/base/out` be. Then it runs `host/check.sh` there, its lines shown as they come: that installs the new build, rebuilds and imports base only when what base is made from changed, checks that `runner doctor` is all `ok`, and runs every boot test (docs/host.md, Boot tests). Then come the laptop checks, what only the laptop's path shows, through this checkout's `client/runner` on a scratch VM, `client-check`, forked from base: `laptop_checks` in the script lists them.

Each step and check gets a `PASS` or `FAIL` line. It ends with every `FAIL` again, the counts, and the host's log path, and exits 1 on any `FAIL`. A host that doesn't answer ends it at once, with exit 2: `client/aws.sh start`. The scratch VM is deleted at the end, after a `FAIL` too, unless `KEEP=1`; Ctrl-C leaves it and says how to delete it. The copy and `host/check.sh` go in as `ubuntu` with `~/.ssh/kvm-host.pem` (`RUNNER_ADMIN` names another login), the laptop checks as `runner` does, on `RUNNER_HOST` or the first host in `~/.config/runner/host`. A new by-hand check goes into `client/check.sh`, never a script of its own.

### When something's off

- `aws.sh: AWS won't take this laptop's credentials`: run `aws login --profile runner`, and check `AWS_PROFILE=runner` is set.
- `runner@kvm-host: Permission denied (publickey)`: the laptop's key line in the host's `~runner/.ssh/authorized_keys` must read `from="…",restrict,pty,port-forwarding ssh-ed25519 AAAA…`, with nothing between `port-forwarding` and the key.
- `runner proxy` prints `open failed: administratively prohibited` when something connects: the laptop's key line lacks `port-forwarding` (Add the laptop's key).
- `runner list` hangs or can't resolve `kvm-host`: Tailscale isn't connected on the laptop, or the host is stopped (`client/aws.sh ip` says).
- `client/aws.sh ssh` times out: after `close` it goes over the tailnet, as `kvm-host`, so Tailscale has to be connected on the laptop. Before `close`, it goes to the public address, which lets in only the laptop's address from when `up` or `open` last ran: run `client/aws.sh open` again.
- A VM won't start or stop: `runner logs dev` shows its console and boot notes.

## Set up a host

The host needs:

- Linux on x86_64 or aarch64, with `/dev/kvm` and systemd.
- On the tailnet, with no other way in over SSH (see Tailnet only).
- `/var/lib/runner` on XFS or btrfs. On ext4 every fork and run is a full copy of the disk.

On AWS, `client/aws.sh` makes one (see On AWS). Install, from a checkout:

```sh
git clone https://github.com/nsnguyen/ai-agent-runner && cd ai-agent-runner
cargo build --release
sudo host/install.sh
```

VMs get 172.16.0.0/16; for another range, `sudo RUNNER_NET=10.20.0.0/16 host/install.sh`, which reruns remember (`/etc/runner/net`). VMs reach the internet but not the tailnet, the LAN, cloud metadata (169.254.0.0/16) or each other, except for DNS (port 53), since they use the host's resolvers. Nor the host itself: it drops any connection a VM opens to it, to its sshd or anything else, on any of its addresses, IPv6 included, while replies to what the host opens, like a URL's connection to the VM, come back. So a resolver on one of the host's own addresses (a local dnsmasq, say) won't answer VMs; one on loopback isn't offered to them anyway. The rules, `host/nat.nft`, go by each VM's tap, not its source address, and drop what a tap brings in from outside the range, so a VM that forges a source outside the range gets no further. Docker and firewalld drop forwarded traffic by default, so on a host running either, allow the VM range there; `runner doctor` warns when one is active, or when the host's rules don't keep VMs off it. It installs the pinned Firecracker (hash-checked, `host/firecracker.pin`) and `runner` into `/usr/local/bin`, makes the `runner` user, lets it start and stop `firecracker@<vm>` units through polkit, and sets up NAT for the VMs. It's safe to rerun, and rerunning it after a `git pull` and a build is how to upgrade. It ends with `runner doctor`, run as the runner user, which says what's still missing: usually a guest kernel at `/var/lib/runner/kernel/vmlinux`, the base disk (`sudo -u runner runner import --replace base - <path/to/base.ext4`), and the laptop's key. Run it again any time with `ssh runner@kvm-host runner doctor`.

### Add the laptop's key

From the laptop, through your own sudo account on the host (`admin` here):

```sh
ssh -t admin@kvm-host "echo 'from=\"100.64.0.0/10,fd7a:115c:a1e0::/48\",restrict,pty,port-forwarding $(cat ~/.ssh/id_ed25519.pub)' | sudo -u runner tee -a ~runner/.ssh/authorized_keys"
```

- `from=` takes this key only from Tailscale's IPv4 and IPv6 ranges: the tailnet-only rule, held per key.
- `restrict` turns off port, agent and X11 forwarding.
- `pty` gives back the terminal `restrict` takes away, so `ssh -t` works for `runner shell`.
- `port-forwarding` gives back `ssh -L`, for `runner proxy`, and with it `-R` and socket forwarding. It reaches nothing new: the key already runs any command on the host as runner. A key line from before `runner proxy` lacks it: edit the line in `~runner/.ssh/authorized_keys` to add it.

### Tailnet only

The host takes SSH only over the tailnet. Never forward port 22 to it from a router or expose it on a public IP. If sshd does listen elsewhere, `from=` still refuses the runner user's key from outside the tailnet.

This needs the host's own sshd, not Tailscale SSH: with Tailscale SSH on, tailscaled answers port 22 on the tailnet address and never reads `authorized_keys`.

### On AWS

`client/aws.sh`, run on the laptop, makes a host on EC2 with the AWS CLI: an `m7i-flex.large` with nested virtualization on, which gives it `/dev/kvm` at a regular instance's price (a `.metal` costs dollars an hour), running Ubuntu 24.04, with a 50 GiB second disk for `/var/lib/runner`.

```sh
brew install awscli
aws configure set region us-west-2
aws login
client/aws.sh up
client/aws.sh ssh ls -l /dev/kvm
```

If `aws login` refuses because the default profile already holds access keys, `aws login --profile runner` and `export AWS_PROFILE=runner` keep the two apart. If `up` says the CLI doesn't know `NestedVirtualization`, `brew upgrade awscli`.

`up` waits a few minutes, until the host has booted. Until it's on the tailnet, only the laptop's address gets in over SSH, as `ubuntu` with the key `up` saves in `~/.ssh/kvm-host.pem`; after the laptop's address changes, `client/aws.sh open` lets in the new one and no other. On the host, put the second disk on XFS before installing. NVMe disks on EC2 aren't named in a fixed order, so find it by its size:

```sh
disk=/dev/$(lsblk -dno NAME,SIZE | awk '$2 == "50G" {print $1}'); echo "$disk"
sudo mkfs.xfs "$disk"
echo "UUID=$(sudo blkid -s UUID -o value "$disk") /var/lib/runner xfs defaults,nofail 0 2" | sudo tee -a /etc/fstab
sudo mkdir -p /var/lib/runner && sudo mount /var/lib/runner
```

Then install as above. The repo is private, so copy the laptop's checkout over rather than clone it, and build the base image there with podman, not docker, which drops the VMs' forwarded traffic:

```sh
rsync -az --exclude target --exclude images/base/out -e "ssh -i ~/.ssh/kvm-host.pem -o IdentitiesOnly=yes" ./ "ubuntu@$(client/aws.sh ip):ai-agent-runner/"
```

Put the host on the tailnet as `kvm-host`, so the name the steps above use resolves (with MagicDNS, which a new tailnet has on; remove an old `kvm-host` from the tailnet first, or the new one becomes `kvm-host-1`), then add the laptop's key through `ubuntu`, the sudo account on AWS:

```sh
curl -fsSL https://tailscale.com/install.sh | sh && sudo tailscale up --hostname kvm-host   # on the host
ssh -t -i ~/.ssh/kvm-host.pem -o IdentitiesOnly=yes ubuntu@kvm-host "echo 'from=\"100.64.0.0/10,fd7a:115c:a1e0::/48\",restrict,pty,port-forwarding $(cat ~/.ssh/id_ed25519.pub)' | sudo -u runner tee -a ~runner/.ssh/authorized_keys"   # on the laptop
```

Then `client/aws.sh close`: from then on nothing gets in from the internet. `client/aws.sh ssh` then goes over the tailnet, as `kvm-host`, and `client/check.sh` upgrades and checks the host (After merging a change). `client/aws.sh stop` stops the host, after which only its disks cost, `start` brings it back at a new public address, and `ip` prints it.

## Build the base image

`base` is the VM every other VM forks from: Ubuntu 24.04 booting systemd, with `runner-guest` as a service, git, curl, build-essential, tmux, Node LTS, Python, Rust (rustup) and Claude Code. You work as `agent`, with passwordless sudo, in `/home/agent`. The image holds no credentials: each boot hands the VM the host's Claude Code token, in memory only, so `claude` there is logged in (docs/host.md, step 6). `runner creds <vm>` says which credentials a VM gets.

On a Linux machine with docker or podman and e2fsprogs:

```sh
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl --bin runner-guest
sudo images/base/build.sh
```

It writes `images/base/out/base.ext4`, a sparse 20 GiB ext4 disk, and `images/base/out/vmlinux`, Firecracker's pinned CI guest kernel. It runs as root so the disk keeps every file's owner. Before it replaces an older disk, it checks the new one with `images/base/check.sh` (runner-guest is enabled, Claude Code's updater is off, `/.runner/llm.txt` and `/.runner/build` are there, and nothing is under `/home/agent/.claude` or `/root/.claude`) and runs `claude --version` in the image. `/.runner/llm.txt` (`images/base/llm.txt`) tells the agent where it is: where to work, what persists, how to checkpoint and restore, and how its credentials arrive.

Settings go after `sudo`, which drops your environment: `sudo SIZE=40G images/base/build.sh`. The others are `OUT`, `ENGINE` and `RUNNER_GUEST`, described at the top of `build.sh`.

On the runner host:

```sh
sudo -u runner runner import --replace base - <images/base/out/base.ext4
sudo install -D -m 0644 images/base/out/vmlinux /var/lib/runner/kernel/vmlinux
```

A disk of `-` is read from stdin, so the runner user needn't be able to read your home; the copy stays sparse. `--replace` puts the new disk on top of base's history as a new generation and keeps base's settings, so `runner undo base` brings the old one back. It's refused while base is up (`runner stop base` first). On a name that doesn't exist, `--replace` just imports; without `--replace`, an existing name is refused. VMs forked from the old base keep their own disks. An empty stdin, or one that ends before the ext4 filesystem it carries does (a dropped ssh), is refused, and base stays as it was.

The disk says what it was built from: `build.sh` writes `/.runner/build` into it, with a fingerprint of what the image is made from (every file and link under `images/base` but the build's output, the kernel pin among them, and the runner-guest binary) and the commit it was built at. `runner import` notes it, and `runner doctor`'s base line shows base's. `images/base/build.sh --fingerprint` prints the fingerprint without root and builds nothing: when it differs from doctor's, base needs rebuilding. It hashes the runner-guest binary you have, so rebuild that first (it warns when the binary is older than `src/`), and run it where you build base: a guest built on another machine can differ byte for byte, which only costs a needless rebuild. It can't see what the Dockerfile downloads, like Ubuntu's packages or Claude Code's `stable`, so it only says when this repo's side changed. A disk with no `/.runner/build`, like one built before it, shows as unknown, and so does base once it has booted and committed a disk of its own, until the next import.

The boot test needs `/dev/kvm` and `firecracker` on PATH. It boots a copy of the disk and checks that runner-guest starts and accepts on its vsock port:

```sh
RUNNER_GUEST_PORT=<port> cargo test --test base_image -- --ignored
```

To change versions, edit the `ARG`s at the top of `images/base/Dockerfile` (Node major, Claude Code version) and the kernel pin at the top of `build.sh`. Claude Code doesn't update itself in a VM: `images/base/managed-settings.json`, which the image installs as `/etc/claude-code/managed-settings.json`, sets `DISABLE_AUTOUPDATER`, so every VM made from a `base` runs the version it was built with, and a newer one comes from rebuilding `base`. A VM forked from an older base keeps updating itself.

## Use it from the laptop

On the laptop, `runner` is a short sh script that runs each command on the host over SSH, all but `proxy` (Ports): `runner shell dev` is `ssh -t runner@kvm-host runner shell dev`. Nothing else goes on the laptop.

```sh
git clone https://github.com/nsnguyen/ai-agent-runner && cd ai-agent-runner
client/install.sh runner@kvm-host
```

It installs `runner` to `~/.local/bin` and saves the host to `~/.config/runner/host`; `RUNNER_HOST` overrides that. The host is anything `ssh` takes, so an alias from `~/.ssh/config` works. Arguments reach the host as typed, spaces and quotes included, and the host's exit code comes back. It asks for a terminal only when stdin and stdout are both one (for `export`, when stdout is), so `runner list | grep dev` gets plain lines and `echo hi | runner exec dev -- cat` works. Rerun `client/install.sh` after a `git pull` to upgrade.

### Claude Code in the cloud

```sh
runner claude dev
```

lands you in Claude Code running in dev, logged in with the host's token (docs/host.md, step 6). If dev doesn't exist it's made from `base` first (`--from` picks another VM to fork), and if it's stopped it's started. Claude Code runs in dev's tmux session named `claude`, started in `/home/agent/work` (`--dir` picks another, which has to exist). Detach with `Ctrl-b d`, or just close the laptop: Claude Code keeps working. `runner claude dev` again reattaches to the same one; a VM never runs two. Anything after `--` goes to `claude` when it starts, as in `runner claude dev -- --continue`; with Claude Code already running, it's left out and `runner` says so. The session is an ordinary one, so `runner sessions dev`, `attach` and `kill` work on it too, and the name `claude` is taken: a session you name `claude` yourself is the one `runner claude` attaches to. If `claude` exits as soon as it starts, say on a flag it doesn't know, its session closes with it and you see only `[exited]`; `runner exec dev --tty -- claude <args>` shows why. There's no MCP tool for this, since it's interactive.

### Shell and exec

`runner shell dev` logs in to dev as `agent`, in `/home/agent`, on a terminal of its own, which follows your window's size. `runner exec dev -- cmd args` runs one command there: no terminal unless `--tty`, `--env K=V` and `--dir` for its environment and directory (a credential's variable, like `GH_TOKEN`, is refused: credentials reach a VM only through `runner creds`), stdin passed through, and the command's exit code back (128 plus the signal for one a signal killed, 255 when it couldn't run). Both start a stopped VM first and wait up to a minute for its guest agent. `--user` runs as someone else.

### Sessions

`runner shell dev --session work` (or `-s work`) lands you in tmux session `work` in dev, making it if it's new. Detach with tmux's `Ctrl-b d`, or just close the laptop: a dropped connection detaches the session and never kills it. `runner attach dev work` gets back in, `runner sessions dev` lists each session's name, the command in its active pane, when it started and whether anyone is attached, and `runner kill dev work` ends one. They're `agent`'s own tmux sessions, so `tmux ls` inside the VM shows the same ones. Only `shell --session` starts a stopped VM; a stopped one has no sessions.

### Services

`runner service add dev web -- python3 -m http.server 8080` keeps a dev server up whenever dev is: a systemd unit in the VM, `runner-svc-web`, runs it as `agent` in `/home/agent/work` (`--dir` picks another, and a relative program like `./serve` is taken from there), now and at every boot, and starts it again a second after it exits. It runs as given, not through a shell: `-- sh -c 'npm i && npm run dev'` for shell syntax. `add` returns once the program has started; one that can't start fails, leaves nothing behind, and says why: its command and the journal's last lines, or plainly that its program couldn't be run. `runner service list dev` shows each one's systemd state, like `active/running`, or `activating/auto-restart` while it crash-loops; `runner service logs dev web` its output (`-f`, `-n 50`); and `runner service rm dev web` stops it for good. Each starts a stopped VM, or wakes a sleeping one, first, and sleep keeps a service running as it was. A service gets no credentials. From the host it answers at `runner ip dev`'s address. MCP has `add_service`, `list_services` and `remove_service`.

### Ports

```sh
runner proxy dev 3000              # localhost:3000 on the laptop is port 3000 in dev
runner proxy dev 3000 5173 8080:80 # several at once; 8080:80 puts dev's port 80 at localhost:8080
```

It starts dev if it's stopped, or wakes it if it's sleeping, asks the host for `runner ip dev`, then runs `ssh -L localhost:3000:<dev's address>:3000` to the host, which reaches dev's address, until Ctrl-C. Only the laptop's own localhost gets the port. A local port that's taken ends it at once (`cannot listen to port`), unless only one of 127.0.0.1 and ::1 is. The program in dev has to listen on dev's address, not just its 127.0.0.1: `vite --host`, `next dev -H 0.0.0.0`; `python3 -m http.server` does already. An open proxy counts as use: the tunnel runs `runner hold dev` on the host, which holds dev in use until the connection closes, by Ctrl-C or a drop, and then dev's idle time starts again. A laptop that sleeps with it open hasn't closed it, so dev stays awake until sshd gives up, about two hours by default. It needs `port-forwarding` in the laptop's key line (Add the laptop's key), and a host runner with `hold`: an older one says `unrecognized subcommand 'hold'` until you upgrade it. `proxy` runs on the laptop, so the host's own `runner` has no such command.

### Headless runs

```sh
runner run dev "Fix the failing test in app/ and commit it"
```

prints the run's id, `dev.1`, at once, while Claude Code works on the prompt in dev as `claude -p`, with stream-json output and its permission prompts skipped: the VM is the sandbox. It runs in `/home/agent/work` unless `--dir` names another, which has to exist, and `--resume <session>` carries on an earlier session, by the `session_id` in that run's first event: one that committed or joined, in the same `--dir`, since the session's record is on the VM's disk. The run doesn't need the connection that started it: close the laptop and it goes on.

`runner output dev.1` prints the stream so far, an event per line, and `--follow` keeps printing until the run ends, then says on stderr how it did. `--from <bytes>` skips what an earlier read printed. `runner runs dev` lists dev's runs, newest first, and `runner cancel dev.1` stops one. A VM has one run at a time, so a second `runner run dev` is refused while one goes on.

If dev is stopped, the run boots it and shuts it down once Claude Code exits: exit 0 commits what it wrote to dev's disk, and anything else, a cancel included, rolls it back, keeping the run's disk in dev's `gens/` for a look until a later commit's gc. So does dev going down under the run, by `runner stop` or a reboot. If dev is running, the run checkpoints it first (`"before run dev.1"`), then joins it as it is, and never stops or starts it: however the run ends, dev runs on as the run left it, with nothing committed, and its tmux sessions, an open `runner claude dev` included, carry on. A run that fails or is cancelled once Claude Code has started isn't rolled back: `runner runs dev` and `--follow` say `not rolled back, checkpoint 2 undoes it`, and `runner restore dev 2` rolls it back by hand, which restarts dev. The checkpoint stays either way, listed as kind `run`, so that undoes a run that exited 0 too, until gc drops it as one of the oldest (Checkpoints, below).

`runner result dev.1` prints, once the run is over, what the runner measured: the exit code, how long it took, whether the disk generation it booted was committed or rolled back, and the paths that changed on that disk. The runner reads those from the disks itself, the generation the run booted against the one it booted from, and never asks the agent. Each directory added or deleted whole is one path with a count of what's under it, and past 200 paths the rest are only counted. Paths only the VM's boot writes, like its journal and systemd's state, are left out and counted in `boot_only`. What the agent says it did is kept apart, in `agent_says`, marked untrusted: check it against `changed`.

```json
{
  "exit_code": 0,
  "duration_ms": 81234,
  "outcome": "committed",
  "from_generation": 2,
  "generation": 3,
  "changed": {
    "paths": [
      {"path": "/home/agent/work/main.rs", "change": "modified"},
      {"path": "/home/agent/work/target", "change": "added", "contains": 812}
    ],
    "more": 0,
    "boot_only": 3
  },
  "agent_says": {"untrusted": true, "text": "Fixed the bug.", "truncated": false}
}
```

The run reads both disks whole, so it goes on looking `running` for a while after Claude Code exits: about 45 seconds per 4.6 GB of disk (#61), and never more than 10 minutes. `runner cancel` then stops the measuring, and `changed` says so. A cancelled run is measured too: only a cancel after Claude Code is done stops the measuring. The paths left out as the boot's are a fixed list, `BOOT_ONLY` in `src/changes.rs`, found by booting a copy of the base image's systemd in a container and measuring its disk (#107): the journal under `/var/log/journal`, `/var/log/wtmp`, systemd's random seed and timer stamps, what the first boot writes, like `/etc/machine-id`, and what the daily timers a boot catches up on write, like `/var/backups`. Every boot empties `/tmp`, so what the last run left there, deleted, is left out too; what this run adds there is listed. Nothing under `/home` is ever left out. A boot of another image may write other paths, which are listed. Paths past 32 KiB of them are only counted, since the agent names them. A run that joined a running VM has no disk of its own to measure, so its `changed` says why instead of listing paths, with null generations, its `checkpoint`, and an `outcome` that's `not_rolled_back` if it failed or was cancelled once Claude Code had started, and null otherwise. One that never got as far as starting its VM has nulls too. With `--keep 0`, a commit's gc deletes the generation the run booted from before it's measured, so `changed` can only say it's gone. A lost run has no result.

The stream is kept on the host, in `/var/lib/runner/vms/dev/runs/1/`, beside Claude Code's stderr, so it can be read with dev off, and goes when dev is deleted.

### Checkpoints

```sh
runner checkpoint dev -m "tests pass"   # a frozen copy of dev's disk, by id
runner checkpoints dev                  # newest first; * marks the current disk
runner restore dev 4                    # dev's disk is checkpoint 4's again
```

A checkpoint is a copy of dev's disk, running or not. Of a running VM, it runs `sync` in the guest, pauses the VM, copies the disk it's running, and resumes it, so dev carries on. On XFS or btrfs the copy is a reflink and the pause takes milliseconds; on ext4 dev stays paused for a full copy of its disk. `checkpoints` lists the generations runs and restores made too, as `auto`; the checkpoint a run takes before it joins a running VM as `run`; and the agent's own as `agent`. `restore` puts a copy of the checkpoint on top of dev's history, so `undo` goes back to what dev had before it, and the checkpoint stays to restore again. A running dev is stopped first, which commits its disk as any clean shutdown does, and started again after (one already stopping is waited out and stays stopped); that ends its sessions and anything else in it, so it's refused while a run is going. Yours stay until dev is deleted. Runs' and the agent's pile up without anyone asking, so gc keeps only the newest 20 of those two kinds together, at every commit and in `runner gc`, plus the one a run going on dev took. Over MCP: `checkpoint_vm`, `list_checkpoints` and `restore_vm`.

The agent in dev can do the same from inside, and `/.runner/llm.txt` in the base image tells it how:

```sh
runner-guest checkpoint -m "before the migration"   # listed on the host as kind agent
runner-guest checkpoints
runner-guest restore 4                              # reboots dev onto checkpoint 4
```

Those calls reach dev's own boot over vsock, so a VM can only checkpoint itself. The agent gets 10 checkpoints an hour, and gc keeps the newest 20 of its and runs' together; yours aren't counted, capped or dropped. Its restore is copied first, then dev shuts down cleanly, which commits its disk, so `undo` goes back to it, and the unit boots dev again onto the copy. It's refused while a run is going. Both need a base built with this runner-guest, and the host's unit from `host/install.sh`.

### Sleep and wake

```sh
runner sleep dev    # dev's memory goes to disk, and its Firecracker stops
runner wake dev     # back where it was, in about a second
runner idle dev 30  # sleep on its own after 30 minutes unused: 10 until set, or 90s, 2h, never
```

You rarely need the first two. A VM sleeps on its own once nothing has used it for its idle time, and anything that needs it wakes it first. Use is a shell, exec, `attach`, `claude`, `sessions` or `kill` connected to it, a headless run going, an MCP `exec` or session call, an open `runner proxy` (Ports), or a terminal on one of its tmux sessions; a detached session doesn't count, since it sleeps with the rest of memory and is there on waking. Neither does traffic to its address, a service's included; a connection through its URL does, and wakes it (A URL per VM, below). Its boot looks every 10 seconds (as often as the idle time, if that's shorter), so the sleep comes up to 10 seconds late. `create --idle` sets it for a new VM, and a fork keeps its source's. `runner logs` says when it went unused.

A sleeping VM takes no CPU or RAM on the host: its memory and devices are in a Firecracker snapshot in its directory, about the size of its memory, while its boot waits to wake it. `wake` loads the snapshot into a fresh Firecracker, so every process, tmux session and Claude Code carries on where it was, and sets the guest's clock, which stood still while it slept. `start`, `exec`, `shell`, `claude`, `sessions`, `attach`, `kill`, `checkpoint`, `export` and `run` wake a sleeping VM first, as do the MCP tools that need its guest. Shells and execs open at the sleep end, since Firecracker closes vsock connections across a snapshot; tmux sessions live on. `sleep` is refused while a run is going, and until the guest's runner-guest is up. `stop` wakes it to shut down cleanly, which commits its disk as any stop does; `restore` stops it the same way and starts it again awake. `undo` and `gc dev` refuse a sleeping VM, as a running one, and a `gc` of every VM skips it, says so, and still exits 0. A snapshot belongs to its boot, so a host crash or a killed boot loses a sleeping VM's memory, and the next boot starts from its last committed disk. The snapshot is the guest's memory, credentials included, so only the runner user can read it, and it goes at the wake (or at the next boot or a delete, after a killed boot). Over MCP: `sleep_vm` and `wake_vm`, `vm_status` says `sleeping`, and `create_vm` takes `idle`.

### A URL per VM

```sh
runner url dev              # http://100.101.102.103:10001: dev's port 8080, from anywhere on the tailnet
runner url dev --port 3000  # to port 3000 in dev instead
runner url dev off          # and `on` again
```

Each VM gets a URL on the tailnet, like a Sprite's but private: open it on the laptop, or anything else on the tailnet, and the VM's app answers. The address is the host's Tailscale one, and the port is 10000 plus the VM's slot, so it stays the same for as long as the VM exists (a deleted VM's goes to the next one made). It goes to port 8080 in the VM until `--port` says otherwise, as plain TCP, so anything served there works, not only HTTP: `runner service add dev web -- python3 -m http.server 8080` is enough to try it.

The VM's boot holds the URL, so it answers while the VM is up. A connection to a sleeping VM wakes it and waits up to 30 seconds for the guest to take it; one to a stopped VM is refused, as nothing starts it. An open connection counts as use, so the VM doesn't idle-sleep under it; one from a client that went silent, like a laptop that slept, is dropped within a minute and a half. It's tailnet only: it listens only on the host's Tailscale address, never 0.0.0.0, and takes connections only from Tailscale's 100.64.0.0/10, so neither the LAN nor another VM gets in through it. Without Tailscale up there's no URL and `runner url` says so; a running VM's boot binds it once Tailscale is up, and takes `on`, `off` and `--port` within a second. A fork keeps its source's setting, on a port of its own. After an upgrade to this runner, a VM that's already up gets its URL at its next start, since its boot is the old one. MCP's `vm_status` gives the URL as `url`. For HTTPS and a name in place of the address, put `tailscale serve` in front (docs/host.md).

### Back up and move

```sh
runner export dev > dev.tar                               # dev as one stream, running or not; tar -xf reads it
runner export dev | aws s3 cp - s3://<bucket>/dev.tar     # straight off the host, through the laptop
aws s3 cp s3://<bucket>/dev.tar - | runner import dev -   # and back, on this host or another
runner export dev --no-checkpoints | zstd > dev.tar.zst   # only its current disk, compressed
zstd -dc dev.tar.zst | runner import dev2 -               # back under another name
```

`export` writes a VM to stdout as one tar: `dev/manifest.json` first, then `dev/disk.ext4`, its current disk, and `dev/checkpoints/<id>.ext4` for each of your checkpoints (kind `checkpoint` in `runner checkpoints`), oldest first. The manifest has a format version, dev's config (sizes, kernel path, idle time, URL setting), its allowlist's names, and each checkpoint's id, time and comment. Each disk goes as a sparse file, only the extents the host's filesystem holds data in, so a 20 GiB disk with 4.6 GB on it streams about 4.6 GB, and `tar -xf` gives it back sparse. Each checkpoint streams its own data in full, blocks it shares with the current disk too, so every checkpoint adds about as much again. Space the guest freed that the host's file still holds goes as data. Services are units on the disk, so they come with it. Not in it: undo's older generations, runs' and the agent's checkpoints, `runs/`, dev's slot, and its sleep snapshot, which holds the guest's memory and credentials. There's no compression: `| zstd` on either end. For `aws s3 cp -`, a stream past 50 GB needs `--expected-size` with at least the stream's bytes: dev's bytes used in `runner list` plus 3%, for tar's headers, is more, and `set -o pipefail` first: aws uploads whatever it's given, so without it an export that failed partway still leaves an object and exits 0.

A running dev is copied as its checkpoint is: `sync` in the guest, a pause, a copy of the disk it runs, a resume, so dev carries on and the export is its disk as a power cut right after the sync would leave it. On XFS or btrfs the pause takes milliseconds; on ext4 it lasts a full copy of the disk, and the copy takes that much room on the host until the export ends. The copy is unlinked from dev's directory as soon as it's open, before a byte goes out, so no checkpoint or generation is left, even when the export is killed while it streams. A sleeping dev is woken for it, as for a checkpoint, and left awake. It's refused while a run is going, as restore and sleep are, since a run's disk may roll back; while dev boots or stops; and onto a terminal on stdout. From the look to the copy it holds dev's run lock, so a run, a sleep or a restore asked for then is refused, and so is a second export. Every disk is open before a byte goes out, so dev can start, stop, or be deleted while the stream goes on. From the laptop it's `ssh runner@kvm-host runner export dev` with no tty, so the bytes pass as they are, and the host needs no cloud credentials; onto a terminal it asks for one, so the host sees it and refuses. There's no MCP tool, since JSON-RPC can't carry a disk.

`import` takes an export back as a new VM, from a file on the host or from stdin (`-`), on the host it came from or another: its current disk, your checkpoints under the same ids, so `runner restore dev2 4` is the same checkpoint 4 it was, with their times and comments, and its sizes, kernel path, idle time and URL setting. Its services are on the disk. It gets a free slot here, so a new address and URL port; undo has nothing to go back to; and it gets the `claude` credential only, as a fork does. Import says when the export named more, and `runner creds dev2 claude github` gives them back, or `--creds claude,github` sets them on import. A kernel path this host hasn't got comes in as it was: import says the VM won't boot until that kernel is there, its settings still change, and a fork of it, being a create, needs `--kernel` naming one here. A name that's taken is refused before anything is read, and an export comes in only as a new VM, never with `--replace`.

Import reads the stream itself, never with tar: only the entries the manifest names, in the order export writes them, each a plain or sparse file, written under names the runner makes and sparse as they came, then tar's end. Anything else, a stream cut short, an entry the manifest doesn't name, a link, a `..` in a name, or a format it doesn't know, is refused, and the VM is built aside and renamed in only at the end, so a refused import leaves nothing and the name stays free. Nothing in the stream is checksummed, so a byte flipped in a disk's data isn't caught: S3 and `zstd` check what they carry. `import` tells an export from a raw disk (Build the base image, `runner import base`) by its first block: a tar header, or not. A compressed stream is refused, so `zstd -dc dev.tar.zst | runner import dev2 -`.

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

The `exec` tool runs a command in a VM, starting it first if it's stopped: `argv`, optional `stdin` text, `env` (variables as an object; `CLAUDE_CODE_OAUTH_TOKEN`, `ANTHROPIC_API_KEY` and `GH_TOKEN` are refused, since credentials reach a VM only through `runner creds`), and `timeout_secs` (120 unless given), after which the command is killed. It returns the exit code or signal, and stdout and stderr, each whole up to 32 KiB; past that, its first and last 16 KiB with a line between saying how many bytes were cut, and a truncated flag, so the error a long command prints last is kept. It runs as `agent`, or as `RUNNER_USER` if the host's environment sets one.

`run_agent`, `run_output`, `cancel_run` and `list_runs` do for an agent what `runner run`, `output`, `cancel` and `runs` do for you. `run_agent` returns the run at once. `run_output` takes a `cursor`, 0 at first and then the one each reply gives, and returns the events past it, up to 64 KiB, with the run's state; while the run is quiet it waits up to 45 seconds for an event, so an agent can poll it in a loop without holding a connection for the whole run. Once the run is over, the reply that reaches the end of its stream has the run's `result` too, as `runner result` prints it.

`list_sessions`, `read_session` and `kill_session` do for an agent what `runner sessions`, `attach` and `kill` do for you. `read_session` returns the last `lines` (100 unless given, at most 2000) of a session's active pane as plain text, history included, cut at 32 KiB from the end.
