// `runner rebase` and MCP's rebase_vm (#157): a VM onto the current base.
// Its new disk is a copy of base's, so it gets base's system, runner-guest
// and Claude Code, with what's its own copied in from its current disk by
// src/graft.rs: /home, /root, its machine id, and the runner's services,
// their units and their links in each *.wants directory (src/services.rs).
// Not /etc/hostname: base ships it empty, and each boot's `ip=` names the
// VM. Claude Code's install under /home/agent is base's, which pins its
// version (images/base/Dockerfile), so it's copied back in from base's disk
// after the VM's /home, unless the VM's /home has a link on the way to it,
// which graft.rs won't write through: the VM keeps its own then, and the
// output says so. The rest of /home is the VM's.
//
// What's lost: apt packages and anything else outside what's kept, users
// added with useradd (their files keep their numeric owners), crontabs and
// /var, and what graft.rs skips, which is named. The new disk is a new
// generation, so `runner undo` brings the old one back. The store does the
// rest (Store::rebase): the VM's lock, which refuses one that's up, and
// base's current disk, which must be one an import made.
use crate::changes::{self, Budget, Disk, Limits};
use crate::graft::{self, Skip, Why};
use crate::services::{self, UNIT_DIR};
use crate::store::{self, Store, Vm};
use ext4_view::{Ext4Error, PathBuf as InPathBuf};
use rmcp::schemars;
use serde::Serialize;
use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

/// What a rebased VM keeps of its own disk, besides its services.
pub const KEPT: &[&str] = &["/home", "/root", "/etc/machine-id"];

/// What under KEPT comes from base all the same: its Claude Code.
pub const FROM_BASE: &[&str] = &[
    "/home/agent/.local/bin/claude",
    "/home/agent/.local/share/claude",
];

// How far each copy may go. A whole /home, toolchains and all, is far more
// than what a run changes, which changes.rs's ten minutes are for.
const LIMITS: Limits = Limits {
    time: Duration::from_secs(30 * 60),
    ..changes::LIMITS
};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(
        "{0} is up, or a run or another runner command holds it: stop it first: runner stop {0}"
    )]
    Busy(String),
    #[error(transparent)]
    Store(store::Error),
    #[error(transparent)]
    Graft(#[from] graft::Error),
}

impl From<store::Error> for Error {
    fn from(e: store::Error) -> Error {
        match e {
            store::Error::Busy(vm) => Error::Busy(vm),
            e => Error::Store(e),
        }
    }
}

