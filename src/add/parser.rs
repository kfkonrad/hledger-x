//! Semantic journal parser: include graph walk and directive extraction.
//!
//! Behaviour verified against hledger 1.99 (see `DESIGN.md`):
//!
//! - include paths resolve relative to the *including* file's directory
//! - globs are expanded in sorted order
//! - include cycles (any revisit of a file) are an error
//! - the walk is depth-first, in file order
//!
//! There are **two** directive scope models, empirically distinct:
//!
//! 1. *Parse state* (`decimal-mark`, `D`) — file-scoped, inherited by included
//!    files, discarded when the include returns.
//! 2. *Declarations* (`account`, `commodity`) — journal-wide, never discarded,
//!    but visible only from their flattened-stream position forward.
//!
//! Transactions are parsed lexically, not semantically: the amount side of a
//! posting stays opaque text. A historical number is never interpreted.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use chrono::NaiveDate;

use crate::lex::{
    is_indented_non_blank, opens_txn, rstrip, split_account_amount, split_amount, split_comment,
};

/// Parse state — scope model 1.
///
/// In effect from its line to the end of its file, propagates into included
/// files, does not escape back to the parent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParseState {
    /// `decimal-mark` directive in effect, if any.
    pub decimal_mark: Option<char>,
    /// `D` directive's sample amount (raw text, e.g. `1000.00 EUR`), if any.
    pub default_commodity: Option<String>,
}

/// A declared account — scope model 2.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Declaration {
    /// Account name.
    pub name: String,
    /// Position in the flattened stream; the declaration is visible from here
    /// forward.
    pub pos: usize,
}

/// A declared commodity — scope model 2.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommodityDecl {
    /// Commodity symbol, e.g. `EUR`.
    pub name: String,
    /// The directive's sample amount when it carried one (`1_000.00 EUR`),
    /// raw. Carries the display style.
    pub sample: Option<String>,
    /// Position in the flattened stream.
    pub pos: usize,
}

/// One posting of a historical transaction. The amount side is opaque text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawPosting {
    /// Account name.
    pub account: String,
    /// Everything after the account/amount separator, verbatim (amount, cost,
    /// assertion — but not the inline comment). Empty for a bare posting.
    pub amount: String,
    /// The commodity token extracted lexically from `amount`, when the lexer
    /// finds one. Used for completion pools only, never for arithmetic.
    pub commodity: Option<String>,
}

/// A historical transaction, parsed lexically.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transaction {
    /// Primary date, fully resolved.
    pub date: NaiveDate,
    /// Description as written, status marker and inline comment stripped.
    pub description: String,
    /// Postings in order.
    pub postings: Vec<RawPosting>,
    /// Index into [`Journal::files`].
    pub file: usize,
    /// 1-based line of the header within its file.
    pub line: usize,
    /// Position of the header in the flattened stream.
    pub pos: usize,
}

/// One file of the include tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceFile {
    /// Path as resolved (absolute).
    pub path: PathBuf,
    /// Stream position just past the file's last line. Appending to this file
    /// inserts at this position — the new-account guard filters declarations
    /// against it.
    pub eof_pos: usize,
    /// Parse state in effect at the end of the file. Text appended to the
    /// file is parsed under this state.
    pub state_at_eof: ParseState,
}

/// The semantic product of the parse, over the whole include tree.
#[derive(Debug, Clone, Default)]
pub struct Journal {
    /// Files in depth-first encounter order; index 0 is the main file.
    pub files: Vec<SourceFile>,
    /// `account` declarations with stream positions.
    pub accounts: Vec<Declaration>,
    /// `commodity` declarations with stream positions.
    pub commodities: Vec<CommodityDecl>,
    /// Transactions in stream order.
    pub transactions: Vec<Transaction>,
    /// Non-fatal problems (missing includes, skipped transactions).
    pub warnings: Vec<String>,
}

/// A fatal parse error. Leniency keeps this list short: only I/O on the main
/// file and include cycles are fatal.
#[derive(Debug)]
pub enum ParseError {
    /// A file could not be read.
    Io(PathBuf, std::io::Error),
    /// A file was included twice (which includes genuine cycles).
    Cycle(PathBuf),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(p, e) => write!(f, "{}: {e}", p.display()),
            Self::Cycle(p) => write!(f, "include cycle: {} included twice", p.display()),
        }
    }
}

