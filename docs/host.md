# Booting VMs on the host

How `start` and `stop` work, and how to set a host up by hand until the host install (#14) does it.

## How it works

- `runner start dev` is `systemctl start firecracker@dev`. The unit runs `runner boot dev`, which takes the VM's lock, copies its current disk into a new generation, starts Firecracker with `api.sock` in the VM's directory, and configures the kernel, that disk, the sizes and a vsock through the API. Then it starts the guest and tells systemd it's ready, so `start` returns once the guest has started.
- Before Firecracker, boot adds the VM's tap (`fc` and its /30 in hex), and removes it once Firecracker exits, before it lets go of the VM. A boot killed first leaves the tap, and the VM's next boot replaces it. The guest's address and gateway go in the kernel's `ip=` argument, with the gateway as its one DNS server; `runner ip dev` prints the address. Boot answers there, on port 53 over UDP and TCP, for as long as it's up, asleep included, and passes each query on to the host's resolvers (`src/dns.rs`). On the host, the rules let a VM reach port 53 only at its own boot, so it uses no other VM's, nor a resolver the host runs; port 53 off the host stays open to it. A VM with a list of names gets answers only for those names, and REFUSED for the rest. A boot that can't bind port 53, under a dnsmasq on every address say, says why in the journal and gives the guest the host's resolvers instead, unless the VM has a list: then it doesn't boot, and says why. Such a dnsmasq started while a VM is up can't bind port 53, so start it first. The unit gives boot CAP_NET_ADMIN for the tap and CAP_NET_BIND_SERVICE for port 53, and boot clears both before running Firecracker.
- The serial console is Firecracker's stdout, so it lands in the unit's journal: `runner logs dev`.
- `runner stop dev` is `systemctl stop`. The unit's ExecStop, `runner halt dev`, sends Ctrl-Alt-Del and waits for boot to let go of the lock. After 30 seconds, systemd kills the VM.
- A clean shutdown (Firecracker exits 0) commits the new generation. A kill, a crash or a guest panic doesn't: the previous generation stays current, and the new disk stays for a look.
- Inside the guest, shut down with `reboot`, not `poweroff`. On x86 Firecracker has no power-off, so after `poweroff` the guest halts but Firecracker keeps running: the VM looks running until a stop kills it, and the run isn't committed.
- When runner-guest starts, before it takes a command, it asks the host for the VM's credentials on vsock port 10023. Firecracker passes that to `vsock.sock_10023`, where boot hands over the ones the VM's allowlist names (docs/design.md, Credentials).
- `runner shell dev` and `runner exec dev` reach the guest's runner-guest through the VM's `vsock.sock`: `CONNECT 10022`, then the protocol in `src/proto.rs`. A stopped VM is started first, and the runner retries for up to a minute while the guest boots. Its SIGTERM or SIGHUP hangs the command up, as closing its terminal does.
- `delete` and `undo` refuse a running VM, because boot holds its lock. `checkpoint` pauses one for its copy, and `restore` stops and starts it, or waits out a stop under way.
- The agent's own calls, `runner-guest checkpoint`, `checkpoints` and `restore`, go to the host on vsock port 10024, which Firecracker passes to `vsock.sock_10024`, where boot serves them for that VM alone (`src/calls.rs`). Its checkpoint is made as `runner checkpoint` makes one, up to 10 an hour. Its restore is copied while the VM runs; then boot sends Ctrl-Alt-Del, commits the disk at the clean shutdown, puts the copy on top, and exits 75, which the unit's `RestartForceExitStatus` boots again. A unit installed before this boots nothing then, and the VM stays stopped on the restored disk: rerun `host/install.sh`.
- A headless run (`runner run`) on a stopped VM starts it with `systemctl start`, but shuts it down with Ctrl-Alt-Del straight to Firecracker, as halt does, so a clean exit commits. Once the guest is up, the run leaves a `rollback` note in the VM's directory, naming itself, and takes it back only when Claude Code exits 0; boot keeps the old generation current on seeing it, so a VM shut down under a run, by a reboot or `runner stop`, rolls back. Each run is driven by `runner drive`, in a session of its own, so it outlives the SSH connection that started it. A host whose logind kills a user's processes at logout (`KillUserProcesses=yes` in `/etc/systemd/logind.conf`; Debian and Ubuntu leave it off) kills runs too: add `runner` to `KillExcludeUsers` there.
- A start or wake that doesn't fit in the host's room is refused (docs/design.md, Guards): MemTotal, from `/proc/meminfo`, less the reserve in `/etc/runner/reserve` (`sudo RUNNER_HOST_RESERVE=2G host/install.sh` sets it; 1 GiB unless set), less each up VM's memory and 64 MiB. `runner doctor`'s `room` line shows both, and `runner hosts` the room.
- At a host shutdown, `runner-park.service` parks each sleeping VM first, so it sleeps on through it (README.md, Sleep and wake); then systemd stops every VM's unit, so a running one shuts down cleanly.
- VMs get 2 vCPUs and 2 GiB unless `runner create --cpus --mem` says otherwise, and the host's kernel at `<root>/kernel/vmlinux` unless `--kernel` names one. A fork keeps its parent's, and its disk's size unless `--disk` grows it. `runner resize` changes a stopped VM's (README.md, Sizes): it grows a disk with e2fsprogs' `e2fsck` and `resize2fs`, by full path in `/usr/sbin`, which `host/install.sh` installs and `runner doctor` checks for.

## Idle power-off

A host can power itself off once nothing has used it for its idle time, which on EC2 stops the instance (docs/design.md, The host stops itself when idle). `client/aws.sh setup` sets an hour on a host it made that has no idle time yet; `host/install.sh` alone leaves it never, so a host aws.sh didn't make stays up.

- `sudo RUNNER_HOST_IDLE=2h host/install.sh` sets it, in `runner idle`'s syntax (`90s`, `60m`, `2h`), or `never`. It's kept in `/etc/runner/host-idle`, so reruns keep it; install.sh writes that file only once one is set.
- With a time set, install.sh enables `runner-host-idle.timer`, which runs `runner host-idle` as runner every minute, and adds a polkit rule letting runner power the host off and nothing more: not past an inhibitor (`systemd-inhibit`), and not while anyone else is logged in, since logind then asks for another action. With `never`, it disables the timer and leaves the rule out.
- Use is a VM running, booting or stopping (a run, a shell, a proxy or a URL connection keeps one running), a login by any user but runner (your admin SSH, or `host/check.sh`), or a runner command at work, which holds `/var/lib/runner/busy.lock`. runner's own SSH sessions don't count unless a command is running in one. The idle time counts from the last use or the host's boot, whichever is later, so a host just started gets all of it.
- Past systemd's own lines as it starts each minute, the look writes nothing to its journal until it powers off, and then says why first: `journalctl -b -1 -u runner-host-idle` after the next boot. `runner doctor`'s `host-idle` line shows the setting, and warns if the timer isn't active, polkit won't let runner power off, or the busy lock won't open.

## A host aws.sh didn't make

`client/aws.sh setup` sets up a host it launched (README.md, On AWS). Elsewhere, these are its steps by hand, from the laptop's checkout, with `admin` as your sudo login on the host and `kvm-host` as its name. Tailscale comes first here, so that name resolves for the rest:

1. Tailscale, on the host, under the name the laptop will use. Remove an old machine of that name from the tailnet first, or this one becomes `kvm-host-1`.
   ```sh
   curl -fsSL https://tailscale.com/install.sh | sh && sudo tailscale up --hostname kvm-host
   ```
2. `/var/lib/runner` on XFS or btrfs. For a spare disk, found by its size, here 50 GiB. `blkid -p` prints nothing for an empty disk: never mkfs one it prints something for.
   ```sh
   disk=/dev/$(lsblk -dno NAME,SIZE | awk '$2 == "50G" {print $1}'); echo "$disk"
   sudo blkid -p "$disk"
   sudo mkfs.xfs "$disk"
   echo "UUID=$(sudo blkid -p -s UUID -o value "$disk") /var/lib/runner xfs defaults,nofail 0 2" | sudo tee -a /etc/fstab
   sudo mkdir -p /var/lib/runner && sudo mount /var/lib/runner
   ```
3. The checkout, since the repo is private, then Rust, `build-essential`, `podman`, and the install (README.md, Set up a host):
   ```sh
   rsync -az --exclude target --exclude images/base/out ./ admin@kvm-host:ai-agent-runner/   # on the laptop
   cd ai-agent-runner && cargo build --release && sudo host/install.sh                       # on the host
   ```
4. The base image and kernel, built with podman, not docker, which drops the VMs' forwarded traffic (README.md, Build the base image).
5. The laptop's key (README.md, Add the laptop's key).
6. The Claude token, as step 6 below says.
7. `ssh runner@kvm-host runner doctor` from the laptop, every line `ok`; then nothing else in over SSH (README.md, Tailnet only).

