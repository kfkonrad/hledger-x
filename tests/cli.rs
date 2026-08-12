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

/// Run the binary in `dir`, with no user config and no `$LEDGER_FILE`, so
/// nothing on the developer's machine reaches the run. `fmt` resolves its
/// journal from the config and the environment now, which makes that
/// isolation load-bearing rather than merely tidy.
fn run_in(dir: &Path, args: &[&str], stdin: &str) -> Output {
    run_env(dir, args, stdin, None)
}

/// As [`run_in`], with an explicit `$LEDGER_FILE` (unset when `None`).
fn run_env(dir: &Path, args: &[&str], stdin: &str, ledger_file: Option<&str>) -> Output {
    let mut cmd = Command::new(BIN);
    cmd.args(args)
        .env("HOME", dir)
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("XDG_STATE_HOME", dir.join("state"));
    match ledger_file {
        Some(v) => cmd.env("LEDGER_FILE", v),
        None => cmd.env_remove("LEDGER_FILE"),
    };
    let mut child = cmd
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

/// Run from a directory that holds nothing at all — for the cases that pass
/// absolute paths and care only about the operands.
fn run(args: &[&str], stdin: &str) -> Output {
    let dir: PathBuf = [env!("CARGO_TARGET_TMPDIR"), "_empty"].iter().collect();
    // Shared by every caller, so it is created but never removed.
    fs::create_dir_all(&dir).unwrap();
    // Shadow the repository's own `.hledger-x.toml` — see `scratch()`.
    write(
        &dir,
        ".hledger-x.toml",
        "# intentionally empty: see scratch()\n",
    );
    run_in(&dir, args, stdin)
}

/// A scratch directory unique to the calling test.
///
/// It gets an empty `.hledger-x.toml` of its own. `run_env` isolates `$HOME`
/// and `$XDG_CONFIG_HOME`, but a *local* config is discovered by walking up
/// from the working directory, and walking up from `target/tmp/<name>/`
/// reaches the repository root — whose `.hledger-x.toml` would otherwise
/// decide how these tests behave. Only the nearest local file is read, so an
/// empty one here shadows it. A test that wants settings overwrites this.
fn scratch(name: &str) -> PathBuf {
    let dir: PathBuf = [env!("CARGO_TARGET_TMPDIR"), name].iter().collect();
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    write(
        &dir,
        ".hledger-x.toml",
        "# intentionally empty: see scratch()\n",
    );
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
fn a_dash_operand_means_stdin() {
    let out = run(&["fmt", "-"], UNFORMATTED);
    assert_eq!(out.code, 0);
    assert_eq!(out.stdout, FORMATTED);
}

#[test]
fn a_dash_cannot_be_combined_with_other_operands() {
    let dir = scratch("fmt_dash_mixed");
    let a = write(&dir, "a.journal", UNFORMATTED);
    let out = run(&["fmt", "-", a.to_str().unwrap()], UNFORMATTED);
    assert_eq!(out.code, 2, "stderr: {}", out.stderr);
    assert!(out.stderr.contains("stdin"), "stderr: {}", out.stderr);
    // Nothing was written on the way to refusing.
    assert_eq!(fs::read_to_string(&a).unwrap(), UNFORMATTED);
}

// ---- what gets formatted: the configured journal, --follow trees, operands ----

/// A root that includes `sub.journal`, both unformatted.
fn tree(dir: &Path) -> (PathBuf, PathBuf) {
    let main = write(
        dir,
        "main.journal",
        &format!("include sub.journal\n\n{UNFORMATTED}"),
    );
    let sub = write(dir, "sub.journal", UNFORMATTED);
    (main, sub)
}

#[test]
fn no_arguments_formats_the_configured_journal_and_its_includes() {
    let dir = scratch("fmt_config_root");
    write(&dir, ".hledger-x.toml", "ledger_file = \"main.journal\"\n");
    let (main, sub) = tree(&dir);
    let out = run_in(&dir, &["fmt"], "");
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    assert!(fs::read_to_string(&main).unwrap().ends_with(FORMATTED));
    assert_eq!(fs::read_to_string(&sub).unwrap(), FORMATTED);
}

#[test]
fn no_arguments_falls_back_to_the_ledger_file_environment() {
    let dir = scratch("fmt_env_root");
    let (main, sub) = tree(&dir);
    let out = run_env(&dir, &["fmt"], "", Some(main.to_str().unwrap()));
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    assert!(fs::read_to_string(&main).unwrap().ends_with(FORMATTED));
    assert_eq!(fs::read_to_string(&sub).unwrap(), FORMATTED);
}

#[test]
fn no_arguments_without_a_journal_is_a_usage_error() {
    let dir = scratch("fmt_no_root");
    let out = run_in(&dir, &["fmt"], "");
    assert_eq!(out.code, 2, "stderr: {}", out.stderr);
    assert!(
        out.stderr.contains("--follow") && out.stderr.contains("LEDGER_FILE"),
        "stderr: {}",
        out.stderr
    );
}

#[test]
fn follow_formats_the_root_and_everything_it_includes() {
    let dir = scratch("fmt_follow");
    let (main, sub) = tree(&dir);
    let out = run_in(&dir, &["fmt", "--follow", main.to_str().unwrap()], "");
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    assert!(fs::read_to_string(&main).unwrap().ends_with(FORMATTED));
    assert_eq!(fs::read_to_string(&sub).unwrap(), FORMATTED);
}

#[test]
fn operands_do_not_follow_includes() {
    let dir = scratch("fmt_operand_no_follow");
    let (main, sub) = tree(&dir);
    let out = run_in(&dir, &["fmt", main.to_str().unwrap()], "");
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    assert!(fs::read_to_string(&main).unwrap().ends_with(FORMATTED));
    assert_eq!(fs::read_to_string(&sub).unwrap(), UNFORMATTED);
}

#[test]
fn follow_is_repeatable_and_combines_with_operands() {
    let dir = scratch("fmt_follow_repeat");
    let (main, sub) = tree(&dir);
    let other = write(&dir, "other.journal", UNFORMATTED);
    let loose = write(&dir, "loose.journal", UNFORMATTED);
    let out = run_in(
        &dir,
        &[
            "fmt",
            "-f",
            "main.journal",
            "--follow",
            "other.journal",
            "loose.journal",
        ],
        "",
    );
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    assert!(fs::read_to_string(&main).unwrap().ends_with(FORMATTED));
    assert_eq!(fs::read_to_string(&sub).unwrap(), FORMATTED);
    assert_eq!(fs::read_to_string(&other).unwrap(), FORMATTED);
    assert_eq!(fs::read_to_string(&loose).unwrap(), FORMATTED);
}

#[test]
fn a_file_reached_twice_is_formatted_once() {
    let dir = scratch("fmt_dedup");
    let (_main, sub) = tree(&dir);
    // `sub.journal` is in main's tree and named as an operand as well.
    let out = run_in(&dir, &["fmt", "-f", "main.journal", "sub.journal"], "");
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    assert_eq!(fs::read_to_string(&sub).unwrap(), FORMATTED);
    assert_eq!(
        out.stdout.matches("sub.journal").count(),
        1,
        "stdout: {}",
        out.stdout
    );
}

#[test]
fn a_followed_file_is_styled_by_the_root_tree() {
    let dir = scratch("fmt_follow_styles");
    // The style is declared in the root; the amount to restyle sits in the
    // included file, which has no directives of its own.
    write(
        &dir,
        "main.journal",
        "commodity 1_000.00 EUR\ninclude sub.journal\n",
    );
    let sub = write(&dir, "sub.journal", "2026-01-01 x\n    a:b  1234EUR\n");
    let out = run_in(&dir, &["fmt", "--follow", "main.journal"], "");
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    assert_eq!(
        fs::read_to_string(&sub).unwrap(),
        "2026-01-01 x\n    a:b  1_234 EUR\n"
    );

    // Reached as a plain operand it sees only its own directives, so the
    // amount stands as written.
    let sub2 = write(&dir, "sub.journal", "2026-01-01 x\n    a:b  1234EUR\n");
    let out = run_in(&dir, &["fmt", "sub.journal"], "");
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    assert_eq!(
        fs::read_to_string(&sub2).unwrap(),
        "2026-01-01 x\n    a:b  1234EUR\n"
    );
}

// ---- reporting ----

#[test]
fn changed_files_are_listed_on_stdout() {
    let dir = scratch("fmt_list");
    write(&dir, "a.journal", UNFORMATTED);
    write(&dir, "b.journal", FORMATTED);
    let out = run_in(&dir, &["fmt", "a.journal", "b.journal"], "");
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    // Only the file that actually changed, named as it was given.
    assert_eq!(out.stdout, "a.journal\n");
}

#[test]
fn quiet_suppresses_the_list_but_not_the_work() {
    let dir = scratch("fmt_quiet");
    let a = write(&dir, "a.journal", UNFORMATTED);
    let out = run_in(&dir, &["fmt", "-q", "a.journal"], "");
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    assert_eq!(out.stdout, "");
    assert_eq!(fs::read_to_string(&a).unwrap(), FORMATTED);
}

#[test]
fn formatting_stdin_lists_nothing() {
    let out = run(&["fmt", "-"], UNFORMATTED);
    // stdout is the payload; a file list would corrupt it.
    assert_eq!(out.stdout, FORMATTED);
}

// ---- --diff ----

#[test]
fn diff_shows_what_changed_and_writes_nothing() {
    let dir = scratch("fmt_diff_write");
    let a = write(&dir, "a.journal", UNFORMATTED);
    let out = run_in(&dir, &["fmt", "--diff", "a.journal"], "");
    // --diff implies --check: showing a change and making it are different
    // requests, so the exit status is 1 and the file is untouched.
    assert_eq!(out.code, 1, "stderr: {}", out.stderr);
    assert_eq!(
        out.stdout,
        "\
--- a/a.journal
+++ b/a.journal
@@ -1,2 +1,2 @@
 2025-01-01 x
-  A:B  1 USD
+    A:B  1 USD
"
    );
    assert_eq!(fs::read_to_string(&a).unwrap(), UNFORMATTED);
}

#[test]
fn diff_and_check_diff_do_the_same_thing() {
    // `--check` adds nothing to `--diff`; the pair is accepted because it
    // reads well, not because it changes anything.
    let dir = scratch("fmt_diff_implies_check");
    let a = write(&dir, "a.journal", UNFORMATTED);
    let plain = run_in(&dir, &["fmt", "--diff", "a.journal"], "");
    let paired = run_in(&dir, &["fmt", "--check", "--diff", "a.journal"], "");
    assert_eq!(plain.code, paired.code);
    assert_eq!(plain.stdout, paired.stdout);
    assert_eq!(plain.stderr, paired.stderr);
    assert_eq!(fs::read_to_string(&a).unwrap(), UNFORMATTED);
}

#[test]
fn check_diff_writes_nothing() {
    let dir = scratch("fmt_diff_check");
    let a = write(&dir, "a.journal", UNFORMATTED);
    let out = run_in(&dir, &["fmt", "--check", "--diff", "a.journal"], "");
    assert_eq!(out.code, 1, "stderr: {}", out.stderr);
    assert!(
        out.stdout.contains("+    A:B  1 USD"),
        "stdout: {}",
        out.stdout
    );
    assert!(
        out.stderr.contains("would reformat:"),
        "stderr: {}",
        out.stderr
    );
    assert_eq!(fs::read_to_string(&a).unwrap(), UNFORMATTED);
}

#[test]
fn diff_replaces_the_plain_changed_file_list() {
    let dir = scratch("fmt_diff_list");
    write(&dir, "a.journal", UNFORMATTED);
    let out = run_in(&dir, &["fmt", "--diff", "a.journal"], "");
    assert_eq!(out.code, 1, "stderr: {}", out.stderr);
    // The headers name the file; a bare `a.journal` line would be noise.
    assert!(
        !out.stdout.lines().any(|l| l == "a.journal"),
        "stdout: {}",
        out.stdout
    );
}

#[test]
fn an_unchanged_file_produces_no_diff() {
    let dir = scratch("fmt_diff_clean");
    write(&dir, "a.journal", FORMATTED);
    let out = run_in(&dir, &["fmt", "--diff", "a.journal"], "");
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    assert_eq!(out.stdout, "");
}

#[test]
fn diff_on_stdin_replaces_the_formatted_payload() {
    let out = run(&["fmt", "--diff", "-"], UNFORMATTED);
    assert_eq!(out.code, 1, "stderr: {}", out.stderr);
    assert!(
        out.stdout.starts_with("--- a/<stdin>\n+++ b/<stdin>\n"),
        "stdout: {}",
        out.stdout
    );
    assert!(
        !out.stdout.contains("2025-01-01 x\n    A:B"),
        "stdout: {}",
        out.stdout
    );
}

#[test]
fn diff_covers_every_file_in_a_followed_tree() {
    let dir = scratch("fmt_diff_follow");
    tree(&dir);
    let out = run_in(
        &dir,
        &["fmt", "--check", "--diff", "-f", "main.journal"],
        "",
    );
    assert_eq!(out.code, 1, "stderr: {}", out.stderr);
    assert!(
        out.stdout.contains("--- a/main.journal") && out.stdout.contains("--- a/sub.journal"),
        "stdout: {}",
        out.stdout
    );
}

#[test]
fn quiet_leaves_diffs_alone() {
    let dir = scratch("fmt_diff_quiet");
    write(&dir, "a.journal", UNFORMATTED);
    // -q suppresses the file list; asking for a diff is asking for output.
    let out = run_in(&dir, &["fmt", "--diff", "-q", "a.journal"], "");
    assert_eq!(out.code, 1, "stderr: {}", out.stderr);
    assert!(
        out.stdout.contains("+    A:B  1 USD"),
        "stdout: {}",
        out.stdout
    );
}

#[test]
fn diff_reflects_sorting() {
    let dir = scratch("fmt_diff_sort");
    write(&dir, "a.journal", "2025-03-02 b\n\n2025-01-05 a\n");
    let out = run_in(
        &dir,
        &["fmt", "--check", "--diff", "--sort", "a.journal"],
        "",
    );
    assert_eq!(out.code, 1, "stderr: {}", out.stderr);
    assert!(
        out.stdout.contains("+2025-01-05 a"),
        "stdout: {}",
        out.stdout
    );
}

// ---- sort as a configured setting ----

#[test]
fn sort_can_come_from_the_config() {
    let dir = scratch("fmt_config_sort");
    write(&dir, ".hledger-x.toml", "sort = true\n");
    let unsorted = "2025-03-02 b\n    A:B  1 USD\n\n2025-01-05 a\n    A:B  2 USD\n";
    let a = write(&dir, "a.journal", unsorted);
    let out = run_in(&dir, &["fmt", "a.journal"], "");
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    assert!(
        fs::read_to_string(&a).unwrap().starts_with("2025-01-05 a"),
        "file was:\n{}",
        fs::read_to_string(&a).unwrap()
    );
}

#[test]
fn no_sort_overrides_the_config() {
    let dir = scratch("fmt_no_sort");
    write(&dir, ".hledger-x.toml", "sort = true\n");
    let unsorted = "2025-03-02 b\n    A:B  1 USD\n\n2025-01-05 a\n    A:B  2 USD\n";
    let a = write(&dir, "a.journal", unsorted);
    let out = run_in(&dir, &["fmt", "--no-sort", "a.journal"], "");
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    assert_eq!(fs::read_to_string(&a).unwrap(), unsorted);
}

#[test]
fn the_configured_sort_also_governs_check() {
    let dir = scratch("fmt_config_sort_check");
    write(&dir, ".hledger-x.toml", "sort = true\n");
    write(&dir, "a.journal", "2025-03-02 b\n\n2025-01-05 a\n");
    assert_eq!(run_in(&dir, &["fmt", "--check", "a.journal"], "").code, 1);
    assert_eq!(
        run_in(&dir, &["fmt", "--check", "--no-sort", "a.journal"], "").code,
        0
    );
}

#[test]
fn files_are_formatted_in_place() {
    let dir = scratch("in_place");
    let a = write(&dir, "a.journal", UNFORMATTED);
    let b = write(&dir, "b.journal", UNFORMATTED);
    let out = run(&["fmt", a.to_str().unwrap(), b.to_str().unwrap()], "");
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
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
        out.stderr.contains("would reformat:") && out.stderr.contains("a.journal"),
        "stderr: {}",
        out.stderr
    );
    // --check must not modify the file.
    assert_eq!(fs::read_to_string(&a).unwrap(), UNFORMATTED);
}

#[test]
fn check_on_stdin_reports_through_the_exit_code_only() {
    let bad = run(&["fmt", "--check", "-"], UNFORMATTED);
    assert_eq!(bad.code, 1);
    assert_eq!(bad.stdout, "");
    let good = run(&["fmt", "--check", "-"], FORMATTED);
    assert_eq!(good.code, 0);
    assert_eq!(good.stdout, "");
}

#[test]
fn sort_reorders_transactions_by_date() {
    let src = "2025-03-02 b\n    A:B  1 USD\n\n2025-01-05 a\n    A:B  2 USD\n";
    let out = run(&["fmt", "--sort", "-"], src);
    assert_eq!(out.code, 0);
    assert_eq!(
        out.stdout,
        "2025-01-05 a\n    A:B  2 USD\n\n2025-03-02 b\n    A:B  1 USD\n"
    );
}

#[test]
fn check_sort_measures_against_the_sorted_form() {
    let unsorted = "2025-03-02 b\n\n2025-01-05 a\n";
    assert_eq!(run(&["fmt", "--check", "--sort", "-"], unsorted).code, 1);
    // Formatted but unsorted still passes a plain --check.
    assert_eq!(run(&["fmt", "--check", "-"], unsorted).code, 0);
}

// ---- exit codes: 0 clean, 1 would reformat, 2 usage, 3 error ----

#[test]
fn an_unreadable_file_is_reported_and_exits_three() {
    let dir = scratch("missing");
    let missing = dir.join("nope.journal");
    let out = run(&["fmt", missing.to_str().unwrap()], "");
    assert_eq!(out.code, 3, "stderr: {}", out.stderr);
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
    assert_eq!(out.code, 3, "stderr: {}", out.stderr);
    assert_eq!(fs::read_to_string(&good).unwrap(), FORMATTED);
}

#[test]
fn an_error_outranks_a_check_failure() {
    let dir = scratch("worst_wins");
    write(&dir, "a.journal", UNFORMATTED);
    let out = run_in(&dir, &["fmt", "--check", "a.journal", "nope.journal"], "");
    assert_eq!(out.code, 3, "stderr: {}", out.stderr);
    assert!(
        out.stderr.contains("would reformat:"),
        "stderr: {}",
        out.stderr
    );
}

#[test]
fn a_directory_operand_is_reported_and_exits_three() {
    let dir = scratch("dir_operand");
    fs::create_dir(dir.join("sub")).unwrap();
    let a = write(&dir, "a.journal", UNFORMATTED);
    let out = run_in(&dir, &["fmt", "sub", "a.journal"], "");
    assert_eq!(out.code, 3, "stderr: {}", out.stderr);
    assert!(
        out.stderr.contains("is a directory") && !out.stderr.contains("os error"),
        "stderr: {}",
        out.stderr
    );
    // The other operand is still processed.
    assert_eq!(fs::read_to_string(&a).unwrap(), FORMATTED);
}

#[test]
fn a_directory_root_is_reported_too() {
    let dir = scratch("dir_root");
    fs::create_dir(dir.join("sub")).unwrap();
    let out = run_in(&dir, &["fmt", "--follow", "sub"], "");
    assert_eq!(out.code, 3, "stderr: {}", out.stderr);
    assert!(
        out.stderr.contains("is a directory"),
        "stderr: {}",
        out.stderr
    );
}

#[test]
fn an_unreadable_root_is_reported_and_exits_three() {
    let dir = scratch("missing_root");
    let out = run_in(&dir, &["fmt", "--follow", "nope.journal"], "");
    assert_eq!(out.code, 3, "stderr: {}", out.stderr);
    assert!(
        out.stderr.contains("nope.journal"),
        "stderr: {}",
        out.stderr
    );
    // Reported once, not once per resolution attempt.
    assert_eq!(out.stderr.matches("nope.journal").count(), 1);
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
    assert!(
        help.stdout.contains("--check")
            && help.stdout.contains("--sort")
            && help.stdout.contains("--follow"),
        "help was:\n{}",
        help.stdout
    );

    let version = run(&["--version"], "");
    assert_eq!(version.code, 0);
    assert!(version.stdout.contains(env!("CARGO_PKG_VERSION")));
}

// ---- hledger-x add (plain line mode: stdin is a pipe) ----

/// Run `hledger-x add` with an isolated HOME/XDG so no user config or
/// recovery state leaks in or out.
fn run_add(dir: &Path, args: &[&str], stdin: &str) -> Output {
    run_add_env(dir, args, stdin, None)
}

/// As [`run_add`], with an explicit `$LEDGER_FILE` (unset when `None`).
fn run_add_env(dir: &Path, args: &[&str], stdin: &str, ledger_file: Option<&str>) -> Output {
    let mut all = vec!["add"];
    all.extend_from_slice(args);
    run_env(dir, &all, stdin, ledger_file)
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
    // account 2 (accept), amount (accept balancing) finishes; EOF quits.
    let input = "\nRewe\n\n18.20 EUR\n\n\n";
    let out = run_add(&dir, &["-f", journal.to_str().unwrap()], input);
    assert_eq!(
        out.code, 0,
        "stderr: {}\nstdout: {}",
        out.stderr, out.stdout
    );
    let written = fs::read_to_string(&journal).unwrap();
    assert!(
        written.contains("Rewe\n    expenses:groceries     18.20 EUR\n"),
        "file was:\n{written}"
    );
    assert!(written.contains("assets:bank:checking  -18.20 EUR\n"));
    assert!(out.stderr.contains("wrote 1 transaction"), "{}", out.stderr);
    assert!(!out.stderr.contains("(s)"), "{}", out.stderr);
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
    assert!(
        out.stderr.contains("does not include that file"),
        "{}",
        out.stderr
    );
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
    let input = "\nRewe\n\n5.00 EUR\nassets:cash\n\n";
    let out = run_add(
        &dir,
        &["-f", main.to_str().unwrap(), "--to", sub.to_str().unwrap()],
        input,
    );
    assert_eq!(
        out.code, 0,
        "stderr: {}\nstdout: {}",
        out.stderr, out.stdout
    );
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
    write(
        &dir,
        ".hledger-x.toml",
        "[add]\ninsertion = \"chronological\"\n",
    );
    let journal = write(
        &dir,
        "main.journal",
        "2026-01-01 early\n    a:b   1.00 EUR\n    c:d  -1.00 EUR\n\n2026-12-31 late\n    a:b   2.00 EUR\n    c:d  -2.00 EUR\n",
    );
    // Dated between the two: must land between them, not at the end.
    let input = "2026-06-15\nMiddle\na:b\n3.00 EUR\nc:d\n\n";
    let out = run_add(&dir, &["-f", journal.to_str().unwrap()], input);
    assert_eq!(
        out.code, 0,
        "stderr: {}\nstdout: {}",
        out.stderr, out.stdout
    );
    let text = fs::read_to_string(&journal).unwrap();
    let early = text.find("early").unwrap();
    let middle = text.find("Middle").unwrap();
    let late = text.find("late").unwrap();
    assert!(early < middle && middle < late, "file was:\n{text}");
}

#[test]
fn add_falls_back_to_the_configured_ledger_file() {
    let dir = scratch("add_config_ledger_file");
    // Relative to the config file's directory.
    write(&dir, ".hledger-x.toml", "ledger_file = \"main.journal\"\n");
    let journal = write(&dir, "main.journal", ADD_JOURNAL);
    let input = "\nRewe\n\n18.20 EUR\n\n\n";
    let out = run_add(&dir, &[], input);
    assert_eq!(
        out.code, 0,
        "stderr: {}\nstdout: {}",
        out.stderr, out.stdout
    );
    assert!(fs::read_to_string(&journal).unwrap().contains("18.20 EUR"));
}

#[test]
fn the_ledger_file_precedence_is_flag_then_config_then_env() {
    let dir = scratch("add_ledger_file_precedence");
    let flagged = write(&dir, "flag.journal", ADD_JOURNAL);
    let configured = write(&dir, "config.journal", ADD_JOURNAL);
    let env = write(&dir, "env.journal", ADD_JOURNAL);
    write(
        &dir,
        ".hledger-x.toml",
        "ledger_file = \"config.journal\"\n",
    );
    let input = "\nRewe\n\n18.20 EUR\n\n\n";

    // The flag beats both.
    let out = run_add_env(
        &dir,
        &["-f", flagged.to_str().unwrap()],
        input,
        Some(env.to_str().unwrap()),
    );
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    assert!(fs::read_to_string(&flagged).unwrap().contains("18.20 EUR"));
    assert!(!fs::read_to_string(&env).unwrap().contains("18.20 EUR"));

    // Without the flag, the config beats the environment.
    let out = run_add_env(&dir, &[], input, Some(env.to_str().unwrap()));
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    assert!(fs::read_to_string(&configured)
        .unwrap()
        .contains("18.20 EUR"));
    assert!(!fs::read_to_string(&env).unwrap().contains("18.20 EUR"));
}

#[test]
fn fmt_does_not_restyle_by_a_style_included_after_the_amounts() {
    // hledger reads includes where they stand, so a style pulled in below a
    // transaction is not known when that transaction is parsed. Restyling by
    // it would change what the amounts mean: `1,234 GBP` read without the
    // style is 1.234.
    let dir = scratch("fmt_include_order");
    write(&dir, "conf.journal", "commodity 1,000.00 GBP\n");
    let main = write(
        &dir,
        "main.journal",
        "2026-01-01 x\n    a:b  1234 GBP\n    c:d  -1234 GBP\n\ninclude conf.journal\n",
    );
    let out = run(&["fmt", main.to_str().unwrap()], "");
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    assert_eq!(
        fs::read_to_string(&main).unwrap(),
        "2026-01-01 x\n    a:b   1234 GBP\n    c:d  -1234 GBP\n\ninclude conf.journal\n"
    );
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
    let out = run(&["fmt", "-"], "2026-01-01 x\n    a:b  1234EUR\n");
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
    let input = "2026-06-15\nIkea\ne:f\n1234EUR\ng:h\n\n";
    let out = run_add(&dir, &["-f", journal.to_str().unwrap()], input);
    assert_eq!(
        out.code, 0,
        "stderr: {}\nstdout: {}",
        out.stderr, out.stdout
    );
    let text = fs::read_to_string(&journal).unwrap();
    // Both sides take the declared style, decimal places included: the
    // amount that was typed and the balancing amount that was generated end
    // up in the same form.
    assert!(
        text.contains("1_234.00 EUR") && text.contains("-1_234.00 EUR"),
        "file was:\n{text}"
    );
}

#[test]
fn add_appends_equity_conversion_postings_when_configured() {
    let dir = scratch("add_equity_conversion");
    write(&dir, ".hledger-x.toml", "[add]\nequity_conversion = true\n");
    let journal = write(
        &dir,
        "main.journal",
        "2026-08-01 seed\n    a:b   1.00 EUR\n    c:d  -1.00 EUR\n",
    );
    // A multi-commodity transaction: balanced at cost, not at face value.
    let input = "2026-08-04\nIVPN\nexpenses:subscriptions:services\n10 USD @@ 9.06 EUR\nassets:dkb:giro\n-9.06 EUR\n\n";
    let out = run_add(&dir, &["-f", journal.to_str().unwrap()], input);
    assert_eq!(
        out.code, 0,
        "stderr: {}\nstdout: {}",
        out.stderr, out.stdout
    );
    let text = fs::read_to_string(&journal).unwrap();
    assert!(
        text.contains("equity:conversion") && text.contains("-10 USD"),
        "file was:\n{text}"
    );
    // Written in the order the commodities appear.
    let conversions: Vec<&str> = text
        .lines()
        .filter(|l| l.contains("equity:conversion"))
        .collect();
    assert_eq!(conversions.len(), 2, "file was:\n{text}");
    assert!(conversions[0].ends_with("-10 USD"), "file was:\n{text}");
    assert!(conversions[1].ends_with("9.06 EUR"), "file was:\n{text}");

    // hledger accepts the result, and its face-value balance is zero.
    if Command::new("hledger").arg("--version").output().is_ok() {
        let bal = Command::new("hledger")
            .args(["-f", journal.to_str().unwrap(), "balance", "--no-total"])
            .output()
            .unwrap();
        assert!(
            bal.status.success(),
            "hledger rejected the journal: {}",
            String::from_utf8_lossy(&bal.stderr)
        );
    }
}

#[test]
fn add_writes_each_conversion_as_its_own_group() {
    let dir = scratch("add_two_conversions");
    write(&dir, ".hledger-x.toml", "[add]\nequity_conversion = true\n");
    let journal = write(
        &dir,
        "main.journal",
        "2026-08-01 seed\n    a:b   1.00 EUR\n    c:d  -1.00 EUR\n",
    );
    // Two conversions in one transaction, with the yen posting typed before
    // the postings it funds.
    let input = "2026-08-04\ntwo conversions\nassets:dollars\n$-135\nassets:yen\n\u{a5}-100\nassets:euros\n\u{20ac}100 @ $1.35\nassets:euros\n\u{20ac}1 @@ \u{a5}100\n\n.\n";
    let out = run_add(&dir, &["-f", journal.to_str().unwrap()], input);
    assert_eq!(
        out.code, 0,
        "stderr: {}\nstdout: {}",
        out.stderr, out.stdout
    );
    let text = fs::read_to_string(&journal).unwrap();
    let body: Vec<&str> = text
        .lines()
        .skip_while(|l| !l.contains("two conversions"))
        .skip(1)
        .take_while(|l| !l.trim().is_empty())
        .map(str::trim)
        .collect();
    // Each conversion's postings, then the pair cancelling its cost; the
    // yen posting has moved beside the conversion it funds.
    assert_eq!(body.len(), 9, "file was:\n{text}");
    assert!(body[0].starts_with("assets:dollars"), "{body:?}");
    assert!(body[1].starts_with("assets:euros"), "{body:?}");
    assert!(body[2].starts_with("equity:conversion"), "{body:?}");
    assert!(body[3].starts_with("equity:conversion"), "{body:?}");
    assert_eq!(body[4], ";", "{body:?}");
    assert!(body[5].starts_with("assets:yen"), "{body:?}");
    assert!(body[6].starts_with("assets:euros"), "{body:?}");
    assert!(body[7].starts_with("equity:conversion"), "{body:?}");
    assert!(body[8].starts_with("equity:conversion"), "{body:?}");

    // The point of the grouping: hledger only matches equity postings with
    // the costs they cancel when they sit in adjacent pairs. A single
    // summed posting per commodity is rejected as unbalanced.
    if Command::new("hledger").arg("--version").output().is_ok() {
        let print = Command::new("hledger")
            .args(["-n", "-f", journal.to_str().unwrap(), "print"])
            .output()
            .unwrap();
        assert!(
            print.status.success(),
            "hledger rejected the journal: {}",
            String::from_utf8_lossy(&print.stderr)
        );
    }
}

#[test]
fn add_leaves_conversions_alone_by_default() {
    let dir = scratch("add_no_equity_conversion");
    let journal = write(
        &dir,
        "main.journal",
        "2026-08-01 seed\n    a:b   1.00 EUR\n    c:d  -1.00 EUR\n",
    );
    let input = "2026-08-04\nIVPN\nexpenses:subscriptions:services\n10 USD @@ 9.06 EUR\nassets:dkb:giro\n-9.06 EUR\n\n";
    let out = run_add(&dir, &["-f", journal.to_str().unwrap()], input);
    assert_eq!(
        out.code, 0,
        "stderr: {}\nstdout: {}",
        out.stderr, out.stdout
    );
    let text = fs::read_to_string(&journal).unwrap();
    assert!(!text.contains("equity:conversion"), "file was:\n{text}");
}

// ---------------------------------------------------------------------------
// Error presentation.
//
// These pin the promise rather than the prose: whatever a message says, it
// must not say it in Rust's or the OS's vocabulary. `eyre`'s `Error:` banner
// and `Location:` frame, backtrace instructions, `os error N`, serde's
// `invalid type`, and rustc's `|`-gutter diagnostic art are all things the
// user has no use for and cannot act on.
// ---------------------------------------------------------------------------

/// Everything a user must never be shown, and what it would mean if they were.
const LEAKS: &[(&str, &str)] = &[
    ("os error", "a raw errno"),
    ("Location:", "an eyre source-location frame"),
    ("RUST_BACKTRACE", "backtrace instructions"),
    ("Backtrace omitted", "backtrace instructions"),
    ("invalid type:", "serde's type vocabulary"),
    ("unknown field", "serde's field vocabulary"),
    ("TOML parse error", "the toml crate's own header"),
    ("  |", "rustc-style diagnostic gutter"),
    ("^^^", "rustc-style diagnostic carets"),
];

#[track_caller]
fn assert_no_leaks(stderr: &str) {
    for (needle, what) in LEAKS {
        assert!(
            !stderr.contains(needle),
            "{what} (`{needle}`) reached the user:\n{stderr}"
        );
    }
}

#[test]
fn a_missing_file_is_reported_in_english() {
    let dir = scratch("err_missing");
    let out = run_in(&dir, &["fmt", "nope.journal"], "");
    assert_eq!(out.code, 3, "stderr: {}", out.stderr);
    assert!(out.stderr.contains("no such file"), "{}", out.stderr);
    assert_no_leaks(&out.stderr);
}

#[test]
fn an_unreadable_file_is_reported_in_english() {
    let dir = scratch("err_perm");
    let p = write(&dir, "locked.journal", "2026-01-01 x\n    a:b  1\n");
    fs::set_permissions(
        &p,
        <fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o000),
    )
    .unwrap();
    let out = run_in(&dir, &["fmt", p.to_str().unwrap()], "");
    fs::set_permissions(
        &p,
        <fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o644),
    )
    .unwrap();
    assert_eq!(out.code, 3, "stderr: {}", out.stderr);
    assert!(out.stderr.contains("permission denied"), "{}", out.stderr);
    assert_no_leaks(&out.stderr);
}

