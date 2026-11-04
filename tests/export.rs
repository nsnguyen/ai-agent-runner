// `runner export`, end to end through the CLI: a stopped VM as one tar
// stream, a manifest first, then each disk, sparse, which GNU tar reads.
// Then `runner import` of that stream, as a new VM. systemctl is faked, so
// neither systemd nor KVM is needed; the disks are sparse files, so a 2 GiB
// one costs what's on it.
mod common;

use chrono::Utc;
use common::{FakeSystemd, text};
use nix::errno::Errno;
use nix::pty::{OpenptyResult, openpty};
use nix::unistd::{Whence, lseek};
use proptest::prelude::*;
use runner::creds::Cred;
use runner::export::{self, FORMAT, Manifest};
use runner::import;
use runner::store::{Checkpoint, Maker, Slept, Store};
use runner::url::Setting;
use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};
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

    // `runner import <args>`, with stdin from `stdin` if given.
    fn import(&self, args: &[&str], stdin: Option<&Path>) -> Output {
        let mut argv = vec!["import"];
        argv.extend(args);
        let mut cmd = self.runner(&argv);
        if let Some(path) = stdin {
            cmd.stdin(File::open(path).unwrap());
        }
        cmd.output().unwrap()
    }

    // Its stdout and stderr.
    fn imported(&self, args: &[&str], stdin: Option<&Path>) -> (String, String) {
        let out = self.import(args, stdin);
        assert!(out.status.success(), "{args:?}: {}", text(&out.stderr));
        (text(&out.stdout), text(&out.stderr))
    }

    // A failed import of `bytes` from stdin as dev2, which left no dev2
    // and nothing staged.
    fn refused_import(&self, bytes: &[u8]) -> String {
        let path = self.path("refused.tar");
        fs::write(&path, bytes).unwrap();
        let out = self.import(&["dev2", "-"], Some(&path));
        assert!(!out.status.success(), "{}", text(&out.stdout));
        assert!(self.store().get("dev2").is_err(), "dev2 is there");
        assert_eq!(self.staged(), Vec::<String>::new());
        text(&out.stderr)
    }

    // What's under vms/ that isn't a VM: a create or import left staged.
    fn staged(&self) -> Vec<String> {
        let vms = self.root().join("vms");
        let Ok(entries) = fs::read_dir(vms) else {
            return Vec::new();
        };
        let names = entries.map(|e| e.unwrap().file_name().to_string_lossy().into_owned());
        names.filter(|n| n.starts_with('.')).collect()
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

// Fails once, at `at` bytes, and takes everything after: what a stream that
// broke partway would show if anything wrote on.
struct Flaky {
    out: Vec<u8>,
    at: usize,
    failed: bool,
}

impl Write for Flaky {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if !self.failed && self.out.len() + buf.len() > self.at {
            self.failed = true;
            return Err(io::Error::other("the pipe broke"));
        }
        self.out.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

// The tar Builder ends the stream as it's dropped; a failed export mustn't,
// so a reader sees it cut short.
#[test]
fn an_export_that_fails_partway_is_left_cut_short_not_ended() {
    let host = Host::new();
    let src = host.path("src.ext4");
    sparse(&src, 64 * MIB, &[(0, 8 * MIB)], 1);
    host.ok(&["import", "dev", &src.display().to_string()]);
    let disks = host.store().open_disks("dev", true).unwrap();
    let mut out = Flaky {
        out: Vec::new(),
        at: 4 << 20,
        failed: false,
    };
    let err = export::write("dev", disks, &mut out)
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("exporting dev: dev/disk.ext4: the pipe broke"),
        "{err}"
    );
    assert!(out.failed);
    // It ends in the disk's data, which is never zero, not in tar's end.
    let tail = &out.out[out.out.len() - 1024..];
    assert!(tail.iter().all(|&b| b != 0), "it ends in zeros");
    let tar = host.path("cut.tar");
    fs::write(&tar, &out.out).unwrap();
    let listed = Command::new("tar").arg("-tf").arg(&tar).output().unwrap();
    assert!(!listed.status.success(), "tar took it whole");
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

// Import: an export back as a new VM.

#[test]
fn an_import_gives_back_each_disk_sparse_with_my_checkpoints_and_settings() {
    let host = Host::new();
    let (first, second) = dev(&host);
    let store = host.store();
    let tar = host.path("dev.tar");
    host.exported(&["dev"], &tar);
    let asked = host.sd.calls().len();

    let (out, err) = host.imported(&["dev2", &tar.display().to_string()], None);
    // Its current disk goes past every checkpoint's id.
    let now = second.id.max(first.id) + 1;
    assert_eq!(out, format!("dev2  gen {now}  {} bytes\n", 2 * GIB));
    assert!(
        err.contains("imported dev's export as dev2: its disk and 2 checkpoints"),
        "{err}"
    );
    // A new VM: nothing to ask systemd.
    assert_eq!(host.sd.calls().len(), asked, "{:?}", host.sd.calls());

    // Every disk byte for byte, still sparse.
    let theirs = host.current("dev");
    let mine = host.current("dev2");
    same(&mine, &theirs);
    assert!(
        allocated(&mine) < 20 * MIB,
        "{} allocated",
        allocated(&mine)
    );
    for c in [&first, &second] {
        let mine = store.disk("dev2", c.id).unwrap();
        same(&mine, &store.disk("dev", c.id).unwrap());
        assert_eq!(fs::metadata(&mine).unwrap().len(), 2 * GIB);
        assert!(
            allocated(&mine) < 20 * MIB,
            "{} allocated",
            allocated(&mine)
        );
    }

    // My checkpoints under the same ids, times and comments, and nothing
    // else: not the agent's or the run's, and nothing for undo.
    let listed = store.checkpoints("dev2").unwrap();
    let rows: Vec<_> = listed
        .iter()
        .map(|c| (c.id, c.auto, c.agent, c.run, c.current))
        .collect();
    assert_eq!(
        rows,
        [
            (now, true, false, false, true),
            (second.id, false, false, false, false),
            (first.id, false, false, false, false),
        ]
    );
    let kept = |c: &Checkpoint| (c.id, c.time, c.comment.clone());
    assert_eq!(kept(&listed[1]), kept(&second));
    assert_eq!(kept(&listed[2]), kept(&first));
    assert_eq!(listed[0].comment.as_deref(), Some("import of dev's export"));
    let undo = host.runner(&["undo", "dev2"]).output().unwrap();
    assert!(!undo.status.success());
    assert!(text(&undo.stderr).contains("no earlier generation"));

    // Its settings, a slot of its own, and claude only, as a fork gets.
    assert_eq!(store.config("dev2").unwrap(), store.config("dev").unwrap());
    let slot = |vm| store.net(vm).unwrap().slot;
    assert_ne!(slot("dev2"), slot("dev"));
    assert_eq!(store.creds("dev2").unwrap(), [Cred::Claude]);
    assert!(
        err.contains("dev had claude github: `runner creds dev2 claude github`"),
        "{err}"
    );
    assert!(store.sleeping("dev2").unwrap().is_none());
    let dir = host.root().join("vms/dev2");
    assert!(!dir.join("runs").exists());
    assert!(!dir.join("creds").exists());
    assert_eq!(host.staged(), Vec::<String>::new());
}

#[test]
fn creds_on_import_set_the_allowlist_and_stdin_takes_an_export() {
    let host = Host::new();
    dev(&host);
    let tar = host.path("dev.tar");
    host.exported(&["dev"], &tar);
    let (_, err) = host.imported(&["dev2", "-", "--creds", "claude,github"], Some(&tar));
    let store = host.store();
    assert_eq!(store.creds("dev2").unwrap(), [Cred::Claude, Cred::Github]);
    assert!(!err.contains("runner creds"), "{err}");
    same(&host.current("dev2"), &host.current("dev"));
}

#[test]
fn a_name_taken_here_is_refused_before_the_stream_is_read() {
    let host = Host::new();
    let src = host.path("src.ext4");
    sparse(&src, MIB, &[(0, MIB)], 1);
    host.ok(&["import", "dev", &src.display().to_string()]);
    let mut child = host
        .runner(&["import", "dev", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Held open and never written: a read would wait for ever.
    let _stdin = child.stdin.take().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() > deadline {
            child.kill().unwrap();
            panic!("import read stdin");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let out = child.wait_with_output().unwrap();
    assert!(!out.status.success());
    let err = text(&out.stderr);
    assert!(err.contains("a VM named dev already exists"), "{err}");
}

#[test]
fn replace_takes_only_a_raw_disk() {
    let host = Host::new();
    dev(&host);
    let tar = host.path("dev.tar");
    host.exported(&["dev"], &tar);
    let before = host.store().get("dev").unwrap();
    let out = host.import(&["--replace", "dev", &tar.display().to_string()], None);
    assert!(!out.status.success());
    let err = text(&out.stderr);
    assert!(err.contains("an export comes in only as a new VM"), "{err}");
    assert_eq!(host.store().get("dev").unwrap(), before);
}

// A tar's entries, each its header and bytes, in order.
fn apart(bytes: &[u8]) -> Vec<(tar::Header, Vec<u8>)> {
    let mut archive = tar::Archive::new(bytes);
    let entries = archive.entries().unwrap();
    entries
        .map(|e| {
            let mut e = e.unwrap();
            let mut data = Vec::new();
            e.read_to_end(&mut data).unwrap();
            (e.header().clone(), data)
        })
        .collect()
}

// And back together, as headers say, ended as tar ends.
fn together(entries: &[(tar::Header, Vec<u8>)]) -> Vec<u8> {
    let mut tar = tar::Builder::new(Vec::new());
    for (header, data) in entries {
        tar.append(header, data.as_slice()).unwrap();
    }
    tar.into_inner().unwrap()
}

// A small export of dev taken apart: a dense 64 KiB disk, so a plain entry,
// and one checkpoint of mine.
fn small(host: &Host) -> Vec<(tar::Header, Vec<u8>)> {
    let src = host.path("small.ext4");
    sparse(&src, 64 << 10, &[(0, 64 << 10)], 3);
    host.ok(&["import", "dev", &src.display().to_string()]);
    host.store().checkpoint("dev", Some("mine")).unwrap();
    let tar = host.path("small.tar");
    host.exported(&["dev"], &tar);
    let entries = apart(&fs::read(&tar).unwrap());
    let names: Vec<_> = entries.iter().map(|(h, _)| text(&h.path_bytes())).collect();
    assert_eq!(
        names,
        [
            "dev/manifest.json",
            "dev/disk.ext4",
            "dev/checkpoints/1.ext4"
        ]
    );
    entries
}

// The entry `entries[i]` with its manifest's JSON edited by `edit`.
fn edit_manifest(entries: &mut [(tar::Header, Vec<u8>)], edit: impl Fn(String) -> String) {
    let (header, data) = &mut entries[0];
    let json = edit(text(data));
    *data = json.into_bytes();
    header.set_size(data.len() as u64);
    header.set_cksum();
}

// Its name, as raw bytes, past what the tar crate lets a builder write.
fn rename(header: &mut tar::Header, name: &str) {
    let field = &mut header.as_old_mut().name;
    field.fill(0);
    field[..name.len()].copy_from_slice(name.as_bytes());
    header.set_cksum();
}

#[test]
fn a_small_export_taken_apart_and_put_back_imports() {
    let host = Host::new();
    let entries = small(&host);
    let path = host.path("again.tar");
    fs::write(&path, together(&entries)).unwrap();
    host.imported(&["dev2", "-"], Some(&path));
    same(&host.current("dev2"), &host.current("dev"));
}

#[test]
fn an_unknown_format_is_refused() {
    let host = Host::new();
    let mut entries = small(&host);
    edit_manifest(&mut entries, |j| {
        let to = j.replacen(&format!("\"format\": {FORMAT}"), "\"format\": 99", 1);
        assert_ne!(to, j);
        to
    });
    let err = host.refused_import(&together(&entries));
    assert!(err.contains("format 99"), "{err}");
    assert!(err.contains(&format!("reads format {FORMAT}")), "{err}");
}

#[test]
fn an_entry_the_manifest_does_not_name_is_refused() {
    let host = Host::new();
    let entries = small(&host);
    let mut extra = entries[1].clone();
    rename(&mut extra.0, "dev/extra");

    // After the last disk.
    let mut after = entries.clone();
    after.push(extra.clone());
    let err = host.refused_import(&together(&after));
    assert!(err.contains("\"dev/extra\""), "{err}");
    assert!(err.contains("the manifest doesn't name"), "{err}");

    // Between two it names.
    let mut between = entries.clone();
    between.insert(2, extra);
    let err = host.refused_import(&together(&between));
    assert!(err.contains("\"dev/extra\""), "{err}");

    // Out of order.
    let mut swapped = entries.clone();
    swapped.swap(1, 2);
    host.refused_import(&together(&swapped));
}

#[test]
fn a_dot_dot_name_is_refused() {
    let host = Host::new();
    let mut entries = small(&host);
    rename(&mut entries[1].0, "dev/../dev/disk.ext4");
    let err = host.refused_import(&together(&entries));
    assert!(err.contains("\"dev/../dev/disk.ext4\""), "{err}");
    // Nor in the manifest.
    let mut entries = small(&Host::new());
    edit_manifest(&mut entries, |j| {
        j.replace("\"dev/disk.ext4\"", "\"../disk.ext4\"")
    });
    rename(&mut entries[1].0, "../disk.ext4");
    host.refused_import(&together(&entries));
}

#[test]
fn a_symlink_or_a_hard_link_is_refused() {
    let host = Host::new();
    let entries = small(&host);
    for kind in [tar::EntryType::Symlink, tar::EntryType::Link] {
        let mut linked = entries.clone();
        let (header, data) = &mut linked[1];
        header.set_entry_type(kind);
        header.set_link_name("/etc/passwd").unwrap();
        header.set_size(0);
        header.set_cksum();
        data.clear();
        let err = host.refused_import(&together(&linked));
        assert!(err.contains("dev/disk.ext4"), "{err}");
        assert!(err.contains("isn't a file"), "{err}");
    }
}

#[test]
fn a_disk_of_another_size_than_the_manifest_says_is_refused() {
    let host = Host::new();
    let mut entries = small(&host);
    let (header, data) = &mut entries[1];
    data.truncate(32 << 10);
    header.set_size(data.len() as u64);
    header.set_cksum();
    let err = host.refused_import(&together(&entries));
    assert!(err.contains("dev/disk.ext4"), "{err}");
    assert!(err.contains(&format!("{} bytes", 64 << 10)), "{err}");
}

#[test]
fn a_stream_cut_at_an_entry_or_before_tar_s_end_is_refused() {
    let host = Host::new();
    let entries = small(&host);
    // Without the last checkpoint, ended as tar ends: GNU tar takes it.
    let err = host.refused_import(&together(&entries[..2]));
    assert!(err.contains("dev/checkpoints/1.ext4"), "{err}");
    // Each entry whole, but not tar's end: a cut at each block of it.
    let whole = together(&entries);
    for less in [1024, 512, 1] {
        let err = host.refused_import(&whole[..whole.len() - less]);
        assert!(err.contains("cut off"), "{less}: {err}");
    }
    // Something past the end that isn't tar's zeros.
    let mut more = whole.clone();
    more.extend_from_slice(b"more");
    host.refused_import(&more);
    // Zeros past it, as GNU tar pads a record, are fine.
    more.truncate(whole.len());
    more.resize(whole.len() + 10240, 0);
    let path = host.path("padded.tar");
    fs::write(&path, &more).unwrap();
    host.imported(&["dev2", "-"], Some(&path));
}

#[test]
fn a_kernel_this_host_lacks_is_kept_and_named() {
    let host = Host::new();
    let mut entries = small(&host);
    edit_manifest(&mut entries, |j| {
        let to = j.replacen(
            "\"config\": {}",
            "\"config\": {\"kernel\": \"/no/such/vmlinux\", \"cpus\": 4}",
            1,
        );
        assert_ne!(to, j);
        to
    });
    let path = host.path("kernel.tar");
    fs::write(&path, together(&entries)).unwrap();
    let (_, err) = host.imported(&["dev2", "-"], Some(&path));
    assert!(err.contains("/no/such/vmlinux isn't on this host"), "{err}");
    assert!(err.contains("won't boot until it is"), "{err}");
    let config = host.store().config("dev2").unwrap();
    assert_eq!(config.kernel, Some(PathBuf::from("/no/such/vmlinux")));
    assert_eq!(config.cpus, Some(4));
    // Its settings still change: only its boot needs the kernel.
    assert_eq!(host.ok(&["idle", "dev2", "30m"]), "dev2  idle 30m\n");
    assert_eq!(
        host.ok(&["url", "dev2", "off"]),
        "dev2's URL is off: `runner url dev2 on` turns it on\n"
    );
    let config = host.store().config("dev2").unwrap();
    assert_eq!(config.kernel, Some(PathBuf::from("/no/such/vmlinux")));
    assert!(config.url.unwrap().off);
    // A fork is a create, which wants a kernel it can boot: one named here.
    let out = host
        .runner(&["create", "dev3", "--from", "dev2"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = text(&out.stderr);
    assert!(err.contains("no kernel file at /no/such/vmlinux"), "{err}");
    let here = host.path("vmlinux");
    fs::write(&here, b"kernel").unwrap();
    // --kernel takes the path canonical, as a tmpdir through a link isn't.
    let here = fs::canonicalize(here).unwrap();
    let arg = here.display().to_string();
    host.ok(&["create", "dev3", "--from", "dev2", "--kernel", &arg]);
    let config = host.store().config("dev3").unwrap();
    assert_eq!(config.kernel, Some(here));
    assert_eq!(config.cpus, Some(4));
}

#[test]
fn a_raw_disk_from_a_path_or_stdin_still_imports_as_one() {
    let host = Host::new();
    // A disk whose first block looks nothing like a tar header.
    let src = host.path("raw.ext4");
    sparse(&src, MIB, &[(0, MIB)], 2);
    host.ok(&["import", "base", &src.display().to_string()]);
    same(&host.current("base"), &src);
    host.imported(&["raw", "-"], Some(&src));
    same(&host.current("raw"), &src);
    // A raw disk takes --creds too.
    host.imported(&["raw2", "-", "--creds", "github"], Some(&src));
    assert_eq!(host.store().creds("raw2").unwrap(), [Cred::Github]);
}

// A real export of dev: disks of 96 blocks of 4 KiB with six extents, past
// the four a sparse header holds, and two checkpoints of mine.
fn real() -> &'static [u8] {
    static BYTES: OnceLock<Vec<u8>> = OnceLock::new();
    BYTES.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("root"));
        let src = dir.path().join("src.ext4");
        let b = 4096;
        let extents = [
            (0, b),
            (2 * b, b),
            (5 * b, 2 * b),
            (20 * b, b),
            (40 * b, b),
            (95 * b, b),
        ];
        sparse(&src, 96 * b, &extents, 4);
        store.import("dev", &src).unwrap();
        store.checkpoint("dev", Some("one")).unwrap();
        let run = store.begin_run("dev").unwrap();
        sparse_over(&run.disk, &[(60 * b, b)], 6);
        run.commit().unwrap();
        store.checkpoint("dev", None).unwrap();
        let mut out = Vec::new();
        export::write("dev", store.open_disks("dev", true).unwrap(), &mut out).unwrap();
        out
    })
}

#[test]
fn a_real_export_imports_whole() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().join("root"));
    let got = import::import(&store, "dev2", real(), None).unwrap();
    assert_eq!(got.manifest.checkpoints.len(), 2);
    assert_eq!(store.checkpoints("dev2").unwrap().len(), 3);
}

#[derive(Debug, Clone)]
enum Harm {
    Cut(usize),
    Flip(usize, u8),
}

fn harm() -> impl Strategy<Value = Harm> {
    let len = real().len();
    let flip =
        |range: std::ops::Range<usize>| (range, 1..=255u8).prop_map(|(at, x)| Harm::Flip(at, x));
    prop_oneof![
        (0..len).prop_map(Harm::Cut),
        flip(0..len),
        // The manifest and the first disk's headers.
        flip(0..4096),
        // The last disk's tail and tar's end.
        flip(len - 2048..len),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(160))]

    // Any cut fails. A flipped byte fails, or lands in a disk's data or
    // tar's padding and imports: format 1 has no checksum of the data.
    // Either way, a failure leaves no VM and nothing staged.
    #[test]
    fn a_cut_or_a_flipped_byte_fails_whole_or_imports_whole(harm in harm()) {
        let mut bytes = real().to_vec();
        match harm {
            Harm::Cut(n) => bytes.truncate(n),
            Harm::Flip(at, x) => bytes[at] ^= x,
        }
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("root"));
        let got = import::import(&store, "dev2", bytes.as_slice(), None);
        if matches!(harm, Harm::Cut(_)) {
            prop_assert!(got.is_err(), "a cut imported");
        }
        let left: Vec<String> = match fs::read_dir(dir.path().join("root/vms")) {
            Ok(entries) => entries
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect(),
            Err(_) => Vec::new(),
        };
        match got {
            Ok(_) => {
                prop_assert_eq!(left, ["dev2"]);
                prop_assert_eq!(store.checkpoints("dev2").unwrap().len(), 3);
            }
            Err(_) => prop_assert!(left.is_empty(), "{:?} left", left),
        }
    }
}

