//! The write path: rendering new transactions, insertion, write modes, and
//! the recovery journal.
//!
//! Everything is buffered during the session and written once on exit; the
//! recovery journal under `$XDG_STATE_HOME/hledger-x/` is the crash net in
//! between.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use chrono::NaiveDate;

use super::parser::FileMap;
use crate::fmt::posting::{parse_posting, render};
use crate::fmt::{format_sorted_with, format_with, unlines, widths_with};
use crate::lex::{is_blank, split_amount};

/// Where a new transaction goes in the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Insertion {
    /// At the end of the file.
    #[default]
    Append,
    /// After the last transaction with a date `<=` the new one, following
    /// `fmt --sort` semantics.
    Chronological,
}

/// Write-mode settings (see `DESIGN.md` § Write modes).
#[derive(Debug, Clone, Copy)]
pub struct WriteOptions {
    /// Reformat the whole file on write (default). When the file is already
    /// formatted and widths do not grow this is byte-identical to a pure
    /// append.
    pub format_file: bool,
    /// Also sort transactions by date (`fmt --sort` semantics).
    pub sort: bool,
    /// Insertion point for new transactions.
    pub insertion: Insertion,
}

impl Default for WriteOptions {
    fn default() -> Self {
        Self {
            format_file: true,
            sort: false,
            insertion: Insertion::Append,
        }
    }
}

/// A completed transaction, ready to write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewTransaction {
    /// Fully resolved date.
    pub date: NaiveDate,
    /// Description as typed.
    pub description: String,
    /// (account, raw amount text) pairs; the amount may be empty.
    pub postings: Vec<(String, String)>,
}

/// The outcome of integrating new transactions into a file.
#[derive(Debug, Clone)]
pub struct Integrated {
    /// The file's new contents.
    pub contents: String,
    /// Non-fatal notes for the user.
    pub warnings: Vec<String>,
}

impl NewTransaction {
    /// The transaction in raw journal syntax (header + minimally separated
    /// postings), before any alignment.
    #[must_use]
    pub fn raw_lines(&self) -> Vec<String> {
        let mut out = vec![format!("{} {}", self.date.format("%Y-%m-%d"), self.description)
            .trim_end()
            .to_owned()];
        for (account, amount) in &self.postings {
            if amount.trim().is_empty() {
                out.push(format!("    {account}"));
            } else {
                out.push(format!("    {account}  {}", amount.trim()));
            }
        }
        out
    }

    /// The widths this transaction's postings need.
    fn own_widths(&self) -> (usize, usize) {
        let acc_w = self
            .postings
            .iter()
            .map(|(a, _)| a.chars().count())
            .max()
            .unwrap_or(0);
        let num_w = self
            .postings
            .iter()
            .map(|(_, amt)| {
                let toks: Vec<&str> = amt.split_whitespace().collect();
                let (num, _c, _r) = split_amount(&toks);
                num.chars().count()
            })
            .max()
            .unwrap_or(0);
        (acc_w, num_w)
    }
}

/// Integrate `txns` into a file's contents per the write options.
///
/// The default path formats the whole file; with `format_file = false` only
/// the added lines are written, rendered against widths computed over the
/// entire file *including* the new transactions.
#[must_use]
pub fn integrate(src: &str, txns: &[NewTransaction], opts: &WriteOptions) -> Integrated {
    let ctx = crate::fmt::scan_ctx(&crate::fmt::lines(src));
    integrate_with(src, txns, opts, &ctx)
}