impl std::error::Error for ParseError {}

impl Journal {
    /// Declared account names visible at `pos` in the flattened stream.
    #[must_use]
    pub fn accounts_visible_at(&self, pos: usize) -> Vec<&str> {
        self.accounts
            .iter()
            .filter(|d| d.pos < pos)
            .map(|d| d.name.as_str())
            .collect()
    }

    /// Whether any `account` directives exist anywhere in the tree (drives the
    /// `new_account` guard default).
    #[must_use]
    pub const fn declares_accounts(&self) -> bool {
        !self.accounts.is_empty()
    }

    /// The file entry for `path`, if it is part of the include tree.
    #[must_use]
    pub fn file(&self, path: &Path) -> Option<&SourceFile> {
        let canon = canonical(path);
        self.files.iter().find(|f| canonical(&f.path) == canon)
    }
}

/// The write-target file map: raw text plus the positions the write path
/// needs. Built for one file only — the file we will modify.
#[derive(Debug, Clone)]
pub struct FileMap {
    /// The file's lines, verbatim.
    pub lines: Vec<String>,
    /// Transactions in the file: 0-based line range `[start, end)` (header
    /// plus posting run) and the header's date, when it parses.
    pub transactions: Vec<TxnSpan>,
    /// Whether the file is already a fixed point of `fmt`.
    pub formatted: bool,
    /// File-wide alignment widths from `fmt`.
    pub acc_w: usize,
    /// See `acc_w`.
    pub num_w: usize,
}

/// One transaction's location within the write-target file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxnSpan {
    /// 0-based first line (the header).
    pub start: usize,
    /// 0-based line just past the posting run.
    pub end: usize,
    /// The header's primary date, if it parses.
    pub date: Option<NaiveDate>,
}

impl FileMap {
    /// Build the map from a file's raw contents.
    #[must_use]
    pub fn build(src: &str) -> Self {
        let lines = crate::fmt::lines(src);
        let (acc_w, num_w) = crate::fmt::widths(&lines);
        let mut transactions = Vec::new();
        let mut i = 0usize;
        while let Some(line) = lines.get(i) {
            if opens_txn(line) {
                let start = i;
                i = i.saturating_add(1);
                while lines.get(i).is_some_and(|l| is_indented_non_blank(l)) {
                    i = i.saturating_add(1);
                }
                let date_tok = line.split_whitespace().next().unwrap_or_default();
                transactions.push(TxnSpan {
                    start,
                    end: i,
                    date: parse_date(date_tok),
                });
            } else {
                i = i.saturating_add(1);
            }
        }
        Self {
            lines: lines.into_iter().map(ToOwned::to_owned).collect(),
            transactions,
            formatted: crate::fmt::is_formatted(src),
            acc_w,
            num_w,
        }
    }

    /// Whether the file's transactions are already in date order (equal dates
    /// count as ordered). Unparseable dates are ignored for the check.
    #[must_use]
    pub fn is_date_sorted(&self) -> bool {
        self.transactions
            .iter()
            .filter_map(|t| t.date)
            .zip(self.transactions.iter().filter_map(|t| t.date).skip(1))
            .all(|(a, b)| a <= b)
    }
}

/// Parse the journal rooted at `main`.
///
/// # Errors
///
/// Only an unreadable *main* file or an include cycle is fatal; everything
/// else degrades to a warning ([`Journal::warnings`]).
pub fn parse_journal(main: &Path) -> Result<Journal, ParseError> {
    let mut walk = Walk {
        journal: Journal::default(),
        visited: HashSet::new(),
        pos: 0,
    };
    let main = absolute(main);
    let src = fs::read_to_string(&main).map_err(|e| ParseError::Io(main.clone(), e))?;
    walk.file(&main, &src, ParseState::default())?;
    Ok(walk.journal)
}

/// Walk state threaded through the include tree.
struct Walk {
    journal: Journal,
    /// Canonicalized paths already entered — any revisit is a cycle error.
    visited: HashSet<PathBuf>,
    /// Flattened stream position, incremented once per line.
    pos: usize,
}