impl From<changes::Error> for Error {
    fn from(e: changes::Error) -> Error {
        Error::Graft(e.into())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// What a rebase did, as `runner rebase` says it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct Rebased {
    /// The VM, at its new generation.
    pub vm: Vm,
    /// The generation of base it's on now.
    pub base_generation: u64,
    /// What it kept of its own disk, besides its services.
    pub kept: Vec<String>,
    /// The services it kept, by name.
    pub services: Vec<String>,
    /// What wasn't copied, so is gone too, in the order the copy met it.
    pub skipped: Vec<Skip>,
    /// How many more weren't copied than are listed.
    pub more: u64,
    /// Where the VM's /home has something other than a directory on the
    /// way to base's Claude Code, like a dotfile manager's link, so the VM
    /// keeps its own. None when base's came.
    pub own_claude: Option<String>,
}

/// Gives the stopped VM `name` the current base's system as a new disk
/// generation, keeping KEPT and its services, with base's Claude Code.
/// `Busy` while it's up or a run on it hasn't ended, as undo is.
pub fn rebase(store: &Store, name: &str) -> Result<Rebased> {
    let (vm, base_generation, (services, copied, own_claude)) =
        store.rebase(name, |from: &Path, base: &Path, to: &Path| -> Result<_> {
            let (mine, services) = units(from)?;
            // base's own, which the VM's take the place of, gone if it has none.
            let (theirs, _) = units(base)?;
            let mut paths: Vec<String> = KEPT.iter().map(|p| p.to_string()).collect();
            paths.extend(mine.union(&theirs).cloned());
            let paths: Vec<&str> = paths.iter().map(String::as_str).collect();
            let mut copied = graft::copy(from, to, &paths, LIMITS)?;
            let own = in_the_way(to)?;
            if own.is_none() {
                let claude = graft::copy(base, to, FROM_BASE, LIMITS)?;
                copied.skipped.extend(claude.skipped);
                copied.more += claude.more;
            }
            Ok((services, copied, own))
        })?;
    Ok(Rebased {
        vm,
        base_generation,
        kept: KEPT.iter().map(|p| p.to_string()).collect(),
        services,
        skipped: copied.skipped,
        more: copied.more,
        own_claude,
    })
}

/// What `runner rebase` prints.
pub fn said(r: &Rebased) -> String {
    let name = &r.vm.name;
    let mut out = format!(
        "{name} is on base's generation {} now: generation {} is current.\n",
        r.base_generation, r.vm.generation
    );
    let mut kept = r.kept.clone();
    if !r.services.is_empty() {
        let n = r.services.len();
        let s = if n == 1 { "" } else { "s" };
        kept.push(format!("{n} service{s} ({})", r.services.join(", ")));
    }
    out += &format!("Kept {}.\n", listed(&kept));
    if let Some(at) = &r.own_claude {
        out += &format!("Claude Code is {name}'s own, not base's: {at} isn't a directory.\n");
    }
    // Quoted: a name the copy skipped can hold a newline.
    for skip in &r.skipped {
        out += &format!("Not copied, so gone: {:?}, {}.\n", skip.path, why(skip.why));
    }
    if r.more > 0 {
        out += &format!("And {} more not copied.\n", r.more);
    }
    out += &format!(
        "Anything installed outside those, like apt packages, is gone, and so are users added with useradd, crontabs and /var: runner undo {name} brings it back.\n"
    );
    out
}

fn listed(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

fn why(why: Why) -> &'static str {
    match why {
        Why::Name => "as its name holds a newline, a carriage return or a double quote",
        Why::Long => "as its path is too long for debugfs",
        Why::Special => "as it's a socket, a device or a fifo",
        Why::Unreadable => "as ext4-view can't read it",
    }
}

// The first of FROM_BASE's parents that `to` has as something other than a
// directory, like a dotfile manager's link, which graft.rs won't write
// through: the VM keeps its own Claude Code then. One whose inode won't
// read counts too.
fn in_the_way(to: &Path) -> Result<Option<String>> {
    let disk = Disk::load(to)?;
    for path in FROM_BASE {
        let (parent, _) = path.rsplit_once('/').expect("an absolute path");
        let mut at = String::new();
        for name in parent.split('/').skip(1) {
            at = format!("{at}/{name}");
            let found = InPathBuf::try_from(at.as_str()).expect("a plain path");
            match disk.fs.symlink_metadata(&found) {
                Ok(m) if m.is_dir() => {}
                Err(Ext4Error::NotFound) => break,
                _ => return Ok(Some(at)),
            }
        }
    }
    Ok(None)
}

// The runner's services on `disk`: the paths of their units and of each
// link to one in a *.wants directory, and their names, in name order. Only
// names services.rs makes count, and only *.wants directories named as
// systemd names a unit, so each path is one debugfs takes. An entry whose
// inode can't be read is still the service's: graft.rs skips it, naming it.
fn units(disk: &Path) -> Result<(BTreeSet<String>, Vec<String>)> {
    let never = || false;
    let mut budget = Budget::new(LIMITS, "reading", &never);
    let disk = Disk::load(disk)?;
    let dir = InPathBuf::try_from(UNIT_DIR).expect("a plain path");
    let entries = match disk.entries_as_read(&dir, &mut budget) {
        Err(changes::Error::Read {
            source: Ext4Error::NotFound | Ext4Error::NotADirectory,
            ..
        }) => return Ok(Default::default()),
        r => r?,
    };
    let (mut paths, mut names) = (BTreeSet::new(), Vec::new());
    for (file, m) in entries {
        let Ok(file) = String::from_utf8(file) else {
            continue;
        };
        if let Some(name) = service(&file) {
            paths.insert(format!("{UNIT_DIR}/{file}"));
            names.push(name);
            continue;
        }
        let wants = file.ends_with(".wants") && file.bytes().all(unit_byte);
        if !(wants && m.is_ok_and(|m| m.is_dir())) {
            continue;
        }
        for (link, _) in disk.entries_as_read(&dir.join(&file), &mut budget)? {
            if let Some(link) = String::from_utf8(link)
                .ok()
                .filter(|l| service(l).is_some())
            {
                paths.insert(format!("{UNIT_DIR}/{file}/{link}"));
            }
        }
    }
    Ok((paths, names))
}

// The service a unit file is for, if it's one services.rs makes.
fn service(file: &str) -> Option<String> {
    let name = file
        .strip_prefix(services::PREFIX)?
        .strip_suffix(".service")?;
    services::check_name(name).ok()?;
    Some(name.to_string())
}

// What systemd takes in a unit's name.
fn unit_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b":-_.\\@".contains(&b)
}