/// [`integrate`] with the caller's declared styles — the include tree's,
/// which the target file's own text cannot see.
#[must_use]
pub fn integrate_with(
    src: &str,
    txns: &[NewTransaction],
    opts: &WriteOptions,
    ctx: &crate::amount::AmountCtx,
) -> Integrated {
    let mut warnings = Vec::new();
    let map = FileMap::build_with(src, ctx);

    if opts.format_file && !map.formatted && !src.is_empty() {
        warnings.push("file was not formatted; it will be reformatted on write".to_owned());
    }
    if opts.insertion == Insertion::Chronological && !opts.sort && !map.is_date_sorted() {
        warnings.push(
            "file is not sorted by date; inserting chronologically anyway".to_owned(),
        );
    }

    let mut lines: Vec<String> = map.lines.clone();
    for txn in txns {
        let at = insertion_index(&FileMap::build(&unlines(&lines)), txn.date, opts.insertion);
        splice(&mut lines, at, &txn.raw_lines());
    }

    if opts.format_file {
        let joined = unlines(&lines);
        let contents = if opts.sort {
            format_sorted_with(&joined, ctx)
        } else {
            format_with(&joined, ctx)
        };
        return Integrated { contents, warnings };
    }

    // format_file = false: render only our own lines, at widths computed over
    // the whole file including the new transactions.
    let joined = unlines(&lines);
    let all: Vec<&str> = crate::fmt::lines(&joined);
    let (mut acc_w, mut num_w) = widths_with(&all, ctx);
    for t in txns {
        let (a, n) = t.own_widths();
        acc_w = acc_w.max(a);
        num_w = num_w.max(n);
    }
    if map.formatted && (acc_w > map.acc_w || num_w > map.num_w) {
        warnings.push(
            "new transaction widens the alignment columns; existing lines are now stale and a later `fmt` run will reflow them"
                .to_owned(),
        );
    }

    // Re-do the insertion, this time rendering our lines aligned.
    let mut lines: Vec<String> = map.lines;
    for txn in txns {
        let at = insertion_index(&FileMap::build(&unlines(&lines)), txn.date, opts.insertion);
        let mut rendered = vec![txn
            .raw_lines()
            .first()
            .cloned()
            .unwrap_or_default()];
        for raw in txn.raw_lines().iter().skip(1) {
            rendered.push(render(
                acc_w,
                num_w,
                &crate::fmt::posting::restyle(parse_posting(raw), ctx),
            ));
        }
        splice(&mut lines, at, &rendered);
    }
    Integrated {
        contents: unlines(&lines),
        warnings,
    }
}

/// Insert `block` at line index `at`, managing blank-line separation: one
/// blank line between the block and any neighbouring content.
fn splice(lines: &mut Vec<String>, at: usize, block: &[String]) {
    let at = at.min(lines.len());
    let mut insert: Vec<String> = Vec::new();
    let before_nonblank = lines
        .get(..at)
        .is_some_and(|s| s.iter().any(|l| !is_blank(l)));
    let needs_gap_before = at
        .checked_sub(1)
        .and_then(|i| lines.get(i))
        .is_some_and(|prev| !is_blank(prev));
    if before_nonblank && needs_gap_before {
        insert.push(String::new());
    }
    insert.extend_from_slice(block);
    if lines.get(at).is_some_and(|next| !is_blank(next)) {
        insert.push(String::new());
    }
    let tail: Vec<String> = lines.split_off(at);
    lines.extend(insert);
    lines.extend(tail);
}

/// The line index at which a transaction dated `date` goes.
fn insertion_index(map: &FileMap, date: NaiveDate, insertion: Insertion) -> usize {
    match insertion {
        Insertion::Append => map.lines.len(),
        Insertion::Chronological => {
            // After the last transaction with date <= the new one. If none,
            // before the first transaction; if there are no transactions at
            // all, the end of the file.
            let mut at = None;
            for t in &map.transactions {
                match t.date {
                    Some(d) if d <= date => at = Some(t.end),
                    _ => {}
                }
            }
            at.unwrap_or_else(|| {
                map.transactions.first().map_or(map.lines.len(), |t| t.start)
            })
        }
    }
}

// ---- recovery journal ----

/// The crash-recovery journal.
///
/// Completed transactions are appended here as the session progresses and
/// the file is removed after a successful write. If it exists at launch, the
/// previous session died — its transactions are replayed into the new
/// session.
#[derive(Debug, Clone)]
pub struct Recovery {
    path: PathBuf,
}

