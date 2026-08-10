//! End-to-end tests for `hledger-x fmt`, driving the built binary.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "test code: a failed assumption should fail the test loudly"
)]

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_hledger-x");

struct Output {
    code: i32,
    stdout: String,
    stderr: String,
}

fn run(args: &[&str], stdin: &str) -> Output {
    let mut child = Command::new(BIN)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    Output {
        code: out.status.code().unwrap(),
        stdout: String::from_utf8(out.stdout).unwrap(),
        stderr: String::from_utf8(out.stderr).unwrap(),
    }
}

/// A scratch directory unique to the calling test.
fn scratch(name: &str) -> PathBuf {
    let dir: PathBuf = [env!("CARGO_TARGET_TMPDIR"), name].iter().collect();
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(dir: &Path, name: &str, contents: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, contents).unwrap();
    path
}

const UNFORMATTED: &str = "2025-01-01 x\n  A:B  1 USD\n";
const FORMATTED: &str = "2025-01-01 x\n    A:B  1 USD\n";

#[test]
fn no_arguments_formats_stdin_to_stdout() {
    let out = run(&["fmt"], UNFORMATTED);
    assert_eq!(out.code, 0);
    assert_eq!(out.stdout, FORMATTED);
}

#[test]
fn a_dash_operand_also_means_stdin() {
    let out = run(&["fmt", "-"], UNFORMATTED);
    assert_eq!(out.code, 0);
    assert_eq!(out.stdout, FORMATTED);
}

#[test]
fn files_are_formatted_in_place() {
    let dir = scratch("in_place");
    let a = write(&dir, "a.journal", UNFORMATTED);
    let b = write(&dir, "b.journal", UNFORMATTED);
    let out = run(&["fmt", a.to_str().unwrap(), b.to_str().unwrap()], "");
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    assert_eq!(out.stdout, "");
    assert_eq!(fs::read_to_string(&a).unwrap(), FORMATTED);
    assert_eq!(fs::read_to_string(&b).unwrap(), FORMATTED);
}

#[test]
fn check_passes_on_formatted_files_and_writes_nothing() {
    let dir = scratch("check_pass");
    let a = write(&dir, "a.journal", FORMATTED);
    let out = run(&["fmt", "--check", a.to_str().unwrap()], "");
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    assert_eq!(fs::read_to_string(&a).unwrap(), FORMATTED);
}

#[test]
fn check_fails_and_lists_offenders_without_writing() {
    let dir = scratch("check_fail");
    let a = write(&dir, "a.journal", UNFORMATTED);
    let out = run(&["fmt", "--check", a.to_str().unwrap()], "");
    assert_eq!(out.code, 1);
    assert!(
        out.stderr.contains("unformatted:") && out.stderr.contains("a.journal"),
        "stderr: {}",
        out.stderr
    );
    // --check must not modify the file.
    assert_eq!(fs::read_to_string(&a).unwrap(), UNFORMATTED);
}

#[test]
fn check_on_stdin_reports_through_the_exit_code_only() {
    let bad = run(&["fmt", "--check"], UNFORMATTED);
    assert_eq!(bad.code, 1);
    assert_eq!(bad.stdout, "");
    let good = run(&["fmt", "--check"], FORMATTED);
    assert_eq!(good.code, 0);
    assert_eq!(good.stdout, "");
}

#[test]
fn sort_reorders_transactions_by_date() {
    let src = "2025-03-02 b\n    A:B  1 USD\n\n2025-01-05 a\n    A:B  2 USD\n";
    let out = run(&["fmt", "--sort"], src);
    assert_eq!(out.code, 0);
    assert_eq!(
        out.stdout,
        "2025-01-05 a\n    A:B  2 USD\n\n2025-03-02 b\n    A:B  1 USD\n"
    );
}

#[test]
fn check_sort_measures_against_the_sorted_form() {
    let unsorted = "2025-03-02 b\n\n2025-01-05 a\n";
    assert_eq!(run(&["fmt", "--check", "--sort"], unsorted).code, 1);
    // Formatted but unsorted still passes a plain --check.
    assert_eq!(run(&["fmt", "--check"], unsorted).code, 0);
}

#[test]
fn an_unreadable_file_is_reported_and_the_run_fails() {
    let dir = scratch("missing");
    let missing = dir.join("nope.journal");
    let out = run(&["fmt", missing.to_str().unwrap()], "");
    assert_ne!(out.code, 0);
    assert!(
        out.stderr.contains("nope.journal"),
        "stderr: {}",
        out.stderr
    );
}