#[test]
fn a_typo_in_the_config_names_the_setting_it_meant() {
    let dir = scratch("err_cfg_typo");
    write(&dir, ".hledger-x.toml", "[add]\nformatfile = true\n");
    let out = run_in(&dir, &["fmt", "-"], "");
    assert_eq!(out.code, 2, "stderr: {}", out.stderr);
    assert!(
        out.stderr.contains("unknown setting `formatfile`")
            && out.stderr.contains("did you mean `format_file`?"),
        "{}",
        out.stderr
    );
    assert_no_leaks(&out.stderr);
}

#[test]
fn an_add_setting_outside_the_add_section_says_where_it_belongs() {
    let dir = scratch("err_cfg_section");
    write(&dir, ".hledger-x.toml", "strict = true\n");
    let out = run_in(&dir, &["fmt", "-"], "");
    assert_eq!(out.code, 2, "stderr: {}", out.stderr);
    assert!(
        out.stderr.contains("unknown setting `strict`")
            && out.stderr.contains("`strict` belongs in the [add] section"),
        "{}",
        out.stderr
    );
    assert_no_leaks(&out.stderr);
}

#[test]
fn a_wrongly_typed_config_value_says_what_it_wanted() {
    let dir = scratch("err_cfg_type");
    write(
        &dir,
        ".hledger-x.toml",
        "sort = true\n[add]\nstrict = \"yes\"\n",
    );
    let out = run_in(&dir, &["fmt", "-"], "");
    assert_eq!(out.code, 2, "stderr: {}", out.stderr);
    assert!(
        out.stderr.contains("line 3") && out.stderr.contains("expected true or false"),
        "{}",
        out.stderr
    );
    assert_no_leaks(&out.stderr);
}