## Set up a host by hand

1. Firecracker at the version in `host/firecracker-version`, on PATH. `runner boot` refuses any other.
   ```
   v=$(cat host/firecracker-version); a=$(uname -m)
   curl -fL https://github.com/firecracker-microvm/firecracker/releases/download/$v/firecracker-$v-$a.tgz | tar xz
   sudo install release-$v-$a/firecracker-$v-$a /usr/local/bin/firecracker
   ```
2. A guest kernel at `/var/lib/runner/kernel/vmlinux`. It needs `CONFIG_SERIO_I8042` and `CONFIG_KEYBOARD_ATKBD`, or Ctrl-Alt-Del never reaches the guest and every stop ends in a kill. Firecracker's CI kernels have both; its [getting started](https://github.com/firecracker-microvm/firecracker/blob/main/docs/getting-started.md) fetches one, and an Ubuntu rootfs.
3. The units, as the user that owns `/var/lib/runner`. The second parks each sleeping VM at a host shutdown, so it sleeps on through it:
   ```
   runner unit --user runner | sudo tee /etc/systemd/system/firecracker@.service
   runner unit --park --user runner | sudo tee /etc/systemd/system/runner-park.service
   sudo systemctl daemon-reload
   sudo systemctl enable --now runner-park.service
   ```
