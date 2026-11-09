// A host's room (src/room.rs): its memory less a reserve for the host and
// what each VM up holds. `runner hosts` reports it, and a boot or wake that
// doesn't fit is refused before anything starts, with systemctl faked and a
// stand-in for a sleeping VM's boot.
mod common;

use common::{FakeSystemd, StandIn, script, text};
use runner::room::{self, FIRECRACKER_MIB, NoRoom, Room};
use runner::sleep::Ask;
use runner::store::{Config, Slept, Store};
use runner::systemd::State;
use std::fs;
use std::process::{Command, Output, Stdio};
use tempfile::TempDir;

const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;

#[test]
fn vms_up_hold_their_memory_and_their_firecracker_s() {
    let held = 2048 * MIB + FIRECRACKER_MIB * MIB;
    for state in [State::Booting, State::Running, State::Stopping] {
        assert_eq!(room::holds(state, 2048), held, "{state}");
    }
    for state in [State::Stopped, State::Sleeping] {
        assert_eq!(room::holds(state, 2048), 0, "{state}");
    }
    assert_eq!(room::needs(4096), 4096 * MIB + FIRECRACKER_MIB * MIB);
}

#[test]
fn room_is_memtotal_less_the_reserve_and_what_the_vms_up_hold() {
    let vms = [
        (State::Running, 4096),
        (State::Booting, 1024),
        (State::Stopping, 512),
        (State::Stopped, 8192),
        (State::Sleeping, 8192),
    ];
    let room = Room::of(16 * GIB, 2 * GIB, vms);
    let held = (4096 + 1024 + 512 + 3 * FIRECRACKER_MIB) * MIB;
    assert_eq!(room.held, held);
    assert_eq!(room.up, 3);
    assert_eq!(room.free(), 14 * GIB - held);
    // Nothing up: all but the reserve.
    assert_eq!(Room::of(16 * GIB, GIB, []).free(), 15 * GIB);
}

#[test]
fn an_overcommitted_host_has_no_room_not_less() {
    let room = Room::of(4 * GIB, GIB, [(State::Running, 4096)]);
    assert_eq!(room.free(), 0);
    assert_eq!(Room::of(GIB / 2, GIB, []).free(), 0);
}

#[test]
fn the_reserve_is_a_gib_unless_set() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("reserve");
    assert_eq!(runner::host::reserve_in(&file).unwrap(), GIB);
    fs::write(&file, "2G\n").unwrap();
    assert_eq!(runner::host::reserve_in(&file).unwrap(), 2 * GIB);
    fs::write(&file, "512\n").unwrap();
    assert_eq!(runner::host::reserve_in(&file).unwrap(), 512 * MIB);
    fs::write(&file, "lots\n").unwrap();
    let e = runner::host::reserve_in(&file).unwrap_err().to_string();
    assert!(e.contains(file.to_str().unwrap()), "{e}");
    assert!(e.contains("RUNNER_HOST_RESERVE"), "{e}");
}

#[test]
fn a_refusal_says_what_the_vm_needs_and_what_the_host_has() {
    let no = NoRoom {
        vm: "dev".into(),
        need: room::needs(4096),
        free: 1536 * MIB,
        host: "kvm-host".into(),
    };
    assert_eq!(
        no.to_string(),
        "dev needs 4.1G; kvm-host has 1.5G free: stop or sleep a VM, or move dev"
    );
}

// What the refusals call this host.
fn here() -> String {
    let name = runner::hosts::hostname();
    if name.is_empty() {
        "this host".into()
    } else {
        name
    }
}

struct Host {
    dir: TempDir,
    store: Store,
    sd: FakeSystemd,
}

