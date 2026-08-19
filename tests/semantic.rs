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

use hledger_x::fmt::{format, format_opts_scanned, format_sorted, Options};

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

/// `hledger print -x`: what the journal says once every inferred amount is
/// spelled out. The reference for `fmt --explicit`'s filling-in.
fn hledger_print_explicit(path: &Path) -> Option<String> {
    let out = Command::new("hledger")
        .args(["print", "-x", "-f"])
        .arg(path)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The commodities hledger finds in a journal.
fn hledger_commodities(path: &Path) -> Vec<String> {
    Command::new("hledger")
        .args(["commodities", "-f"])
        .arg(path)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// `hledger balance`: every account's total. Padding an amount to its
/// declared decimal places changes what `print` emits by design, so
/// `--explicit` is held to the stronger and more meaningful question — did
/// any *value* move?
///
/// Every commodity's display style is pinned with `-c` first. hledger derives
/// a commodity's report precision from the amounts it finds in the journal,
/// so writing one full-precision inferred amount silently grows the decimals
/// of *every* amount in that commodity — which reads as a value change in the
/// output and is nothing of the kind. Pinning the style removes that
/// confound and leaves the question the oracle is actually asking.
fn hledger_balance(path: &Path) -> Option<String> {
    let mut cmd = Command::new("hledger");
    cmd.args(["balance", "--no-total", "-f"]).arg(path);
    for c in hledger_commodities(path) {
        cmd.arg("-c").arg(format!("{c} 1000.00000000"));
    }
    let out = cmd.output().ok()?;
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

/// Every shape of inference `--explicit` claims to handle. None of these
/// declares a commodity style, so `hledger print -x` is an exact oracle:
/// writing the inferred amounts in must leave hledger nothing left to infer.
const INFERENCE_FIXTURES: &[(&str, &str)] = &[
    ("one-elided", "2026-01-01 x\n    a  10 EUR\n    b\n"),
    (
        "through-a-cost",
        "2026-01-01 x\n    a  10 EUR @ 1.1 USD\n    b\n",
    ),
    (
        "assertion-only",
        "2026-01-01 x\n    a  10 EUR\n    b   = -10 EUR\n",
    ),
    (
        "zero-remainder",
        "2026-01-01 x\n    a  10 EUR\n    b  -10 EUR\n    c\n",
    ),
    (
        "multi-commodity-split",
        "2026-01-01 x\n    a  10 EUR\n    a  5 USD\n    b\n",
    ),
    (
        "unbalanced-virtual-is-excluded",
        "2026-01-01 x\n    a  10 EUR\n    (v)  3 EUR\n    b\n",
    ),
    (
        "balanced-virtual-balances-alone",
        "2026-01-01 x\n    a  10 EUR\n    b  -10 EUR\n    [v]  3 EUR\n    [w]\n",
    ),
    ("unitless", "2026-01-01 x\n    a  10\n    b\n"),
    // No `commodity` directive settles these, so the style of the generated
    // amount comes from the postings it balances against. `print -x` is still
    // the oracle: it makes the same choice.
    ("attached-left-symbol", "2026-01-01 x\n    a  $10\n    b\n"),
    (
        "sign-between-symbol-and-digits",
        "2026-01-01 x\n    a  $-10\n    b\n",
    ),
    (
        "attached-right-symbol",
        "2026-01-01 x\n    a  10\u{20ac}\n    b\n",
    ),
    ("symbol-first", "2026-01-01 x\n    a  USD 10\n    b\n"),
    (
        "typed-digit-grouping",
        "2026-01-01 x\n    a  1,234.50 USD\n    b\n",
    ),
    (
        "zero-remainder-keeps-the-style",
        "2026-01-01 x\n    a  $10\n    b  $-10\n    c\n",
    ),
    (
        "style-from-a-cost-tail",
        "2026-01-01 x\n    a  10 EUR @ $1.1\n    b\n",
    ),
    // All four assertion operators, and operators with their amount attached.
    // Each of these used to leave the whole amount unparseable, so nothing
    // was filled in at all.
    (
        "assertion-eq",
        "2026-01-01 x\n    a  10 USD = 10 USD\n    b\n",
    ),
    (
        "assertion-eqeq",
        "2026-01-01 x\n    a  10 USD == 10 USD\n    b\n",
    ),
    (
        "assertion-subaccount",
        "2026-01-01 x\n    a  10 USD =* 10 USD\n    b\n",
    ),
    (
        "assertion-subaccount-strict",
        "2026-01-01 x\n    a  10 USD ==* 10 USD\n    b\n",
    ),
    (
        "attached-cost",
        "2026-01-01 x\n    a  10 USD @1.1EUR\n    b\n",
    ),
    (
        "attached-total-cost",
        "2026-01-01 x\n    a  10 USD @@13EUR\n    b\n",
    ),
    (
        "attached-assertion",
        "2026-01-01 x\n    a  10 USD =10USD\n    b\n",
    ),
    // A space as the digit group mark: hledger honours it, and a number
    // containing one is still a single amount.
    (
        "space-group-in-the-text",
        "2026-01-01 x\n    a  1 234.00 USD\n    b\n",
    ),
    (
        "space-group-mixed-widths",
        "2026-01-01 x\n    a  1 234.00 USD\n    b  -50.00 USD\n    c\n",
    ),
    (
        "unitless-space-group",
        "2026-01-01 x\n    a  1 000\n    b\n",
    ),
    (
        "unitless-comma-group",
        "2026-01-01 x\n    a  1,000\n    b\n",
    ),
    // A status flag is not part of the account, so it must not hide the
    // brackets that say which group a posting balances in.
    (
        "flagged-virtual-posting",
        "2026-01-01 x\n    a  10 EUR\n    * (v)  3 EUR\n    b\n",
    ),
    (
        "flagged-balanced-virtual",
        "2026-01-01 x\n    a  10 EUR\n    b\n    ! [c]  4 EUR\n    ! [d]\n",
    ),
    // `D` names the commodity of a bare amount, and --explicit writes it out.
    // Positional: the second fixture's `D` comes after the transaction, so
    // hledger does not apply it and neither may we.
    (
        "default-commodity-applies",
        "D 1000.00 GBP\n\n2026-01-01 x\n    a  10\n    b\n",
    ),
    // A lone tab between account and amount is the one thing hledgers
    // disagree about: releases read it as part of the account name, an
    // unreleased master build reads it as a separator. `--explicit` infers
    // nothing in such a transaction, so this fixture holds under either —
    // and it catches the version we would break if it ever inferred again.
    (
        "tab-separated-posting",
        "2026-01-01 x\n    a:b\t1234 EUR\n    c:d  -1234 EUR\n",
    ),
    (
        "default-commodity-is-positional",
        "2026-01-01 x\n    a  10\n    b\n\nD 1000.00 GBP\n",
    ),
];

/// Fixtures where `hledger print -x` legitimately differs from our output, so
/// the oracle is `hledger balance` — the question that actually matters, did
/// any value move?
///
/// Two reasons it differs. Padding decimals to a declared style is a
/// deliberate deviation: `print` renders a written `1234 EUR` as `1,234.`
/// under `commodity 1,000.00 EUR`. And where a remainder spans commodities,
/// we emit the split postings in the order the commodities first appear in
/// the transaction, which is the order a person wrote them; hledger emits
/// them in its own.
const VALUE_ONLY_FIXTURES: &[(&str, &str)] = &[
    (
        "default-commodity-loses-to-an-explicit-one",
        "D 1000.00 GBP\n\n2026-01-01 x\n    a  10\n    a  5 EUR\n    b\n",
    ),
    (
        "declared-style-padding",
        "commodity 1,000.00 EUR\n\n2026-01-01 x\n    a  1234 EUR\n    b\n",
    ),
    (
        "padding-is-a-floor-not-a-ceiling",
        "commodity 1,000.00 EUR\n\n2026-01-01 x\n    a  4.0001 EUR\n    b\n",
    ),
    (
        "space-group-declared",
        "commodity 1 000.00 USD\n\n2026-01-01 x\n    a  1234567 USD\n    b\n",
    ),
    (
        "space-group-symbol-first",
        "commodity USD 1 000.00\n\n2026-01-01 x\n    a  USD 1234567\n    b\n",
    ),
    (
        "space-group-attached-symbol",
        "commodity 1 000.00\u{a3}\n\n2026-01-01 x\n    a  1234567\u{a3}\n    b\n",
    ),
    (
        "padding-reaches-cost-tails",
        "commodity 1,000.00 EUR\ncommodity 1,000.00 USD\n\n2026-01-01 x\n    a  10 EUR @ 1.1 USD\n    b\n",
    ),
];

const EXPLICIT: Options = Options {
    sort: false,
    explicit: true,
};

/// Format with `--explicit`, checking it is a fixed point of itself, and
/// write the result out for hledger to read.
fn explicit_output(tmp: &Path, name: &str, src: &str) -> (PathBuf, PathBuf) {
    let before_path = tmp.join(format!("{name}.before.journal"));
    fs::write(&before_path, src).unwrap();
    let out = format_opts_scanned(src, EXPLICIT);
    assert_ne!(out, *src, "{name}: fixture was expected to change");
    assert_eq!(
        format_opts_scanned(&out, EXPLICIT),
        out,
        "{name}: --explicit is not idempotent"
    );
    let after_path = tmp.join(format!("{name}.after.journal"));
    fs::write(&after_path, &out).unwrap();
    (before_path, after_path)
}

#[test]
fn explicit_infers_exactly_what_hledger_infers() {
    if !have_hledger() {
        eprintln!("hledger not on PATH; skipping semantic check");
        return;
    }
    let tmp: PathBuf = [env!("CARGO_TARGET_TMPDIR"), "semantic-explicit"]
        .iter()
        .collect();
    fs::create_dir_all(&tmp).unwrap();

    for (name, src) in INFERENCE_FIXTURES {
        let (before_path, after_path) = explicit_output(&tmp, name, src);
        let before = hledger_print_explicit(&before_path)
            .unwrap_or_else(|| panic!("{name}: hledger rejected the fixture"));
        let after = hledger_print_explicit(&after_path)
            .unwrap_or_else(|| panic!("{name}: hledger rejected the output"));
        assert_eq!(after, before, "{name}: inferred amounts differ");
    }
}

#[test]
fn explicit_moves_no_value() {
    if !have_hledger() {
        eprintln!("hledger not on PATH; skipping semantic check");
        return;
    }
    let tmp: PathBuf = [env!("CARGO_TARGET_TMPDIR"), "semantic-explicit-values"]
        .iter()
        .collect();
    fs::create_dir_all(&tmp).unwrap();

    for (name, src) in VALUE_ONLY_FIXTURES {
        let (before_path, after_path) = explicit_output(&tmp, name, src);
        let before = hledger_balance(&before_path)
            .unwrap_or_else(|| panic!("{name}: hledger rejected the fixture"));
        let after = hledger_balance(&after_path)
            .unwrap_or_else(|| panic!("{name}: hledger rejected the output"));
        assert_eq!(after, before, "{name}: balances changed");
    }
}

#[test]
fn explicit_leaves_the_golden_fixtures_inferring_the_same_amounts() {
    if !have_hledger() {
        eprintln!("hledger not on PATH; skipping semantic check");
        return;
    }
    let data: PathBuf = [env!("CARGO_MANIFEST_DIR"), "tests", "golden"]
        .iter()
        .collect();
    let tmp: PathBuf = [env!("CARGO_TARGET_TMPDIR"), "semantic-explicit-golden"]
        .iter()
        .collect();
    fs::create_dir_all(&tmp).unwrap();

    let mut checked = 0;
    for entry in fs::read_dir(&data).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if !name.ends_with(".in.ledger") {
            continue;
        }
        let src = fs::read_to_string(&path).unwrap();
        if src.lines().any(|l| l.trim_start().starts_with("include ")) {
            continue;
        }
        let Some(before) = hledger_print_explicit(&path) else {
            continue; // not a standalone journal
        };
        let out = format_opts_scanned(&src, EXPLICIT);
        let after_path = tmp.join(&name);
        fs::write(&after_path, &out).unwrap();
        assert_eq!(
            format_opts_scanned(&out, EXPLICIT),
            out,
            "{name}: --explicit is not idempotent"
        );
        let after = hledger_print_explicit(&after_path)
            .unwrap_or_else(|| panic!("{name}: hledger rejected --explicit output"));
        // A fixture that declares a commodity style is padded on purpose, so
        // only its *values* have to match; anything else must be identical.
        if declares_a_style(&src) {
            assert_eq!(
                hledger_balance(&after_path),
                hledger_balance(&path),
                "{name}: balances changed under --explicit"
            );
        } else {
            assert_eq!(after, before, "{name}: inferred amounts differ");
        }
        checked += 1;
    }
    assert!(checked > 0, "no fixtures were checked");
}

/// Whether a journal declares any commodity display style, and so is subject
/// to `--explicit`'s decimal padding.
fn declares_a_style(src: &str) -> bool {
    src.lines().any(|l| {
        l.starts_with("commodity ") || l.starts_with("D ") || l.trim_start().starts_with("format ")
    })
}