4. That user in the `kvm` and `systemd-journal` groups, and allowed to start and stop `firecracker@*` units: a polkit rule, which #14 adds.
5. NAT for the VMs' range, with a filter that drops what they open to the host itself (`host/nat.nft`), and `iproute2`. `host/install.sh` does both.
6. Claude Code's token, for the VMs. Make it with `claude setup-token` on any machine with a browser, then on the host:
   ```
   sudo install -d -m 0700 /etc/credstore
   sudo sh -c 'umask 077; cat > /etc/credstore/CLAUDE_CODE_OAUTH_TOKEN'   # paste it, then Enter and ^D
   ```
   The unit loads `CLAUDE_CODE_OAUTH_TOKEN`, `ANTHROPIC_API_KEY` and `GH_TOKEN` from `/etc/credstore/` at each boot, and skips one that isn't there. That needs systemd 251 or later (Debian 12, Ubuntu 24.04); an older one loads nothing, and `runner logs` says the token is missing. An API key goes in `ANTHROPIC_API_KEY` the same way, and is used only without the token. A GitHub token in `GH_TOKEN` reaches only VMs given it with `runner creds <vm> claude github`. Each VM's guest gets its credentials from boot over vsock and keeps them in memory: `runner status dev` says which it took, and `runner logs dev` names any the host lacks. Only a runner-guest from this version on asks for them: rebuild `base` (images/base), then make VMs from it. A VM forked from an older base keeps its older guest, and gets none, until `runner rebase <vm>` gives it base's (README.md, Rebase onto a new base).

Then: `runner import base rootfs.ext4 && runner create dev && runner start dev && runner logs dev`. A new base later goes on top of the old one: `runner import --replace base - <rootfs.ext4`, with base stopped, and `runner undo base` brings the old one back (README.md, Build the base image).

The CLI's RUNNER_ROOT has to be the unit's: `start` checks the VM in its own root, but systemd boots the one in the unit's.

## HTTPS for a VM's URL (optional)

`runner url dev` is plain HTTP on the tailnet, which WireGuard already encrypts. When a browser needs HTTPS (a service worker, a secure cookie) or you'd rather have a name than an address, `tailscale serve` can front it with a certificate for the host's MagicDNS name. Turn HTTPS on for the tailnet first (admin console, DNS), then on the host:

```
sudo tailscale serve --bg --https=443 http://100.101.102.103:10001    # dev's URL, as `runner url dev` prints it
sudo tailscale serve --bg --https=4443 http://100.101.102.103:10002   # another VM, another HTTPS port
tailscale serve status                                               # https://kvm-host.<tailnet>.ts.net/ is dev
sudo tailscale serve --https=443 off
```