impl Walk {
    /// Process one file. `state` is the parse state inherited from the parent
    /// (a copy — mutations here must not escape back).
    fn file(&mut self, path: &Path, src: &str, mut state: ParseState) -> Result<(), ParseError> {
        let canon = canonical(path);
        if !self.visited.insert(canon) {
            return Err(ParseError::Cycle(path.to_path_buf()));
        }
        let file_idx = self.journal.files.len();
        // Reserve the slot now so `file` indices reflect encounter order; the
        // eof fields are patched when the file is done.
        self.journal.files.push(SourceFile {
            path: path.to_path_buf(),
            eof_pos: 0,
            state_at_eof: ParseState::default(),
        });

        let lines: Vec<&str> = crate::fmt::lines(src);
        let mut i = 0usize;
        while let Some(line) = lines.get(i) {
            let lineno = i.saturating_add(1);
            let line_pos = self.pos;
            self.pos = self.pos.saturating_add(1);
            i = i.saturating_add(1);

            if opens_txn(line) {
                // Consume the posting run along with the header.
                let mut run: Vec<&str> = Vec::new();
                while let Some(next) = lines.get(i) {
                    if !is_indented_non_blank(next) {
                        break;
                    }
                    run.push(next);
                    self.pos = self.pos.saturating_add(1);
                    i = i.saturating_add(1);
                }
                match parse_transaction(line, &run, file_idx, lineno, line_pos) {
                    Ok(txn) => self.journal.transactions.push(txn),
                    Err(why) => self.journal.warnings.push(format!(
                        "{}:{lineno}: skipping unparseable transaction ({why})",
                        path.display()
                    )),
                }
            } else if let Some(pattern) = directive_arg(line, "include") {
                self.include(path, pattern, &state)?;
            } else if let Some(name) = directive_arg(line, "account") {
                self.journal.accounts.push(Declaration {
                    name: name.to_owned(),
                    pos: line_pos,
                });
            } else if let Some(sample) = directive_arg(line, "commodity") {
                self.journal.commodities.push(parse_commodity(sample, line_pos));
            } else if let Some(mark) = directive_arg(line, "decimal-mark") {
                state.decimal_mark = mark.chars().next();
            } else if let Some(sample) = directive_arg(line, "D") {
                state.default_commodity = Some(sample.to_owned());
            }
            // Anything else — blank lines, comments, other directives,
            // indented lines outside a transaction — is ignored: `add` is an
            // entry tool, not a validator.
        }

        if let Some(entry) = self.journal.files.get_mut(file_idx) {
            entry.eof_pos = self.pos;
            entry.state_at_eof = state;
        }
        Ok(())
    }

    /// Expand and walk one `include` directive.
    fn include(&mut self, parent: &Path, pattern: &str, state: &ParseState) -> Result<(), ParseError> {
        let base = parent.parent().map_or_else(PathBuf::new, Path::to_path_buf);
        let targets = expand_include(&base, pattern);
        if targets.is_empty() {
            self.journal.warnings.push(format!(
                "{}: include not found: {pattern} — accounts and history from it are unavailable",
                parent.display()
            ));
            return Ok(());
        }
        for target in targets {
            match fs::read_to_string(&target) {
                Ok(src) => self.file(&target, &src, state.clone())?,
                Err(e) => self.journal.warnings.push(format!(
                    "{}: include not readable: {}: {e} — accounts and history from it are unavailable",
                    parent.display(),
                    target.display()
                )),
            }
        }
        Ok(())
    }
}

/// Expand an include pattern relative to `base`. Globs expand in sorted
/// order; a literal path is returned only if it exists.
fn expand_include(base: &Path, pattern: &str) -> Vec<PathBuf> {
    let joined = base.join(pattern);
    let is_glob = pattern.contains(['*', '?', '[']);
    if is_glob {
        let Some(pat) = joined.to_str() else {
            return Vec::new();
        };
        glob::glob(pat).map_or_else(
            |_| Vec::new(),
            |paths| {
                let mut v: Vec<PathBuf> = paths.filter_map(Result::ok).collect();
                v.sort();
                v
            },
        )
    } else if joined.exists() {
        vec![joined]
    } else {
        Vec::new()
    }
}

