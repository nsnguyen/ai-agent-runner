// `runner export`, end to end through the CLI: a stopped VM as one tar
// stream, a manifest first, then each disk, sparse, which GNU tar reads.
// systemctl is faked, so neither systemd nor KVM is needed; the disks are
// sparse files, so a 2 GiB one costs what's on it.
mod common;

use chrono::Utc;
use common::{FakeSystemd, text};
use nix::errno::Errno;
use nix::pty::{OpenptyResult, openpty};
use nix::unistd::{Whence, lseek};
use proptest::prelude::*;
use runner::creds::Cred;
use runner::export::{FORMAT, Manifest};
use runner::store::{Checkpoint, Maker, Slept, Store};
use runner::url::Setting;
use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::Read;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use tempfile::TempDir;

const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;

struct Host {
    dir: TempDir,
    sd: FakeSystemd,
}

impl Host {
    fn new() -> Host {
        let dir = tempfile::tempdir().unwrap();
        let sd = FakeSystemd::new(dir.path());
        Host { dir, sd }
    }

    fn root(&self) -> PathBuf {
        self.dir.path().join("root")
    }

    fn store(&self) -> Store {
        Store::new(self.root())
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn runner(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_runner"));
        cmd.env("RUNNER_ROOT", self.root())
            .env("PATH", self.sd.path())
            .args(args)
            .stdin(Stdio::null());
        cmd
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.runner(args).output().unwrap();
        assert!(out.status.success(), "{args:?}: {}", text(&out.stderr));
        text(&out.stdout)
    }

    // `runner export <args> > to`, as a file takes it.
    fn export(&self, args: &[&str], to: &Path) -> Output {
        let file = File::create(to).unwrap();
        let mut argv = vec!["export"];
        argv.extend(args);
        self.runner(&argv)
            .stdout(file)
            .stderr(Stdio::piped())
            .output()
            .unwrap()
    }

    fn exported(&self, args: &[&str], to: &Path) -> String {
        let out = self.export(args, to);
        assert!(out.status.success(), "{args:?}: {}", text(&out.stderr));
        text(&out.stderr)
    }

    // A failed export, which wrote nothing to stdout.
    fn refused(&self, args: &[&str]) -> String {
        let to = self.path("refused.tar");
        let out = self.export(args, &to);
        assert!(!out.status.success(), "{args:?}: {}", text(&out.stderr));
        assert_eq!(fs::metadata(&to).unwrap().len(), 0, "it wrote to stdout");
        text(&out.stderr)
    }

    fn current(&self, vm: &str) -> PathBuf {
        let store = self.store();
        store.disk(vm, store.get(vm).unwrap().generation).unwrap()
    }
}

// A sparse file `size` long, with data only in `extents`: bytes from a
// pattern `seed` picks, never zero, so a copy can't make holes of them.
fn sparse(path: &Path, size: u64, extents: &[(u64, u64)], seed: u8) {
    let file = File::create(path).unwrap();
    file.set_len(size).unwrap();
    for &(at, len) in extents {
        let mut done = 0;
        while done < len {
            let n = (len - done).min(MIB);
            let bytes: Vec<u8> = (0..n).map(|i| pattern(at + done + i, seed)).collect();
            file.write_all_at(&bytes, at + done).unwrap();
            done += n;
        }
    }
}

fn pattern(at: u64, seed: u8) -> u8 {
    ((at.wrapping_mul(2654435761) >> 7) as u8 ^ seed) | 1
}

// The data extents lseek finds, as an export reads them.
fn data(file: &File, len: u64) -> Vec<(u64, u64)> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < len {
        let start = match lseek(file, at as i64, Whence::SeekData) {
            Ok(start) => start as u64,
            Err(Errno::ENXIO) => break,
            Err(e) => panic!("SEEK_DATA: {e}"),
        };
        let end = lseek(file, start as i64, Whence::SeekHole).unwrap() as u64;
        out.push((start, end.min(len)));
        at = end;
    }
    out
}

// Byte for byte, without reading the holes the two share: outside the data
// either has, both are zeros.
fn same(a: &Path, b: &Path) {
    let (fa, fb) = (File::open(a).unwrap(), File::open(b).unwrap());
    let len = fa.metadata().unwrap().len();
    assert_eq!(
        len,
        fb.metadata().unwrap().len(),
        "{} against {}",
        a.display(),
        b.display()
    );
    let mut spans = data(&fa, len);
    spans.extend(data(&fb, len));
    spans.sort();
    let (mut ba, mut bb) = (vec![0; MIB as usize], vec![0; MIB as usize]);
    let mut done = 0;
    for (start, end) in spans {
        let mut at = start.max(done);
        while at < end {
            let n = (end - at).min(MIB) as usize;
            fa.read_exact_at(&mut ba[..n], at).unwrap();
            fb.read_exact_at(&mut bb[..n], at).unwrap();
            assert!(
                ba[..n] == bb[..n],
                "{} differs from {} near {at}",
                a.display(),
                b.display()
            );
            at += n as u64;
        }
        done = done.max(end);
    }
}