// base, and a fork of it for each of `vms` with that much memory in MiB, all
// stopped, on a host of `total` bytes with the default reserve. A stop runs
// `runner halt`, as the unit does.
fn host(vms: &[(&str, u32)], total: u64) -> Host {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path());
    let disk = dir.path().join("rootfs.ext4");
    fs::write(&disk, b"rootfs").unwrap();
    store.import("base", &disk).unwrap();
    let sd = FakeSystemd::new(dir.path());
    let mut halts = String::new();
    for &(name, mem) in vms {
        let sized = Config {
            mem_mib: Some(mem),
            ..Config::default()
        };
        store.fork_with("base", name, &sized).unwrap();
        sd.set_vm_state(name, "inactive");
        halts += &format!(
            "[ \"$2\" != firecracker@{name}.service ] || exec '{}' halt {name}\n",
            env!("CARGO_BIN_EXE_runner")
        );
    }
    script(&sd.bin.join("on-stop"), &halts);
    let h = Host { dir, store, sd };
    h.total(total);
    h
}

impl Host {
    fn total(&self, bytes: u64) {
        let text = format!("MemTotal: {} kB\nMemAvailable: 1 kB\n", bytes / 1024);
        fs::write(self.dir.path().join("meminfo"), text).unwrap();
    }

    fn runner(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_runner"));
        cmd.env("RUNNER_ROOT", self.dir.path())
            .env("PATH", self.sd.path())
            .env("RUNNER_MEMINFO", self.dir.path().join("meminfo"))
            .env("RUNNER_RESERVE", self.dir.path().join("reserve"))
            .args(args);
        cmd
    }

    fn run(&self, args: &[&str]) -> Output {
        self.runner(args).output().unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(out.status.success(), "{args:?}: {}", text(&out.stderr));
        text(&out.stdout)
    }

    fn refused(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(!out.status.success(), "{args:?}: {}", text(&out.stdout));
        text(&out.stderr)
    }

    // Asleep, its unit still up, as its boot leaves it.
    fn asleep(&self, vm: &str) {
        self.sd.set_vm_state(vm, "active");
        let snap = self.store.snapshot(vm).unwrap();
        snap.make_room().unwrap();
        let generation = self.store.get(vm).unwrap().generation + 1;
        let at = chrono::Utc::now();
        snap.note(&Slept {
            generation,
            at,
            parked: false,
        })
        .unwrap();
    }

    fn starts(&self) -> Vec<String> {
        let calls = self.sd.calls().into_iter();
        calls
            .filter(|c| c.starts_with("systemctl start "))
            .collect()
    }
}

// 6 GiB, less the 1 GiB reserve and web's 2 GiB and 64 MiB: 2.9G, short of
// dev's 4 GiB and 64 MiB.
#[test]
fn a_start_that_doesn_t_fit_is_refused_and_nothing_boots() {
    let h = host(&[("dev", 4096), ("web", 2048)], 6 * GIB);
    h.sd.set_vm_state("web", "active");
    let err = h.refused(&["start", "dev"]);
    let want = format!(
        "dev needs 4.1G; {} has 2.9G free: stop or sleep a VM, or move dev",
        here()
    );
    assert!(err.contains(&want), "{err}");
    assert!(h.starts().is_empty(), "{:?}", h.starts());
    assert!(h.ok(&["status", "dev"]).ends_with("  stopped\n"));
    // With web stopped, 5 GiB: it fits.
    h.sd.set_vm_state("web", "inactive");
    assert!(h.ok(&["start", "dev"]).ends_with("  running\n"));
    assert_eq!(h.starts(), ["systemctl start firecracker@dev.service"]);
}

// A VM asleep holds nothing, so one fits beside it.
#[test]
fn a_sleeping_vm_leaves_its_room_to_the_others() {
    let h = host(&[("dev", 4096), ("web", 2048)], 6 * GIB);
    h.asleep("web");
    assert!(h.ok(&["start", "dev"]).ends_with("  running\n"));
}

// A VM that's up already needs no more room, so starting it again on a
// full host is fine.
#[test]
fn a_vm_up_already_starts_on_a_full_host() {
    let h = host(&[("dev", 4096)], 2 * GIB);
    h.sd.set_vm_state("dev", "active");
    assert!(h.ok(&["start", "dev"]).ends_with("  running\n"));
}