/// If `line` is the directive `keyword`, return its argument with any inline
/// comment stripped. `keyword` must be followed by whitespace.
fn directive_arg<'a>(line: &'a str, keyword: &str) -> Option<&'a str> {
    let rest = line.strip_prefix(keyword)?;
    if !rest.chars().next().is_some_and(char::is_whitespace) {
        return None;
    }
    let (body, _comment) = split_comment(rest.trim_start_matches(char::is_whitespace));
    // The argument itself ends at a 2+ space run (which is what separates an
    // inline comment-less annotation in e.g. `account a:b  ; type: Asset`).
    let (arg, _tail) = split_account_amount(body);
    let arg = rstrip(arg);
    if arg.is_empty() {
        None
    } else {
        Some(arg)
    }
}

/// Parse a `commodity` directive argument: either a bare symbol (`USD`) or a
/// sample amount carrying the display style (`1_000.00 EUR`).
fn parse_commodity(sample: &str, pos: usize) -> CommodityDecl {
    let toks: Vec<&str> = sample.split_whitespace().collect();
    let (num, commodity, _rest) = split_amount(&toks);
    if commodity.is_empty() {
        // Either a bare symbol or a left-symbol sample like `$1000.00`.
        CommodityDecl {
            name: num,
            sample: None,
            pos,
        }
    } else {
        CommodityDecl {
            name: commodity,
            sample: Some(sample.to_owned()),
            pos,
        }
    }
}

/// Parse a transaction header plus its indented run, lexically.
fn parse_transaction(
    header: &str,
    run: &[&str],
    file: usize,
    line: usize,
    pos: usize,
) -> Result<Transaction, String> {
    let (body, _comment) = split_comment(header);
    let mut parts = body.splitn(2, char::is_whitespace);
    let date_tok = parts.next().unwrap_or_default();
    let rest = parts.next().unwrap_or_default();
    let date = parse_date(date_tok).ok_or_else(|| format!("bad date {date_tok:?}"))?;
    let description = strip_status(rest).to_owned();

    let mut postings = Vec::new();
    for l in run {
        let s = rstrip(l.trim_start_matches(char::is_whitespace));
        if s.starts_with(';') {
            continue; // in-transaction comment line
        }
        let (pbody, _comment) = split_comment(s);
        let (account, amount) = split_account_amount(pbody);
        if account.is_empty() {
            continue;
        }
        let toks: Vec<&str> = amount.split_whitespace().collect();
        let (_num, commodity, _rest) = split_amount(&toks);
        postings.push(RawPosting {
            account: account.to_owned(),
            amount: amount.to_owned(),
            commodity: if commodity.is_empty() {
                None
            } else {
                Some(commodity)
            },
        });
    }
    Ok(Transaction {
        date,
        description,
        postings,
        file,
        line,
        pos,
    })
}

/// Parse a header's primary date. A secondary date (`=DATE`) is dropped.
/// Only full `Y-M-D` dates (separators `-`, `/`, `.`) are accepted; partial
/// dates need the `Y` directive, which is epic 3.
fn parse_date(token: &str) -> Option<NaiveDate> {
    let primary = token.split('=').next().unwrap_or_default();
    let parts: Vec<&str> = primary.split(['/', '-', '.']).collect();
    let [y, m, d] = parts.as_slice() else {
        return None;
    };
    NaiveDate::from_ymd_opt(y.parse().ok()?, m.parse().ok()?, d.parse().ok()?)
}

/// Drop a leading `*` or `!` status marker from a description.
fn strip_status(desc: &str) -> &str {
    let trimmed = desc.trim_start_matches(char::is_whitespace);
    trimmed
        .strip_prefix(['*', '!'])
        .map_or(trimmed, |rest| rest.trim_start_matches(char::is_whitespace))
}

/// Canonicalize for identity comparisons, falling back to the path itself for
/// files that do not (yet) exist.
fn canonical(p: &Path) -> PathBuf {
    fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// Make absolute against cwd without touching the filesystem.
fn absolute(p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().map_or_else(|_| p.to_path_buf(), |cwd| cwd.join(p))
    }
}