#[test]
fn add_reports_a_missing_journal_without_a_report_banner() {
    let dir = scratch("err_add_missing");
    let out = run_add(&dir, &["-f", "nope.journal"], "");
    assert_ne!(out.code, 0);
    assert!(out.stderr.starts_with("hledger-x add: "), "{}", out.stderr);
    assert!(out.stderr.contains("no such file"), "{}", out.stderr);
    assert_no_leaks(&out.stderr);
}

#[test]
fn add_reports_a_bad_config_without_a_report_banner() {
    let dir = scratch("err_add_cfg");
    write(
        &dir,
        ".hledger-x.toml",
        "[add]\nequity_conversion_account = \"\"\n",
    );
    let out = run_add(&dir, &[], "");
    assert_eq!(out.code, 2, "stderr: {}", out.stderr);
    assert!(out.stderr.starts_with("hledger-x add: "), "{}", out.stderr);
    assert_no_leaks(&out.stderr);
}

/// A journal that both leaves an amount to be inferred and writes one short
/// of its commodity's declared decimal places — the two things `--explicit`
/// changes.
const IMPLIED: &str = "commodity 1,000.00 EUR\n\n2025-01-01 x\n    A:B  1 EUR\n    C:D\n";
const SPELLED_OUT: &str =
    "commodity 1,000.00 EUR\n\n2025-01-01 x\n    A:B   1.00 EUR\n    C:D  -1.00 EUR\n";

