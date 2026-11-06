// A listed VM's place in the host's rules (host/nat.nft), which its boot
// keeps with nft, on the CAP_NET_ADMIN it has for the VM's tap. The rules'
// set `listed` holds the taps of VMs with a list of names, and the forward
// chain lets a listed tap's packets through only to an address paired with
// that tap in the set `reach`, besides what's already open: never to the
// private ranges, and not to port 53 off the host either.
//
// The VM's DNS hands on the addresses of each answer it lets through
// (dns::Policy::hand_on), and each goes in as a pair for the answer's TTL,
// but at least MIN_HOLD, since programs keep answers past it, and at most
// MAX_HOLD, past which nft refuses it. The DNS reads the VM's list each
// second, and the fence follows it (Fence::track): a list made puts the tap
// in, one dropped takes it out with its pairs, and a name taken off takes
// the pairs out, so the VM's names are looked up again. Boot looks every few
// seconds whether the tap is still in (Fence::look), since a firewall reload
// empties the sets, and puts it back with its pairs.
//
// Every change is one nft transaction, so packets see the rules before it or
// after, never halfway.
use crate::dns::{Address, List};
use crate::host;
use crate::store::{Error, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io::{self, Write};
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// The set of listed VMs' taps, in host/nat.nft's table `ip runner`.
pub const LISTED: &str = "listed";
/// The set of tap . address pairs a listed tap may reach, each with a timeout.
pub const REACH: &str = "reach";
/// The least time an address goes in for, whatever its TTL.
pub const MIN_HOLD: Duration = Duration::from_secs(10 * 60);
/// The most: DNS allows TTLs of decades, and nft refuses timeouts that long.
pub const MAX_HOLD: Duration = Duration::from_secs(7 * 24 * 60 * 60);
// An answer that would keep a pair in at most this much longer leaves it
// as it is, so a name looked up again and again doesn't run nft each time.
const SLACK: Duration = Duration::from_secs(60);

/// A VM's tap in the host's rules, held to its list of names, or out of
/// them for a VM with none. Boot shares it with its DNS's threads.
pub struct Fence {
    nft: PathBuf,
    name: String,
    tap: String,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// The list the tap is held to, while it's in `listed`.
    list: Option<List>,
    /// Each address paired with the tap, and until when.
    pairs: BTreeMap<Ipv4Addr, Instant>,
    /// Ended: nothing goes in again.
    ended: bool,
    /// The last failure noted, so one that repeats is noted once.
    said: Option<String>,
}

impl Fence {
    /// The tap as the VM's boot starts: in `listed` if the VM has a list,
    /// with no pairs left from a boot that was killed, else out of it, where
    /// such a boot may have left it. An unlisted VM's tap is taken out only
    /// if it's there, so an unlisted VM boots on rules without the sets.
    pub fn start(nft: &Path, name: &str, tap: &str, list: Option<&List>) -> Result<Fence> {
        let fence = Fence {
            nft: nft.to_path_buf(),
            name: name.to_string(),
            tap: tap.to_string(),
            state: Mutex::new(State::default()),
        };
        match list {
            Some(_) => fence.follow(list)?,
            None if fence.has_tap() => fence.take_out(&mut fence.state())?,
            None => {}
        }
        Ok(fence)
    }

    /// Holds the tap to `list`, or lets it go with None. Nothing changes for
    /// the same list, or one with names added; a name taken off takes the
    /// tap's pairs out. A change that fails is tried again at the next call.
    pub fn follow(&self, list: Option<&List>) -> Result<()> {
        let mut state = self.state();
        if state.ended {
            return Ok(());
        }
        match (&state.list, list) {
            (None, None) => {}
            (None, Some(_)) => self.put_in(&mut state)?,
            (Some(_), None) => self.take_out(&mut state)?,
            (Some(was), Some(now)) => {
                if was.names().iter().any(|n| !now.names().contains(n)) {
                    self.flush(&mut state)?;
                }
            }
        }
        state.list = list.cloned();
        state.said = None;
        Ok(())
    }

    /// `follow`, noting a failure once instead of returning it: what the
    /// VM's DNS does each time it reads the list.
    pub fn track(&self, list: Option<&List>) {
        if let Err(e) = self.follow(list) {
            self.say(format!(
                "{}'s list of names isn't in the host's rules yet, and boot tries again \
                 each second: {e}",
                self.name
            ));
        }
    }

    /// Pairs each address with the tap, for its TTL, at least MIN_HOLD and
    /// at most MAX_HOLD, unless it's in for about as long already. Only
    /// while the tap is held to a list. A failure is noted.
    pub fn hand_on(&self, addresses: &[Address]) {
        let mut state = self.state();
        if state.list.is_none() {
            return;
        }
        let now = Instant::now();
        state.pairs.retain(|_, until| *until > now);
        let mut fresh: BTreeMap<Ipv4Addr, Duration> = BTreeMap::new();
        for a in addresses {
            let hold = a.ttl.clamp(MIN_HOLD, MAX_HOLD);
            let kept = state
                .pairs
                .get(&a.ip)
                .is_some_and(|until| *until + SLACK >= now + hold);
            if !kept {
                let longest = fresh.entry(a.ip).or_default();
                *longest = (*longest).max(hold);
            }
        }
        if fresh.is_empty() {
            return;
        }
        let timed = self.timed(fresh.iter().map(|(ip, hold)| (*ip, *hold)));
        let keys = self.keys(fresh.keys().copied());
        // An add alone leaves a pair that's in already with the time it had.
        let script = format!(
            "add element {table} {REACH} {{ {timed} }}; delete element {table} {REACH} {{ {keys} }}; \
             add element {table} {REACH} {{ {timed} }}",
            table = table()
        );
        match self.nft(&[&script]) {
            Ok(_) => {
                for (ip, hold) in fresh {
                    state.pairs.insert(ip, now + hold);
                }
            }
            Err(e) => {
                drop(state);
                self.say(format!(
                    "{}'s DNS answered, but the host's rules didn't take the addresses, so \
                     it can't reach them: {e}",
                    self.name
                ));
            }
        }
    }

    /// Puts the tap back in `listed` if the rules have lost it, as a
    /// firewall reload empties the sets, with each pair for what's left of
    /// its time.
    pub fn look(&self) {
        let mut state = self.state();
        if state.list.is_none() || self.has_tap() {
            return;
        }
        let now = Instant::now();
        state.pairs.retain(|_, until| *until > now);
        let mut script = format!("add element {} {LISTED} {{ \"{}\" }}", table(), self.tap);
        if !state.pairs.is_empty() {
            let left = state.pairs.iter().map(|(ip, until)| (*ip, *until - now));
            script += &format!(
                "; add element {} {REACH} {{ {} }}",
                table(),
                self.timed(left)
            );
        }
        let pairs = state.pairs.len();
        drop(state);
        match self.nft(&[&script]) {
            Ok(_) => note(format_args!(
                "{}'s tap was out of the host's rules, as after a firewall reload: it's back \
                 in, with {pairs} addresses",
                self.name
            )),
            Err(e) => self.say(format!(
                "{} isn't held to its list of names: the host's rules lost its tap, and \
                 putting it back failed: {e}",
                self.name
            )),
        }
    }

    /// Takes the tap and its pairs out, if it's in, as the VM goes down.
    /// From here nothing goes in, so a query that outlasts the boot hands on
    /// nothing. A failure is noted: the next boot in the tap's slot takes it
    /// out.
    pub fn end(&self) {
        let mut state = self.state();
        state.ended = true;
        if state.list.is_none() {
            return;
        }
        state.list = None;
        if let Err(e) = self.take_out(&mut state) {
            drop(state);
            note(format_args!(
                "{}'s tap stays in the host's rules until its slot's next boot: {e}",
                self.name
            ));
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    // Into `listed`, with none of the pairs a boot that was killed left: a
    // new VM in the slot mustn't reach what an old one could.
    fn put_in(&self, state: &mut State) -> Result<()> {
        let stale = self.pairs_in_rules()?;
        let mut script = self.forget(&stale);
        script.push(format!(
            "add element {} {LISTED} {{ \"{}\" }}",
            table(),
            self.tap
        ));
        self.nft(&[&script.join("; ")])?;
        state.pairs.clear();
        Ok(())
    }

    // Out of `listed`, and its pairs out of `reach`: those the rules list,
    // or if they can't, those this fence put in. Added first, so a tap
    // that's out already doesn't fail the rest.
    fn take_out(&self, state: &mut State) -> Result<()> {
        let mut pairs: BTreeSet<Ipv4Addr> = state.pairs.keys().copied().collect();
        pairs.extend(self.pairs_in_rules().unwrap_or_default());
        let mut script = self.forget(&pairs);
        let tap = format!("{{ \"{}\" }}", self.tap);
        script.push(format!(
            "add element {table} {LISTED} {tap}; delete element {table} {LISTED} {tap}",
            table = table()
        ));
        self.nft(&[&script.join("; ")])?;
        state.pairs.clear();
        Ok(())
    }

    // The tap's pairs out, so each name is looked up, and put in, again.
    fn flush(&self, state: &mut State) -> Result<()> {
        let mut pairs: BTreeSet<Ipv4Addr> = state.pairs.keys().copied().collect();
        pairs.extend(self.pairs_in_rules()?);
        let script = self.forget(&pairs);
        if !script.is_empty() {
            self.nft(&[&script.join("; ")])?;
        }
        state.pairs.clear();
        Ok(())
    }

    // The commands that take these pairs out of `reach`, added first so one
    // that has run out since doesn't fail the rest. None for no pairs.
    fn forget<'a>(&self, pairs: impl IntoIterator<Item = &'a Ipv4Addr>) -> Vec<String> {
        let pairs: Vec<Ipv4Addr> = pairs.into_iter().copied().collect();
        if pairs.is_empty() {
            return Vec::new();
        }
        let timed = self.timed(pairs.iter().map(|ip| (*ip, Duration::from_secs(1))));
        let keys = self.keys(pairs.iter().copied());
        vec![
            format!("add element {} {REACH} {{ {timed} }}", table()),
            format!("delete element {} {REACH} {{ {keys} }}", table()),
        ]
    }

    // Whether the tap is in `listed`. No, too, if nft or the set isn't there.
    fn has_tap(&self) -> bool {
        let get = format!("get element {} {LISTED} {{ \"{}\" }}", table(), self.tap);
        self.nft(&[&get]).is_ok()
    }

    // The addresses `reach` pairs with this tap, as nft lists them.
    fn pairs_in_rules(&self) -> Result<BTreeSet<Ipv4Addr>> {
        let [family, name] = host::NAT_TABLE;
        let listed = self.nft(&["-j", "list", "set", family, name, REACH])?;
        paired(&listed, &self.tap).ok_or_else(|| Error::Command {
            cmd: format!("{} -j list set {family} {name} {REACH}", self.nft.display()),
            why: "its output isn't the JSON nft lists a set as".to_string(),
        })
    }

    fn keys(&self, ips: impl Iterator<Item = Ipv4Addr>) -> String {
        let keys: Vec<String> = ips.map(|ip| format!("\"{}\" . {ip}", self.tap)).collect();
        keys.join(", ")
    }

    // Pairs with their timeouts, in whole seconds, one at least.
    fn timed(&self, pairs: impl Iterator<Item = (Ipv4Addr, Duration)>) -> String {
        let timed: Vec<String> = pairs
            .map(|(ip, hold)| {
                let secs = hold.as_secs_f64().ceil().max(1.0) as u64;
                format!("\"{}\" . {ip} timeout {secs}s", self.tap)
            })
            .collect();
        timed.join(", ")
    }

    // Runs nft quietly and returns what it printed; a failure says what was
    // run and the first line nft said.
    fn nft(&self, args: &[&str]) -> Result<String> {
        let failed = |why: String| Error::Command {
            cmd: format!("{} {}", self.nft.display(), args.join(" ")),
            why,
        };
        let out = Command::new(&self.nft)
            .args(args)
            .output()
            .map_err(|e| failed(e.to_string()))?;
        if out.status.success() {
            return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
        }
        let said = String::from_utf8_lossy(&out.stderr);
        let first = said.lines().map(str::trim).find(|l| !l.is_empty());
        Err(failed(
            first.map_or_else(|| out.status.to_string(), String::from),
        ))
    }

    // Notes `msg`, unless it's what was noted last.
    fn say(&self, msg: String) {
        let mut state = self.state();
        if state.said.as_ref() != Some(&msg) {
            note(format_args!("{msg}"));
            state.said = Some(msg);
        }
    }
}

impl fmt::Debug for Fence {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Fence").field("tap", &self.tap).finish()
    }
}

// host/nat.nft's table, as nft names it.
fn table() -> String {
    host::NAT_TABLE.join(" ")
}

/// The addresses a set listed by `nft -j list set` pairs with `tap`, or None
/// if it isn't such a listing.
pub fn paired(json: &str, tap: &str) -> Option<BTreeSet<Ipv4Addr>> {
    let listed: serde_json::Value = serde_json::from_str(json).ok()?;
    let mut found = BTreeSet::new();
    for item in listed.get("nftables")?.as_array()? {
        let Some(elems) = item.pointer("/set/elem").and_then(|e| e.as_array()) else {
            continue;
        };
        for elem in elems {
            // With a timeout, the pair is wrapped in an "elem" of its own.
            let concat = elem
                .pointer("/elem/val/concat")
                .or_else(|| elem.get("concat"))
                .and_then(|c| c.as_array());
            if let Some([t, ip]) = concat.map(Vec::as_slice)
                && t.as_str() == Some(tap)
                && let Some(ip) = ip.as_str().and_then(|ip| ip.parse().ok())
            {
                found.insert(ip);
            }
        }
    }
    Some(found)
}

// As boot's notes: under the unit, stderr is the journal stream Firecracker
// made non-blocking, where eprintln! would panic.
fn note(msg: fmt::Arguments) {
    let _ = writeln!(io::stderr(), "runner: {msg}");
}
