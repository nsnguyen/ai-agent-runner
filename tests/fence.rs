// A listed VM's place in the host's rules, which its boot keeps with nft:
// here a fake one that logs what it's asked (common::FakeNft). tests/net.rs
// runs the real nft on the real rules, as root, and tests/boot.rs a boot.
mod common;

use common::FakeNft;
use runner::dns::{Address, List};
use runner::fence::{Fence, MAX_HOLD, MIN_HOLD};
use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;
use tempfile::TempDir;

const TAP: &str = "fcac100004";
const OTHER: &str = "fcac100008";
const LIST: &str = "-j list set ip runner reach";
const GET: &str = r#"get element ip runner listed { "fcac100004" }"#;
const ADD_TAP: &str = r#"add element ip runner listed { "fcac100004" }"#;
const TAKE_TAP: &str = r#"add element ip runner listed { "fcac100004" }; delete element ip runner listed { "fcac100004" }"#;

// Writing a script and running it from several threads at once can fail
// with ETXTBSY, so these take turns.
static TURN: Mutex<()> = Mutex::new(());

struct Nft {
    _turn: MutexGuard<'static, ()>,
    _dir: TempDir,
    fake: FakeNft,
}

fn nft() -> Nft {
    let turn = TURN.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let fake = FakeNft::new(&dir.path().join("nft"));
    Nft {
        _turn: turn,
        _dir: dir,
        fake,
    }
}

fn list(names: &[&str]) -> List {
    List::new(names.iter().map(|n| n.parse().unwrap()))
}

fn addr(ip: &str, secs: u64) -> Address {
    Address {
        ip: ip.parse().unwrap(),
        ttl: Duration::from_secs(secs),
    }
}

fn listed(nft: &FakeNft, names: &[&str]) -> Fence {
    Fence::start(&nft.bin, "dev", TAP, Some(&list(names))).unwrap()
}

