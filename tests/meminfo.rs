// host::mem_available(): MemAvailable from /proc/meminfo, the free memory
// `runner hosts` reports and `runner create` places by.
use proptest::prelude::*;
use runner::host::{mem_available_in, parse_mem_available};

#[test]
fn reads_memavailable_in_kib_as_bytes() {
    let real = "MemTotal:        8039504 kB\nMemFree:          301492 kB\n\
                MemAvailable:    6151284 kB\nBuffers:          147536 kB\n";
    assert_eq!(parse_mem_available(real), Some(6_151_284 * 1024));
}

#[test]
fn without_memavailable_there_is_none() {
    assert_eq!(parse_mem_available(""), None);
    assert_eq!(parse_mem_available("MemTotal: 1 kB\nMemFree: 1 kB\n"), None);
    assert_eq!(parse_mem_available("MemAvailable: lots kB\n"), None);
    // Not a field that starts with the name.
    assert_eq!(parse_mem_available("XMemAvailable: 5 kB\n"), None);
}

#[test]
fn the_file_is_named_in_the_error() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("nope");
    let e = mem_available_in(&missing).unwrap_err().to_string();
    assert!(e.contains(missing.to_str().unwrap()), "{e}");
    let empty = dir.path().join("empty");
    std::fs::write(&empty, "").unwrap();
    let e = mem_available_in(&empty).unwrap_err().to_string();
    assert!(e.contains("no MemAvailable"), "{e}");
    assert!(e.contains(empty.to_str().unwrap()), "{e}");
}

proptest! {
    // Wherever MemAvailable sits among other fields, and however it's spaced.
    #[test]
    fn finds_memavailable_among_any_other_fields(
        kib in 0u64..(1 << 40),
        before in prop::collection::vec("[A-Za-z_()]{1,12}: +[0-9]{1,9}( kB)?", 0..8),
        after in prop::collection::vec("[A-Za-z_()]{1,12}: +[0-9]{1,9}( kB)?", 0..8),
        pad in " {1,10}",
    ) {
        prop_assume!(before.iter().chain(&after).all(|l| !l.starts_with("MemAvailable:")));
        let mut lines = before.clone();
        lines.push(format!("MemAvailable:{pad}{kib} kB"));
        lines.extend(after.clone());
        let text = lines.join("\n") + "\n";
        prop_assert_eq!(parse_mem_available(&text), Some(kib * 1024));
    }
}
