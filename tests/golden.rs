//! Golden fixtures ported from `hledger-fmt/test/testdata`. Byte-for-byte
//! equality with the reference formatter's output is the acceptance criterion
//! for epic 1. `blanks` is ours, not the reference's: hledger-fmt leaves blank
//! lines exactly as it finds them.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "test code: a failed assumption should fail the test loudly"
)]

use std::fs;
use std::path::PathBuf;

use hledger_x::fmt::{
    format, format_opts_scanned, format_sorted, is_formatted, is_formatted_sorted, Options,
};

const FIXTURES: [&str; 7] = [
    "postings",
    "directives",
    "trailing",
    "multi",
    "assertion",
    "blanks",
    "comment-block",
];
const SORT_FIXTURES: [&str; 1] = ["sort"];
/// Fixtures whose golden is `--explicit`'s output rather than plain `fmt`'s.
/// `explicit` is a reference journal: one transaction per case the flag
/// touches, plus the ones it deliberately leaves alone.
const EXPLICIT_FIXTURES: [&str; 1] = ["explicit"];

const EXPLICIT: Options = Options {
    sort: false,
    explicit: true,
};

fn data(name: &str) -> String {
    let path: PathBuf = [env!("CARGO_MANIFEST_DIR"), "tests", "golden", name]
        .iter()
        .collect();
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

fn input(name: &str) -> String {
    data(&format!("{name}.in.ledger"))
}

fn golden(name: &str) -> String {
    data(&format!("{name}.golden"))
}

fn sorted_golden(name: &str) -> String {
    data(&format!("{name}.sorted.golden"))
}

#[test]
fn formatting_reproduces_the_golden_output() {
    for name in FIXTURES {
        assert_eq!(format(&input(name)), golden(name), "fixture {name}");
    }
}

#[test]
fn formatting_is_idempotent() {
    for name in FIXTURES {
        let once = format(&input(name));
        assert_eq!(format(&once), once, "fixture {name}");
        let g = golden(name);
        assert_eq!(format(&g), g, "golden {name}");
    }
}

#[test]
fn check_accepts_goldens_and_rejects_unformatted_input() {
    for name in FIXTURES {
        assert!(is_formatted(&golden(name)), "golden {name}");
        let raw = input(name);
        assert_eq!(is_formatted(&raw), raw == golden(name), "input {name}");
    }
}

#[test]
fn sorting_reproduces_the_sorted_golden() {
    for name in SORT_FIXTURES {
        assert_eq!(
            format_sorted(&input(name)),
            sorted_golden(name),
            "fixture {name}"
        );
    }
}

#[test]
fn sorting_is_idempotent_and_passes_check_sort() {
    for name in SORT_FIXTURES {
        let g = sorted_golden(name);
        assert_eq!(format_sorted(&g), g, "golden {name}");
        assert!(is_formatted_sorted(&g), "golden {name}");
    }
}

fn explicit_golden(name: &str) -> String {
    data(&format!("{name}.explicit.golden"))
}

#[test]
fn explicit_reproduces_the_explicit_golden() {
    for name in EXPLICIT_FIXTURES {
        assert_eq!(
            format_opts_scanned(&input(name), EXPLICIT),
            explicit_golden(name),
            "fixture {name}"
        );
    }
}

#[test]
fn explicit_is_idempotent_and_passes_check_explicit() {
    for name in EXPLICIT_FIXTURES {
        let g = explicit_golden(name);
        assert_eq!(format_opts_scanned(&g, EXPLICIT), g, "golden {name}");
    }
}

#[test]
fn plain_formatting_neither_pads_nor_fills() {
    // The same fixture under plain `fmt`: whatever --explicit adds must be
    // absent here, or the flag is not opt-in at all.
    for name in EXPLICIT_FIXTURES {
        let plain = format(&input(name));
        assert_eq!(format(&plain), plain, "fixture {name} is not idempotent");
        assert_ne!(
            plain,
            explicit_golden(name),
            "fixture {name}: plain fmt should differ from --explicit"
        );
        // Padding is the visible half: no amount gains decimals it did not
        // have. `1234 EUR` stays `1234 EUR` under a declared `1,000.00 EUR`.
        assert!(
            plain.contains("1234 EUR"),
            "fixture {name}: plain fmt padded an amount"
        );
        // And nothing is filled in: the bare posting is still bare.
        assert!(
            plain.lines().any(|l| l.trim() == "t01:assets"),
            "fixture {name}: plain fmt filled in an amount"
        );
    }
}