/// Debug-render warnings for tests.
#[must_use]
pub fn warnings_text(j: &Journal) -> String {
    let mut out = String::new();
    for w in &j.warnings {
        let _ = writeln!(out, "{w}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// Build a file tree; returns the tempdir. Paths are relative,
    /// directories created as needed.
    fn tree(files: &[(&str, &str)]) -> TempDir {
        let dir = TempDir::new().unwrap();
        for (path, content) in files {
            let full = dir.path().join(path);
            if let Some(parent) = full.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(full, content).unwrap();
        }
        dir
    }

    fn parse(dir: &TempDir, main: &str) -> Journal {
        parse_journal(&dir.path().join(main)).unwrap()
    }

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    #[test]
    fn a_single_file_parses_transactions() {
        let t = tree(&[(
            "main.journal",
            "2026-01-05 Rewe\n    expenses:groceries    23.45 EUR\n    assets:bank  -23.45 EUR\n",
        )]);
        let j = parse(&t, "main.journal");
        assert_eq!(j.transactions.len(), 1);
        let txn = &j.transactions[0];
        assert_eq!(txn.date, d(2026, 1, 5));
        assert_eq!(txn.description, "Rewe");
        assert_eq!(txn.postings.len(), 2);
        assert_eq!(txn.postings[0].account, "expenses:groceries");
        assert_eq!(txn.postings[0].amount, "23.45 EUR");
        assert_eq!(txn.postings[0].commodity.as_deref(), Some("EUR"));
        assert_eq!(txn.line, 1);
    }

    #[test]
    fn amounts_are_carried_as_raw_text() {
        // Underscore grouping and costs must survive verbatim — they are
        // never interpreted.
        let t = tree(&[(
            "main.journal",
            "2026-01-05 x\n    a:b    50_888.56 EUR @ 1.2 USD  ; note\n    c:d\n",
        )]);
        let j = parse(&t, "main.journal");
        let p = &j.transactions[0].postings[0];
        assert_eq!(p.amount, "50_888.56 EUR @ 1.2 USD");
        assert_eq!(p.commodity.as_deref(), Some("EUR"));
        // Bare posting: empty amount, no commodity.
        let bare = &j.transactions[0].postings[1];
        assert_eq!(bare.amount, "");
        assert_eq!(bare.commodity, None);
    }

    #[test]
    fn status_markers_and_comments_are_stripped_from_descriptions() {
        let t = tree(&[(
            "main.journal",
            "2026-01-05 * Rewe  ; tag: x\n    a  1 EUR\n\n2026-01-06 ! Edeka\n    a  1 EUR\n",
        )]);
        let j = parse(&t, "main.journal");
        assert_eq!(j.transactions[0].description, "Rewe");
        assert_eq!(j.transactions[1].description, "Edeka");
    }

    #[test]
    fn unparseable_dates_skip_the_transaction_with_a_warning() {
        let t = tree(&[(
            "main.journal",
            "2026-13-40 nonsense\n    a  1 EUR\n\n2026-01-05 good\n    a  1 EUR\n",
        )]);
        let j = parse(&t, "main.journal");
        assert_eq!(j.transactions.len(), 1);
        assert_eq!(j.transactions[0].description, "good");
        assert_eq!(j.warnings.len(), 1);
        assert!(j.warnings[0].contains("skipping"), "{}", j.warnings[0]);
    }

    #[test]
    fn periodic_and_auto_transactions_are_not_transactions() {
        let t = tree(&[(
            "main.journal",
            "~ monthly\n    a  1 EUR\n\n= expenses:food\n    b  1 EUR\n",
        )]);
        let j = parse(&t, "main.journal");
        assert!(j.transactions.is_empty());
        assert!(j.warnings.is_empty());
    }

    #[test]
    fn includes_resolve_relative_to_the_including_file() {
        let t = tree(&[
            ("main.journal", "include g/sub/mid.journal\n"),
            ("g/sub/mid.journal", "include deep/d.journal\n"),
            (
                "g/sub/deep/d.journal",
                "2026-01-05 deep\n    a  1 EUR\n",
            ),
        ]);
        let j = parse(&t, "main.journal");
        assert_eq!(j.transactions.len(), 1);
        assert_eq!(j.transactions[0].description, "deep");
        assert_eq!(j.files.len(), 3);
    }

    #[test]
    fn include_globs_expand_in_sorted_order() {
        let t = tree(&[
            ("main.journal", "include sub/g*.journal\n"),
            ("sub/g2.journal", "2026-01-02 two\n"),
            ("sub/g1.journal", "2026-01-01 one\n"),
        ]);
        let j = parse(&t, "main.journal");
        let descs: Vec<&str> = j
            .transactions
            .iter()
            .map(|t| t.description.as_str())
            .collect();
        assert_eq!(descs, vec!["one", "two"]);
    }

    #[test]
    fn a_missing_include_warns_loudly_and_continues() {
        let t = tree(&[(
            "main.journal",
            "include nope.journal\n2026-01-05 x\n    a  1 EUR\n",
        )]);
        let j = parse(&t, "main.journal");
        assert_eq!(j.transactions.len(), 1);
        assert_eq!(j.warnings.len(), 1);
        assert!(j.warnings[0].contains("nope.journal"));
    }

    #[test]
    fn an_include_cycle_is_a_fatal_error() {
        let t = tree(&[
            ("a.journal", "include b.journal\n"),
            ("b.journal", "include a.journal\n"),
        ]);
        let err = parse_journal(&t.path().join("a.journal")).unwrap_err();
        assert!(matches!(err, ParseError::Cycle(_)));
    }

    #[test]
    fn including_the_same_file_twice_is_also_a_cycle_error() {
        let t = tree(&[
            ("main.journal", "include x.journal\ninclude x.journal\n"),
            ("x.journal", "2026-01-01 x\n"),
        ]);
        let err = parse_journal(&t.path().join("main.journal")).unwrap_err();
        assert!(matches!(err, ParseError::Cycle(_)));
    }

    #[test]
    fn parse_state_propagates_into_includes_but_not_back_out() {
        // decimal-mark in an included file must NOT be in effect in the
        // parent afterwards (verified hledger behaviour, DESIGN round 1
        // test A); a parent's mark must be visible inside the include
        // (test C).
        let t = tree(&[
            (
                "main.journal",
                "decimal-mark ,\ninclude sub.journal\n",
            ),
            ("sub.journal", "include leaf.journal\n"),
            ("leaf.journal", ""),
            (
                "main2.journal",
                "include marked.journal\n",
            ),
            ("marked.journal", "decimal-mark ,\n"),
        ]);
        let j = parse(&t, "main.journal");
        // Propagated down two levels.
        let leaf = j.file(&t.path().join("leaf.journal")).unwrap();
        assert_eq!(leaf.state_at_eof.decimal_mark, Some(','));

        let j2 = parse(&t, "main2.journal");
        // Not escaped back to the parent.
        let main2 = j2.file(&t.path().join("main2.journal")).unwrap();
        assert_eq!(main2.state_at_eof.decimal_mark, None);
        let marked = j2.file(&t.path().join("marked.journal")).unwrap();
        assert_eq!(marked.state_at_eof.decimal_mark, Some(','));
    }

    #[test]
    fn default_commodity_is_parse_state() {
        let t = tree(&[("main.journal", "D 1000.00 EUR\n")]);
        let j = parse(&t, "main.journal");
        assert_eq!(
            j.files[0].state_at_eof.default_commodity.as_deref(),
            Some("1000.00 EUR")
        );
    }

    #[test]
    fn declarations_escape_includes_and_carry_stream_positions() {
        // DESIGN round 2 test A: account declared in an include is visible in
        // the parent's later content.
        let t = tree(&[
            (
                "main.journal",
                "include conf.journal\n2026-01-05 x\n    assets:bank  1 EUR\n",
            ),
            (
                "conf.journal",
                "account assets:bank\ncommodity 1_000.00 EUR\n",
            ),
        ]);
        let j = parse(&t, "main.journal");
        assert_eq!(j.accounts.len(), 1);
        assert_eq!(j.accounts[0].name, "assets:bank");
        assert_eq!(j.commodities.len(), 1);
        assert_eq!(j.commodities[0].name, "EUR");
        assert_eq!(j.commodities[0].sample.as_deref(), Some("1_000.00 EUR"));

        // The declaration (inside the include) precedes the main file's
        // eof position, so it is visible at an append to main.
        let main = j.file(&t.path().join("main.journal")).unwrap();
        assert!(j.accounts[0].pos < main.eof_pos);
        assert_eq!(j.accounts_visible_at(main.eof_pos), vec!["assets:bank"]);
    }

    #[test]
    fn declarations_after_the_insertion_point_are_not_visible() {
        // An include *below* the insertion point carries declarations that
        // hledger will not accept for text appended above it. eof of the
        // included file > eof… wait — appending to the *include* target.
        // Append target here is sub.journal; the account declared later in
        // main must not count as visible there.
        let t = tree(&[
            (
                "main.journal",
                "include sub.journal\naccount declared:late\n",
            ),
            ("sub.journal", "2026-01-01 x\n"),
        ]);
        let j = parse(&t, "main.journal");
        let sub = j.file(&t.path().join("sub.journal")).unwrap();
        assert!(j.accounts_visible_at(sub.eof_pos).is_empty());
        let main = j.file(&t.path().join("main.journal")).unwrap();
        assert_eq!(j.accounts_visible_at(main.eof_pos), vec!["declared:late"]);
    }

    #[test]
    fn account_directive_inline_comments_are_not_part_of_the_name() {
        let t = tree(&[(
            "main.journal",
            "account assets:dkb:giro  ; type: Asset\naccount equity:opening-closing-balances\n",
        )]);
        let j = parse(&t, "main.journal");
        let names: Vec<&str> = j.accounts.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["assets:dkb:giro", "equity:opening-closing-balances"]
        );
    }

    #[test]
    fn bare_commodity_directives_declare_without_a_style() {
        let t = tree(&[("main.journal", "commodity USD\n")]);
        let j = parse(&t, "main.journal");
        assert_eq!(j.commodities[0].name, "USD");
        assert_eq!(j.commodities[0].sample, None);
    }

    #[test]
    fn unknown_directives_are_ignored_silently() {
        let t = tree(&[(
            "main.journal",
            "payee Rewe\ntag project\nP 2026-01-01 USD 0.9 EUR\napply account assets\nend apply account\n",
        )]);
        let j = parse(&t, "main.journal");
        assert!(j.warnings.is_empty());
        assert!(j.transactions.is_empty());
    }

    #[test]
    fn account_and_commodity_subdirective_blocks_are_not_postings() {
        let t = tree(&[(
            "main.journal",
            "account assets:bank\n    note Checking\ncommodity EUR\n    format 1.000,00 EUR\n",
        )]);
        let j = parse(&t, "main.journal");
        assert!(j.transactions.is_empty());
        assert_eq!(j.accounts.len(), 1);
    }

    #[test]
    fn file_map_records_transaction_spans_and_widths() {
        let src = "include conf.journal\n\n2026-01-05 a\n    aa:bb   1.00 EUR\n    cc:dd  -1.00 EUR\n\n2026-01-07 b\n    aa:bb   2.00 EUR\n";
        let map = FileMap::build(src);
        assert_eq!(map.transactions.len(), 2);
        assert_eq!(map.transactions[0].start, 2);
        assert_eq!(map.transactions[0].end, 5);
        assert_eq!(map.transactions[0].date, Some(d(2026, 1, 5)));
        assert_eq!(map.transactions[1].start, 6);
        assert_eq!(map.transactions[1].end, 8);
        assert!(map.formatted);
        assert_eq!(map.acc_w, 5);
        assert_eq!(map.num_w, 5);
        assert!(map.is_date_sorted());
    }

    #[test]
    fn file_map_detects_unsorted_and_unformatted_files() {
        let unsorted = "2026-01-07 b\n    a  1 EUR\n\n2026-01-05 a\n    a  1 EUR\n";
        assert!(!FileMap::build(unsorted).is_date_sorted());
        let unformatted = "2026-01-05 a\n  a  1 EUR\n";
        assert!(!FileMap::build(unformatted).formatted);
    }

    #[test]
    fn directive_keyword_must_be_followed_by_whitespace() {
        // `accountant` is not an `account` directive.
        let t = tree(&[("main.journal", "accountant fees\nDecimal x\n")]);
        let j = parse(&t, "main.journal");
        assert!(j.accounts.is_empty());
    }
}
