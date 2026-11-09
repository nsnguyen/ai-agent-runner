// host::mem_total(): MemTotal from /proc/meminfo, which a host's room counts
// down from (src/room.rs; tests/room.rs).
use proptest::prelude::*;
use runner::host::{mem_total_in, parse_mem_total};

#[test]
fn reads_memtotal_in_kib_as_bytes() {
    let real = "MemTotal:        8039504 kB\nMemFree:          301492 kB\n\
                MemAvailable:    6151284 kB\nBuffers:          147536 kB\n";
    assert_eq!(parse_mem_total(real), Some(8_039_504 * 1024));
}

#[test]
fn without_memtotal_there_is_none() {
    assert_eq!(parse_mem_total(""), None);
    assert_eq!(parse_mem_total("MemFree: 1 kB\nMemAvailable: 1 kB\n"), None);
    assert_eq!(parse_mem_total("MemTotal: lots kB\n"), None);
    // Not a field that starts with the name.
    assert_eq!(parse_mem_total("XMemTotal: 5 kB\n"), None);
}

#[test]
fn the_file_is_named_in_the_error() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("nope");
    let e = mem_total_in(&missing).unwrap_err().to_string();
    assert!(e.contains(missing.to_str().unwrap()), "{e}");
    let empty = dir.path().join("empty");
    std::fs::write(&empty, "").unwrap();
    let e = mem_total_in(&empty).unwrap_err().to_string();
    assert!(e.contains("no MemTotal"), "{e}");
    assert!(e.contains(empty.to_str().unwrap()), "{e}");
}

proptest! {
    // Wherever MemTotal sits among other fields, and however it's spaced.
    #[test]
    fn finds_memtotal_among_any_other_fields(
        kib in 0u64..(1 << 40),
        before in prop::collection::vec("[A-Za-z_()]{1,12}: +[0-9]{1,9}( kB)?", 0..8),
        after in prop::collection::vec("[A-Za-z_()]{1,12}: +[0-9]{1,9}( kB)?", 0..8),
        pad in " {1,10}",
    ) {
        prop_assume!(before.iter().chain(&after).all(|l| !l.starts_with("MemTotal:")));
        let mut lines = before.clone();
        lines.push(format!("MemTotal:{pad}{kib} kB"));
        lines.extend(after.clone());
        let text = lines.join("\n") + "\n";
        prop_assert_eq!(parse_mem_total(&text), Some(kib * 1024));
    }
}
