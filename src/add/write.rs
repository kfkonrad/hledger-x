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

use super::parser::{FileMap, SourceFile};
use super::scope::Scope;
use crate::fmt::posting::{parse_posting, render};
use crate::fmt::{format_opts_at, merged_styles, unlines, widths_with_at, Options};
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

/// One posting of a transaction being written.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Posting {
    /// Account name, **resolved** — spelled for the insertion point only at
    /// the moment of writing (see [`NewTransaction::raw_lines_in`]).
    pub account: String,
    /// Raw amount text; may be empty for a bare posting.
    pub amount: String,
    /// Comment text *without* the leading `;`, as typed. Empty for none.
    pub comment: String,
}

impl Posting {
    /// A posting with no comment.
    #[must_use]
    pub fn new(account: &str, amount: &str) -> Self {
        Self {
            account: account.to_owned(),
            amount: amount.to_owned(),
            comment: String::new(),
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
    /// Transaction comment text without the leading `;`. Empty for none.
    pub comment: String,
    /// Postings in order.
    pub postings: Vec<Posting>,
}

/// The text of a `; comment` with the marker and surrounding space removed —
/// the form a field's buffer holds after the `;` and the user edits.
#[must_use]
pub fn comment_text(comment: Option<&str>) -> String {
    comment.map_or_else(String::new, |c| {
        c.trim_start().trim_start_matches(';').trim().to_owned()
    })
}

/// Render a comment as the trailing `  ; text` of a line, or nothing.
#[must_use]
pub fn comment_suffix(comment: &str) -> String {
    let c = comment.trim();
    if c.is_empty() {
        String::new()
    } else if c.starts_with(';') {
        format!("  {c}")
    } else {
        format!("  ; {c}")
    }
}

/// The outcome of integrating new transactions into a file.
#[derive(Debug, Clone)]
pub struct Integrated {
    /// The file's new contents.
    pub contents: String,
    /// Non-fatal notes for the user.
    pub warnings: Vec<String>,
}

/// Whether the new transactions could be expressed at their insertion point.
#[derive(Debug, Clone)]
pub enum Integration {
    /// Ready to write.
    Ready(Integrated),
    /// At least one account cannot be written where it would go — an
    /// `apply account` or `alias` in effect there rewrites every spelling of
    /// it into something else. Refusing is the only correct answer: writing
    /// the name as-is would silently enter a different account.
    Refused(Vec<String>),
}

/// The `apply account` / `alias` scope of the write-target file, by line.
///
/// `insertion = append` lands at end of file, where an *unclosed* region may
/// still be open; `insertion = chronological` can land mid-file, inside a
/// region that is closed further down. Both are reachable, so the write path
/// needs the scope at an arbitrary line, not just at eof.
#[derive(Debug, Clone, Default)]
pub struct TargetScopes {
    /// `(0-based line, scope in effect from there)`, ascending.
    checkpoints: Vec<(usize, Scope)>,
    /// The scope at end of file.
    at_eof: Scope,
}

impl TargetScopes {
    /// The scopes of a parsed source file.
    #[must_use]
    pub fn of(file: &SourceFile) -> Self {
        Self {
            checkpoints: file
                .states
                .iter()
                .map(|(at, s)| (*at, s.scope.clone()))
                .collect(),
            at_eof: file.state_at_eof.scope.clone(),
        }
    }

    /// The scope in effect just before original 0-based `line`; `None` means
    /// end of file.
    fn at(&self, line: Option<usize>) -> &Scope {
        let Some(line) = line else {
            return &self.at_eof;
        };
        self.checkpoints
            .iter()
            .rev()
            .find(|(at, _)| *at <= line)
            .map_or(&self.at_eof, |(_, s)| s)
    }

    /// Whether anything anywhere in this file rewrites account names — the
    /// cheap check for warning the user at session start.
    #[must_use]
    pub fn any_active(&self) -> bool {
        self.at_eof.is_active() || self.checkpoints.iter().any(|(_, s)| s.is_active())
    }
}

impl NewTransaction {
    /// The transaction in raw journal syntax (header + minimally separated
    /// postings), before any alignment.
    ///
    /// The date is always written in full `YYYY-MM-DD` form, even into a file
    /// where a `Y` directive would let the year be omitted: explicit over
    /// implicit, and a full date cannot be silently rebound by someone later
    /// editing the `Y` line above it.
    #[must_use]
    pub fn raw_lines(&self) -> Vec<String> {
        self.raw_lines_in(&Scope::default())
            .unwrap_or_else(|_| Vec::new())
    }

    /// [`Self::raw_lines`] with account names spelled for `scope` — the text
    /// that reads back as the account the user chose.
    ///
    /// # Errors
    ///
    /// The names that cannot be expressed under `scope` at all.
    pub fn raw_lines_in(&self, scope: &Scope) -> Result<Vec<String>, Vec<String>> {
        let header = format!("{} {}", self.date.format("%Y-%m-%d"), self.description)
            .trim_end()
            .to_owned();
        let mut out = vec![format!("{header}{}", comment_suffix(&self.comment))];
        let mut refused: Vec<String> = Vec::new();
        for p in &self.postings {
            let Some(spelled) = scope.spell(&p.account) else {
                if !refused.contains(&p.account) {
                    refused.push(p.account.clone());
                }
                continue;
            };
            let body = if p.amount.trim().is_empty() {
                format!("    {spelled}")
            } else {
                format!("    {spelled}  {}", p.amount.trim())
            };
            out.push(format!("{body}{}", comment_suffix(&p.comment)));
        }
        if refused.is_empty() {
            Ok(out)
        } else {
            Err(refused)
        }
    }

    /// The widths this transaction's postings need.
    fn own_widths(&self) -> (usize, usize) {
        let acc_w = self
            .postings
            .iter()
            .map(|p| p.account.chars().count())
            .max()
            .unwrap_or(0);
        let num_w = self
            .postings
            .iter()
            .map(|p| {
                let toks: Vec<&str> = p.amount.split_whitespace().collect();
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
    integrate_with(src, txns, opts, &[])
}

/// [`integrate`] with the styles the target file inherits — from the include
/// tree ahead of it, and from its own `include` lines, each at the line it
/// arrives on (see `fmt::format_opts_at`).
#[must_use]
pub fn integrate_with(
    src: &str,
    txns: &[NewTransaction],
    opts: &WriteOptions,
    ctx: &[(usize, crate::amount::AmountCtx)],
) -> Integrated {
    match integrate_in(src, txns, opts, ctx, &TargetScopes::default()) {
        Integration::Ready(out) => out,
        // Unreachable with an empty scope — it spells every name as itself —
        // but expressed as data rather than a panic.
        Integration::Refused(reasons) => Integrated {
            contents: src.to_owned(),
            warnings: reasons,
        },
    }
}

/// [`integrate_with`] against the target file's `apply account` / `alias`
/// scopes, which decide how each account name is spelled where it lands.
#[must_use]
pub fn integrate_in(
    src: &str,
    txns: &[NewTransaction],
    opts: &WriteOptions,
    ctx: &[(usize, crate::amount::AmountCtx)],
    scopes: &TargetScopes,
) -> Integration {
    let mut warnings = Vec::new();
    let map = FileMap::build_with(src, ctx);

    if opts.format_file && !map.formatted && !src.is_empty() {
        warnings.push("file was not formatted; it will be reformatted on write".to_owned());
    }
    if opts.insertion == Insertion::Chronological && !opts.sort && !map.is_date_sorted() {
        warnings.push("file is not sorted by date; inserting chronologically anyway".to_owned());
    }

    // Pass 1: place every transaction and spell its accounts for wherever it
    // landed. Nothing is written until all of them succeed.
    let placed = match place(&map, txns, *opts, scopes) {
        Ok(placed) => placed,
        Err(reasons) => return Integration::Refused(reasons),
    };
    for note in placed.iter().filter_map(|p| p.warning.clone()) {
        if !warnings.contains(&note) {
            warnings.push(note);
        }
    }

    let mut lines: Vec<String> = map.lines.clone();
    for p in &placed {
        splice(&mut lines, p.at, &p.raw);
    }

    if opts.format_file {
        let joined = unlines(&lines);
        let contents = if opts.sort {
            format_opts_at(&joined, ctx, Options::sorted(true))
        } else {
            format_opts_at(&joined, ctx, Options::default())
        };
        return Integration::Ready(Integrated { contents, warnings });
    }

    // format_file = false: render only our own lines, at widths computed over
    // the whole file including the new transactions.
    let joined = unlines(&lines);
    let all: Vec<&str> = crate::fmt::lines(&joined);
    let (mut acc_w, mut num_w) = widths_with_at(&all, ctx);
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

    // Re-do the insertion, this time rendering our lines aligned. The
    // recorded positions stay valid: the same splices happen in the same
    // order with blocks of the same length, so the indices repeat exactly.
    // Our lines go at the end of the file, so every inherited style is in
    // effect.
    let at_eof = merged_styles(ctx);
    let mut lines: Vec<String> = map.lines;
    for p in &placed {
        let mut rendered = vec![p.raw.first().cloned().unwrap_or_default()];
        for raw in p.raw.iter().skip(1) {
            rendered.push(render(
                acc_w,
                num_w,
                &crate::fmt::posting::restyle(parse_posting(raw), &at_eof),
            ));
        }
        splice(&mut lines, p.at, &rendered);
    }
    Integration::Ready(Integrated {
        contents: unlines(&lines),
        warnings,
    })
}

/// One transaction placed: where it goes and the lines to put there, already
/// spelled for the scope in effect at that point.
struct Placed {
    /// Insertion index in the buffer *as it stands when this transaction is
    /// spliced* — positions are recorded in the order they are applied, not
    /// in original-file coordinates.
    at: usize,
    raw: Vec<String>,
    warning: Option<String>,
}

/// Decide where each transaction goes and how its accounts are spelled there.
///
/// The placement walk is incremental — each transaction is positioned against
/// the file as the previous ones left it, which is what makes chronological
/// insertion of out-of-order transactions come out in date order. Alongside
/// it runs a map from buffer line to *original* line, because the scope is
/// only known for the original file; inserted transactions carry no
/// directives, so they never change it.
fn place(
    map: &FileMap,
    txns: &[NewTransaction],
    opts: WriteOptions,
    scopes: &TargetScopes,
) -> Result<Vec<Placed>, Vec<String>> {
    let mut out = Vec::new();
    let mut refusals: Vec<String> = Vec::new();
    let mut lines: Vec<String> = map.lines.clone();
    let mut origins: Vec<Option<usize>> = (0..lines.len()).map(Some).collect();

    for txn in txns {
        let at = insertion_index(&FileMap::build(&unlines(&lines)), txn.date, opts.insertion);
        // The scope just before the next surviving original line; past the
        // last of them, that is end of file.
        let next_original = origins
            .get(at..)
            .and_then(|rest| rest.iter().flatten().next());
        let scope = scopes.at(next_original.copied());
        let warning = scope.active_directives().first().map(|(directive, _)| {
            format!(
                "`{directive}` is in effect at the insertion point; account names are written as it requires"
            )
        });
        match txn.raw_lines_in(scope) {
            Ok(raw) => {
                let before = lines.len();
                splice(&mut lines, at, &raw);
                // Mirror the splice in `origins`: everything inserted is new.
                let added = lines.len().saturating_sub(before);
                let tail: Vec<Option<usize>> = origins.split_off(at.min(origins.len()));
                origins.extend(std::iter::repeat_n(None, added));
                origins.extend(tail);
                out.push(Placed { at, raw, warning });
            }
            Err(names) => {
                let directives: Vec<String> = scope
                    .active_directives()
                    .into_iter()
                    .map(|(d, o)| format!("`{d}` (line {})", o.line))
                    .collect();
                for name in names {
                    let reason = format!(
                        "cannot write `{name}` here: {} rewrites every spelling of it",
                        directives.join(" and ")
                    );
                    if !refusals.contains(&reason) {
                        refusals.push(reason);
                    }
                }
            }
        }
    }
    if refusals.is_empty() {
        Ok(out)
    } else {
        Err(refusals)
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
                map.transactions
                    .first()
                    .map_or(map.lines.len(), |t| t.start)
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
        let (header_body, header_comment) = crate::lex::split_comment(header);
        let description = header_body
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
                crate::fmt::posting::Posting::Bare(account, comment) => {
                    postings.push(Posting {
                        account,
                        amount: String::new(),
                        comment: comment_text(comment.as_deref()),
                    });
                }
                crate::fmt::posting::Posting::Amount {
                    account,
                    num,
                    commodity,
                    rest,
                    comment,
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
                    postings.push(Posting {
                        account,
                        amount,
                        comment: comment_text(comment.as_deref()),
                    });
                }
                crate::fmt::posting::Posting::Comment(_) => {}
            }
        }
        out.push(NewTransaction {
            date,
            description,
            comment: comment_text(header_comment),
            postings,
        });
    }
    out
}

/// `$XDG_STATE_HOME/hledger-x`, defaulting to `~/.local/state/hledger-x`.
fn state_dir() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map_or_else(
            || {
                std::env::var_os("HOME")
                    .map_or_else(|| PathBuf::from("."), PathBuf::from)
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
            comment: String::new(),
            postings: postings.iter().map(|(a, b)| Posting::new(a, b)).collect(),
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
        let new = txn(
            d(2026, 1, 6),
            "b",
            &[("aa:bb", "2.00 EUR"), ("cc:dd", "-2.00 EUR")],
        );
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
        let new = txn(
            d(2026, 1, 6),
            "b",
            &[("expenses:very:long", "-7485978.18 EUR")],
        );
        let out = integrate(src, &[new], &OPTS);
        assert_eq!(
            out.contents,
            "2026-01-05 a\n    aa:bb                      1.00 EUR\n\n2026-01-06 b\n    expenses:very:long  -7485978.18 EUR\n"
        );
    }

    #[test]
    fn an_unformatted_file_warns_before_being_reformatted() {
        let src = "2026-01-05 a\n  aa:bb  1.00 EUR\n";
        let out = integrate(
            src,
            &[txn(d(2026, 1, 6), "b", &[("aa:bb", "1 EUR")])],
            &OPTS,
        );
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
            &[txn(
                d(2026, 1, 6),
                "b",
                &[("a:b", "1 EUR"), ("c:d", "-1 EUR"), ("e:f", "")],
            )],
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
        let out = integrate(
            src,
            &[txn(d(2026, 1, 1), "first", &[("x:y", "2 EUR")])],
            &opts,
        );
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
        let out = integrate(
            src,
            &[txn(d(2026, 1, 7), "mid", &[("x:y", "2 EUR")])],
            &opts,
        );
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
            &[txn(
                d(2026, 1, 6),
                "b",
                &[("expenses:long", "-12.00 EUR"), ("x:y", "12.00 EUR")],
            )],
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
            &[txn(
                d(2026, 1, 6),
                "b",
                &[("x:y", "1.00 EUR"), ("expenses:long", "-1.00 EUR")],
            )],
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

    // ---- epic 3: writing under `apply account` / `alias` ----

    /// A `TargetScopes` whose whole file carries `scope`.
    fn everywhere(scope: Scope) -> TargetScopes {
        TargetScopes {
            checkpoints: vec![(0, scope.clone())],
            at_eof: scope,
        }
    }

    fn apply_scope(prefix: &str) -> Scope {
        let mut s = Scope::default();
        s.push_apply(prefix, crate::add::scope::Origin { file: 0, line: 1 });
        s
    }

    fn ready(i: Integration) -> Integrated {
        match i {
            Integration::Ready(out) => out,
            Integration::Refused(reasons) => {
                panic!("unexpectedly refused: {}", reasons.join("; "))
            }
        }
    }

    #[test]
    fn chronological_insertion_of_out_of_order_transactions_still_sorts() {
        // The later-entered transaction is the earlier-dated one; it must end
        // up first. Placing every transaction against the *original* file and
        // shifting by a running total gets this wrong.
        let src = "2026-01-01 a\n    x:y  1 EUR\n\n2026-01-20 z\n    x:y  1 EUR\n";
        let opts = WriteOptions {
            insertion: Insertion::Chronological,
            ..WriteOptions::default()
        };
        let out = integrate(
            src,
            &[
                txn(d(2026, 1, 15), "later-entered", &[("x:y", "2 EUR")]),
                txn(d(2026, 1, 5), "earlier-dated", &[("x:y", "3 EUR")]),
            ],
            &opts,
        );
        let order: Vec<&str> = out
            .contents
            .lines()
            .filter(|l| !l.starts_with(char::is_whitespace) && !l.is_empty())
            .collect();
        assert_eq!(
            order,
            vec![
                "2026-01-01 a",
                "2026-01-05 earlier-dated",
                "2026-01-15 later-entered",
                "2026-01-20 z",
            ]
        );
    }

    #[test]
    fn an_account_written_into_an_apply_region_loses_the_prefix() {
        // The whole point: hledger reads `checking` here as
        // `assets:bank:checking`, so writing the resolved name would produce
        // `assets:bank:assets:bank:checking`.
        let src = "apply account assets:bank\n\n2026-01-05 a\n    checking  1 EUR\n";
        let out = ready(integrate_in(
            src,
            &[txn(
                d(2026, 1, 6),
                "b",
                &[
                    ("assets:bank:checking", "2 EUR"),
                    ("assets:bank:savings", "-2 EUR"),
                ],
            )],
            &WriteOptions::default(),
            &[],
            &everywhere(apply_scope("assets:bank")),
        ));
        assert!(
            out.contents
                .contains("    checking   2 EUR\n    savings   -2 EUR"),
            "{}",
            out.contents
        );
        assert!(
            !out.contents.contains("assets:bank:checking  "),
            "{}",
            out.contents
        );
        assert!(
            out.warnings
                .iter()
                .any(|w| w.contains("apply account assets:bank")),
            "{:?}",
            out.warnings
        );
    }

    #[test]
    fn an_unwritable_account_refuses_the_whole_write() {
        // `expenses:groceries` is outside the applied subtree, so no text at
        // this insertion point reads back as it.
        let result = integrate_in(
            "apply account assets:bank\n",
            &[txn(
                d(2026, 1, 6),
                "b",
                &[
                    ("expenses:groceries", "2 EUR"),
                    ("assets:bank:checking", "-2 EUR"),
                ],
            )],
            &WriteOptions::default(),
            &[],
            &everywhere(apply_scope("assets:bank")),
        );
        match result {
            Integration::Refused(reasons) => {
                assert_eq!(reasons.len(), 1);
                assert!(reasons[0].contains("expenses:groceries"), "{}", reasons[0]);
                assert!(
                    reasons[0].contains("apply account assets:bank"),
                    "{}",
                    reasons[0]
                );
                assert!(reasons[0].contains("line 1"), "{}", reasons[0]);
            }
            Integration::Ready(out) => panic!("should have refused, wrote:\n{}", out.contents),
        }
    }

    #[test]
    fn a_chronological_insertion_uses_the_scope_where_it_lands_not_at_eof() {
        // The region is *closed* before end of file, so an append would be
        // unaffected — but a chronological insertion lands inside it.
        let src = "2026-01-01 a\n    q  1 EUR\n\napply account P\n\n2026-01-05 b\n    q  1 EUR\n\nend apply account\n\n2026-01-10 c\n    q  1 EUR\n";
        let scopes = TargetScopes {
            checkpoints: vec![
                (0, Scope::default()),
                (4, apply_scope("P")),
                (9, Scope::default()),
            ],
            ..TargetScopes::default()
        };
        let out = ready(integrate_in(
            src,
            &[txn(
                d(2026, 1, 7),
                "mid",
                &[("P:q", "2 EUR"), ("P:r", "-2 EUR")],
            )],
            &WriteOptions {
                insertion: Insertion::Chronological,
                ..WriteOptions::default()
            },
            &[],
            &scopes,
        ));
        // Written unprefixed, because it lands inside the region.
        assert!(
            out.contents
                .contains("2026-01-07 mid\n    q   2 EUR\n    r  -2 EUR"),
            "{}",
            out.contents
        );
    }

    #[test]
    fn appending_past_a_closed_region_is_unaffected() {
        let src = "apply account P\n\n2026-01-05 b\n    q  1 EUR\n\nend apply account\n";
        let scopes = TargetScopes {
            checkpoints: vec![
                (0, Scope::default()),
                (1, apply_scope("P")),
                (5, Scope::default()),
            ],
            ..TargetScopes::default()
        };
        let out = ready(integrate_in(
            src,
            &[txn(
                d(2026, 1, 6),
                "after",
                &[("full:name", "2 EUR"), ("other", "-2 EUR")],
            )],
            &WriteOptions::default(),
            &[],
            &scopes,
        ));
        assert!(
            out.contents.contains("    full:name   2 EUR"),
            "{}",
            out.contents
        );
        assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    }

    #[test]
    fn dates_are_always_written_in_full_even_under_a_year_directive() {
        let out = integrate(
            "Y 2026\n\n01-05 a\n    x:y  1 EUR\n",
            &[txn(d(2026, 1, 6), "b", &[("x:y", "1 EUR")])],
            &OPTS,
        );
        assert!(out.contents.contains("2026-01-06 b"), "{}", out.contents);
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
        let name = rec
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(name.starts_with("recovery-"));
        assert!(name.ends_with(".journal"));
        assert!(!name.contains(' '));
        assert!(!name.contains('/'));
    }
}
