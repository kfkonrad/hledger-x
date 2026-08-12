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

use hledger_x::fmt::{format, format_sorted};

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

/// `_` as a digit group mark is newer than hledger 1.52.1, which rejects it
/// outright in a `commodity`/`D` directive. hledger-x supports it either way —
/// the `amount` and `fmt` unit tests cover it with no hledger involved — so
/// only the cross-check against a real hledger has to be conditional. Probed
/// rather than version-sniffed: the question is what this binary accepts.
fn hledger_accepts_underscore_group_mark(tmp: &Path) -> bool {
    let probe = tmp.join("underscore-probe.journal");
    let src = "commodity 1_000.00 EUR\n\n2026-01-01 x\n    a  1 EUR\n    b  -1 EUR\n";
    fs::write(&probe, src).unwrap();
    hledger_print(&probe).is_some()
}

/// Restyling fixtures: declared styles plus deliberately mis-styled amounts.
/// Everything here restyles somewhere — the invariant is that `hledger
/// print` cannot tell the difference. Group marks here are ones every hledger
/// accepts; `_` lives in [`UNDERSCORE_FIXTURES`].
const RESTYLE_FIXTURES: &[(&str, &str)] = &[
    (
        "sides-and-grouping",
        "commodity 1,000.00 EUR\n\n2026-01-01 x\n    a  1234EUR\n    b  EUR-1234\n",
    ),
    (
        "comma-decimal-reinterpretation",
        // Under the comma-decimal style hledger reads `10.5` as 105.
        "commodity 1.000,00 EUR\n\n2026-01-01 x\n    a  10.5 EUR\n    b  -105,00 EUR\n",
    ),
    (
        "decimal-mark-directive",
        // The forced mark stays in the output; only the grouping normalizes.
        // The group mark must not be the forced decimal mark, hence the space.
        "decimal-mark ,\ncommodity 1 000.00 EUR\n\n2026-01-01 x\n    a  1000,5 EUR\n    b  -1000,5 EUR\n",
    ),
    (
        "cost-and-assertion-tails",
        "commodity 1 000.00 EUR\ncommodity 1,000.00 USD\n\n2026-01-01 x\n    a  10EUR @ 1.1USD\n    b  -11 USD = -11USD\n",
    ),
    (
        "format-subdirective-and-d",
        "commodity EUR\n    format 1.000,00 EUR\nD 1,000.00 GBP\n\n2026-01-01 x\n    a  1000EUR\n    b  -1000 EUR\n\n2026-01-02 y\n    a  10GBP\n    b  -10 GBP\n",
    ),
];

/// The same invariant for `_` group marks, run only against an hledger that
/// understands them.
const UNDERSCORE_FIXTURES: &[(&str, &str)] = &[
    (
        "underscore-sides-and-grouping",
        "commodity 1_000.00 EUR\n\n2026-01-01 x\n    a  1234EUR\n    b  EUR-1234\n",
    ),
    (
        "underscore-decimal-mark-directive",
        "decimal-mark ,\ncommodity 1_000.00 EUR\n\n2026-01-01 x\n    a  1000,5 EUR\n    b  -1000,5 EUR\n",
    ),
    (
        "underscore-d-directive",
        "D 1_000.00 GBP\n\n2026-01-01 x\n    a  1234GBP\n    b  -1234 GBP\n",
    ),
];

/// Format each fixture and assert `hledger print` cannot tell before from
/// after.
fn check_restyle_fixtures(tmp: &Path, fixtures: &[(&str, &str)]) {
    for (name, src) in fixtures {
        let before_path = tmp.join(format!("{name}.before.journal"));
        fs::write(&before_path, src).unwrap();
        let before = hledger_print(&before_path)
            .unwrap_or_else(|| panic!("{name}: hledger rejected the fixture"));
        let out = format(src);
        assert_ne!(out, *src, "{name}: fixture was expected to restyle");
        assert_eq!(format(&out), out, "{name}: restyling is not idempotent");
        let after_path = tmp.join(format!("{name}.after.journal"));
        fs::write(&after_path, &out).unwrap();
        let after = hledger_print(&after_path)
            .unwrap_or_else(|| panic!("{name}: hledger rejected the output"));
        assert_eq!(after, before, "{name}: print output changed");
    }
}

#[test]
fn restyling_preserves_hledger_semantics() {
    if !have_hledger() {
        eprintln!("hledger not on PATH; skipping semantic check");
        return;
    }
    let tmp: PathBuf = [env!("CARGO_TARGET_TMPDIR"), "semantic-restyle"]
        .iter()
        .collect();
    fs::create_dir_all(&tmp).unwrap();
    check_restyle_fixtures(&tmp, RESTYLE_FIXTURES);
}

#[test]
fn restyling_preserves_hledger_semantics_with_underscore_group_marks() {
    if !have_hledger() {
        eprintln!("hledger not on PATH; skipping semantic check");
        return;
    }
    let tmp: PathBuf = [env!("CARGO_TARGET_TMPDIR"), "semantic-underscore"]
        .iter()
        .collect();
    fs::create_dir_all(&tmp).unwrap();
    if !hledger_accepts_underscore_group_mark(&tmp) {
        eprintln!("this hledger rejects `_` group marks; skipping");
        return;
    }
    check_restyle_fixtures(&tmp, UNDERSCORE_FIXTURES);
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