#[test]
fn one_bad_file_does_not_stop_the_others() {
    let dir = scratch("partial");
    let good = write(&dir, "good.journal", UNFORMATTED);
    let missing = dir.join("nope.journal");
    let out = run(
        &["fmt", missing.to_str().unwrap(), good.to_str().unwrap()],
        "",
    );
    assert_ne!(out.code, 0);
    assert_eq!(fs::read_to_string(&good).unwrap(), FORMATTED);
}

#[test]
fn an_already_formatted_file_is_left_untouched_on_disk() {
    let dir = scratch("untouched");
    let a = write(&dir, "a.journal", FORMATTED);
    let before = fs::metadata(&a).unwrap().modified().unwrap();
    let out = run(&["fmt", a.to_str().unwrap()], "");
    assert_eq!(out.code, 0);
    assert_eq!(fs::metadata(&a).unwrap().modified().unwrap(), before);
}

#[test]
fn unknown_options_are_a_usage_error() {
    let out = run(&["fmt", "--nope"], "");
    assert_eq!(out.code, 2);
    assert!(!out.stderr.is_empty());
}

#[test]
fn help_and_version_are_available() {
    let help = run(&["fmt", "--help"], "");
    assert_eq!(help.code, 0);
    assert!(help.stdout.contains("--check") && help.stdout.contains("--sort"));

    let version = run(&["--version"], "");
    assert_eq!(version.code, 0);
    assert!(version.stdout.contains(env!("CARGO_PKG_VERSION")));
}

// ---- hledger-x add (plain line mode: stdin is a pipe) ----

/// Run `hledger-x add` with an isolated HOME/XDG so no user config or
/// recovery state leaks in or out.
fn run_add(dir: &Path, args: &[&str], stdin: &str) -> Output {
    let mut child = Command::new(BIN)
        .arg("add")
        .args(args)
        .env("HOME", dir)
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("XDG_STATE_HOME", dir.join("state"))
        .env_remove("LEDGER_FILE")
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    Output {
        code: out.status.code().unwrap(),
        stdout: String::from_utf8(out.stdout).unwrap(),
        stderr: String::from_utf8(out.stderr).unwrap(),
    }
}

const ADD_JOURNAL: &str = "\
2026-08-05 Rewe
    expenses:groceries        12.00 EUR
    assets:bank:checking     -12.00 EUR
";

#[test]
fn add_appends_a_formatted_transaction() {
    let dir = scratch("add_appends");
    let journal = write(&dir, "main.journal", ADD_JOURNAL);
    // date (accept today), description, account (accept template), amount,
    // account 2 (accept), amount (accept balancing), empty account finishes,
    // EOF quits.
    let input = "\nRewe\n\n18.20 EUR\n\n\n.\n";
    let out = run_add(&dir, &["-f", journal.to_str().unwrap()], input);
    assert_eq!(out.code, 0, "stderr: {}\nstdout: {}", out.stderr, out.stdout);
    let written = fs::read_to_string(&journal).unwrap();
    assert!(
        written.contains("Rewe\n    expenses:groceries     18.20 EUR\n"),
        "file was:\n{written}"
    );
    assert!(written.contains("assets:bank:checking  -18.20 EUR\n"));
    assert!(out.stderr.contains("wrote 1 transaction(s)"), "{}", out.stderr);
}

#[test]
fn add_without_a_journal_errors() {
    let dir = scratch("add_nofile");
    let out = run_add(&dir, &[], "");
    assert_ne!(out.code, 0);
    assert!(out.stderr.contains("LEDGER_FILE"), "{}", out.stderr);
}

#[test]
fn add_to_an_unreachable_target_errors() {
    let dir = scratch("add_unreachable");
    let journal = write(&dir, "main.journal", ADD_JOURNAL);
    let stray = write(&dir, "stray.journal", "");
    let out = run_add(
        &dir,
        &[
            "-f",
            journal.to_str().unwrap(),
            "--to",
            stray.to_str().unwrap(),
        ],
        "",
    );
    assert_ne!(out.code, 0);
    assert!(out.stderr.contains("include graph"), "{}", out.stderr);
}