impl Recovery {
    /// A recovery journal at an explicit path (tests, unusual setups).
    #[must_use]
    pub const fn at(path: PathBuf) -> Self {
        Self { path }
    }

    /// The recovery file for a given write target.
    #[must_use]
    pub fn for_target(target: &Path) -> Self {
        let mut name = String::from("recovery-");
        // A readable, filesystem-safe encoding of the target path.
        for c in target.to_string_lossy().chars() {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                name.push(c);
            } else {
                name.push('_');
            }
        }
        name.push_str(".journal");
        Self {
            path: state_dir().join(name),
        }
    }

    /// Where the recovery journal lives (for messages).
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one completed transaction.
    ///
    /// # Errors
    ///
    /// I/O. The caller warns and continues — recovery is best-effort and must
    /// never block entry.
    pub fn record(&self, txn: &NewTransaction) -> io::Result<()> {
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir)?;
        }
        let mut text = unlines(&txn.raw_lines());
        text.push('\n');
        let existing = fs::read_to_string(&self.path).unwrap_or_default();
        fs::write(&self.path, format!("{existing}{text}"))
    }

    /// The transactions left behind by a dead session, if any.
    #[must_use]
    pub fn pending(&self) -> Vec<NewTransaction> {
        let Ok(src) = fs::read_to_string(&self.path) else {
            return Vec::new();
        };
        parse_transactions(&src)
    }

    /// Remove the file after a successful write (or a deliberate abort).
    pub fn clear(&self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Parse journal text back into [`NewTransaction`]s (recovery replay and the
/// Ctrl-E editor round-trip). Lenient: unparseable blocks are dropped.
#[must_use]
pub fn parse_transactions(src: &str) -> Vec<NewTransaction> {
    let map = FileMap::build(src);
    let mut out = Vec::new();
    for span in &map.transactions {
        let Some(date) = span.date else { continue };
        let Some(header) = map.lines.get(span.start) else {
            continue;
        };
        let description = header
            .split_once(char::is_whitespace)
            .map_or("", |(_, d)| d)
            .trim()
            .to_owned();
        let mut postings = Vec::new();
        for line in map
            .lines
            .get(span.start.saturating_add(1)..span.end)
            .unwrap_or(&[])
        {
            match parse_posting(line) {
                crate::fmt::posting::Posting::Bare(account, _) => {
                    postings.push((account, String::new()));
                }
                crate::fmt::posting::Posting::Amount {
                    account,
                    num,
                    commodity,
                    rest,
                    ..
                } => {
                    let mut amount = num;
                    if !commodity.is_empty() {
                        amount.push(' ');
                        amount.push_str(&commodity);
                    }
                    if !rest.is_empty() {
                        amount.push(' ');
                        amount.push_str(&rest.join(" "));
                    }
                    postings.push((account, amount));
                }
                crate::fmt::posting::Posting::Comment(_) => {}
            }
        }
        out.push(NewTransaction {
            date,
            description,
            postings,
        });
    }
    out
}

/// `$XDG_STATE_HOME/hledger-x`, defaulting to `~/.local/state/hledger-x`.
fn state_dir() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME").map_or_else(
        || {
            std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from)
                .join(".local")
                .join("state")
        },
        PathBuf::from,
    )
    .join("hledger-x")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    fn txn(date: NaiveDate, desc: &str, postings: &[(&str, &str)]) -> NewTransaction {
        NewTransaction {
            date,
            description: desc.into(),
            postings: postings
                .iter()
                .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
                .collect(),
        }
    }

    const OPTS: WriteOptions = WriteOptions {
        format_file: true,
        sort: false,
        insertion: Insertion::Append,
    };

    #[test]
    fn appending_to_a_formatted_file_is_a_pure_append() {
        let src = "2026-01-05 a\n    aa:bb   1.00 EUR\n    cc:dd  -1.00 EUR\n";
        let new = txn(d(2026, 1, 6), "b", &[("aa:bb", "2.00 EUR"), ("cc:dd", "-2.00 EUR")]);
        let out = integrate(src, &[new], &OPTS);
        assert_eq!(
            out.contents,
            "2026-01-05 a\n    aa:bb   1.00 EUR\n    cc:dd  -1.00 EUR\n\n2026-01-06 b\n    aa:bb   2.00 EUR\n    cc:dd  -2.00 EUR\n"
        );
        assert!(out.warnings.is_empty());
    }

    #[test]
    fn a_wider_transaction_reflows_the_whole_file_in_format_mode() {
        let src = "2026-01-05 a\n    aa:bb  1.00 EUR\n";
        let new = txn(d(2026, 1, 6), "b", &[("expenses:very:long", "-7485978.18 EUR")]);
        let out = integrate(src, &[new], &OPTS);
        assert_eq!(
            out.contents,
            "2026-01-05 a\n    aa:bb                      1.00 EUR\n\n2026-01-06 b\n    expenses:very:long  -7485978.18 EUR\n"
        );
    }

    #[test]
    fn an_unformatted_file_warns_before_being_reformatted() {
        let src = "2026-01-05 a\n  aa:bb  1.00 EUR\n";
        let out = integrate(src, &[txn(d(2026, 1, 6), "b", &[("aa:bb", "1 EUR")])], &OPTS);
        assert!(out.warnings.iter().any(|w| w.contains("reformatted")));
    }

    #[test]
    fn writing_into_an_empty_file_produces_no_leading_blank() {
        let out = integrate("", &[txn(d(2026, 1, 6), "b", &[("a:b", "1 EUR")])], &OPTS);
        assert_eq!(out.contents, "2026-01-06 b\n    a:b  1 EUR\n");
    }

    #[test]
    fn amountless_postings_write_bare() {
        let out = integrate(
            "",
            &[txn(d(2026, 1, 6), "b", &[("a:b", "1 EUR"), ("c:d", "-1 EUR"), ("e:f", "")])],
            &OPTS,
        );
        assert_eq!(
            out.contents,
            "2026-01-06 b\n    a:b   1 EUR\n    c:d  -1 EUR\n    e:f\n"
        );
    }

    #[test]
    fn chronological_insertion_lands_after_the_last_earlier_or_equal_date() {
        let src = "2026-01-05 a\n    x:y  1 EUR\n\n2026-01-10 c\n    x:y  1 EUR\n";
        let opts = WriteOptions {
            insertion: Insertion::Chronological,
            ..WriteOptions::default()
        };
        let out = integrate(src, &[txn(d(2026, 1, 7), "b", &[("x:y", "2 EUR")])], &opts);
        assert_eq!(
            out.contents,
            "2026-01-05 a\n    x:y  1 EUR\n\n2026-01-07 b\n    x:y  2 EUR\n\n2026-01-10 c\n    x:y  1 EUR\n"
        );
        assert!(out.warnings.is_empty());
    }

    #[test]
    fn chronological_insertion_before_everything_when_earliest() {
        let src = "2026-01-05 a\n    x:y  1 EUR\n";
        let opts = WriteOptions {
            insertion: Insertion::Chronological,
            ..WriteOptions::default()
        };
        let out = integrate(src, &[txn(d(2026, 1, 1), "first", &[("x:y", "2 EUR")])], &opts);
        assert_eq!(
            out.contents,
            "2026-01-01 first\n    x:y  2 EUR\n\n2026-01-05 a\n    x:y  1 EUR\n"
        );
    }

    #[test]
    fn chronological_insertion_into_an_unsorted_file_warns_and_proceeds() {
        let src = "2026-01-10 late\n    x:y  1 EUR\n\n2026-01-05 early\n    x:y  1 EUR\n";
        let opts = WriteOptions {
            insertion: Insertion::Chronological,
            ..WriteOptions::default()
        };
        let out = integrate(src, &[txn(d(2026, 1, 7), "mid", &[("x:y", "2 EUR")])], &opts);
        assert!(out.warnings.iter().any(|w| w.contains("not sorted")));
    }

    #[test]
    fn sort_mode_places_the_new_transaction_by_date() {
        let src = "2026-01-05 a\n    x:y  1 EUR\n\n2026-01-10 c\n    x:y  1 EUR\n";
        let opts = WriteOptions {
            sort: true,
            ..WriteOptions::default()
        };
        let out = integrate(src, &[txn(d(2026, 1, 7), "b", &[("x:y", "2 EUR")])], &opts);
        assert_eq!(
            out.contents,
            "2026-01-05 a\n    x:y  1 EUR\n\n2026-01-07 b\n    x:y  2 EUR\n\n2026-01-10 c\n    x:y  1 EUR\n"
        );
    }

    #[test]
    fn no_format_mode_leaves_existing_lines_untouched_and_aligns_only_ours() {
        // The existing file is formatted at narrow widths; our wider
        // transaction must not reflow it, but our own lines are aligned at
        // the would-be file-wide widths, and the stale-columns warning fires.
        let src = "2026-01-05 a\n    x:y  1 EUR\n";
        let opts = WriteOptions {
            format_file: false,
            ..WriteOptions::default()
        };
        let out = integrate(
            src,
            &[txn(d(2026, 1, 6), "b", &[("expenses:long", "-12.00 EUR"), ("x:y", "12.00 EUR")])],
            &opts,
        );
        assert_eq!(
            out.contents,
            "2026-01-05 a\n    x:y  1 EUR\n\n2026-01-06 b\n    expenses:long  -12.00 EUR\n    x:y             12.00 EUR\n"
        );
        assert!(out.warnings.iter().any(|w| w.contains("stale")));
    }

    #[test]
    fn no_format_mode_on_a_matching_width_file_is_a_fixed_point() {
        let src = "2026-01-05 a\n    expenses:long  -12.00 EUR\n    x:y             12.00 EUR\n";
        let opts = WriteOptions {
            format_file: false,
            ..WriteOptions::default()
        };
        let out = integrate(
            src,
            &[txn(d(2026, 1, 6), "b", &[("x:y", "1.00 EUR"), ("expenses:long", "-1.00 EUR")])],
            &opts,
        );
        // A later fmt run must be a no-op.
        assert_eq!(crate::fmt::format(&out.contents), out.contents);
        assert!(out.warnings.is_empty());
    }

    #[test]
    fn multiple_transactions_integrate_in_order() {
        let out = integrate(
            "",
            &[
                txn(d(2026, 1, 6), "one", &[("a:b", "1 EUR")]),
                txn(d(2026, 1, 7), "two", &[("a:b", "2 EUR")]),
            ],
            &OPTS,
        );
        assert_eq!(
            out.contents,
            "2026-01-06 one\n    a:b  1 EUR\n\n2026-01-07 two\n    a:b  2 EUR\n"
        );
    }

    #[test]
    fn recovery_round_trips_transactions() {
        let dir = tempfile::TempDir::new().unwrap();
        // Point XDG_STATE_HOME at the tempdir via a directly constructed path.
        let rec = Recovery {
            path: dir.path().join("recovery-test.journal"),
        };
        let t1 = txn(d(2026, 1, 6), "one", &[("a:b", "1 EUR"), ("c:d", "-1 EUR")]);
        let t2 = txn(d(2026, 1, 7), "two", &[("a:b", "2.50 EUR"), ("c:d", "")]);
        rec.record(&t1).unwrap();
        rec.record(&t2).unwrap();
        assert_eq!(rec.pending(), vec![t1, t2]);
        rec.clear();
        assert!(rec.pending().is_empty());
    }

    #[test]
    fn recovery_for_target_derives_a_safe_filename() {
        let rec = Recovery::for_target(Path::new("/home/user/my journal.ledger"));
        let name = rec.path().file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("recovery-"));
        assert!(name.ends_with(".journal"));
        assert!(!name.contains(' '));
        assert!(!name.contains('/'));
    }
}
