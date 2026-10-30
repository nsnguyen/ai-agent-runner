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

VMs get 172.16.0.0/16; for another range, `sudo RUNNER_NET=10.20.0.0/16 host/install.sh`. It installs the pinned Firecracker (hash-checked, `host/firecracker.pin`) and `runner` into `/usr/local/bin`, makes the `runner` user, lets it start and stop `firecracker@<vm>` units through polkit, and sets up NAT for the VMs. It's safe to rerun, and rerunning it after a `git pull` and a build is how to upgrade. It ends with `runner doctor`, run as the runner user, which says what's still missing: usually a guest kernel at `/var/lib/runner/kernel/vmlinux`, the base disk (`runner import base <disk>`), and the laptop's key. Run it again any time with `ssh runner@kvm-host runner doctor`.

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
