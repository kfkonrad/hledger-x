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
//! 1. *Parse state* (`decimal-mark`, `D`, `Y`, `apply account`, `alias`) —
//!    file-scoped, inherited by included files, discarded when the include
//!    returns.
//! 2. *Declarations* (`account`, `commodity`, `payee`, `tag`) — journal-wide,
//!    never discarded, but visible only from their flattened-stream position
//!    forward.
//!
//! Transactions are parsed lexically, not semantically: the amount side of a
//! posting stays opaque text. A historical number is never interpreted.
//!
//! Account names, by contrast, *are* interpreted — they have to be. Under an
//! `apply account` block or an `alias`, the text in the file is not the
//! account hledger sees, and an index full of half-names would poison
//! completion silently. See [`crate::add::scope`].

use std::collections::HashSet;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use chrono::{Datelike as _, NaiveDate};

use crate::add::scope::{Alias, Origin, Scope};
use crate::lex::{
    closes_comment_block, directive_arg, directive_rest, is_directive, is_indented_non_blank,
    opens_comment_block, opens_txn, rstrip, split_account_amount, split_amount, split_comment,
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
    /// `Y`/`year` directive in effect. `None` means partial dates fall back to
    /// the current year, which is what hledger does (verified — the first
    /// control for this used the current year and so proved nothing).
    pub year: Option<i32>,
    /// `apply account` / `alias` rewriting in effect.
    pub scope: Scope,
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
    /// Account name, **resolved** — `apply account` and `alias` applied, so
    /// this is the name hledger sees, not necessarily the text in the file.
    pub account: String,
    /// Everything after the account/amount separator, verbatim (amount, cost,
    /// assertion — but not the inline comment). Empty for a bare posting.
    pub amount: String,
    /// The commodity token extracted lexically from `amount`, when the lexer
    /// finds one. Used for completion pools only, never for arithmetic.
    pub commodity: Option<String>,
    /// The posting's inline comment including the leading `;`, if any. Feeds
    /// the tag pool and comment pre-fill.
    pub comment: Option<String>,
}

/// A historical transaction, parsed lexically.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transaction {
    /// Primary date, fully resolved.
    pub date: NaiveDate,
    /// Description as written, status marker and inline comment stripped.
    pub description: String,
    /// The header's inline comment including the leading `;`, if any.
    pub comment: Option<String>,
    /// Postings in order.
    pub postings: Vec<RawPosting>,
    /// Index into [`Journal::files`].
    pub file: usize,
    /// 1-based line of the header within its file.
    pub line: usize,
    /// Position of the header in the flattened stream.
    pub pos: usize,
}

/// An `include` line and the span of the stream its target occupies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IncludeSpan {
    /// 0-based index of the `include` line within its own file.
    pub line: usize,
    /// Stream positions the included subtree occupies: `[start, end)`.
    pub start_pos: usize,
    /// One past the subtree's last position.
    pub end_pos: usize,
}

/// One file of the include tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceFile {
    /// Path as resolved (absolute).
    pub path: PathBuf,
    /// Stream position of the file's first line. Declarations before it are
    /// the ones hledger has already read when it starts parsing this file —
    /// which is what decides how the file's own amounts are read.
    pub start_pos: usize,
    /// Stream position just past the file's last line. Appending to this file
    /// inserts at this position — the new-account guard filters declarations
    /// against it.
    pub eof_pos: usize,
    /// Parse state in effect at the end of the file. Text appended to the
    /// file is parsed under this state.
    pub state_at_eof: ParseState,
    /// The file's own `include` lines, in order, with the span of the stream
    /// each one pulls in. What those files declare comes into effect at that
    /// line, not before it.
    pub includes: Vec<IncludeSpan>,
    /// Parse state checkpoints: `(0-based line, state in effect from there)`,
    /// ascending. `insertion = chronological` can land anywhere in the file,
    /// including inside a *closed* `apply account` region, so the write path
    /// needs the state at an arbitrary line rather than only at eof.
    pub states: Vec<(usize, ParseState)>,
}

impl SourceFile {
    /// The parse state in effect at 0-based `line` of this file.
    #[must_use]
    pub fn state_at(&self, line: usize) -> &ParseState {
        self.states
            .iter()
            .rev()
            .find(|(at, _)| *at <= line)
            .map_or(&self.state_at_eof, |(_, s)| s)
    }
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
    /// `D` directives: the sample amount as written, with stream positions.
    pub defaults: Vec<Declaration>,
    /// `payee` declarations with stream positions.
    pub payees: Vec<Declaration>,
    /// `tag` declarations with stream positions.
    pub tags: Vec<Declaration>,
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
            Self::Io(p, e) => write!(
                f,
                "{}: {}",
                crate::errors::display_path(p),
                crate::errors::io_reason(e)
            ),
            Self::Cycle(p) => write!(
                f,
                "include cycle: {} is included twice",
                crate::errors::display_path(p)
            ),
        }
    }
}

