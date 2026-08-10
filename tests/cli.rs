//! End-to-end tests for `rledger fmt`, driving the built binary.
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

const BIN: &str = env!("CARGO_BIN_EXE_rledger");

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

#[test]
fn add_is_not_implemented_yet_and_says_so() {
    let out = run(&["add"], "");
    assert_ne!(out.code, 0);
    assert!(
        out.stderr.to_lowercase().contains("not implemented"),
        "stderr: {}",
        out.stderr
    );
}