#[test]
fn add_to_an_included_file_writes_there() {
    let dir = scratch("add_to_include");
    let main = write(
        &dir,
        "main.journal",
        "include sub.journal\n\n2026-08-05 Rewe\n    expenses:groceries  12.00 EUR\n    assets:cash        -12.00 EUR\n",
    );
    let sub = write(&dir, "sub.journal", "");
    let input = "\nRewe\n\n5.00 EUR\nassets:cash\n\n.\n";
    let out = run_add(
        &dir,
        &[
            "-f",
            main.to_str().unwrap(),
            "--to",
            sub.to_str().unwrap(),
        ],
        input,
    );
    assert_eq!(out.code, 0, "stderr: {}\nstdout: {}", out.stderr, out.stdout);
    let sub_text = fs::read_to_string(&sub).unwrap();
    assert!(sub_text.contains("Rewe"), "sub was:\n{sub_text}");
    // The main file is untouched.
    assert!(!fs::read_to_string(&main).unwrap().contains("5.00 EUR"));
}

#[test]
fn add_with_no_input_writes_nothing() {
    let dir = scratch("add_noinput");
    let journal = write(&dir, "main.journal", ADD_JOURNAL);
    let before = fs::read_to_string(&journal).unwrap();
    let out = run_add(&dir, &["-f", journal.to_str().unwrap()], "");
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    assert!(out.stderr.contains("no transactions"), "{}", out.stderr);
    assert_eq!(fs::read_to_string(&journal).unwrap(), before);
}

#[test]
fn add_respects_local_config() {
    let dir = scratch("add_local_config");
    write(&dir, ".hledger-x.toml", "insertion = \"chronological\"\n");
    let journal = write(
        &dir,
        "main.journal",
        "2026-01-01 early\n    a:b   1.00 EUR\n    c:d  -1.00 EUR\n\n2026-12-31 late\n    a:b   2.00 EUR\n    c:d  -2.00 EUR\n",
    );
    // Dated between the two: must land between them, not at the end.
    let input = "2026-06-15\nMiddle\na:b\n3.00 EUR\nc:d\n\n.\n";
    let out = run_add(&dir, &["-f", journal.to_str().unwrap()], input);
    assert_eq!(out.code, 0, "stderr: {}\nstdout: {}", out.stderr, out.stdout);
    let text = fs::read_to_string(&journal).unwrap();
    let early = text.find("early").unwrap();
    let middle = text.find("Middle").unwrap();
    let late = text.find("late").unwrap();
    assert!(early < middle && middle < late, "file was:\n{text}");
}

#[test]
fn fmt_restyles_amounts_using_styles_from_included_files() {
    let dir = scratch("fmt_include_styles");
    write(&dir, "conf.journal", "commodity 1_000.00 EUR\n");
    let main = write(
        &dir,
        "main.journal",
        "include conf.journal\n\n2026-01-01 x\n    a:b  1234EUR\n    c:d  -1234 EUR\n",
    );
    let out = run(&["fmt", main.to_str().unwrap()], "");
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    let text = fs::read_to_string(&main).unwrap();
    assert_eq!(
        text,
        "include conf.journal\n\n2026-01-01 x\n    a:b   1_234 EUR\n    c:d  -1_234 EUR\n"
    );
    // The included file itself is untouched.
    assert_eq!(
        fs::read_to_string(dir.join("conf.journal")).unwrap(),
        "commodity 1_000.00 EUR\n"
    );
    // stdin has no include tree: only the text's own directives apply.
    let out = run(&["fmt"], "2026-01-01 x\n    a:b  1234EUR\n");
    assert_eq!(out.stdout, "2026-01-01 x\n    a:b  1234EUR\n");
}

#[test]
fn add_writes_typed_amounts_in_the_declared_style() {
    let dir = scratch("add_restyle");
    write(&dir, "conf.journal", "commodity 1_000.00 EUR\n");
    let journal = write(
        &dir,
        "main.journal",
        "include conf.journal\n\n2026-01-01 seed\n    a:b   1.00 EUR\n    c:d  -1.00 EUR\n",
    );
    let input = "2026-06-15\nIkea\ne:f\n1234EUR\ng:h\n\n.\n";
    let out = run_add(&dir, &["-f", journal.to_str().unwrap()], input);
    assert_eq!(out.code, 0, "stderr: {}\nstdout: {}", out.stderr, out.stdout);
    let text = fs::read_to_string(&journal).unwrap();
    // The typed amount is restyled with its precision kept; the generated
    // balancing amount pads to the style's decimal places.
    assert!(
        text.contains("1_234 EUR") && text.contains("-1_234.00 EUR"),
        "file was:\n{text}"
    );
}