// The CLI's look at the first block: a header with a byte flipped is still
// an export, refused, not taken for a disk.
#[test]
fn an_export_whose_first_header_has_a_byte_flipped_is_refused_not_taken_for_a_disk() {
    let host = Host::new();
    let mut bytes = together(&small(&host));
    // Its mtime: the checksum no longer adds up.
    bytes[140] ^= 1;
    let err = host.refused_import(&bytes);
    assert!(err.contains("checksum"), "{err}");
}

#[test]
fn a_compressed_stream_is_refused() {
    let host = Host::new();
    let tar = together(&small(&host));
    let gzip = [&[0x1f, 0x8b, 0x08, 0x00][..], &tar].concat();
    let err = host.refused_import(&gzip);
    assert!(err.contains("stdin is gzip-compressed"), "{err}");
    assert!(
        err.contains("zstd -dc dev.tar.zst | runner import dev2 -"),
        "{err}"
    );
    let zstd = [&[0x28, 0xb5, 0x2f, 0xfd][..], &tar].concat();
    let path = host.path("dev.tar.zst");
    fs::write(&path, zstd).unwrap();
    let out = host.import(&["dev2", &path.display().to_string()], None);
    assert!(!out.status.success());
    let err = text(&out.stderr);
    assert!(err.contains("dev.tar.zst is zstd-compressed"), "{err}");
    assert!(host.store().get("dev2").is_err());
}

#[test]
fn a_checkpoint_id_past_any_vm_s_is_refused() {
    let host = Host::new();
    let mut entries = small(&host);
    let id = u64::MAX - 1;
    edit_manifest(&mut entries, |j| {
        let to = j
            .replace("\"id\": 1,", &format!("\"id\": {id},"))
            .replace("checkpoints/1.ext4", &format!("checkpoints/{id}.ext4"));
        assert_ne!(to, j);
        to
    });
    rename(&mut entries[2].0, &format!("dev/checkpoints/{id}.ext4"));
    let err = host.refused_import(&together(&entries));
    assert!(err.contains(&format!("checkpoint {id}, past any")), "{err}");
}

#[test]
fn creds_chosen_on_import_get_no_hint() {
    let host = Host::new();
    dev(&host);
    let tar = host.path("dev.tar");
    host.exported(&["dev"], &tar);
    let (_, err) = host.imported(&["dev2", "-", "--creds", "github"], Some(&tar));
    assert_eq!(host.store().creds("dev2").unwrap(), [Cred::Github]);
    assert!(!err.contains("runner creds"), "{err}");
}