#[test]
fn a_wake_that_doesn_t_fit_is_refused_and_the_vm_sleeps_on() {
    let h = host(&[("dev", 4096), ("web", 2048)], 6 * GIB);
    h.sd.set_vm_state("web", "active");
    h.asleep("dev");
    let boot = StandIn::serve(&h.store, "dev");
    let want = format!(
        "dev needs 4.1G; {} has 2.9G free: stop or sleep a VM, or move dev",
        here()
    );
    for verb in ["wake", "start"] {
        let err = h.refused(&[verb, "dev"]);
        assert!(err.contains(&want), "{verb}: {err}");
    }
    assert!(boot.asked().is_empty(), "{:?}", boot.asked());
    assert!(h.ok(&["status", "dev"]).ends_with("  sleeping\n"));
    h.sd.set_vm_state("web", "inactive");
    assert!(h.ok(&["wake", "dev"]).ends_with("  running\n"));
    assert_eq!(boot.asked(), [Ask::Wake]);
}

// A stop wakes a sleeping VM only to shut it down, so a full host can
// always stop its VMs.
#[test]
fn a_full_host_still_stops_a_sleeping_vm() {
    let h = host(&[("dev", 4096), ("web", 2048)], 6 * GIB);
    h.sd.set_vm_state("web", "active");
    h.asleep("dev");
    let boot = StandIn::serve(&h.store, "dev");
    assert!(h.ok(&["stop", "dev"]).ends_with("  stopped\n"));
    assert_eq!(boot.asked(), [Ask::Halt]);
}

// Room for one of two 2 GiB VMs. Each fake start takes half a second
// before systemd says the VM is up, so the two checks overlap unless the
// first holds the room until then.
#[test]
fn two_starts_at_once_against_room_for_one_boot_one() {
    let h = host(&[("one", 2048), ("two", 2048)], (1024 + 2112 + 1000) * MIB);
    script(&h.sd.bin.join("on-start"), "sleep 0.5\n");
    let both: Vec<_> = ["one", "two"]
        .iter()
        .map(|vm| {
            let mut start = h.runner(&["start", vm]);
            start.stdout(Stdio::piped()).stderr(Stdio::piped());
            start.spawn().unwrap()
        })
        .collect();
    let outs: Vec<Output> = both
        .into_iter()
        .map(|c| c.wait_with_output().unwrap())
        .collect();
    let booted = outs.iter().filter(|o| o.status.success()).count();
    let said: Vec<String> = outs.iter().map(|o| text(&o.stderr)).collect();
    assert_eq!(booted, 1, "{said:?}");
    let refused = (outs.iter().zip(&said))
        .find(|(o, _)| !o.status.success())
        .map(|(_, s)| s)
        .unwrap();
    assert!(refused.contains("needs 2.1G"), "{refused}");
    assert!(refused.contains("has 1.0G free"), "{refused}");
    assert_eq!(h.starts().len(), 1, "{:?}", h.starts());
}

// `runner hosts` reports room, not MemAvailable: 16 GiB, less a 2 GiB
// reserve, dev's 4 GiB running and old's 2 GiB stopping, each with 64 MiB.
// web sleeps, holding nothing, but it's up.
#[test]
fn hosts_shows_the_room() {
    let h = host(&[("dev", 4096), ("web", 2048), ("old", 2048)], 16 * GIB);
    fs::write(h.dir.path().join("reserve"), "2G\n").unwrap();
    h.sd.set_vm_state("dev", "active");
    h.sd.set_vm_state("old", "deactivating");
    h.asleep("web");
    let free = (16384 - 2048 - 4096 - 2048 - 2 * FIRECRACKER_MIB) * MIB;
    assert_eq!(
        h.ok(&["hosts"]),
        format!("4 VMs  3 up  {free} bytes free\n")
    );
}

#[test]
fn a_bad_reserve_fails_the_start_saying_where() {
    let h = host(&[("dev", 2048)], 16 * GIB);
    fs::write(h.dir.path().join("reserve"), "lots\n").unwrap();
    let err = h.refused(&["start", "dev"]);
    assert!(err.contains("reserve"), "{err}");
    assert!(h.starts().is_empty(), "{:?}", h.starts());
}

fn install_sh() -> String {
    fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/host/install.sh")).unwrap()
}