fn allocated(path: &Path) -> u64 {
    fs::metadata(path).unwrap().blocks() * 512
}

// GNU tar's listing, in the stream's order.
fn listing(tar: &Path) -> Vec<String> {
    let out = Command::new("tar").arg("-tf").arg(tar).output().unwrap();
    assert!(out.status.success(), "tar -tf: {}", text(&out.stderr));
    text(&out.stdout).lines().map(str::to_string).collect()
}

// `tar -xf`, into a directory of its own.
fn extract(tar: &Path, into: &Path) {
    fs::create_dir_all(into).unwrap();
    let out = Command::new("tar")
        .arg("-xf")
        .arg(tar)
        .current_dir(into)
        .output()
        .unwrap();
    assert!(out.status.success(), "tar -xf: {}", text(&out.stderr));
}

fn manifest(dir: &Path, vm: &str) -> Manifest {
    let bytes = fs::read(dir.join(vm).join("manifest.json")).unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

// 16 MiB of data on a 2 GiB disk: at the start, unaligned in the middle,
// and at the very end.
const LAYOUT: &[(u64, u64)] = &[
    (0, 4 * MIB),
    (700 * MIB, 4 * MIB),
    (GIB + 12345, 4 * MIB),
    (2 * GIB - 4 * MIB, 4 * MIB),
];

// A run on `vm` that rewrites its first MiB, committed, so the current disk
// differs from the last checkpoint without holding more data.
fn rewrite(store: &Store, vm: &str, seed: u8) {
    let run = store.begin_run(vm).unwrap();
    sparse_over(&run.disk, &[(0, MIB)], seed);
    run.commit().unwrap();
}

fn sparse_over(path: &Path, extents: &[(u64, u64)], seed: u8) {
    let file = fs::OpenOptions::new().write(true).open(path).unwrap();
    for &(at, len) in extents {
        let bytes: Vec<u8> = (0..len).map(|i| pattern(at + i, seed)).collect();
        file.write_all_at(&bytes, at).unwrap();
    }
}

// dev, forked from an imported 2 GiB disk with its own sizes, idle time,
// URL and allowlist, and two checkpoints of mine, each of a different disk.
// Then everything an export leaves out: an agent's and a run's checkpoint,
// a sleep snapshot and a run's records.
fn dev(host: &Host) -> (Checkpoint, Checkpoint) {
    let src = host.path("src.ext4");
    sparse(&src, 2 * GIB, LAYOUT, 1);
    host.ok(&["import", "img", &src.display().to_string()]);
    host.ok(&[
        "create", "dev", "--from", "img", "--cpus", "4", "--mem", "4G", "--idle", "30m",
    ]);
    let store = host.store();
    let url = Setting {
        off: false,
        port: Some(3000),
    };
    store.set_url("dev", url).unwrap();
    store
        .set_creds("dev", &[Cred::Github, Cred::Claude])
        .unwrap();
    let first = store.checkpoint("dev", Some("first")).unwrap();
    rewrite(&store, "dev", 2);
    let second = store.checkpoint("dev", Some("tests\npass")).unwrap();
    rewrite(&store, "dev", 3);

    store
        .checkpoint_by("dev", Some("the agent's"), Maker::Agent)
        .unwrap();
    store
        .checkpoint_by("dev", Some("a run's"), Maker::Run)
        .unwrap();
    let snapshot = store.snapshot("dev").unwrap();
    snapshot.make_room().unwrap();
    fs::write(snapshot.vmstate(), b"vmstate").unwrap();
    fs::write(snapshot.mem(), b"the guest's memory").unwrap();
    // As a boot killed asleep leaves it: stopped, its snapshot still there.
    let slept = Slept {
        generation: 1,
        at: Utc::now(),
    };
    snapshot.note(&slept).unwrap();
    let runs = store.runs("dev").unwrap();
    fs::create_dir_all(runs.join("1")).unwrap();
    fs::write(runs.join("1").join("stream.jsonl"), b"{}\n").unwrap();
    (first, second)
}

#[test]
fn an_export_streams_only_data_and_tar_gives_each_disk_back() {
    let host = Host::new();
    let (first, second) = dev(&host);
    let store = host.store();
    let tar = host.path("dev.tar");
    let said = host.exported(&["dev"], &tar);

    // Three disks of 16 MiB each, and headers.
    let size = fs::metadata(&tar).unwrap().len();
    assert!(size < 64 * MIB, "the export is {size} bytes");
    let (a, b) = (first.id, second.id);
    let disks = [
        "dev/disk.ext4".to_string(),
        format!("dev/checkpoints/{a}.ext4"),
        format!("dev/checkpoints/{b}.ext4"),
    ];
    let mut want = vec!["dev/manifest.json".to_string()];
    want.extend(disks.iter().cloned());
    assert_eq!(listing(&tar), want);
    assert!(said.contains("exported dev"), "{said}");
    assert!(said.contains(&format!("{size} bytes")), "{said}");

    let out = host.path("out");
    extract(&tar, &out);
    let theirs = [
        host.current("dev"),
        store.disk("dev", a).unwrap(),
        store.disk("dev", b).unwrap(),
    ];
    for (name, theirs) in disks.iter().zip(&theirs) {
        let got = out.join(name);
        same(&got, theirs);
        assert_eq!(fs::metadata(&got).unwrap().len(), 2 * GIB, "{name}");
        assert!(
            allocated(&got) < 20 * MIB,
            "{name}: {} allocated",
            allocated(&got)
        );
    }
    // Each checkpoint is its own disk, not the current one again.
    let reads = |p: &Path| {
        let mut b = vec![0; 64];
        File::open(p).unwrap().read_exact(&mut b).unwrap();
        b
    };
    assert_ne!(reads(&out.join(&disks[0])), reads(&out.join(&disks[1])));
    assert_ne!(reads(&out.join(&disks[1])), reads(&out.join(&disks[2])));

    let m = manifest(&out, "dev");
    assert_eq!(m.format, FORMAT);
    assert_eq!(m.name, "dev");
    assert_eq!(m.config, store.config("dev").unwrap());
    assert_eq!(m.config.cpus, Some(4));
    assert_eq!(m.config.url.and_then(|u| u.port), Some(3000));
    assert_eq!(m.creds, [Cred::Claude, Cred::Github]);
    assert_eq!(m.disk.path, disks[0]);
    assert_eq!(m.disk.generation, store.get("dev").unwrap().generation);
    assert_eq!(m.disk.bytes, 2 * GIB);
    let got: Vec<_> = m
        .checkpoints
        .iter()
        .map(|c| (c.id, c.time, c.comment.clone(), c.path.clone(), c.bytes))
        .collect();
    let want: Vec<_> = [&first, &second]
        .iter()
        .zip(&disks[1..])
        .map(|(c, path)| (c.id, c.time, c.comment.clone(), path.clone(), 2 * GIB))
        .collect();
    assert_eq!(got, want);
    assert_eq!(second.comment.as_deref(), Some("tests\npass"));
}

#[test]
fn no_checkpoints_holds_only_the_current_disk() {
    let host = Host::new();
    // Dense: every byte data, so it goes as a plain file.
    let src = host.path("src.ext4");
    sparse(&src, 3 * MIB + 5, &[(0, 3 * MIB + 5)], 7);
    host.ok(&["import", "dev", &src.display().to_string()]);
    host.store().checkpoint("dev", Some("mine")).unwrap();

    let tar = host.path("dev.tar");
    host.exported(&["dev", "--no-checkpoints"], &tar);
    assert_eq!(listing(&tar), ["dev/manifest.json", "dev/disk.ext4"]);
    let out = host.path("out");
    extract(&tar, &out);
    assert_eq!(
        fs::read(out.join("dev/disk.ext4")).unwrap(),
        fs::read(&src).unwrap()
    );
    let m = manifest(&out, "dev");
    assert!(m.checkpoints.is_empty(), "{:?}", m.checkpoints);
    // A VM that never set them gets the defaults: no config, claude only.
    assert_eq!(m.config, Default::default());
    assert_eq!(m.creds, [Cred::Claude]);
}

#[test]
fn a_running_or_sleeping_vm_is_refused() {
    let host = Host::new();
    let src = host.path("src.ext4");
    sparse(&src, 4 * MIB, &[(0, MIB)], 1);
    host.ok(&["import", "dev", &src.display().to_string()]);

    host.sd.set_state("active");
    let err = host.refused(&["dev"]);
    assert!(err.contains("dev is running: stop it first"), "{err}");
    assert!(err.contains("runner stop dev"), "{err}");

    // Asleep, its unit stays active, with its snapshot noted.
    let snapshot = host.store().snapshot("dev").unwrap();
    snapshot.make_room().unwrap();
    let slept = Slept {
        generation: 0,
        at: Utc::now(),
    };
    snapshot.note(&slept).unwrap();
    let err = host.refused(&["dev"]);
    assert!(err.contains("dev is sleeping: stop it first"), "{err}");

    host.sd.set_state("activating");
    let err = host.refused(&["dev"]);
    assert!(err.contains("dev is booting: stop it first"), "{err}");
}

#[test]
fn a_vm_another_command_holds_is_refused() {
    let host = Host::new();
    let src = host.path("src.ext4");
    sparse(&src, 4 * MIB, &[(0, MIB)], 1);
    host.ok(&["import", "dev", &src.display().to_string()]);
    let run = host.store().begin_run("dev").unwrap();
    let err = host.refused(&["dev"]);
    assert!(err.contains("dev is busy"), "{err}");
    run.discard();
}

#[test]
fn a_vm_that_is_not_there_is_refused() {
    let host = Host::new();
    let err = host.refused(&["nope"]);
    assert!(err.contains("no VM named nope"), "{err}");
}

#[test]
fn a_terminal_on_stdout_is_refused_before_anything_is_asked() {
    let host = Host::new();
    let OpenptyResult {
        master: _master,
        slave,
    } = openpty(None, None).unwrap();
    let out = host
        .runner(&["export", "dev"])
        .stdout(Stdio::from(slave))
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = text(&out.stderr);
    assert!(err.contains("stdout is a terminal"), "{err}");
    assert!(err.contains("runner export dev > dev.tar"), "{err}");
    assert!(host.sd.calls().is_empty(), "{:?}", host.sd.calls());
}

// Every file is open before a byte goes out, and the VM's lock let go, so a
// delete then goes ahead, and the export still streams every disk whole.
#[test]
fn a_vm_deleted_while_its_export_streams_still_exports_whole() {
    let host = Host::new();
    let src = host.path("src.ext4");
    sparse(&src, 2 * GIB, LAYOUT, 1);
    host.ok(&["import", "dev", &src.display().to_string()]);
    let store = host.store();
    let mine = store.checkpoint("dev", Some("mine")).unwrap();

    let mut child = host
        .runner(&["export", "dev"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut head = vec![0; 512];
    stdout.read_exact(&mut head).unwrap();
    // The export blocks on the pipe, far short of the end.
    store.delete("dev").unwrap();
    assert!(store.get("dev").is_err());
    let mut rest = Vec::new();
    stdout.read_to_end(&mut rest).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));

    head.extend(rest);
    let tar = host.path("dev.tar");
    fs::write(&tar, &head).unwrap();
    let dir = host.path("out");
    extract(&tar, &dir);
    same(&dir.join("dev/disk.ext4"), &src);
    same(&dir.join(format!("dev/checkpoints/{}.ext4", mine.id)), &src);
}

// Past 8 GiB, tar's octal fields run out, and GNU tar's base-256 takes over:
// for the size, and for each extent's offset.
#[test]
fn a_disk_past_8_gib_streams_sparse_and_tar_reads_it() {
    let host = Host::new();
    let src = host.path("src.ext4");
    sparse(&src, 20 * GIB, &[(0, MIB), (9 * GIB + 3, MIB)], 5);
    host.ok(&["import", "dev", &src.display().to_string()]);
    let tar = host.path("dev.tar");
    host.exported(&["dev"], &tar);
    assert!(fs::metadata(&tar).unwrap().len() < 4 * MIB);
    let out = host.path("out");
    extract(&tar, &out);
    same(&out.join("dev/disk.ext4"), &src);
}

// A disk of up to 96 blocks of 4 KiB, data in any of them, and its length
// cut anywhere in the last block.
fn layout() -> impl Strategy<Value = (BTreeSet<u64>, u64)> {
    (1..=96u64).prop_flat_map(|blocks| {
        let data = proptest::collection::btree_set(0..blocks, 0..=blocks as usize);
        (data, 0..4096u64).prop_map(move |(data, cut)| (data, blocks * 4096 - cut))
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    // Over the extents map: none, a few in the header, past 4 into an
    // extension header, past 25 into another, starting or ending in a hole.
    #[test]
    fn tar_gives_back_any_sparse_disk_byte_for_byte((blocks, len) in layout()) {
        let host = Host::new();
        let src = host.path("src.ext4");
        let extents: Vec<(u64, u64)> = blocks
            .iter()
            .filter(|&&b| b * 4096 < len)
            .map(|&b| (b * 4096, 4096.min(len - b * 4096)))
            .collect();
        sparse(&src, len, &extents, 9);
        host.ok(&["import", "dev", &src.display().to_string()]);
        let tar = host.path("dev.tar");
        host.exported(&["dev"], &tar);
        let out = host.path("out");
        extract(&tar, &out);
        prop_assert_eq!(
            fs::read(out.join("dev/disk.ext4")).unwrap(),
            fs::read(&src).unwrap()
        );
    }
}