Give each an HTTPS port that isn't one of the URL ports (10000 and up), nor 8443, which Funnel gives MCP over HTTP (README.md, From claude.ai and the phone). `tailscale serve` keeps them on the tailnet; `runner doctor` warns if Funnel puts one on the internet. tailscaled connects to the URL from the host's own tailnet address, so the URL takes it, and it wakes a sleeping VM as any connection does.

## Boot tests

They need KVM, so they're `#[ignore]`d and skipped everywhere else. From the laptop, `client/check.sh` copies the checkout to the host and runs `host/check.sh` there, then the laptop checks (README.md, After merging a change). On a host set up as above, from a checkout, as the sudo user (`ubuntu` on AWS), not root:

```
host/check.sh             # all of them, about 12 minutes
host/check.sh sleep       # only the tests whose names contain "sleep"
FORCE_BASE=1 host/check.sh
```

First, unless its last run built these very files (a hash of them all but `.git`, `target` and `images/base/out`, kept in `target/check/built-from` once every build through the tests' has gone), it touches every file under `src` and `tests`, and its log says so: cargo rebuilds by file times, and a copy by `rsync -a` keeps the laptop's, which can be older than the host's last build, so cargo would test that build instead. Then it does each step a milestone check needs, with a `PASS` or `FAIL` line for each:

1. `cargo build --release && sudo host/install.sh`.
2. Builds runner-guest for musl, then holds `images/base/build.sh --fingerprint` against what `runner doctor` says base was built from and against the disk in `images/base/out`. When the disk or `images/base/out/vmlinux` is missing, or the disk differs, it rebuilds them (`sudo ENGINE=podman images/base/build.sh`; `ENGINE=docker` picks docker), and when either differs, imports it: `sudo -u runner runner import --replace base - <images/base/out/base.ext4`. It installs `images/base/out/vmlinux` as the host's kernel where it differs. `FORCE_BASE=1` rebuilds and imports anyway.
3. `sudo -u runner runner doctor`: every line `ok`, since a `warn` is a `FAIL` here too, and base built from this checkout.
4. Every `#[ignore]`d test, in every test file, one at a time, a line each as it ends: `CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER='sudo -E' RUNNER_TEST_DISK=$PWD/images/base/out/base.ext4 RUNNER_TEST_KERNEL=$PWD/images/base/out/vmlinux RUNNER_GUEST_PORT=10022 cargo test --no-fail-fast -- --ignored --test-threads=1`, with the filter after it. A test file that doesn't build is a `FAIL`, and then none run.

A failed install or base stops it there. It ends with a count and the path of its log, `target/check/<time>-<random>.log`, which holds each step's full output, and exits 1 on any `FAIL`.

The runner variable builds as you and runs only the tests as root, which `runner boot` needs for the VM's tap. The tests that boot by hand do that in a root of their own. The ones under the unit fork the host's `base` in the unit's root and run `runner` as that root's owner, as SSH would; the unit boots them with the installed runner, which is why the install comes first. They boot the guest in the host's `base`, so a rebuilt image reaches them only once it's `base` again, which is why the import comes before them. One test at a time, since a small host (2 vCPUs) can't boot nine VMs at once. The URL's test reaches the VM through the host's tailnet address, so it needs Tailscale up. The boot and net tests also check that Firecracker doesn't inherit boot's CAP_NET_ADMIN or CAP_NET_BIND_SERVICE, that a real tap comes and goes, and, in network namespaces of its own, that `host/nat.nft` drops a VM's connection to the host's port 22 but lets the host's own connection to a VM get its replies, that a VM's DNS reaches its own boot but not another's, nor a resolver the host runs on every address, that a VM that forges a source outside the range gets nothing past it, to the host or beyond, and that a VM with a list of names reaches only the addresses paired with its tap, with the real nft. Booted in a network namespace of its own, with `unshare`, a listed VM's tap goes in the rules before Firecracker and comes out last, the rules follow its list, and a look puts its tap back after a reload empties them.

The rootfs's init must take Ctrl-Alt-Del (systemd does) and print a login prompt on the serial console.
