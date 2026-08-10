//! Golden fixtures ported from `hledger-fmt/test/testdata`. Byte-for-byte
//! equality with the reference formatter's output is the acceptance criterion
//! for epic 1.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "test code: a failed assumption should fail the test loudly"
)]

use std::fs;
use std::path::PathBuf;

use rledger::fmt::{format, format_sorted, is_formatted, is_formatted_sorted};

const FIXTURES: [&str; 5] = ["postings", "directives", "trailing", "multi", "assertion"];
const SORT_FIXTURES: [&str; 1] = ["sort"];

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
