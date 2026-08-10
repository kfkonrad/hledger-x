//! The semantic invariant, and the safety net behind every layout rule: for
//! every self-contained fixture, `hledger print` must be byte-identical before
//! and after formatting. If it differs, the formatter changed meaning, not just
//! whitespace.
//!
//! Skipped (not failed) when `hledger` is not on PATH.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "test code: a failed assumption should fail the test loudly"
)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use rledger::fmt::{format, format_sorted};

fn hledger_print(path: &Path) -> Option<String> {
    let out = Command::new("hledger")
        .args(["print", "-f"])
        .arg(path)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

fn have_hledger() -> bool {
    Command::new("hledger")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

#[test]
fn hledger_print_is_unchanged_by_formatting() {
    if !have_hledger() {
        eprintln!("hledger not on PATH; skipping semantic check");
        return;
    }
    let data: PathBuf = [env!("CARGO_MANIFEST_DIR"), "tests", "golden"]
        .iter()
        .collect();
    let tmp: PathBuf = [env!("CARGO_TARGET_TMPDIR"), "semantic"].iter().collect();
    fs::create_dir_all(&tmp).unwrap();

    let mut checked = 0;
    for entry in fs::read_dir(&data).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if !name.ends_with(".in.ledger") {
            continue;
        }
        let src = fs::read_to_string(&path).unwrap();
        // Only self-contained journals: skip fixtures that `include` others.
        if src.lines().any(|l| l.trim_start().starts_with("include ")) {
            continue;
        }
        let Some(before) = hledger_print(&path) else {
            continue; // not a standalone journal
        };

        for (label, out) in [("fmt", format(&src)), ("fmt --sort", format_sorted(&src))] {
            let after_path = tmp.join(&name);
            fs::write(&after_path, &out).unwrap();
            let after = hledger_print(&after_path)
                .unwrap_or_else(|| panic!("{name} ({label}): hledger rejected the output"));
            // `hledger print` sorts by date itself, so --sort must not change
            // its output either.
            assert_eq!(after, before, "{name} ({label}): print output changed");
        }
        checked += 1;
    }
    assert!(checked > 0, "no fixtures were checked");
}