impl std::error::Error for ParseError {}

/// The declarations of one kind visible at `pos` in the flattened stream.
fn visible_at(decls: &[Declaration], pos: usize) -> Vec<&str> {
    decls
        .iter()
        .filter(|d| d.pos < pos)
        .map(|d| d.name.as_str())
        .collect()
}

impl Journal {
    /// Declared account names visible at `pos` in the flattened stream.
    #[must_use]
    pub fn accounts_visible_at(&self, pos: usize) -> Vec<&str> {
        visible_at(&self.accounts, pos)
    }

    /// Whether any `account` directives exist anywhere in the tree (drives the
    /// `new_account` guard default).
    #[must_use]
    pub const fn declares_accounts(&self) -> bool {
        !self.accounts.is_empty()
    }

    /// Declared payee names visible at `pos` — what `hledger check payees`
    /// would accept there.
    #[must_use]
    pub fn payees_visible_at(&self, pos: usize) -> Vec<&str> {
        visible_at(&self.payees, pos)
    }

    /// Declared tag names visible at `pos`.
    #[must_use]
    pub fn tags_visible_at(&self, pos: usize) -> Vec<&str> {
        visible_at(&self.tags, pos)
    }

    /// The file entry for `path`, if it is part of the include tree.
    #[must_use]
    pub fn file(&self, path: &Path) -> Option<&SourceFile> {
        self.file_index(path).and_then(|i| self.files.get(i))
    }

    /// The index of `path` in [`Journal::files`], if it is part of the tree.
    #[must_use]
    pub fn file_index(&self, path: &Path) -> Option<usize> {
        let canon = canonical(path);
        self.files.iter().position(|f| canonical(&f.path) == canon)
    }

    /// Declared display styles over the whole include tree, for amount
    /// parsing and restyling.
    ///
    /// Everything the tree declares, wherever it stands. Right for a caller
    /// reading amounts at the end of the tree — which is where `add` inserts
    /// — and wrong for one reading them in the middle of it, which wants
    /// [`Journal::amount_ctx_at`].
    #[must_use]
    pub fn amount_ctx(&self) -> crate::amount::AmountCtx {
        self.amount_ctx_at(usize::MAX)
    }

    /// The declared display styles the file at index `idx` inherits, as a
    /// line-indexed list: what is in effect where the file begins, followed
    /// by what each of its `include` lines brings in, at that line.
    ///
    /// hledger reads a journal top to bottom and follows each `include` where
    /// it stands, so a style is in effect from its own declaration onward and
    /// no earlier. That is not bookkeeping: reading `1,234 GBP` turns on
    /// whether the `,` groups digits or marks the decimal, and a style
    /// declared below the amount does not answer it. The file's own
    /// directives are absent here — the formatter reads those as it goes.
    #[must_use]
    pub fn inherited_styles(&self, idx: usize) -> Vec<(usize, crate::amount::AmountCtx)> {
        let Some(file) = self.files.get(idx) else {
            return Vec::new();
        };
        let mut out = vec![(0, self.amount_ctx_at(file.start_pos))];
        out.extend(file.includes.iter().map(|inc| {
            (
                inc.line,
                self.styles_where(|p| p >= inc.start_pos && p < inc.end_pos),
            )
        }));
        out
    }

    /// The declared display styles in effect at `pos` in the flattened
    /// stream: those declared before it, and no others.
    ///
    /// hledger parses a journal top to bottom, following each `include` where
    /// it stands, so a directive further down has not been read yet when an
    /// amount above it is parsed. That matters beyond bookkeeping: reading
    /// `1,234 GBP` needs to know whether the `,` groups digits or marks the
    /// decimal, and hledger answers with the declarations it has seen so far.
    ///
    /// A `commodity` style outranks a `D` style for the same commodity,
    /// whichever came first — hledger 1.99's verified precedence.
    #[must_use]
    pub fn amount_ctx_at(&self, pos: usize) -> crate::amount::AmountCtx {
        self.styles_where(|p| p < pos)
    }

