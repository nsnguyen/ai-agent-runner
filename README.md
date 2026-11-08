# ai-agent-runner

`runner` runs my AI agents on Firecracker microVMs, one microVM per agent, like fly.io Sprites: local Claude Code starts and drives one over MCP, or I land in its terminal over SSH. Design: [docs/design.md](docs/design.md). New to it? [EXAMPLES.md](EXAMPLES.md) shows each command with an example.

## Day to day, on the AWS host

For a host already set up as Set up a host and On AWS below say: `kvm-host` on the tailnet, the laptop's key in, `runner` installed on the laptop. Everything here runs on the laptop, from this checkout, with Tailscale connected.

### 1. Start the host

```sh
aws login --profile runner      # only if aws.sh says AWS won't take the credentials: a login lasts 12 hours
export AWS_PROFILE=runner       # or put it in ~/.zshrc once
runner list                     # over the tailnet; base and your VMs, once it has started the host if it's stopped
```

Any `runner` command starts its host if it finds it stopped. First it asks the host over SSH whether it answers, as one more connection (`ControlMaster auto` and `ControlPersist 10m` for the host in `~/.ssh/config` make that quick); when it doesn't, `runner` asks the `aws.sh` beside it (`client/install.sh` puts it in `~/.local/bin`) what AWS has the host as, and only if it's stopped, stopping or starting, runs `aws.sh start`, saying so on stderr, waits for SSH to answer (a few minutes at most, then the command goes anyway), and then runs the command. Only a host that doesn't answer is started, so a command that ran never runs twice, even when it exits 255, as `runner exec` does for one it couldn't run, or its session drops as the host stops; nor is one AWS has running, or one aws.sh didn't make. Without the AWS CLI on the laptop, nothing is asked first. `client/aws.sh start` starts every host at once, waiting until each has booted.

