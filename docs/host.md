# Booting VMs on the host

How `start` and `stop` work, and how to set a host up by hand until the host install (#14) does it.

## How it works

- `runner start dev` is `systemctl start firecracker@dev`. The unit runs `runner boot dev`, which takes the VM's lock, copies its current disk into a new generation, starts Firecracker with `api.sock` in the VM's directory, and configures the kernel, that disk, the sizes and a vsock through the API. Then it starts the guest and tells systemd it's ready, so `start` returns once the guest has started.
- Before Firecracker, boot adds the VM's tap (`fc` and its /30 in hex), and removes it once Firecracker exits, before it lets go of the VM. A boot killed first leaves the tap, and the VM's next boot replaces it. The guest's address, gateway and the host's DNS servers go in the kernel's `ip=` argument; `runner ip dev` prints the address. The unit gives boot CAP_NET_ADMIN for the tap, and boot clears it before running Firecracker.
- The serial console is Firecracker's stdout, so it lands in the unit's journal: `runner logs dev`.
- `runner stop dev` is `systemctl stop`. The unit's ExecStop, `runner halt dev`, sends Ctrl-Alt-Del and waits for boot to let go of the lock. After 30 seconds, systemd kills the VM.
- A clean shutdown (Firecracker exits 0) commits the new generation. A kill, a crash or a guest panic doesn't: the previous generation stays current, and the new disk stays for a look.
- Inside the guest, shut down with `reboot`, not `poweroff`. On x86 Firecracker has no power-off, so after `poweroff` the guest halts but Firecracker keeps running: the VM looks running until a stop kills it, and the run isn't committed.
- `runner shell dev` and `runner exec dev` reach the guest's runner-guest through the VM's `vsock.sock`: `CONNECT 10022`, then the protocol in `src/proto.rs`. A stopped VM is started first, and the runner retries for up to a minute while the guest boots. Its SIGTERM or SIGHUP hangs the command up, as closing its terminal does.
- `delete` and `undo` refuse a running VM, because boot holds its lock.
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
5. NAT for the VMs' range, and `iproute2`. `host/install.sh` does both.

Then: `runner import base rootfs.ext4 && runner create dev && runner start dev && runner logs dev`.

The CLI's RUNNER_ROOT has to be the unit's: `start` checks the VM in its own root, but systemd boots the one in the unit's.

## Boot tests

They need KVM, so they're `#[ignore]`d and skipped everywhere else:

```
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER='sudo -E'
RUNNER_TEST_DISK=rootfs.ext4 RUNNER_TEST_KERNEL=vmlinux cargo test --test kvm -- --ignored
```

The runner variable builds as you and runs only the tests as root, which `runner boot` needs for the VM's tap. With it set, `cargo test --test boot --test net -- --ignored` also checks that Firecracker doesn't inherit boot's CAP_NET_ADMIN, and that a real tap comes and goes.

The rootfs's init must take Ctrl-Alt-Del (systemd does) and print a login prompt on the serial console.