// Pairs of this tap, as one transaction puts them in: added, then deleted
// and added again, since an add alone leaves a pair that's in already with
// the time it had.
fn put_in(pairs: &[(&str, u64)]) -> String {
    let timed: Vec<String> = pairs
        .iter()
        .map(|(ip, secs)| format!(r#""{TAP}" . {ip} timeout {secs}s"#))
        .collect();
    let keys: Vec<String> = pairs
        .iter()
        .map(|(ip, _)| format!(r#""{TAP}" . {ip}"#))
        .collect();
    let (timed, keys) = (timed.join(", "), keys.join(", "));
    format!(
        "add element ip runner reach {{ {timed} }}; delete element ip runner reach {{ {keys} }}; \
         add element ip runner reach {{ {timed} }}"
    )
}

// Pairs of this tap, taken out: added first, so one that has run out since
// doesn't fail the rest.
fn take_out(ips: &[&str]) -> String {
    let timed: Vec<String> = ips
        .iter()
        .map(|ip| format!(r#""{TAP}" . {ip} timeout 1s"#))
        .collect();
    let keys: Vec<String> = ips.iter().map(|ip| format!(r#""{TAP}" . {ip}"#)).collect();
    format!(
        "add element ip runner reach {{ {} }}; delete element ip runner reach {{ {} }}",
        timed.join(", "),
        keys.join(", ")
    )
}

#[test]
fn a_listed_vm_s_tap_goes_in_with_none_of_the_pairs_a_killed_boot_left() {
    let nft = nft();
    let fake = &nft.fake;
    // A boot of this tap that was killed left a pair; another VM's are its own.
    fake.pairs(&[(TAP, "203.0.113.9"), (OTHER, "203.0.113.10")]);
    let fence = listed(fake, &["github.com"]);
    let gone = take_out(&["203.0.113.9"]);
    assert_eq!(
        fake.calls(),
        [LIST.to_string(), format!("{gone}; {ADD_TAP}")]
    );
    // The same list, read again each second, changes nothing.
    fence.follow(Some(&list(&["github.com"]))).unwrap();
    assert_eq!(fake.calls().len(), 2);
}

#[test]
fn a_listed_vm_with_no_pairs_left_only_puts_its_tap_in() {
    let nft = nft();
    let _fence = listed(&nft.fake, &["github.com"]);
    assert_eq!(nft.fake.calls(), [LIST, ADD_TAP]);
}

#[test]
fn an_unlisted_vm_s_tap_is_taken_out_only_if_a_killed_boot_left_it_in() {
    let nft = nft();
    let fake = &nft.fake;
    let fence = Fence::start(&fake.bin, "dev", TAP, None).unwrap();
    assert_eq!(fake.calls(), [GET]);
    // Nothing to hand on to, or put back.
    fence.hand_on(&[addr("203.0.113.1", 60)]);
    fence.look();
    fence.end();
    assert_eq!(fake.calls(), [GET]);

    // Left in, with a pair.
    fake.forget();
    fake.leave_tap();
    fake.pairs(&[(TAP, "203.0.113.9")]);
    Fence::start(&fake.bin, "dev", TAP, None).unwrap();
    let gone = take_out(&["203.0.113.9"]);
    assert_eq!(
        fake.calls(),
        [
            GET.to_string(),
            LIST.to_string(),
            format!("{gone}; {TAKE_TAP}")
        ]
    );
    assert!(!fake.has_tap());
}

#[test]
fn each_answer_s_addresses_go_in_for_their_ttl_but_at_least_ten_minutes() {
    assert_eq!(MIN_HOLD, Duration::from_secs(600));
    let nft = nft();
    let fake = &nft.fake;
    let fence = listed(fake, &["github.com"]);
    fake.forget();
    // At most a week: nft refuses timeouts far past that, and DNS allows
    // TTLs of 68 years.
    fence.hand_on(&[
        addr("203.0.113.1", 30),
        addr("203.0.113.2", 3600),
        addr("203.0.113.3", u64::from(u32::MAX)),
    ]);
    let week = MAX_HOLD.as_secs();
    assert_eq!(week, 7 * 24 * 3600);
    let want = put_in(&[
        ("203.0.113.1", 600),
        ("203.0.113.2", 3600),
        ("203.0.113.3", week),
    ]);
    assert_eq!(fake.calls(), [want]);
}

#[test]
fn an_address_in_for_as_long_already_isn_t_put_in_again() {
    let nft = nft();
    let fake = &nft.fake;
    let fence = listed(fake, &["github.com"]);
    fake.forget();
    fence.hand_on(&[addr("203.0.113.1", 30)]);
    // Asked again and again, as programs do.
    fence.hand_on(&[addr("203.0.113.1", 30)]);
    fence.hand_on(&[addr("203.0.113.1", 600)]);
    assert_eq!(fake.calls(), [put_in(&[("203.0.113.1", 600)])]);
    // An answer that keeps it longer puts it in again, for that long.
    fence.hand_on(&[addr("203.0.113.1", 3600)]);
    assert_eq!(fake.calls().len(), 2);
    assert_eq!(fake.calls()[1], put_in(&[("203.0.113.1", 3600)]));
    // The same address twice in an answer goes in once, for the longer.
    fence.hand_on(&[addr("203.0.113.2", 700), addr("203.0.113.2", 900)]);
    assert_eq!(fake.calls()[2], put_in(&[("203.0.113.2", 900)]));
}

#[test]
fn taking_a_name_off_flushes_the_vm_s_pairs_and_adding_one_doesn_t() {
    let nft = nft();
    let fake = &nft.fake;
    let fence = listed(fake, &["github.com", "example.com"]);
    fence.hand_on(&[addr("203.0.113.1", 60)]);
    fake.forget();
    fence
        .follow(Some(&list(&["github.com", "example.com", "rust-lang.org"])))
        .unwrap();
    assert_eq!(fake.calls(), Vec::<String>::new());

    // What the rules hold, which a boot that was killed may have added to:
    // this tap's go, and only those.
    fake.pairs(&[(TAP, "203.0.113.7"), (OTHER, "203.0.113.8")]);
    fence
        .follow(Some(&list(&["github.com", "rust-lang.org"])))
        .unwrap();
    let gone = take_out(&["203.0.113.1", "203.0.113.7"]);
    assert_eq!(fake.calls(), [LIST.to_string(), gone]);
    // So the next answer puts it in again.
    fake.forget();
    fence.hand_on(&[addr("203.0.113.1", 60)]);
    assert_eq!(fake.calls(), [put_in(&[("203.0.113.1", 600)])]);
}

#[test]
fn dropping_the_list_takes_the_tap_out_with_its_pairs_and_a_new_list_puts_it_back() {
    let nft = nft();
    let fake = &nft.fake;
    let fence = listed(fake, &["github.com"]);
    fence.hand_on(&[addr("203.0.113.1", 60)]);
    fake.forget();
    fence.follow(None).unwrap();
    let gone = take_out(&["203.0.113.1"]);
    assert_eq!(
        fake.calls(),
        [LIST.to_string(), format!("{gone}; {TAKE_TAP}")]
    );
    // Unlisted, nothing goes in, nor back.
    fake.forget();
    fence.hand_on(&[addr("203.0.113.2", 60)]);
    fence.look();
    fence.follow(None).unwrap();
    assert_eq!(fake.calls(), Vec::<String>::new());
    fence.follow(Some(&list(&["github.com"]))).unwrap();
    assert_eq!(fake.calls(), [LIST, ADD_TAP]);
}

#[test]
fn a_tap_the_rules_lost_is_put_back_with_its_pairs_at_the_next_look() {
    let nft = nft();
    let fake = &nft.fake;
    let fence = listed(fake, &["github.com"]);
    fence.hand_on(&[addr("203.0.113.1", 3600), addr("203.0.113.2", 60)]);
    fake.forget();
    fence.look();
    assert_eq!(fake.calls(), [GET]);

    fake.lose_taps();
    fence.look();
    let calls = fake.calls();
    assert_eq!(calls.len(), 3, "{calls:?}");
    assert_eq!(calls[1], GET);
    // Each for what's left of its time, a second or so less by now.
    let (tap, pairs) = calls[2].split_once("; ").unwrap();
    assert_eq!(tap, ADD_TAP);
    let pairs = pairs
        .strip_prefix("add element ip runner reach { ")
        .and_then(|p| p.strip_suffix(" }"))
        .unwrap_or_else(|| panic!("{pairs}"));
    let pairs: Vec<(&str, u64)> = pairs
        .split(", ")
        .map(|p| {
            let (key, secs) = p.split_once(" timeout ").unwrap();
            (key, secs.strip_suffix('s').unwrap().parse().unwrap())
        })
        .collect();
    assert_eq!(pairs.len(), 2, "{pairs:?}");
    assert_eq!(pairs[0].0, format!(r#""{TAP}" . 203.0.113.1"#));
    assert!((3590..=3600).contains(&pairs[0].1), "{pairs:?}");
    assert_eq!(pairs[1].0, format!(r#""{TAP}" . 203.0.113.2"#));
    assert!((590..=600).contains(&pairs[1].1), "{pairs:?}");
    assert!(fake.has_tap());
}

#[test]
fn a_tap_that_can_t_go_in_is_an_error_that_says_why() {
    let nft = nft();
    let fake = &nft.fake;
    // As on a host whose rules are from before the sets.
    fake.fail("ip runner");
    let err = Fence::start(&fake.bin, "dev", TAP, Some(&list(&["github.com"])))
        .err()
        .expect("the tap went in");
    let msg = err.to_string();
    assert!(msg.contains(LIST), "{msg}");
    assert!(msg.contains("No such file or directory"), "{msg}");

    fake.heal();
    fake.fail(ADD_TAP);
    let err = Fence::start(&fake.bin, "dev", TAP, Some(&list(&["github.com"])))
        .err()
        .expect("the tap went in");
    assert!(err.to_string().contains(ADD_TAP), "{err}");
}

#[test]
fn a_change_that_fails_is_tried_again_at_the_next_read() {
    let nft = nft();
    let fake = &nft.fake;
    let fence = Fence::start(&fake.bin, "dev", TAP, None).unwrap();
    fake.fail(ADD_TAP);
    fence.follow(Some(&list(&["github.com"]))).unwrap_err();
    // Noted, not returned, as the VM's DNS reads its list.
    fence.track(Some(&list(&["github.com"])));
    fake.heal();
    fake.forget();
    fence.track(Some(&list(&["github.com"])));
    assert_eq!(fake.calls(), [LIST, ADD_TAP]);
}

#[test]
fn end_takes_the_tap_and_its_pairs_out_and_nothing_goes_in_after() {
    let nft = nft();
    let fake = &nft.fake;
    let fence = listed(fake, &["github.com"]);
    fence.hand_on(&[addr("203.0.113.1", 60)]);
    fake.forget();
    fence.end();
    let gone = take_out(&["203.0.113.1"]);
    assert_eq!(
        fake.calls(),
        [LIST.to_string(), format!("{gone}; {TAKE_TAP}")]
    );
    // A query that outlasts the boot hands on nothing, and the list no longer counts.
    fake.forget();
    fence.hand_on(&[addr("203.0.113.2", 60)]);
    fence.follow(Some(&list(&["github.com"]))).unwrap();
    fence.look();
    fence.end();
    assert_eq!(fake.calls(), Vec::<String>::new());
}

#[test]
fn a_missing_nft_is_an_error_not_a_panic() {
    let missing = Path::new("/nonexistent/nft");
    let err = Fence::start(missing, "dev", TAP, Some(&list(&["github.com"])))
        .err()
        .expect("the tap went in");
    assert!(err.to_string().contains("/nonexistent/nft"), "{err}");
    // Unlisted, there's nothing to take out.
    let fence = Fence::start(missing, "dev", TAP, None).unwrap();
    fence.look();
    fence.end();
}