The host comes back at a new public address, which doesn't matter: Tailscale reaches it as `kvm-host`. It powers itself off once it's been idle for an hour: every VM stopped or asleep, no one logged in to the host itself, and no `runner` command at work. The laptop's own `runner` connections count only while a command runs, so a `runner mcp` left open doesn't keep it up. The next command starts it again. With several hosts (Several hosts), a command starts only the host it goes to, and `client/aws.sh start` and `stop` take every one `runner hosts` lists, side by side (On AWS).

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
client/aws.sh stop        # every host: about $0.10 an hour each while it runs, only its disks while stopped
```

`client/aws.sh stop` waits until every host has stopped. Leave it out and each host stops itself an hour after its last use (step 1). Either way, a VM left asleep sleeps on through it, and `runner start dev`, or a command that wakes it, resumes it where it was; one left running shuts down cleanly, as a `runner stop` does. `client/aws.sh state` says whether it's stopped. For another idle time, `sudo RUNNER_HOST_IDLE=2h host/install.sh` on the host, or `never`; reruns keep it (`/etc/runner/host-idle`). Next time, start again at step 1.

### After merging a change

The host runs whatever was last copied to it. One command copies this checkout over and checks it end to end:

```sh
git checkout main && git pull
client/check.sh          # about 30 minutes, 40 when base is rebuilt
client/check.sh host     # only the copy and host/check.sh
client/check.sh laptop   # only the laptop checks
```

It names the branch and commit, then copies the checkout to `ubuntu@kvm-host:ai-agent-runner/` with `--delete`, leaving the host's `target` and `images/base/out` be. Then it runs `host/check.sh` there, its lines shown as they come: that installs the new build, rebuilds and imports base only when what base is made from changed, checks that `runner doctor` is all `ok`, and runs every boot test (docs/host.md, Boot tests). Then come the laptop checks, what only the laptop's path shows, through this checkout's `client/runner` on a scratch VM, `client-check`, forked from base: `laptop_checks` in the script lists them.

Each step and check gets a `PASS` or `FAIL` line. It ends with every `FAIL` again, the counts, and the host's log path, and exits 1 on any `FAIL`. A host that doesn't answer ends it at once, with exit 2: `client/aws.sh start`. The scratch VM is deleted at the end, after a `FAIL` too, unless `KEEP=1`; Ctrl-C leaves it and says how to delete it. The copy and `host/check.sh` go in as `ubuntu` with `~/.ssh/kvm-host.pem` (`RUNNER_ADMIN` names another login), the laptop checks as `runner` does, on `RUNNER_HOST`, else the first host in `~/.config/runner/hosts`, else the one in `~/.config/runner/host`. A new by-hand check goes into `client/check.sh`, never a script of its own.

With several hosts (Several hosts), `client/check.sh` copies to and checks only the first. Give each other host the same build first, so a move and MCP across hosts find the same runner on both, then run `client/check.sh` as above; its laptop checks then try MCP across hosts and a move there and back:

```sh
RUNNER_HOST=runner@kvm-host-2 RUNNER_AWS_NAME=kvm-host-2 client/check.sh host   # kvm-host-2's copy, install and boot tests, with ~/.ssh/kvm-host-2.pem
```

### When something's off

- `aws.sh: AWS won't take this laptop's credentials`: run `aws login --profile runner`, and check `AWS_PROFILE=runner` is set.
- `runner@kvm-host: Permission denied (publickey)`: the laptop's key line in the host's `~runner/.ssh/authorized_keys` must read `from="…",restrict,pty,port-forwarding ssh-ed25519 AAAA…`, with nothing between `port-forwarding` and the key.
- `runner@kvm-host-2: Permission denied (publickey)` from a host, not the laptop: in `list_hosts`' `unreachable`, or from `move`. kvm-host-2 doesn't have that host's key, say because the host had none when `runner hosts add` ran, on a build from before hosts shared keys. Upgrade that host (After merging a change), then `runner hosts add runner@kvm-host-2` again (Keys between hosts).
- `runner proxy` prints `open failed: administratively prohibited` when something connects: the laptop's key line lacks `port-forwarding` (Add the laptop's key).
- `runner list` hangs or can't resolve `kvm-host`: Tailscale isn't connected on the laptop, or the host is stopped and `runner` didn't start it (`client/aws.sh state` says).
- A stopped host that `runner` doesn't start: it needs `aws.sh` beside it (rerun `client/install.sh`), the AWS CLI on PATH, and `AWS_PROFILE` set where it runs, so an app's MCP entry needs them in its `env` (MCP clients). Signed out, aws.sh says so, then ssh's error comes, and nothing starts.
- `client/aws.sh ssh` times out: after `close` it goes over the tailnet, as `kvm-host`, so Tailscale has to be connected on the laptop. Before `close`, it goes to the public address, which lets in only the laptop's address from when `up` or `open` last ran: run `client/aws.sh open` again.
- A VM won't start or stop: `runner logs dev` shows its console and boot notes.

## Set up a host

The host needs:

- Linux on x86_64 or aarch64, with `/dev/kvm` and systemd.
- On the tailnet, with no other way in over SSH (see Tailnet only).
- `/var/lib/runner` on XFS or btrfs. On ext4 every fork and run is a full copy of the disk.

On AWS, `client/aws.sh up` makes one and does all of this (see On AWS). Elsewhere, install from a checkout:

```sh
git clone https://github.com/nsnguyen/ai-agent-runner && cd ai-agent-runner
cargo build --release
sudo host/install.sh
```

VMs get 172.16.0.0/16; for another range, `sudo RUNNER_NET=10.20.0.0/16 host/install.sh`, which reruns remember (`/etc/runner/net`). The host stays up until it's stopped; `sudo RUNNER_HOST_IDLE=1h host/install.sh` has it power itself off once nothing has used it for an hour, which reruns remember too (`/etc/runner/host-idle`; `never` turns it off again). It's for a host that a power-off only stops, as on EC2, and `runner doctor` shows it (docs/host.md, Idle power-off). VMs reach the internet but not the tailnet, the LAN, cloud metadata (169.254.0.0/16) or each other, except for DNS (port 53). Nor the host itself: it drops any connection a VM opens to it, to its sshd or anything else, on any of its addresses, IPv6 included, while replies to what the host opens, like a URL's connection to the VM, come back. A VM's DNS server is its gateway, where its boot answers on port 53 and passes each query on to the host's resolvers; on the host itself, the rules let a VM reach port 53 only there. So a resolver the host runs (a local dnsmasq, say) won't answer VMs on any address, and one on loopback isn't offered to them anyway. One on every address takes port 53 before a boot can, and then the VM gets the host's resolvers directly, which `runner logs` says, unless it has a list of names: then it doesn't boot, since only its boot holds it to them; started while a VM is up, it can't bind port 53, so start it first. A VM with a list of names reaches only the addresses its boot's DNS gave it for those names: its boot puts the VM's tap in the rules' set of listed taps before the VM starts, and each answer's addresses in beside it, paired with the tap, for the answer's TTL but at least 10 minutes (a week at most). Anything else the VM opens is dropped: an address it didn't look up, or got from another resolver, which it can't reach on port 53 either, and any private address, a listed name's too. What's open already stays open, so a list made on a running VM, which counts within a second, cuts no connection, and a name taken off makes the VM look its names up again. A VM whose boot couldn't serve its DNS has the host's resolvers instead, and a list made on it counts only from its next boot, which won't start without serving it. The URL and `runner proxy` still reach it, since the host opens those. A firewall reload empties the rules' sets, so for up to 10 seconds after one, until each boot puts its own back, a listed VM reaches what an unlisted one does, and a connection it opens then stays open. A list narrows the ways out; it doesn't close them: an allowed address can serve other sites too, as a CDN's does, and api.anthropic.com, on every list, takes anyone's key. On rules from an install.sh before lists, which `runner doctor` warns of, a VM with a list doesn't boot. The rules, `host/nat.nft`, go by each VM's tap, not its source address, and drop what a tap brings in from outside the range, so a VM that forges a source outside the range gets no further. Docker and firewalld drop forwarded traffic by default, so on a host running either, allow the VM range there; `runner doctor` warns when one is active, or when the host's rules don't keep VMs off it. It installs the pinned Firecracker (hash-checked, `host/firecracker.pin`) and `runner` into `/usr/local/bin`, makes the `runner` user and its SSH key (Keys between hosts), lets it start and stop `firecracker@<vm>` units through polkit, sets up NAT for the VMs, and starts `runner-park.service`, which keeps sleeping VMs asleep through a host shutdown (Sleep and wake). It's safe to rerun, and rerunning it after a `git pull` and a build is how to upgrade. It ends with `runner doctor`, run as the runner user, which says what's still missing: usually a guest kernel at `/var/lib/runner/kernel/vmlinux`, the base disk (`sudo -u runner runner import --replace base - <path/to/base.ext4`), and the laptop's key. Run it again any time with `ssh runner@kvm-host runner doctor`.

### Add the laptop's key

From the laptop, through your own sudo account on the host (`admin` here):

```sh
ssh -t admin@kvm-host "echo 'from=\"100.64.0.0/10,fd7a:115c:a1e0::/48\",restrict,pty,port-forwarding $(cat ~/.ssh/id_ed25519.pub)' | sudo -u runner tee -a ~runner/.ssh/authorized_keys"
```

- `from=` takes this key only from Tailscale's IPv4 and IPv6 ranges: the tailnet-only rule, held per key.
- `restrict` turns off port, agent and X11 forwarding.
- `pty` gives back the terminal `restrict` takes away, so `ssh -t` works for `runner shell`.
- `port-forwarding` gives back `ssh -L`, for `runner proxy`, and with it `-R` and socket forwarding. It reaches nothing new: the key already runs any command on the host as runner. A key line from before `runner proxy` lacks it: edit the line in `~runner/.ssh/authorized_keys` to add it.

### Keys between hosts

Each host's runner user has an SSH key, `~runner/.ssh/id_ed25519`, which `host/install.sh` makes if it's missing; `runner doctor` warns when there's none. With several hosts, `runner hosts add` on the laptop (Several hosts) puts each host's public key in every other host's `~runner/.ssh/authorized_keys`, ending in the host's name:

```
from="100.64.0.0/10,fd7a:115c:a1e0::/48",restrict,command="/usr/local/bin/runner peer" ssh-ed25519 AAAA… kvm-host-2
```

So one host reaches another only over the tailnet, with no terminal or forwarding, and whatever it asks for, sshd runs `runner peer`. That runs `runner mcp`, or `runner import <vm> -` with import's own flags, and refuses anything else, a shell or `runner exec` included, without running it. A host that's taken over can drive every host's VMs through MCP, and with `import --creds` make a VM of its own that gets any credential another host has, so that host's tokens too, but it can't log in to another host or run anything else there. Your laptop's key is still the only one that can. `runner hosts rm` takes a host's line out of the others, and theirs out of it. A line `hosts add` didn't write, like the laptop's, is never touched.

### Tailnet only

The host takes SSH only over the tailnet. Never forward port 22 to it from a router or expose it on a public IP. If sshd does listen elsewhere, `from=` still refuses the runner user's key from outside the tailnet.

This needs the host's own sshd, not Tailscale SSH: with Tailscale SSH on, tailscaled answers port 22 on the tailnet address and never reads `authorized_keys`.

### On AWS

`client/aws.sh`, run on the laptop, makes a host on EC2 with the AWS CLI and sets it up: an `m7i-flex.large` with nested virtualization on, which gives it `/dev/kvm` at a regular instance's price (a `.metal` costs dollars an hour), running Ubuntu 24.04, with a 50 GiB second disk for `/var/lib/runner`. From this checkout, which it copies there (the repo is private, so the host can't clone it):

```sh
brew install awscli
aws configure set region us-west-2
aws login
client/aws.sh up                                # kvm-host; most of the time goes on building base
RUNNER_AWS_NAME=kvm-host-2 client/aws.sh up     # another host, with kvm-host's tokens
```

If `aws login` refuses because the default profile already holds access keys, `aws login --profile runner` and `export AWS_PROFILE=runner` keep the two apart. If `up` says the CLI doesn't know `NestedVirtualization`, `brew upgrade awscli`. It needs `~/.ssh/id_ed25519.pub` (`ssh-keygen -t ed25519`), the key the laptop's `runner` logs in with, and Tailscale connected on the laptop.

`up` launches the host if it isn't there, waits until it has booted, and lets in only this laptop's address over SSH, as `ubuntu` with the key it saves in `~/.ssh/<name>.pem`. Then it sets the host up over SSH, a `==>` line for each step:

1. XFS on the 50 GiB disk, mounted at `/var/lib/runner`. It finds the disk by its size, since EC2 doesn't name NVMe disks in a fixed order, and never formats one that holds anything but XFS.
2. This checkout, by rsync; Rust, the build tools and podman; `cargo build --release && sudo host/install.sh`, with `RUNNER_HOST_IDLE=1h` on a host with no idle time yet, so it powers itself off once idle for an hour (Day to day, step 1). A rerun keeps the one it has.
3. The base image and kernel, built there with podman (docker drops the VMs' forwarded traffic), imported as `base` and installed.
4. Tailscale, as the host's name. **It stops for the login link it prints: open it and add the host.** A tailnet that already has a machine of that name calls this one `kvm-host-2-1`, say, and the setup stops there: remove the old one in the admin console, rename this one, and rerun.
5. The laptop's key line in `~runner/.ssh/authorized_keys`, as Add the laptop's key writes it.
6. The credentials. The Claude token comes from the host `RUNNER_AWS_TOKEN_FROM` names (`kvm-host`), host to host through one pipe, with `~/.ssh/<that name>.pem`, so it never lands in an argument, a variable, a file or on the screen on the laptop; `GH_TOKEN` and `ANTHROPIC_API_KEY` come the same way when that host has them. With no such host, or for `kvm-host` itself, **it asks for the token**: paste it (`claude setup-token` makes one), then Enter. It's read on the host with echo off. An empty one writes nothing and stops the setup.
7. `runner doctor` as `runner@<name>`, over the tailnet, as the laptop's `runner` will reach it, and only once every line is `ok`, `close`: from then on nothing gets in from the internet. A line that isn't leaves SSH open and says which.

Each step checks first, so after a failure, fix what it says and rerun `client/aws.sh setup` (with the same `RUNNER_AWS_NAME`): it redoes nothing that's done, and `up` on a host that's there does the same. Until `close`, each run first lets in this laptop's address as it is now, as `open` does; after it, the run goes over the tailnet. The name is the host's tailnet name too, so lowercase letters, digits and dashes. It ends with the next step on the laptop, `runner hosts add runner@<name>`; the first host goes in with `client/install.sh runner@kvm-host` (Use it from the laptop). The same steps by hand, for a host elsewhere, are in docs/host.md (A host aws.sh didn't make).

Once closed, `client/aws.sh ssh` goes over the tailnet, as `kvm-host`, and `client/check.sh` upgrades and checks the host (After merging a change). After the laptop's address changes, `client/aws.sh open` lets in the new one and no other, until the next `close`. `client/aws.sh stop` stops the host, after which only its disks cost, `start` brings it back at a new public address, and `ip` prints it. `state` prints what AWS has it as, `none` for a host up didn't make, as `runner` asks it (Day to day, step 1). `up` launches the host to stop, not terminate, when it powers itself off, which is EC2's default, and `start` sets it back on a host set to terminate, since a terminate loses the host and its VMs. `client/install.sh` puts a copy of aws.sh in `~/.local/bin`; `up` and `setup` copy the checkout they're in, so they run only from one.

With several hosts, `stop` and `start` take every host in the laptop's `~/.config/runner/hosts` (Several hosts), by its name: `runner@kvm-host-2` is the instance `kvm-host-2`. They ask AWS for all of them in one call and wait for them side by side. A listed host with no instance of that name launched with the key pair of that name, as `up` launches them, is one aws.sh didn't make: it's named and left alone, and `runner` never starts it. `RUNNER_AWS_NAME=kvm-host-2 client/aws.sh stop` stops that one alone, and every other command (`up`, `setup`, `ip`, `ssh`, `open`, `close`) acts on one host, `RUNNER_AWS_NAME`'s or `kvm-host`.

## Build the base image

`base` is the VM every other VM forks from: Ubuntu 24.04 booting systemd, with `runner-guest` as a service, git, curl, build-essential, tmux, Node LTS, Python, Rust (rustup), Claude Code, and Docker Engine with Compose v2. You work as `agent`, with passwordless sudo, in `/home/agent`. The image holds no credentials: each boot hands the VM the host's Claude Code token, in memory only, so `claude` there is logged in (docs/host.md, step 6). `runner creds <vm>` says which credentials a VM gets.

On a Linux machine with docker or podman and e2fsprogs:

```sh
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl --bin runner-guest
sudo images/base/build.sh
```

It writes `images/base/out/base.ext4`, a sparse 20 GiB ext4 disk, and `images/base/out/vmlinux`, Firecracker's pinned CI guest kernel. It runs as root so the disk keeps every file's owner. Before it replaces an older disk, it checks the new one with `images/base/check.sh` (runner-guest is enabled, Claude Code's updater is off, docker is enabled with `agent` in its group, iptables and ip6tables are the legacy variants, dockerd's raw-table drop-in is there, `/.runner/llm.txt` and `/.runner/build` are there, and nothing is under `/home/agent/.claude` or `/root/.claude`) and runs `claude --version` and the docker clients in the image. `/.runner/llm.txt` (`images/base/llm.txt`) tells the agent where it is: where to work, what persists, how to checkpoint and restore, how its credentials arrive, and what limits Docker.

Docker is Ubuntu's (`docker.io`, `docker-compose-v2`, `docker-buildx`), so it updates when base is rebuilt. `agent` is in the docker group, so it needs no sudo, and dockerd and containerd start at boot, so a container with a restart policy comes back after a reboot. `runner start` still returns once runner-guest is up, without waiting on dockerd; a `docker` command that comes first waits on docker.socket until dockerd is up. Containers log with the `local` driver, which rotates (`images/base/docker-daemon.json`). The pinned kernel has no nf_tables, so iptables and ip6tables are their legacy variants, and no iptables raw table, so a drop-in (`images/base/docker-no-raw-table.conf`) sets `DOCKER_INSECURE_NO_IPTABLES_RAW=1` for dockerd. That drops Docker's guard against other machines on the link reaching a published port directly; a VM's link has one other end, the host. Images, containers and volumes are in the VM's `/var/lib/docker`, so checkpoints, forks, export and sleep carry them, and `rebase`, which keeps `/home` but not `/var`, doesn't. The VM's disk and memory are what limit them, and an image pulled in one VM isn't in another. dockerd stops each container at shutdown, which counts toward `runner stop`'s 30 seconds before it kills the VM and drops what the boot wrote, so a container's stop timeout (Compose's `stop_grace_period`, 10 seconds by default) should stay well under that. What dockerd and containerd write on their own, their state directories and containerd's empty `/etc/cni` and `/opt/containerd` at a VM's first boot and four bolt databases at every boot, is on the list a run's result leaves out (`src/changes.rs`, `BOOT_ONLY`); what the agent's images, containers and volumes change there is one path for each directory, with a count (Headless runs). A VM with a list of names needs Docker Hub's on it (Names a VM can reach).

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

To change versions, edit the `ARG`s at the top of `images/base/Dockerfile` (Node major, Claude Code version) and the kernel pin at the top of `build.sh`. Claude Code doesn't update itself in a VM: `images/base/managed-settings.json`, which the image installs as `/etc/claude-code/managed-settings.json`, sets `DISABLE_AUTOUPDATER`, so every VM made from a `base` runs the version it was built with, and a newer one comes from rebuilding `base`. A VM forked from an older base keeps the Claude Code and runner-guest it came with, and one from before the updater was off keeps updating itself, until `runner rebase` gives it the current base's (Rebase onto a new base).

## Use it from the laptop

On the laptop, `runner` is a short sh script that runs each command on the host over SSH, all but `proxy` (Ports): `runner shell dev` is `ssh -t runner@kvm-host runner shell dev`. Nothing else goes on the laptop.

```sh
git clone https://github.com/nsnguyen/ai-agent-runner && cd ai-agent-runner
client/install.sh runner@kvm-host
```

It installs `runner` to `~/.local/bin`, with `aws.sh` beside it to start a stopped host (Day to day, step 1), and saves the host to `~/.config/runner/host`; `RUNNER_HOST` overrides that. More hosts go in `~/.config/runner/hosts` (Several hosts). The host is anything `ssh` takes, so an alias from `~/.ssh/config` works. Arguments reach the host as typed, spaces and quotes included, and the host's exit code comes back. It asks for a terminal only when stdin and stdout are both one (for `export`, when stdout is), so `runner list | grep dev` gets plain lines and `echo hi | runner exec dev -- cat` works. Rerun `client/install.sh` after a `git pull` to upgrade.

### Several hosts

```sh
runner hosts add runner@kvm-host-2      # checks it answers, lists it in ~/.config/runner/hosts, shares keys
runner hosts                            # each host: its VMs, how many are up, its free memory
runner list                             # every host's VMs, each line led by its host
runner create dev2                      # on the host with the most free memory
runner create dev2 --host kvm-host-2    # or on that one
runner exec dev2 -- hostname            # wherever dev2 is
runner hosts rm kvm-host-2              # out of the list, and its key out of the others; its VMs stay on it
```

`~/.config/runner/hosts` lists one host a line, as ssh takes it; the first is the one `runner mcp` goes to, which reaches the rest (MCP clients), and `client/install.sh <host>` puts that host first. The first `hosts add` starts the list from `~/.config/runner/host`. Every `hosts add`, of a new host or one listed already, then puts every listed host's key on every other host (Keys between hosts): each host that answers gets a line for each other host whose key came, in place of any it had for that host or that key, and loses the line of any host that isn't listed. A host that doesn't answer is named and keeps what it had. One with no key, or the same key as another host, is named too: it still gets the others' lines, and they keep its old one, if any. Either way `hosts add` exits 1, and once it's fixed, `hosts add` again finishes the job. A key that another line of a host already has, like the laptop's, gets no peer line there. `hosts rm` takes the host off the list first, then its line out of every host that answers, and theirs out of it. List the hosts by their tailnet names, like `runner@kvm-host-2`, not `~/.ssh/config` aliases: those names are what the hosts are to reach each other by. `--host` takes a host as listed or by its name, `kvm-host-2`, sends any command there, and never reaches the host; a program's own, as in `runner exec dev vite --host`, does. `RUNNER_HOST` still means one host alone, and with one host every command goes there as it always has, with no host column.

A command naming a VM goes to its host, `proxy` included, since every host gives out the same addresses. The laptop keeps where each VM was in `~/.cache/runner/vms`, asks that host whether it still has the VM (`runner ip`, with no stdin, so nothing piped in is lost), and if it hasn't, or the VM isn't in the cache, asks every host for its list at once and goes to the one that has it. A VM no host has goes to the first, which says so. A run id (`output`, `result` and `cancel dev.1`, and `--run dev.1`) goes to the host of the VM before its last dot. Names are one per VM across hosts: `create` and `import` refuse one any host has. `base` is the exception, on every host, and a command naming it goes to the first host unless `--host` says. A fork of a VM other than base goes to that VM's host, `import` places as `create` does, and `runner claude dev` making a new dev puts it where `create` would. `move dev kvm-host-2` goes to dev's host with kvm-host-2 as listed, refuses a host that isn't, and leaves the cache with dev on kvm-host-2 (Back up and move). `gc` with no VM and `doctor` run on every host, each line led by its host; other commands that name no VM go to the first. A command that goes to one host starts it if it's stopped (Day to day, step 1), and `move` starts its target first too; one that asks every host, like `list`, `hosts`, `doctor`, or `create` choosing where, starts none and names each stopped one: `runner: kvm-host-2 is stopped`. `runner list --host kvm-host-2` starts that one.

Asking every host is ssh with BatchMode, no stdin, a 5-second ConnectTimeout and keepalives, all at once. A host that doesn't answer is named on stderr: `list`, `hosts`, `doctor` and `gc` show the rest and exit 1, and `create` places on the rest but still refuses a name that host had when last listed. BatchMode can't ask to accept a new host's key, so add a host with `runner hosts add`, which can. Free memory is MemAvailable, which counts a running VM's memory only as far as its guest has used it, so a host of idle VMs looks freer than it is once they all get busy. With several hosts, each command naming a VM takes one more SSH connection, for the check; `ControlMaster auto` and `ControlPersist 10m` for the hosts in `~/.ssh/config` make that quick.

### Claude Code in the cloud

```sh
runner claude dev
```

lands you in Claude Code running in dev, logged in with the host's token (docs/host.md, step 6). If dev doesn't exist it's made from `base` first (`--from` picks another VM to fork), and if it's stopped it's started. Claude Code runs in dev's tmux session named `claude`, started in `/home/agent/work` (`--dir` picks another, which has to exist). Detach with `Ctrl-b d`, or just close the laptop: Claude Code keeps working. `runner claude dev` again reattaches to the same one; a VM never runs two. Anything after `--` goes to `claude` when it starts, as in `runner claude dev -- --continue`; with Claude Code already running, it's left out and `runner` says so. The session is an ordinary one, so `runner sessions dev`, `attach` and `kill` work on it too, and the name `claude` is taken: a session you name `claude` yourself is the one `runner claude` attaches to. If `claude` exits as soon as it starts, say on a flag it doesn't know, its session closes with it and you see only `[exited]`; `runner exec dev --tty -- claude <args>` shows why. There's no MCP tool for this, since it's interactive.

`runner claude --run dev.1` takes a headless run that's going (Headless runs, below) over on this terminal. It stops the run's `claude -p` as `runner cancel` does, SIGTERM and then SIGKILL 10 seconds on, but rolls nothing back and leaves dev up. Then it opens `claude --resume <session>` in dev's `claude` session, in the run's `--dir`, as the run's user, so you see what the run did and pick up from there. From then on it's an ordinary `runner claude` session: detach, and `runner claude dev` reattaches. Anything after `--` goes to `claude` after the session; claude asks before its tools as `runner claude`'s does, and `-- --dangerously-skip-permissions` skips that as the run did. It's checked before anything stops, and refused with the run going on, until `runner runs dev` shows the run's session, while dev has a `claude` session already (`runner kill dev claude` ends it), and without a terminal. A run whose Claude Code exits on its own first ends as it would have, and the takeover says so and opens nothing: run it again to pick that up. A run that's over is taken over from the disk it ended on (Headless runs, below). `--run` takes no VM, `--from` or `--dir`: the run has them.

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

prints the run's id, `dev.1`, at once, while Claude Code works on the prompt in dev as `claude -p`, with stream-json output and its permission prompts skipped: the VM is the sandbox. It runs in `/home/agent/work` unless `--dir` names another, which has to exist, and `--resume <session>` carries on an earlier run's session, as `runner runs dev` shows it: one that committed or joined, in the same `--dir`, since the session's record is on the VM's disk. The run doesn't need the connection that started it: close the laptop and it goes on.

`runner output dev.1` prints the stream so far, an event per line, and `--follow` keeps printing until the run ends, then says on stderr how it did. `--from <bytes>` skips what an earlier read printed. `runner runs dev` lists dev's runs, newest first: each one's id, when it started, how it stands, its Claude Code session, and its prompt. The session is the `session_id` of the stream's init event, once Claude Code has started and written it, and `-` until then, or if it isn't in the stream's first 64 KiB, or isn't a UUID, as Claude Code's are. `runner cancel dev.1` stops a run. A VM has one run at a time, so a second `runner run dev` is refused while one goes on.

If dev is stopped, the run boots it and shuts it down once Claude Code exits: exit 0 commits what it wrote to dev's disk, and anything else, a cancel included, rolls it back, keeping the run's disk in dev's `gens/` for a look until a later commit's gc. So does dev going down under the run, by `runner stop` or a reboot. If dev is running, the run checkpoints it first (`"before run dev.1"`), then joins it as it is, and never stops or starts it: however the run ends, dev runs on as the run left it, with nothing committed, and its tmux sessions, an open `runner claude dev` included, carry on. A run that fails or is cancelled once Claude Code has started isn't rolled back: `runner runs dev` and `--follow` say `not rolled back, checkpoint 2 undoes it`, and `runner restore dev 2` rolls it back by hand, which restarts dev. The checkpoint stays either way, listed as kind `run`, so that undoes a run that exited 0 too, until gc drops it as one of the oldest (Checkpoints, below).

A run taken over by `runner claude --run dev.1` (Claude Code in the cloud, above) ends `taken over`, with no `outcome`: dev runs on, on the run's disk, so its result's `changed` says it wasn't measured. One that booted dev takes back its ask to roll back, so dev's next clean shutdown, a `runner stop dev` say, commits what the run wrote and what the session wrote since, together, as any boot does, and `undo` goes back to before the run. One that joined dev keeps its checkpoint: `runner runs dev` says `taken over, checkpoint 2 undoes it`.

`runner claude --run dev.1` takes over a run that's over too, so one that failed, was cancelled or stopped short is picked up where it was. One that committed or joined dev left its session on dev's own disk: dev is started if it's stopped, and the session resumes as for a run going, refused while dev has a `claude` session. One that rolled back left it on the run's own disk, kept in dev's `gens/` until a later commit's gc: the takeover puts a copy of that on top of dev's history, as `restore` does, listed in `runner checkpoints dev` as `"takeover of dev.1"`, and boots dev onto it. It says which generation was current before, so `runner restore dev <that one>` brings the old disk back. That's refused while a run is going, while dev is up, since putting a disk back needs dev stopped, which would end its sessions (`runner stop dev` first), and once gc has dropped the disk. A run that's lost, or ended before Claude Code named its session, has no session to resume. Before claude starts, the runner looks for the session's record, `~/.claude/projects/*/<session>.jsonl` of the run's user, on dev's disk: a restore, undo or rollback since can have taken it, and then the runner says so in place of claude's own "No conversation found". From then on the run counts as taken over, as one taken over while it went does: its session is on dev's own disk, so taking it over again puts nothing back, and while dev has a `claude` session, the takeover points at `runner claude dev`, which reattaches.

`runner result dev.1` prints, once the run is over, what the runner measured: the exit code, how long it took, whether the disk generation it booted was committed or rolled back, and the paths that changed on that disk. The runner reads those from the disks itself, the generation the run booted against the one it booted from, and never asks the agent. Each directory added or deleted whole is one path with a count of what's under it, and past 200 paths the rest are only counted. Docker's and containerd's state, `/var/lib/docker` and `/var/lib/containerd`, where a pulled image or a written volume changes thousands of paths, is one path each, with `changed_under` counting what changed under it. Paths only the VM's boot writes, like its journal and systemd's state, are left out and counted in `boot_only`. What the agent says it did is kept apart, in `agent_says`, marked untrusted: check it against `changed`.

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
      {"path": "/home/agent/work/target", "change": "added", "contains": 812},
      {"path": "/var/lib/docker", "change": "modified", "changed_under": 4210}
    ],
    "more": 0,
    "boot_only": 7
  },
  "agent_says": {"untrusted": true, "text": "Fixed the bug.", "truncated": false}
}
```

The run reads both disks whole, so it goes on looking `running` for a while after Claude Code exits: about 45 seconds per 4.6 GB of disk (#61), and never more than 10 minutes. `runner cancel` then stops the measuring, and `changed` says so. A cancelled run is measured too: only a cancel after Claude Code is done stops the measuring. The paths left out as the boot's are a fixed list, `BOOT_ONLY` in `src/changes.rs`, found by booting a copy of the base image's systemd in a container and measuring its disk (#107): the journal under `/var/log/journal`, `/var/log/wtmp`, systemd's random seed and timer stamps, what the first boot writes, like `/etc/machine-id`, and what the daily timers a boot catches up on write, like `/var/backups`. So are the bolt databases dockerd and containerd rewrite at every start (#170), and, at a VM's first boot, what their first start makes: empty directories and a few small files like `/var/lib/docker/engine-id`. If that's all there is in them, each directory is one path left out; what the agent's first run put beside it is summed as `added`. containerd's first start also makes `/etc/cni` and `/opt/containerd`, holding only empty directories; each is one path left out, and listed whole once the agent puts a file in it. Every boot empties `/tmp`, so what the last run left there, deleted, is left out too; what this run adds there is listed. Nothing under `/home` is ever left out. A boot of another image may write other paths, which are listed. Paths past 32 KiB of them are only counted, since the agent names them. A run that joined a running VM has no disk of its own to measure, so its `changed` says why instead of listing paths, with null generations, its `checkpoint`, and an `outcome` that's `not_rolled_back` if it failed or was cancelled once Claude Code had started, and null otherwise. One that never got as far as starting its VM has nulls too. With `--keep 0`, a commit's gc deletes the generation the run booted from before it's measured, so `changed` can only say it's gone. A lost run has no result.

The stream is kept on the host, in `/var/lib/runner/vms/dev/runs/1/`, beside Claude Code's stderr, so it can be read with dev off, and goes when dev is deleted.

#### Best of N

```sh
runner run dev --n 3 "Make the flaky test in app/ pass"
```

forks dev into `dev-1`, `dev-2` and `dev-3`, starts the prompt in each as an ordinary run, and prints `dev-1.1`, `dev-2.1` and `dev-3.1` at once, so three agents try it side by side and you keep the best. Each try is a whole run, so best of 3 costs three times the tokens. `output`, `result`, `cancel` and `runs` work on each try's run as on any, and `--dir` and `--resume` reach every one. Each fork is new and stopped, so its run boots it and commits only a clean exit, and its `result` measures what that try changed against dev's disk. dev itself isn't touched and has no run: a stopped dev's tries fork its current disk, and a running or sleeping one is checkpointed first, `"before best of 3"`, kind `run`, and the tries fork that, so they get what dev has now, not its last commit, while dev and its sessions carry on. The tries get the default credentials, `claude` only, as every fork does, so three agents can't all push with dev's `github`; otherwise they take dev's sizes, kernel, idle time and URL setting.

N is 2 to 4, and all N start at once, with no queue. Nothing is made, and the error says why:

- if any of the names `dev-1` to `dev-N` is taken, on any host;
- if N times dev's memory is more than the host's free memory (MemAvailable) less 1 GiB. The error names the largest N that fits: 3 on the 8 GiB host with a 2 GiB dev stopped. vCPUs aren't counted, since the agents mostly wait on the model;
- while dev has a run going;
- while an earlier batch's tries are still VMs, since it's one batch at a time. `runner keep` (below) deletes them.

The tries are made on dev's host. A try whose run can't start is deleted, with the ones after it, and the runs that started go on. dev records its batch in its directory, in `tries.json`: the prompt, when, the generation the tries forked, and their names and runs. On ext4 each try is a full copy of dev's disk, so put `/var/lib/runner` on XFS or btrfs. Over MCP, `run_best_of` takes `name`, `prompt`, `n`, `dir` and `resume`, and returns the N runs.

`runner tries dev` shows the tries side by side, a row each, so you can pick one without reading three streams:

```
RUN      STATE                  EXIT  TOOK   CHANGED  AGENT SAYS (UNTRUSTED)
dev-1.1  exited 0, committed    0     2m14s  2        "Fixed the flaky test."
dev-2.1  exited 1, rolled back  1     1m03s  1        "I couldn't make it pass: the fixture is missing."
dev-3.1  running                -     2h00m  -        -
```

STATE is how the run stands, as `runs` says it. TOOK is the result's duration, or how long so far for a run still going. CHANGED counts the paths the result lists and counts past those, less the ones only the VM's boot writes: `runner result dev-1.1` lists them. AGENT SAYS is the first line of the agent's last message that has text on it, quoted, so control characters show escaped, and cut at 60 characters. It's the agent's own word, so check it against CHANGED. `--json` prints the batch's prompt, start and generation, and each try's run with its whole result, the `changed` paths and all of `agent_says` included. `--wait` waits until every try's run has ended, then prints. A try deleted or moved since shows as `gone`, and the rest still show; with no batch, `tries` says `no tries yet`. Over MCP, `list_tries` takes `name` and returns each try's run, and its result once that's over, at once: wait on a run with `run_output`, as for any.

```sh
runner keep dev-2      # dev gets dev-2's disk; dev-1 to dev-3 are deleted
runner keep dev --none # or keep none: the tries are deleted, dev is as it was
```

`keep dev-2` puts dev-2's current disk on top of dev's history as a new generation, as `restore` puts a checkpoint's copy, so `runner undo dev` goes back to dev from before the keep, and deletes every try of the batch, dev-2 too. dev keeps its name, address, URL, credentials, settings and history. It prints the Claude Code session of dev-2's newest run that committed, joined it running, or was taken over (`runner claude --run`), whose conversation is on the disk kept, so `runner run dev --resume <session> "..."` carries that conversation on in dev; a later run that rolled back took its session with its disk, so it isn't named, unless `runner claude --run` took it over since and put that disk back, which then names it over any run before the takeover, since none of their work is on that disk:

```
kept dev-2 on dev: generation 7 is current, and `runner undo dev` goes back
cancelled dev-3.1
deleted dev-1, dev-2 and dev-3
session 0b5c1e8e-6f1d-4f43-9a7e-2d3c4b5a6978: `runner run dev --resume 0b5c1e8e-6f1d-4f43-9a7e-2d3c4b5a6978 <prompt>` carries on dev-2.1's conversation
```

A try still at the disk it forked, because its run rolled back or never got going, has nothing of its own to keep, so it's refused, and so is one whose run is still going; any other is kept from its current disk, whatever later runs in it did. A try that's up, say one you started by hand, is refused until you `runner stop` it, whether it's the one kept or one to delete. Tries whose runs booted them and are still going are cancelled first, all at once, then deleted with the rest, and dev's record of the batch goes with them, so `runner tries dev` says `no tries yet` and a VM made later under a try's name isn't taken for one. A running or sleeping dev is stopped, which commits its disk, put on the kept disk and started again, as `restore` does, so what dev wrote since the batch forked is under the keep, where `undo` finds it. keep is refused while dev has a run going. Everything is checked before anything changes. Over MCP, `keep_try` takes a try's `name`, or dev's `name` and `none: true`, and returns the kept try, its run and `session`, dev's `generation`, and the runs cancelled and tries deleted.

### Sizes

```sh
runner create big --cpus 4 --mem 8G --disk 40G   # a fork with sizes of its own, its disk grown as it's made
runner stop dev
runner resize dev --disk 40G --mem 4G --cpus 4   # any of the three, from dev's next boot
runner status dev                                # dev  gen 7  42949672960 bytes  cpus 4  mem 4G  stopped
runner undo dev                                  # the disk as it was before the resize
```

A VM gets 2 vCPUs and 2 GiB of memory unless `create --cpus --mem` says otherwise, and the disk of the VM it forks: 20 GiB from a base built as it comes. `list` and `status` show each VM's disk in bytes, its vCPUs and its memory. `resize` changes a stopped VM's. Memory and vCPUs go in its config, as `create`'s do, and count from its next boot. The disk only grows, since shrinking means moving the guest's data: the bigger one is a new generation, noted `resize to 40G` in `runner checkpoints`, so `undo` takes it back; memory and vCPUs are settings, not generations, so `undo` leaves them and `resize` sets them back. It's a copy of the current disk, a reflink on XFS or btrfs, lengthened with a hole so it stays sparse, then `e2fsck -f -p` and `resize2fs` on the copy, never on the disk dev had. Its files are as they were, and the guest's `df -h /` shows the new size at its next boot, and `runner rebase` keeps it. A step that fails drops the copy, and dev stays as it was. Only writing memory and vCPUs comes after the disk, so if that fails, say on a full host disk, the bigger disk is current already: `resize` again without `--disk` sets them. It's refused, with nothing changed, while dev is running, booting, stopping or sleeping (`runner stop dev` first: a sleeping VM's snapshot is tied to its disk and memory), for a disk no bigger than the one it has, and for a size `--mem` wouldn't take: MiB, or a whole number with M, G or T. `create --disk` forks and grows in one step, its source untouched; the source's own size makes a plain fork, and a smaller one is refused. MCP has `resize_vm`, and `create_vm` takes `disk`. `host/install.sh` installs e2fsprogs, and `runner doctor` warns when it's missing.

`base` can be resized too: forks made after that get the bigger disk, and those made before keep theirs. A resized base isn't a disk an import made, though, so `runner rebase` refuses it, and so does `host/check.sh`'s rebase test, until base is imported again. The resize keeps base's fingerprint, so `host/check.sh` won't import it on its own: `runner undo base`, `FORCE_BASE=1 host/check.sh`, or `runner import --replace base` by hand brings it back. For a base that stays bigger, build it bigger instead: `sudo SIZE=40G images/base/build.sh` (Build the base image), then import it.

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

### Rebase onto a new base

```sh
runner stop dev
runner rebase dev    # dev on the current base's system, with its own files
runner undo dev      # dev's disk from before
```

A VM keeps the system it was forked with: a rebuilt base's runner-guest, Claude Code and packages reach only VMs made from it. `rebase` gives a stopped dev a new disk generation, a copy of base's current disk with dev's `/home`, `/root`, `/etc/machine-id` and services (their units and the links that start them) copied in. Claude Code's install under `/home/agent`, `~/.local/bin/claude` and `~/.local/share/claude`, is base's, which pins its version; the rest of `/home` is dev's, dotfiles and rustup's toolchains included. A dev whose `~/.local/share` or `~/.local/bin` is a link, say a dotfile manager's, keeps its own Claude Code, and the output says so. `/etc/hostname` is base's, which each boot's `ip=` fills in. Everything else dev changed is gone, and the output says so: apt packages and anything else installed outside those paths, users added with `useradd` (their files keep their numeric owners), crontabs and `/var`. So is a file the copy can't take, like one whose name holds a newline or a double quote, or a socket, and the output names each. Hard links come out as separate copies, so rustup's proxies in `~/.cargo/bin` take a full copy each, and extended attributes, file capabilities among them, aren't copied. dev's memory, vCPUs, kernel, credentials, idle time and URL stay its own, and its disk's size unless base's is bigger.

```
dev is on base's generation 4 now: generation 13 is current.
Kept /home, /root, /etc/machine-id and 2 services (api, web).
Anything installed outside those, like apt packages, is gone, and so are users added with useradd, crontabs and /var: runner undo dev brings it back.
```

It's refused while dev is up or a run on it is going, as `undo` is, and when base's current disk didn't come from `runner import`: a base that booted has a disk a guest wrote, which the copy's debugfs must never open, so import base again first. The copy reads dev's disk without mounting it, as a run's changes are measured, and writes the new one with e2fsprogs' debugfs (`src/graft.rs`), so it takes about as long as reading and writing dev's `/home`: minutes for a big one, and each of its two copies gives up past half an hour. The new disk is dev's size, or base's if that's bigger, so a dev grown with `runner resize` stays that big. A `/home` that doesn't fit on it beside base's system fails and leaves dev as it was. Over MCP: `rebase_vm`.

### Sleep and wake

```sh
runner sleep dev    # dev's memory goes to disk, and its Firecracker stops
runner wake dev     # back where it was, in about a second
runner idle dev 30  # sleep on its own after 30 minutes unused: 10 until set, or 90s, 2h, never
```

You rarely need the first two. A VM sleeps on its own once nothing has used it for its idle time, and anything that needs it wakes it first. Use is a shell, exec, `attach`, `claude`, `sessions` or `kill` connected to it, a headless run going, an MCP `exec` or session call, an open `runner proxy` (Ports), or a terminal on one of its tmux sessions; a detached session doesn't count, since it sleeps with the rest of memory and is there on waking. Neither does traffic to its address, a service's included; a connection through its URL does, and wakes it (A URL per VM, below). Its boot looks every 10 seconds (as often as the idle time, if that's shorter), so the sleep comes up to 10 seconds late. `create --idle` sets it for a new VM, and a fork keeps its source's. `runner logs` says when it went unused.

A sleeping VM takes no CPU or RAM on the host: its memory and devices are in a Firecracker snapshot in its directory, about the size of its memory, while its boot waits to wake it. `wake` loads the snapshot into a fresh Firecracker, so every process, tmux session and Claude Code carries on where it was, and sets the guest's clock, which stood still while it slept. `start`, `exec`, `shell`, `claude`, `sessions`, `attach`, `kill`, `checkpoint`, `export` and `run` wake a sleeping VM first, as do the MCP tools that need its guest. Shells and execs open at the sleep end, since Firecracker closes vsock connections across a snapshot; tmux sessions live on. `sleep` is refused while a run is going, and until the guest's runner-guest is up. `stop` wakes it to shut down cleanly, which commits its disk as any stop does; `restore` stops it the same way and starts it again awake. `undo` and `gc dev` refuse a sleeping VM, as a running one, and a `gc` of every VM skips it, says so, and still exits 0. A host shutdown, by `client/aws.sh stop`, the host's own idle stop (Day to day, step 1) or a reboot, parks each sleeping VM: `runner-park.service`, which `host/install.sh` installs, runs `runner park --all` before the VMs' units stop, and each one's boot leaves its snapshot and the disk it runs on, uncommitted, and ends. Its next `start`, or a command that wakes it, boots it onto that disk and loads the snapshot, so its processes and tmux sessions carry on where it slept, and sets its clock; its URL answers once it's up again. Meanwhile it's `sleeping`, and `undo`, `gc` and `delete` refuse it; `stop` and `restore` resume it to shut it down cleanly. A running VM still shuts down cleanly at a host shutdown. A parked VM whose snapshot won't load, after a Firecracker upgrade say, boots its last committed disk instead, keeps the disk it was parked on in its `gens/` for a look, and `runner logs` says why; if Firecracker doesn't come up at all, the start fails and it stays parked. Otherwise a snapshot belongs to its boot, so a host crash or a killed boot loses a sleeping VM's memory, and the next boot starts from its last committed disk. The snapshot is the guest's memory, credentials included, so only the runner user can read it, and it goes at the wake or the resume (or at the next boot or a delete, after a killed boot). Over MCP: `sleep_vm` and `wake_vm`, `vm_status` says `sleeping`, and `create_vm` takes `idle`.

### A URL per VM

```sh
runner url dev              # http://100.101.102.103:10001: dev's port 8080, from anywhere on the tailnet
runner url dev --port 3000  # to port 3000 in dev instead
runner url dev off          # and `on` again
```

Each VM gets a URL on the tailnet, like a Sprite's but private: open it on the laptop, or anything else on the tailnet, and the VM's app answers. The address is the host's Tailscale one, and the port is 10000 plus the VM's slot, so it stays the same for as long as the VM exists (a deleted VM's goes to the next one made). It goes to port 8080 in the VM until `--port` says otherwise, as plain TCP, so anything served there works, not only HTTP: `runner service add dev web -- python3 -m http.server 8080` is enough to try it.

The VM's boot holds the URL, so it answers while the VM is up. A connection to a sleeping VM wakes it and waits up to 30 seconds for the guest to take it; one to a stopped VM is refused, as nothing starts it. An open connection counts as use, so the VM doesn't idle-sleep under it; one from a client that went silent, like a laptop that slept, is dropped within a minute and a half. It's tailnet only: it listens only on the host's Tailscale address, never 0.0.0.0, and takes connections only from Tailscale's 100.64.0.0/10, so neither the LAN nor another VM gets in through it. Without Tailscale up there's no URL and `runner url` says so; a running VM's boot binds it once Tailscale is up, and takes `on`, `off` and `--port` within a second. A fork keeps its source's setting, on a port of its own. After an upgrade to this runner, a VM that's already up gets its URL at its next start, since its boot is the old one. MCP's `vm_status` gives the URL as `url`. For HTTPS and a name in place of the address, put `tailscale serve` in front (docs/host.md).

### Names a VM can reach

```sh
runner net dev allow github.com registry.npmjs.org  # dev reaches only these, and api.anthropic.com
runner net dev deny registry.npmjs.org              # one off again
runner net dev list                                 # the list, or that dev is open
runner net dev open                                 # no list: the whole internet again
```

A VM is open, reaching the whole internet, until `allow` gives it a list. Then its DNS answers only the names on it, each with every name under it (github.com covers api.github.com), and api.anthropic.com always, so Claude Code keeps working. Every other name gets REFUSED, and the host's rules let the VM reach only the addresses those answers gave (Set up a host, for how, and what a list doesn't close). A running VM takes a change within a second, with no reboot; one whose boot couldn't serve its DNS takes it at its next boot. Each prints the list as it now is:

```
dev  allows github.com registry.npmjs.org, and always api.anthropic.com
dev  open: the whole internet
```

`deny` keeps the list even when it takes the last name off, which leaves only api.anthropic.com; `open` drops it. On a VM with no list, `deny` fails and says it's open: a list allows names, it doesn't block them. A name is a hostname of two labels or more, with no wildcard (none is needed) and no address; one bad name fails the whole command. Two commands at once each keep their change. A fork keeps its source's list, and an export, import or move carries it. Over MCP, `vm_allowlist` reads it, and no tool changes it: a local Claude Code reads VMs' output, so it's what an agent in one would talk into widening its list. In the VM, `/.runner/llm.txt` tells the agent that a REFUSED name is off your list for that VM, and to ask you instead of working around it. Docker Hub takes three names, which a VM with a list needs before `docker pull` or `docker compose up` works there: `runner net dev allow registry-1.docker.io auth.docker.io production.cloudfront.docker.com`.

### Back up and move

```sh
runner export dev > dev.tar                               # dev as one stream, running or not; tar -xf reads it
runner export dev | aws s3 cp - s3://<bucket>/dev.tar     # straight off the host, through the laptop
aws s3 cp s3://<bucket>/dev.tar - | runner import dev -   # and back, on this host or another
runner export dev --no-checkpoints | zstd > dev.tar.zst   # only its current disk, compressed
zstd -dc dev.tar.zst | runner import dev2 -               # back under another name
runner move dev kvm-host-2                                # or to another host, host to host, same name
```

`export` writes a VM to stdout as one tar: `dev/manifest.json` first, then `dev/disk.ext4`, its current disk, and `dev/checkpoints/<id>.ext4` for each of your checkpoints (kind `checkpoint` in `runner checkpoints`), oldest first. The manifest has a format version (2 when dev has a list of names, else 1, which older runners read too), dev's config (sizes, kernel path, idle time, URL setting, list of names), its allowlist's names, and each checkpoint's id, time and comment. Each disk goes as a sparse file, only the extents the host's filesystem holds data in, so a 20 GiB disk with 4.6 GB on it streams about 4.6 GB, and `tar -xf` gives it back sparse. Each checkpoint streams its own data in full, blocks it shares with the current disk too, so every checkpoint adds about as much again. Space the guest freed that the host's file still holds goes as data. Services are units on the disk, so they come with it. Not in it: undo's older generations, runs' and the agent's checkpoints, `runs/`, dev's slot, and its sleep snapshot, which holds the guest's memory and credentials. There's no compression: `| zstd` on either end. For `aws s3 cp -`, a stream past 50 GB needs `--expected-size` with at least the stream's bytes: dev's bytes used in `runner list` plus 3%, for tar's headers, is more, and `set -o pipefail` first: aws uploads whatever it's given, so without it an export that failed partway still leaves an object and exits 0.

A running dev is copied as its checkpoint is: `sync` in the guest, a pause, a copy of the disk it runs, a resume, so dev carries on and the export is its disk as a power cut right after the sync would leave it. On XFS or btrfs the pause takes milliseconds; on ext4 it lasts a full copy of the disk, and the copy takes that much room on the host until the export ends. The copy is unlinked from dev's directory as soon as it's open, before a byte goes out, so no checkpoint or generation is left, even when the export is killed while it streams. A sleeping dev is woken for it, as for a checkpoint, and left awake. It's refused while a run is going, as restore and sleep are, since a run's disk may roll back; while dev boots or stops; and onto a terminal on stdout. From the look to the copy it holds dev's run lock, so a run, a sleep or a restore asked for then is refused, and so is a second export. Every disk is open before a byte goes out, so dev can start, stop, or be deleted while the stream goes on. From the laptop it's `ssh runner@kvm-host runner export dev` with no tty, so the bytes pass as they are, and the host needs no cloud credentials; onto a terminal it asks for one, so the host sees it and refuses. There's no MCP tool, since JSON-RPC can't carry a disk.

`import` takes an export back as a new VM, from a file on the host or from stdin (`-`), on the host it came from or another: its current disk, your checkpoints under the same ids, so `runner restore dev2 4` is the same checkpoint 4 it was, with their times and comments, and its sizes, kernel path, idle time, URL setting and list of names. Its services are on the disk. It gets a free slot here, so a new address and URL port; undo has nothing to go back to; and it gets the `claude` credential only, as a fork does. Import says when the export named more, and `runner creds dev2 claude github` gives them back, or `--creds claude,github` sets them on import. A kernel path this host hasn't got comes in as it was: import says the VM won't boot until that kernel is there, its settings still change, and a fork of it, being a create, needs `--kernel` naming one here. A name that's taken is refused before anything is read, and an export comes in only as a new VM, never with `--replace`.

Import reads the stream itself, never with tar: only the entries the manifest names, in the order export writes them, each a plain or sparse file, written under names the runner makes and sparse as they came, then tar's end. Anything else, a stream cut short, an entry the manifest doesn't name, a link, a `..` in a name, or a format it doesn't know, is refused, and the VM is built aside and renamed in only at the end, so a refused import leaves nothing and the name stays free. Nothing in the stream is checksummed, so a byte flipped in a disk's data isn't caught: S3 and `zstd` check what they carry. `import` tells an export from a raw disk (Build the base image, `runner import base`) by its first block: a tar header, or not. A compressed stream is refused, so `zstd -dc dev.tar.zst | runner import dev2 -`.

`move` takes dev to another host on the list, under the same name, with everything its export carries, and its allowlist too, since the move is yours and not a stream from elsewhere. On dev's host, the laptop's `runner move dev kvm-host-2` runs `runner move dev runner@kvm-host-2`, which streams dev's export into `ssh runner@kvm-host-2 runner import dev - --creds …`, with that host's own key over the tailnet (Keys between hosts), so the bytes go host to host, never through the laptop. It asks kvm-host-2 first, and refuses before anything stops if it has a dev or doesn't answer; it refuses dev's own host too, and a run going. A running or sleeping dev is shut down, cleanly, so its disk is committed, which ends its processes and sessions; it's started on kvm-host-2 once it's there, awake, and move prints its new URL if its URL is on. From the stop to the delete it holds dev's lock, so nothing starts it meanwhile, and it deletes dev here only once the import there has worked. A failed import, or a stream cut off on the way, leaves nothing there and dev here as it was, started again if it was up. When ssh fails only after the whole stream went, move asks kvm-host-2 whether it has dev now, and goes on if it has. If kvm-host-2 doesn't answer, dev stays here as it was, started again if it was up, and move says to check both hosts, since kvm-host-2 may have it too. If the delete here fails after the import worked, move exits 1, saying both hosts have it. A dev that doesn't shut down cleanly, killed after 30 seconds or crashed, has nothing committed since it booted, so it isn't moved: it stays here, stopped, with that boot's disk kept for a look until its next commit's gc. Once begun, a move goes to its end: Ctrl-C and a dropped connection don't stop it partway, and `move_vm` finishes even if the MCP session ends. Like an import, dev gets a slot of its own there, so a new address and URL port. Its runs stay behind, so `runner runs dev` starts empty there. The laptop sends every command naming dev to kvm-host-2 from then on. MCP's `move_vm` does the same, through the first host, from any host to any other.

### MCP clients

`runner mcp` is `ssh -T -o BatchMode=yes runner@kvm-host runner mcp`, to the first host (Several hosts): no terminal, so stdout carries only JSON-RPC, and no prompts, so the SSH key has to load without one (from the agent, or with no passphrase). Run `runner list` once from a terminal first, so ssh can ask you to accept the host's key. On a stopped host it starts the host first, aws.sh's lines on stderr only, which takes minutes: longer than a client waits for a server to start, so it may show `runner` failed; `/mcp` reconnects once the host is up.

With several hosts it's still that one session, and every other listed host goes along as a peer, with the name the first is listed by: `runner mcp --name runner@kvm-host --peer runner@kvm-host-2 …` (`--host` picks another host to go to; the rest are its peers). That host's runner opens one `ssh <peer> runner mcp` per peer when a call first needs it, with its own key only (`~runner/.ssh/id_ed25519`, Keys between hosts), BatchMode, and the peer's host key taken on first sight, and keeps it for every call after. `list_vms` gives each VM's `host`, and `list_hosts` every host's row, this one first, each host by its name as listed, without its `user@` (the kernel's name for the first host, if `--name` is left out). `create_vm` places a VM as `runner create` does: on `host` if given, a fork of a VM other than base on that VM's host, else on the host with the most free memory, and it refuses a name any host has. `move_vm` goes to the VM's host with `host` as listed, which that host's runner dials itself, and is refused there for the host it's on already. Every other call naming a VM, or a run (before its last dot), goes as it came to the host that has the VM, looked for here first, then on the peer it was last listed or made on, then on every peer, and comes back as that host answered; one that names no VM, like `doctor`, stays on the first host, and so does `base`, which is on every host. A host that doesn't answer within 20 seconds, or whose ssh fails, is named in `list_vms`' and `list_hosts`' `unreachable`, with what ssh said, while the rest answer, and a call to a VM that isn't on a host that answered fails saying which didn't: the one the VM was on when last listed or made there, if any. A session that ended, say when its host rebooted, is dialed again on the next call, and calls waiting on one dial take its answer. A peer's own MCP takes no `--peer`, so it forwards nothing. ssh's keepalives end a peer's session that stops answering in about 15 seconds, failing what it was running there; the call isn't sent again, since it may have run. A host rebuilt under the same name has a new host key, which ssh then refuses: on each other host, `sudo -u runner ssh-keygen -R <its name>`.

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

Nor do they get `AWS_PROFILE`, or the AWS CLI on PATH, which starting a stopped host takes: add `"env": {"AWS_PROFILE": "runner", "PATH": "/opt/homebrew/bin:/usr/bin:/bin"}` beside `args`. Restart the app after editing its config.

The `exec` tool runs a command in a VM, starting it first if it's stopped: `argv`, optional `stdin` text, `env` (variables as an object; `CLAUDE_CODE_OAUTH_TOKEN`, `ANTHROPIC_API_KEY` and `GH_TOKEN` are refused, since credentials reach a VM only through `runner creds`), and `timeout_secs` (120 unless given), after which the command is killed. It returns the exit code or signal, and stdout and stderr, each whole up to 32 KiB; past that, its first and last 16 KiB with a line between saying how many bytes were cut, and a truncated flag, so the error a long command prints last is kept. It runs as `agent`, or as `RUNNER_USER` if the host's environment sets one.

`run_agent`, `run_best_of`, `run_output`, `cancel_run`, `list_runs`, `list_tries` and `keep_try` do for an agent what `runner run`, `run --n`, `output`, `cancel`, `runs`, `tries` and `keep` do for you. `run_agent` returns the run at once, and `run_best_of` the N runs. `run_output` takes a `cursor`, 0 at first and then the one each reply gives, and returns the events past it, up to 64 KiB, with the run's state; while the run is quiet it waits up to 45 seconds for an event, so an agent can poll it in a loop without holding a connection for the whole run. Once the run is over, the reply that reaches the end of its stream has the run's `result` too, as `runner result` prints it. Each run carries its Claude Code session as `session` once `runner runs` would show it, which `run_agent`'s and `run_best_of`'s `resume` take.

`list_sessions`, `read_session` and `kill_session` do for an agent what `runner sessions`, `attach` and `kill` do for you. `read_session` returns the last `lines` (100 unless given, at most 2000) of a session's active pane as plain text, history included, cut at 32 KiB from the end.