#[test]
fn explicit_fills_in_and_pads_on_stdin() {
    let out = run(&["fmt", "--explicit", "-"], IMPLIED);
    assert_eq!(out.code, 0);
    assert_eq!(out.stdout, SPELLED_OUT);
    // And `-x`, the way hledger spells the same flag.
    assert_eq!(run(&["fmt", "-x", "-"], IMPLIED).stdout, SPELLED_OUT);
}

#[test]
fn without_explicit_nothing_is_filled_in_or_padded() {
    let out = run(&["fmt", "-"], IMPLIED);
    assert_eq!(out.code, 0);
    assert_eq!(out.stdout, IMPLIED);
}

#[test]
fn check_honours_explicit() {
    let dir = scratch("check-explicit");
    let path = write(&dir, "a.journal", IMPLIED);

    // The file is already formatted, so a plain --check is happy with it.
    let plain = run_in(&dir, &["fmt", "--check", "a.journal"], "");
    assert_eq!(plain.code, 0, "{}", plain.stderr);

    // --explicit raises the bar, and --check has to apply the same bar.
    let strict = run_in(&dir, &["fmt", "--check", "--explicit", "a.journal"], "");
    assert_eq!(strict.code, 1);
    assert!(
        strict.stderr.contains("would reformat: a.journal"),
        "{}",
        strict.stderr
    );
    // --check still writes nothing.
    assert_eq!(fs::read_to_string(&path).unwrap(), IMPLIED);

    // Once spelled out, --check --explicit passes.
    write(&dir, "a.journal", SPELLED_OUT);
    let after = run_in(&dir, &["fmt", "--check", "--explicit", "a.journal"], "");
    assert_eq!(after.code, 0, "{}", after.stderr);
}

#[test]
fn explicit_rewrites_the_file_in_place() {
    let dir = scratch("explicit-in-place");
    let path = write(&dir, "a.journal", IMPLIED);
    let out = run_in(&dir, &["fmt", "--explicit", "a.journal"], "");
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(fs::read_to_string(&path).unwrap(), SPELLED_OUT);
}

#[test]
fn explicit_cannot_be_turned_on_from_the_config() {
    // It rewrites amounts rather than layout, so it is a flag only: a config
    // naming it is a config error, not a silent no-op.
    let dir = scratch("explicit-not-configurable");
    write(&dir, ".hledger-x.toml", "explicit = true\n");
    write(&dir, "a.journal", IMPLIED);
    let out = run_in(&dir, &["fmt", "a.journal"], "");
    assert_ne!(out.code, 0, "an unknown config key should be rejected");
    assert!(out.stderr.contains("config"), "{}", out.stderr);
}