// install.sh's lines that read, check and keep RUNNER_HOST_RESERVE, with
// /etc/runner moved to `etc`.
fn reserve_lines(etc: &std::path::Path) -> String {
    let script = install_sh();
    let read = script
        .lines()
        .find(|l| l.starts_with("reserve="))
        .expect("install.sh reads RUNNER_HOST_RESERVE");
    let start = script.find("bad_reserve=").unwrap();
    let end = start + script[start..].find("\n\n").unwrap();
    let check = &script[start..end];
    let keep_at = script.find("echo \"$reserve\" >\"$tmp/reserve\"").unwrap();
    let keep_end = keep_at + script[keep_at..].find("/etc/runner/reserve\n").unwrap();
    let keep = &script[keep_at..keep_end + "/etc/runner/reserve\n".len()];
    format!("{read}\n{check}\n{keep}").replace("/etc/runner", etc.to_str().unwrap())
}

#[test]
fn install_sh_keeps_the_reserve_for_reruns() {
    let dir = tempfile::tempdir().unwrap();
    let etc = dir.path().join("etc");
    fs::create_dir(&etc).unwrap();
    let tmp = dir.path().join("tmp");
    fs::create_dir(&tmp).unwrap();
    let body = format!(
        "set -euo pipefail\ndie() {{ echo \"$*\" >&2; exit 1; }}\ntmp={}\n{}",
        tmp.display(),
        reserve_lines(&etc)
    );
    let install = |reserve: Option<&str>| {
        let mut cmd = Command::new("bash");
        cmd.arg("-c").arg(&body).env_remove("RUNNER_HOST_RESERVE");
        if let Some(r) = reserve {
            cmd.env("RUNNER_HOST_RESERVE", r);
        }
        let out = cmd.output().unwrap();
        assert!(out.status.success(), "{}", text(&out.stderr));
        fs::read_to_string(etc.join("reserve")).unwrap()
    };
    assert_eq!(install(None), "1G\n");
    assert_eq!(install(Some("2G")), "2G\n");
    assert_eq!(install(None), "2G\n");
    assert_eq!(
        runner::host::reserve_in(&etc.join("reserve")).unwrap(),
        2 * GIB
    );
}

// install.sh's own check of RUNNER_HOST_RESERVE, run in bash, against what
// the runner reads: the same, but for T and counts of more than six digits,
// which it refuses so none overflows the runner's.
#[test]
fn install_sh_takes_the_reserves_the_runner_reads() {
    let script = install_sh();
    let start = script.find("bad_reserve=").unwrap();
    let end = start + script[start..].find("\n\n").unwrap();
    let check = &script[start..end];
    let values = [
        "1G", "2G", "512", "512M", "2g", "2GiB", "2gib", "007G", "999999G", "1T", "0", "0G", "",
        "-1G", "1.5G", " 1G", "1G ", "lots", "G", "1K", "1000000G",
    ];
    let out = Command::new("bash")
        .arg("-c")
        .arg(format!(
            "die() {{ echo bad; exit; }}\nfor reserve; do (\n{check}\necho ok\n); done"
        ))
        .arg("bash")
        .args(values)
        .output()
        .unwrap();
    let said = text(&out.stdout);
    let verdicts: Vec<&str> = said.lines().collect();
    assert_eq!(verdicts.len(), values.len(), "{said}{}", text(&out.stderr));
    for (value, verdict) in values.iter().zip(verdicts) {
        let runner = runner::store::parse_mem(value).is_ok();
        let digits = value.bytes().filter(u8::is_ascii_digit).count();
        let want = runner && digits <= 6 && !value.to_lowercase().contains('t');
        assert_eq!(
            verdict == "ok",
            want,
            "{value:?}: install.sh {verdict}, runner {runner}"
        );
    }
}

// A VM whose state systemd can't say counts as up, rather than stopping
// every other VM's boot.
#[test]
fn a_vm_systemd_can_t_place_counts_as_up() {
    let h = host(&[("dev", 2048), ("odd", 2048)], 16 * GIB);
    h.sd.set_vm_state("odd", "bogus");
    assert!(h.ok(&["start", "dev"]).ends_with("  running\n"));
    h.sd.set_vm_state("dev", "inactive");
    h.total(5 * GIB);
    let err = h.refused(&["start", "dev"]);
    assert!(err.contains("has 1.9G free"), "{err}");
}