    /// The styles declared by the directives whose positions `keep` accepts.
    fn styles_where(&self, keep: impl Fn(usize) -> bool) -> crate::amount::AmountCtx {
        let mut ctx = crate::amount::AmountCtx::default();
        for d in self.defaults.iter().filter(|d| keep(d.pos)) {
            if let Some((name, style)) = crate::amount::style_from_sample(&d.name) {
                // `D` also supplies the decimal mark for reading an amount in
                // any commodity that declares no style of its own — the one
                // journal-wide parsing fallback hledger has.
                ctx.styles.insert(name, style.clone());
                ctx.default_style = Some(style);
            }
        }
        // Applied second, so a `commodity` style wins wherever it stands.
        for c in self.commodities.iter().filter(|c| keep(c.pos)) {
            if let Some(sample) = &c.sample {
                if let Some((name, style)) = crate::amount::style_from_sample(sample) {
                    ctx.styles.insert(name, style);
                }
            }
        }
        ctx
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
    /// Build the map from a file's raw contents, styles taken from the text
    /// itself.
    #[must_use]
    pub fn build(src: &str) -> Self {
        Self::build_inner(src, &[])
    }

    /// Build the map with the styles the file inherits — from the include
    /// tree ahead of it, and from its own `include` lines, each at the line
    /// it arrives on (see `fmt::format_opts_at`).
    #[must_use]
    pub fn build_with(src: &str, inherited: &[(usize, crate::amount::AmountCtx)]) -> Self {
        Self::build_inner(src, inherited)
    }

    fn build_inner(src: &str, inherited: &[(usize, crate::amount::AmountCtx)]) -> Self {
        let lines = crate::fmt::lines(src);
        let (acc_w, num_w) = crate::fmt::widths_with_at(&lines, inherited);
        let mut transactions = Vec::new();
        let mut i = 0usize;
        // A `comment` block is opaque: a date-looking line inside it is prose,
        // not a transaction, and must not become an insertion point.
        let mut opaque = false;
        // The file's own `Y` directives, so a partial-dated transaction still
        // gets a date for chronological insertion and the sortedness check.
        let mut year: Option<i32> = None;
        while let Some(line) = lines.get(i) {
            if opaque {
                opaque = !closes_comment_block(line);
                i = i.saturating_add(1);
            } else if opens_comment_block(line) {
                opaque = true;
                i = i.saturating_add(1);
            } else if opens_txn(line) {
                let start = i;
                i = i.saturating_add(1);
                while lines.get(i).is_some_and(|l| is_indented_non_blank(l)) {
                    i = i.saturating_add(1);
                }
                let date_tok = line.split_whitespace().next().unwrap_or_default();
                transactions.push(TxnSpan {
                    start,
                    end: i,
                    date: parse_date(date_tok, year),
                });
            } else {
                if let Some(y) = year_directive(line) {
                    year = Some(y);
                }
                i = i.saturating_add(1);
            }
        }
        Self {
            lines: lines.into_iter().map(ToOwned::to_owned).collect(),
            transactions,
            formatted: crate::fmt::is_formatted_at(src, inherited),
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
            start_pos: self.pos,
            eof_pos: 0,
            state_at_eof: ParseState::default(),
            includes: Vec::new(),
            // The inherited state is in effect from the file's first line.
            states: vec![(0, state.clone())],
        });

        let lines: Vec<&str> = crate::fmt::lines(src);
        let mut i = 0usize;
        // Index of the `commodity` declaration whose indented subdirective
        // block we are inside, if any — where a `format` line may still
        // supply the display-style sample.
        let mut open_commodity: Option<usize> = None;
        // Inside a `comment` block nothing is journal syntax: not the
        // transactions, not the declarations, and — the one with teeth — not
        // the `include` lines, which hledger does not follow either.
        let mut opaque = false;
        while let Some(line) = lines.get(i) {
            let lineno = i.saturating_add(1);
            let line_pos = self.pos;
            self.pos = self.pos.saturating_add(1);
            i = i.saturating_add(1);

            if opaque {
                opaque = !closes_comment_block(line);
                continue;
            }
            if opens_comment_block(line) {
                opaque = true;
                open_commodity = None;
                continue;
            }

            if is_indented_non_blank(line) {
                // An indented line outside a transaction: a subdirective. The
                // only one carrying semantics we use is `format` under
                // `commodity`; everything else stays ignored.
                if let Some(ci) = open_commodity {
                    if let Some(sample) = directive_arg(line.trim_start(), "format") {
                        if let Some(decl) = self.journal.commodities.get_mut(ci) {
                            decl.sample = Some(sample.to_owned());
                        }
                    }
                }
                continue;
            }
            open_commodity = None;

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
                match parse_transaction(line, &run, file_idx, lineno, line_pos, &state) {
                    Ok(txn) => self.journal.transactions.push(txn),
                    Err(why) => self.journal.warnings.push(format!(
                        "{}:{lineno}: skipping unparseable transaction ({why})",
                        crate::errors::display_path(path)
                    )),
                }
                continue; // the state cannot change inside a transaction
            }

            let at = Origin {
                file: file_idx,
                line: lineno,
            };
            if self.directive(line, path, at, line_pos, &mut state, &mut open_commodity)? {
                // A parse-state directive: it takes effect from the next line,
                // which `i` already points at.
                self.checkpoint(file_idx, i, &state);
            }
        }

