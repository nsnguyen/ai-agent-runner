# Booting VMs on the host

How `start` and `stop` work, and how to set a host up by hand until the host install (#14) does it.

## How it works

- `runner start dev` is `systemctl start firecracker@dev`. The unit runs `runner boot dev`, which takes the VM's lock, copies its current disk into a new generation, starts Firecracker with `api.sock` in the VM's directory, and configures the kernel, that disk, the sizes and a vsock through the API. Then it starts the guest and tells systemd it's ready, so `start` returns once the guest has started.
- Before Firecracker, boot adds the VM's tap (`fc` and its /30 in hex), and removes it once Firecracker exits, before it lets go of the VM. A boot killed first leaves the tap, and the VM's next boot replaces it. The guest's address, gateway and the host's DNS servers go in the kernel's `ip=` argument; `runner ip dev` prints the address. The unit gives boot CAP_NET_ADMIN for the tap, and boot clears it before running Firecracker.
- The serial console is Firecracker's stdout, so it lands in the unit's journal: `runner logs dev`.
- `runner stop dev` is `systemctl stop`. The unit's ExecStop, `runner halt dev`, sends Ctrl-Alt-Del and waits for boot to let go of the lock. After 30 seconds, systemd kills the VM.
- A clean shutdown (Firecracker exits 0) commits the new generation. A kill, a crash or a guest panic doesn't: the previous generation stays current, and the new disk stays for a look.
- Inside the guest, shut down with `reboot`, not `poweroff`. On x86 Firecracker has no power-off, so after `poweroff` the guest halts but Firecracker keeps running: the VM looks running until a stop kills it, and the run isn't committed.
- When runner-guest starts, before it takes a command, it asks the host for the VM's credentials on vsock port 10023. Firecracker passes that to `vsock.sock_10023`, where boot hands over the ones the VM's allowlist names (docs/design.md, Credentials).
- `runner shell dev` and `runner exec dev` reach the guest's runner-guest through the VM's `vsock.sock`: `CONNECT 10022`, then the protocol in `src/proto.rs`. A stopped VM is started first, and the runner retries for up to a minute while the guest boots. Its SIGTERM or SIGHUP hangs the command up, as closing its terminal does.
- `delete` and `undo` refuse a running VM, because boot holds its lock. `checkpoint` pauses one for its copy, and `restore` stops and starts it, or waits out a stop under way.
- The agent's own calls, `runner-guest checkpoint`, `checkpoints` and `restore`, go to the host on vsock port 10024, which Firecracker passes to `vsock.sock_10024`, where boot serves them for that VM alone (`src/calls.rs`). Its checkpoint is made as `runner checkpoint` makes one, up to 10 an hour. Its restore is copied while the VM runs; then boot sends Ctrl-Alt-Del, commits the disk at the clean shutdown, puts the copy on top, and exits 75, which the unit's `RestartForceExitStatus` boots again. A unit installed before this boots nothing then, and the VM stays stopped on the restored disk: rerun `host/install.sh`.
- A headless run (`runner run`) on a stopped VM starts it with `systemctl start`, but shuts it down with Ctrl-Alt-Del straight to Firecracker, as halt does, so a clean exit commits. Once the guest is up, the run leaves a `rollback` note in the VM's directory, naming itself, and takes it back only when Claude Code exits 0; boot keeps the old generation current on seeing it, so a VM shut down under a run, by a reboot or `runner stop`, rolls back. Each run is driven by `runner drive`, in a session of its own, so it outlives the SSH connection that started it. A host whose logind kills a user's processes at logout (`KillUserProcesses=yes` in `/etc/systemd/logind.conf`; Debian and Ubuntu leave it off) kills runs too: add `runner` to `KillExcludeUsers` there.
- VMs get 2 vCPUs and 2 GiB unless `runner create --cpus --mem` says otherwise, and the host's kernel at `<root>/kernel/vmlinux` unless `--kernel` names one. A fork keeps its parent's.

## Set up a host by hand

1. Firecracker at the version in `host/firecracker-version`, on PATH. `runner boot` refuses any other.
   ```
   v=$(cat host/firecracker-version); a=$(uname -m)
   curl -fL https://github.com/firecracker-microvm/firecracker/releases/download/$v/firecracker-$v-$a.tgz | tar xz
   sudo install release-$v-$a/firecracker-$v-$a /usr/local/bin/firecracker
   ```
2. A guest kernel at `/var/lib/runner/kernel/vmlinux`. It needs `CONFIG_SERIO_I8042` and `CONFIG_KEYBOARD_ATKBD`, or Ctrl-Alt-Del never reaches the guest and every stop ends in a kill. Firecracker's CI kernels have both; its [getting started](https://github.com/firecracker-microvm/firecracker/blob/main/docs/getting-started.md) fetches one, and an Ubuntu rootfs.
3. The unit, as the user that owns `/var/lib/runner`:
   ```
   runner unit --user runner | sudo tee /etc/systemd/system/firecracker@.service
   sudo systemctl daemon-reload
   ```
4. That user in the `kvm` and `systemd-journal` groups, and allowed to start and stop `firecracker@*` units: a polkit rule, which #14 adds.
5. NAT for the VMs' range, with a filter that drops what they open to the host itself (`host/nat.nft`), and `iproute2`. `host/install.sh` does both.
6. Claude Code's token, for the VMs. Make it with `claude setup-token` on any machine with a browser, then on the host:
   ```
   sudo install -d -m 0700 /etc/credstore
   sudo sh -c 'umask 077; cat > /etc/credstore/CLAUDE_CODE_OAUTH_TOKEN'   # paste it, then Enter and ^D
   ```
   The unit loads `CLAUDE_CODE_OAUTH_TOKEN`, `ANTHROPIC_API_KEY` and `GH_TOKEN` from `/etc/credstore/` at each boot, and skips one that isn't there. That needs systemd 251 or later (Debian 12, Ubuntu 24.04); an older one loads nothing, and `runner logs` says the token is missing. An API key goes in `ANTHROPIC_API_KEY` the same way, and is used only without the token. A GitHub token in `GH_TOKEN` reaches only VMs given it with `runner creds <vm> claude github`. Each VM's guest gets its credentials from boot over vsock and keeps them in memory: `runner status dev` says which it took, and `runner logs dev` names any the host lacks. Only a runner-guest from this version on asks for them: rebuild `base` (images/base), then make VMs from it. A VM forked from an older base keeps its older guest, and gets none.

Then: `runner import base rootfs.ext4 && runner create dev && runner start dev && runner logs dev`.

The CLI's RUNNER_ROOT has to be the unit's: `start` checks the VM in its own root, but systemd boots the one in the unit's.

## HTTPS for a VM's URL (optional)

`runner url dev` is plain HTTP on the tailnet, which WireGuard already encrypts. When a browser needs HTTPS (a service worker, a secure cookie) or you'd rather have a name than an address, `tailscale serve` can front it with a certificate for the host's MagicDNS name. Turn HTTPS on for the tailnet first (admin console, DNS), then on the host:

```
sudo tailscale serve --bg --https=443 http://100.101.102.103:10001    # dev's URL, as `runner url dev` prints it
sudo tailscale serve --bg --https=8443 http://100.101.102.103:10002   # another VM, another HTTPS port
tailscale serve status                                               # https://kvm-host.<tailnet>.ts.net/ is dev
sudo tailscale serve --https=443 off
```

Give each an HTTPS port that isn't one of the URL ports (10000 and up). tailscaled connects to the URL from the host's own tailnet address, so the URL takes it, and it wakes a sleeping VM as any connection does.

## Boot tests

They need KVM, so they're `#[ignore]`d and skipped everywhere else. On a host set up as above, with `runner doctor` all ok, from a checkout:

```
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER='sudo -E'
RUNNER_TEST_DISK=$PWD/images/base/out/base.ext4 RUNNER_TEST_KERNEL=$PWD/images/base/out/vmlinux cargo test --test kvm -- --ignored --test-threads=1
```

The runner variable builds as you and runs only the tests as root, which `runner boot` needs for the VM's tap. The tests that boot by hand do that in a root of their own. The ones under the unit fork the host's `base` in the unit's root and run `runner` as that root's owner, as SSH would; the unit boots them with the installed runner, so after a change run `cargo build --release && sudo host/install.sh` first. They boot the guest in the host's `base`, so a rebuilt image reaches them only once it's `base` again. One test at a time, since a small host (2 vCPUs) can't boot nine VMs at once. The URL's test reaches the VM through the host's tailnet address, so it needs Tailscale up. With it set, `cargo test --test boot --test net -- --ignored` also checks that Firecracker doesn't inherit boot's CAP_NET_ADMIN, that a real tap comes and goes, and, in network namespaces of its own, that `host/nat.nft` drops a VM's connection to the host's port 22 but lets the host's own connection to a VM get its replies, and that a VM that forges a source outside the range gets nothing past it, to the host or beyond.

The rootfs's init must take Ctrl-Alt-Del (systemd does) and print a login prompt on the serial console.