        if let Some(entry) = self.journal.files.get_mut(file_idx) {
            entry.eof_pos = self.pos;
            entry.state_at_eof = state;
        }
        Ok(())
    }

    /// Handle one non-transaction line. Returns whether it changed the parse
    /// state, which is what needs a checkpoint recorded.
    ///
    /// Anything unrecognized — blank lines, comments, other directives — is
    /// ignored: `add` is an entry tool, not a validator.
    fn directive(
        &mut self,
        line: &str,
        path: &Path,
        at: Origin,
        line_pos: usize,
        state: &mut ParseState,
        open_commodity: &mut Option<usize>,
    ) -> Result<bool, ParseError> {
        if let Some(pattern) = directive_arg(line, "include") {
            // An include's declarations escape into the parent, but its parse
            // state does not, so nothing to checkpoint here.
            let start_pos = self.pos;
            self.include(path, pattern, state)?;
            let span = IncludeSpan {
                line: at.line.saturating_sub(1),
                start_pos,
                end_pos: self.pos,
            };
            if let Some(entry) = self.journal.files.get_mut(at.file) {
                entry.includes.push(span);
            }
        } else if let Some(name) = directive_arg(line, "account") {
            // `apply account` prefixes declarations too (verified, AA4):
            // inside `apply account assets`, `account checking` declares
            // `assets:checking`, and declaring the resolved name there is the
            // error.
            self.journal.accounts.push(Declaration {
                name: state.scope.resolve(name),
                pos: line_pos,
            });
        } else if let Some(sample) = directive_arg(line, "commodity") {
            *open_commodity = Some(self.journal.commodities.len());
            self.journal
                .commodities
                .push(parse_commodity(sample, line_pos));
        } else if let Some(name) = directive_rest(line, "payee") {
            self.journal.payees.push(Declaration {
                name: name.to_owned(),
                pos: line_pos,
            });
        } else if let Some(name) = directive_rest(line, "tag") {
            self.journal.tags.push(Declaration {
                name: name.to_owned(),
                pos: line_pos,
            });
        } else if let Some(mark) = directive_arg(line, "decimal-mark") {
            state.decimal_mark = mark.chars().next();
            return Ok(true);
        } else if let Some(sample) = directive_arg(line, "D") {
            state.default_commodity = Some(sample.to_owned());
            self.journal.defaults.push(Declaration {
                name: sample.to_owned(),
                pos: line_pos,
            });
            return Ok(true);
        } else if let Some(y) = year_directive(line) {
            state.year = Some(y);
            return Ok(true);
        } else if let Some(prefix) = directive_rest(line, "apply account") {
            state.scope.push_apply(prefix, at);
            return Ok(true);
        } else if is_directive(line, "end apply account") {
            state.scope.pop_apply();
            return Ok(true);
        } else if is_directive(line, "end aliases") {
            state.scope.clear_aliases();
            return Ok(true);
        } else if let Some(arg) = directive_rest(line, "alias") {
            if let Some(alias) = Alias::parse(arg, at) {
                state.scope.push_alias(alias);
                return Ok(true);
            }
            self.journal.warnings.push(format!(
                "{}:{}: ignoring alias that is not `OLD=NEW` or `/REGEX/=REPLACEMENT`",
                crate::errors::display_path(path),
                at.line
            ));
        }
        Ok(false)
    }

    /// Record the parse state in effect from 0-based `line` of `file_idx`.
    fn checkpoint(&mut self, file_idx: usize, line: usize, state: &ParseState) {
        if let Some(entry) = self.journal.files.get_mut(file_idx) {
            // A directive on the very first line replaces the inherited entry
            // rather than adding a redundant one.
            if entry.states.last().is_some_and(|(at, _)| *at == line) {
                entry.states.pop();
            }
            entry.states.push((line, state.clone()));
        }
    }

    /// Expand and walk one `include` directive.
    fn include(
        &mut self,
        parent: &Path,
        pattern: &str,
        state: &ParseState,
    ) -> Result<(), ParseError> {
        let base = parent.parent().map_or_else(PathBuf::new, Path::to_path_buf);
        let targets = expand_include(&base, pattern);
        if targets.is_empty() {
            self.journal.warnings.push(format!(
                "{}: include not found: {pattern} — accounts and history from it are unavailable",
                crate::errors::display_path(parent)
            ));
            return Ok(());
        }
        for target in targets {
            match fs::read_to_string(&target) {
                Ok(src) => self.file(&target, &src, state.clone())?,
                Err(e) => self.journal.warnings.push(format!(
                    "{}: include not readable: {}: {} — accounts and history from it are unavailable",
                    crate::errors::display_path(parent),
                    crate::errors::display_path(&target),
                    crate::errors::io_reason(&e)
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

/// Parse a `commodity` directive argument: a bare symbol (`USD`), a sample
/// amount with a separate symbol (`1_000.00 EUR`), or one with an attached
/// symbol (`$1000.00`).
fn parse_commodity(sample: &str, pos: usize) -> CommodityDecl {
    let toks: Vec<&str> = sample.split_whitespace().collect();
    let (num, commodity, _rest) = split_amount(&toks);
    if !commodity.is_empty() {
        return CommodityDecl {
            name: commodity,
            sample: Some(sample.to_owned()),
            pos,
        };
    }
    // An attached symbol still carries a style; a bare symbol does not.
    crate::amount::style_from_sample(sample).map_or(
        CommodityDecl {
            name: num,
            sample: None,
            pos,
        },
        |(name, _style)| CommodityDecl {
            name,
            sample: Some(sample.to_owned()),
            pos,
        },
    )
}

/// Parse a transaction header plus its indented run, lexically.
fn parse_transaction(
    header: &str,
    run: &[&str],
    file: usize,
    line: usize,
    pos: usize,
    state: &ParseState,
) -> Result<Transaction, String> {
    let (body, comment) = split_comment(header);
    let mut parts = body.splitn(2, char::is_whitespace);
    let date_tok = parts.next().unwrap_or_default();
    let rest = parts.next().unwrap_or_default();
    let date = parse_date(date_tok, state.year).ok_or_else(|| format!("bad date {date_tok:?}"))?;
    let description = strip_status(rest).to_owned();

    let mut postings = Vec::new();
    for l in run {
        let s = rstrip(l.trim_start_matches(char::is_whitespace));
        if s.starts_with(';') {
            continue; // in-transaction comment line
        }
        let (pbody, comment) = split_comment(s);
        let (account, amount) = split_account_amount(pbody);
        // A posting may carry a status flag of its own (`* assets:bank`).
        // It is not part of the account name, and indexing it as one made an
        // account that is already in the journal read as new — no completion
        // for it, and strict mode asking about it every time.
        let account = strip_status(account);
        if account.is_empty() {
            continue;
        }
        let toks: Vec<&str> = amount.split_whitespace().collect();
        let (_num, commodity, _rest) = split_amount(&toks);
        postings.push(RawPosting {
            // Resolved, so the index holds the account hledger sees rather
            // than the remainder the file happens to spell it with.
            account: state.scope.resolve(account),
            amount: amount.to_owned(),
            commodity: if commodity.is_empty() {
                None
            } else {
                Some(commodity)
            },
            comment: comment.map(ToOwned::to_owned),
        });
    }
    Ok(Transaction {
        date,
        description,
        comment: comment.map(ToOwned::to_owned),
        postings,
        file,
        line,
        pos,
    })
}

/// Parse a header's primary date. A secondary date (`=DATE`) is dropped.
///
/// A full `Y-M-D` date (separators `-`, `/`, `.`) is taken as written. A
/// partial `M-D` date takes the `Y`/`year` directive in effect, falling back
/// to the current year — which is what hledger does, so a partial date is
/// always resolvable and never skips its transaction.
fn parse_date(token: &str, year: Option<i32>) -> Option<NaiveDate> {
    let primary = token.split('=').next().unwrap_or_default();
    let parts: Vec<&str> = primary.split(['/', '-', '.']).collect();
    match parts.as_slice() {
        [y, m, d] => NaiveDate::from_ymd_opt(y.parse().ok()?, m.parse().ok()?, d.parse().ok()?),
        [m, d] => {
            let y = year.unwrap_or_else(|| chrono::Local::now().date_naive().year());
            NaiveDate::from_ymd_opt(y, m.parse().ok()?, d.parse().ok()?)
        }
        _ => None,
    }
}

/// The year of a `Y 2019` / `year 2019` directive.
fn year_directive(line: &str) -> Option<i32> {
    let arg = directive_arg(line, "Y").or_else(|| directive_arg(line, "year"))?;
    arg.parse().ok()
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
    fn a_posting_status_flag_is_not_part_of_the_account_name() {
        // `* assets:bank` is assets:bank, marked cleared. Indexing the flag
        // as part of the name hid the account from completion and made
        // strict mode treat an account already in the journal as new.
        let t = tree(&[(
            "main.journal",
            "2026-01-01 x\n    * assets:bank  10 EUR\n    ! expenses:food  -10 EUR\n",
        )]);
        let j = parse(&t, "main.journal");
        let used: Vec<&str> = j
            .transactions
            .iter()
            .flat_map(|t| t.postings.iter().map(|p| p.account.as_str()))
            .collect();
        assert_eq!(used, vec!["assets:bank", "expenses:food"]);
    }

    #[test]
    fn nothing_inside_a_comment_block_is_journal_syntax() {
        // hledger ignores all of this, including the `include`; so must we.
        let t = tree(&[(
            "main.journal",
            "account Assets:Real\n\
                 comment\n\
                 account Assets:Prose\n\
                 commodity EUR\n\
                 include missing.journal\n\
                 2025-06-06 not a transaction\n\
                     Expenses:Prose  1 EUR\n\
                 end comment\n\
                 2025-01-01 real\n\
                     Assets:Real  1 EUR\n\
                     Expenses:Real\n",
        )]);
        let j = parse(&t, "main.journal");
        let accounts: Vec<&str> = j.accounts.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(accounts, vec!["Assets:Real"]);
        assert!(j.commodities.is_empty());
        assert_eq!(j.transactions.len(), 1);
        assert_eq!(j.transactions[0].date, d(2025, 1, 1));
        assert_eq!(
            j.files.len(),
            1,
            "the include inside the block was followed"
        );
    }

    #[test]
    fn an_unterminated_comment_block_is_opaque_to_end_of_file() {
        let t = tree(&[(
            "main.journal",
            "2025-01-01 real\n    a:b  1 EUR\n    c:d\n\ncomment\n2025-06-06 prose\n    e:f  1 EUR\n",
        )]);
        let j = parse(&t, "main.journal");
        assert_eq!(j.transactions.len(), 1);
    }

    #[test]
    fn a_format_subdirective_supplies_the_commodity_sample() {
        let t = tree(&[(
            "main.journal",
            "commodity EUR\n    format 1.000,00 EUR\ncommodity USD\n",
        )]);
        let j = parse(&t, "main.journal");
        assert_eq!(j.commodities.len(), 2);
        assert_eq!(j.commodities[0].name, "EUR");
        assert_eq!(j.commodities[0].sample.as_deref(), Some("1.000,00 EUR"));
        assert_eq!(j.commodities[1].sample, None);
    }

    #[test]
    fn inherited_styles_arrive_where_the_include_stands() {
        let t = tree(&[
            (
                "main.journal",
                "include early.journal\n2026-01-01 x\n    a  1 EUR\n\ninclude late.journal\n",
            ),
            ("early.journal", "commodity 1_000.00 EUR\n"),
            ("late.journal", "commodity 1.000,00 USD\n"),
        ]);
        let j = parse(&t, "main.journal");
        let inherited = j.inherited_styles(0);
        // Nothing precedes the main file, then one entry per include line.
        assert!(inherited[0].1.styles.is_empty());
        assert_eq!(inherited[1].0, 0); // `include early.journal`
        assert_eq!(inherited[1].1.styles["EUR"].group_sep, Some('_'));
        assert_eq!(inherited[2].0, 4); // `include late.journal`
        assert_eq!(inherited[2].1.styles["USD"].group_sep, Some('.'));
        // An included file inherits what precedes it and declares the rest
        // itself, so its own entry is empty.
        let late = j.file_index(&t.path().join("late.journal")).unwrap();
        assert_eq!(late, 2);
        assert_eq!(
            j.inherited_styles(late)[0].1.styles["EUR"].group_sep,
            Some('_')
        );
    }

    #[test]
    fn amount_ctx_at_ignores_declarations_below_it() {
        let t = tree(&[(
            "main.journal",
            "2026-01-01 x\n    a  1 EUR\n\ncommodity 1_000.00 EUR\n",
        )]);
        let j = parse(&t, "main.journal");
        assert!(j.amount_ctx_at(1).styles.is_empty());
        assert_eq!(j.amount_ctx().styles["EUR"].group_sep, Some('_'));
    }

    #[test]
    fn amount_ctx_collects_styles_with_hledger_precedence() {
        // D ranks below commodity, even when D comes later; the last
        // commodity declaration wins — both verified against hledger 1.99.
        let t = tree(&[
            (
                "main.journal",
                "include sub.journal\ncommodity 1_000.00 EUR\nD 1.000,00 EUR\n",
            ),
            (
                "sub.journal",
                "commodity 1,000.00 USD\ncommodity 1_000.00 USD\n",
            ),
        ]);
        let ctx = parse(&t, "main.journal").amount_ctx();
        assert_eq!(ctx.styles["EUR"].group_sep, Some('_'));
        assert_eq!(ctx.styles["USD"].group_sep, Some('_'));
        assert_eq!(ctx.decimal_mark, None);
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
            ("g/sub/deep/d.journal", "2026-01-05 deep\n    a  1 EUR\n"),
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
            ("main.journal", "decimal-mark ,\ninclude sub.journal\n"),
            ("sub.journal", "include leaf.journal\n"),
            ("leaf.journal", ""),
            ("main2.journal", "include marked.journal\n"),
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
    fn attached_symbol_commodity_directives_keep_their_style() {
        let t = tree(&[("main.journal", "commodity $1000.00\n")]);
        let j = parse(&t, "main.journal");
        assert_eq!(j.commodities[0].name, "$");
        assert_eq!(j.commodities[0].sample.as_deref(), Some("$1000.00"));
    }

    #[test]
    fn unknown_directives_are_ignored_silently() {
        let t = tree(&[(
            "main.journal",
            "P 2026-01-01 USD 0.9 EUR\nN EUR\nassert foo\n",
        )]);
        let j = parse(&t, "main.journal");
        assert!(j.warnings.is_empty());
        assert!(j.transactions.is_empty());
    }

    // ---- epic 3: payee and tag declarations (scope model 2) ----

    #[test]
    fn payee_and_tag_declarations_carry_stream_positions() {
        let t = tree(&[
            (
                "main.journal",
                "include conf.journal\npayee Rewe\ntag project\n",
            ),
            ("conf.journal", "payee Deutsche Bahn\ntag client\n"),
        ]);
        let j = parse(&t, "main.journal");
        let payees: Vec<&str> = j.payees.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(payees, vec!["Deutsche Bahn", "Rewe"]);
        let tags: Vec<&str> = j.tags.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(tags, vec!["client", "project"]);

        // Position-sensitive, exactly like `account`.
        let conf = j.file(&t.path().join("conf.journal")).unwrap();
        assert_eq!(j.payees_visible_at(conf.eof_pos), vec!["Deutsche Bahn"]);
        let main = j.file(&t.path().join("main.journal")).unwrap();
        assert_eq!(
            j.payees_visible_at(main.eof_pos),
            vec!["Deutsche Bahn", "Rewe"]
        );
    }

    #[test]
    fn a_payee_name_may_contain_a_run_of_spaces() {
        // `directive_arg`'s 2-space annotation cut would truncate this.
        let t = tree(&[("main.journal", "payee Cafe  Central\n")]);
        assert_eq!(parse(&t, "main.journal").payees[0].name, "Cafe  Central");
    }

    // ---- epic 3: Y / year (scope model 1) ----

    #[test]
    fn the_year_directive_resolves_partial_dates() {
        // DESIGN test Y1 / Y5 / Y6.
        let t = tree(&[(
            "main.journal",
            "Y 2019\n01-15 first\n    a  1 EUR\n\nY 2021\n03-03 second\n    a  1 EUR\n\n2024-06-06 full\n    a  1 EUR\n",
        )]);
        let j = parse(&t, "main.journal");
        let dates: Vec<NaiveDate> = j.transactions.iter().map(|t| t.date).collect();
        assert_eq!(dates, vec![d(2019, 1, 15), d(2021, 3, 3), d(2024, 6, 6)]);
        assert!(j.warnings.is_empty());
    }

    #[test]
    fn the_long_year_spelling_works_too() {
        let t = tree(&[("main.journal", "year 2019\n01-15 x\n    a  1 EUR\n")]);
        assert_eq!(
            parse(&t, "main.journal").transactions[0].date,
            d(2019, 1, 15)
        );
    }

    #[test]
    fn a_partial_date_without_a_year_directive_takes_the_current_year() {
        // DESIGN test Y2 — the control that nearly proved nothing. A partial
        // date is always resolvable, so it never skips its transaction.
        let t = tree(&[("main.journal", "01-15 x\n    a  1 EUR\n")]);
        let j = parse(&t, "main.journal");
        let this_year = chrono::Local::now().date_naive().year();
        assert_eq!(j.transactions[0].date, d(this_year, 1, 15));
        assert!(j.warnings.is_empty());
    }

    #[test]
    fn the_year_directive_is_parse_state_not_a_declaration() {
        // DESIGN tests Y3 and Y4: inherited into includes, discarded on
        // return.
        let t = tree(&[
            (
                "main.journal",
                "Y 2019\ninclude sub.journal\n02-20 parent-after\n    a  1 EUR\n",
            ),
            ("sub.journal", "01-15 in-include\n    a  1 EUR\n"),
            (
                "main2.journal",
                "include opener.journal\n02-20 after\n    a  1 EUR\n",
            ),
            ("opener.journal", "Y 2019\n01-15 inside\n    a  1 EUR\n"),
        ]);
        let j = parse(&t, "main.journal");
        assert_eq!(j.transactions[0].date, d(2019, 1, 15)); // into the include
        assert_eq!(j.transactions[1].date, d(2019, 2, 20)); // still after it

        let j2 = parse(&t, "main2.journal");
        let this_year = chrono::Local::now().date_naive().year();
        assert_eq!(j2.transactions[0].date, d(2019, 1, 15));
        assert_eq!(j2.transactions[1].date, d(this_year, 2, 20)); // discarded
    }

    // ---- epic 3: apply account / alias resolution on read ----

    #[test]
    fn apply_account_resolves_posting_accounts() {
        let t = tree(&[(
            "main.journal",
            "apply account assets:bank\n2026-01-05 x\n    checking   1 EUR\n    savings   -1 EUR\n",
        )]);
        let j = parse(&t, "main.journal");
        let names: Vec<&str> = j.transactions[0]
            .postings
            .iter()
            .map(|p| p.account.as_str())
            .collect();
        assert_eq!(names, vec!["assets:bank:checking", "assets:bank:savings"]);
    }

    #[test]
    fn apply_account_prefixes_account_declarations() {
        // DESIGN test AA4 — the surprise. Verified against hledger 1.99 with
        // a control that fails.
        let t = tree(&[(
            "main.journal",
            "apply account assets\naccount checking\nend apply account\naccount cash\n",
        )]);
        let j = parse(&t, "main.journal");
        let names: Vec<&str> = j.accounts.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, vec!["assets:checking", "cash"]);
    }

    #[test]
    fn an_unclosed_apply_account_runs_to_end_of_file() {
        // DESIGN test AA5. Unclosed regions are legal, which is exactly what
        // an `append` insertion lands inside of.
        let t = tree(&[(
            "main.journal",
            "apply account a\n2026-01-05 x\n    q  1 EUR\n",
        )]);
        let j = parse(&t, "main.journal");
        assert_eq!(j.transactions[0].postings[0].account, "a:q");
        let main = j.file(&t.path().join("main.journal")).unwrap();
        assert!(main.state_at_eof.scope.is_active());
        assert_eq!(main.state_at_eof.scope.prefix().as_deref(), Some("a"));
    }

    #[test]
    fn apply_account_is_discarded_when_an_include_returns() {
        let t = tree(&[
            (
                "main.journal",
                "include opener.journal\n2026-01-06 after\n    q  1 EUR\n",
            ),
            (
                "opener.journal",
                "apply account a\n2026-01-05 inside\n    q  1 EUR\n",
            ),
        ]);
        let j = parse(&t, "main.journal");
        assert_eq!(j.transactions[0].postings[0].account, "a:q");
        assert_eq!(j.transactions[1].postings[0].account, "q");
    }

    #[test]
    fn aliases_resolve_and_end_aliases_stops_them() {
        let t = tree(&[(
            "main.journal",
            "alias bank=assets:checking\n2026-01-05 x\n    bank      1 EUR\n    bank:sub  1 EUR\n\nend aliases\n\n2026-01-06 y\n    bank  1 EUR\n",
        )]);
        let j = parse(&t, "main.journal");
        let first: Vec<&str> = j.transactions[0]
            .postings
            .iter()
            .map(|p| p.account.as_str())
            .collect();
        assert_eq!(first, vec!["assets:checking", "assets:checking:sub"]);
        assert_eq!(j.transactions[1].postings[0].account, "bank");
    }

    #[test]
    fn an_unparseable_alias_warns_and_is_skipped() {
        let t = tree(&[(
            "main.journal",
            "alias nonsense\n2026-01-05 x\n    q  1 EUR\n",
        )]);
        let j = parse(&t, "main.journal");
        assert_eq!(j.transactions[0].postings[0].account, "q");
        assert_eq!(j.warnings.len(), 1);
        assert!(j.warnings[0].contains("alias"), "{}", j.warnings[0]);
    }

    #[test]
    fn state_checkpoints_answer_for_an_arbitrary_line() {
        // What `insertion = chronological` needs: a mid-file insertion can
        // land inside a *closed* region.
        let src = "2026-01-01 a\n    q  1 EUR\n\napply account P\n\n2026-01-05 b\n    q  1 EUR\n\nend apply account\n\n2026-01-10 c\n    q  1 EUR\n";
        let t = tree(&[("main.journal", src)]);
        let j = parse(&t, "main.journal");
        let f = j.file(&t.path().join("main.journal")).unwrap();
        // Line 0 is before the block, line 5 is inside it, line 10 after.
        assert!(!f.state_at(0).scope.is_active());
        assert_eq!(f.state_at(5).scope.prefix().as_deref(), Some("P"));
        assert!(!f.state_at(10).scope.is_active());
        assert!(!f.state_at_eof.scope.is_active());
    }

    #[test]
    fn posting_and_header_comments_are_captured() {
        let t = tree(&[(
            "main.journal",
            "2026-01-05 Rewe  ; trip: berlin\n    a  1 EUR  ; note: x\n    b  -1 EUR\n",
        )]);
        let j = parse(&t, "main.journal");
        let txn = &j.transactions[0];
        assert_eq!(txn.description, "Rewe");
        assert_eq!(txn.comment.as_deref(), Some("; trip: berlin"));
        assert_eq!(txn.postings[0].comment.as_deref(), Some("; note: x"));
        assert_eq!(txn.postings[1].comment, None);
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
