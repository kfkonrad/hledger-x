//! Format-preserving hledger journal formatter.
//!
//! Line-oriented: each physical line is classified and only posting lines
//! are reflowed; everything else passes through (directives and comments
//! verbatim, transaction headers with trailing whitespace trimmed). Because
//! directives pass through untouched, price-only and include-only files are
//! safe by construction.
//!
//! Declared commodity display styles are the one piece of journal semantics
//! the formatter *reads* (it still never writes directives): amounts of a
//! commodity with a declared style are re-rendered in that style, the way
//! `hledger print` shows them — symbol side, spacing, digit grouping and
//! decimal mark normalized, entered precision kept. Styles come from the
//! text's own `commodity` / `D` directives via [`scan_ctx`], or from a
//! caller-supplied [`AmountCtx`] covering the whole include tree.
//!
//! Blank lines between top-level blocks are normalized to exactly one where a
//! transaction is involved, and collapsed to at most one everywhere else; see
//! [`blank`].
//!
//! Amounts are aligned to a single file-wide column: the account field is
//! padded past the longest account name in the file, and every first-amount
//! number is right-aligned to one shared column across all transactions.

pub mod blank;
mod explicit;
pub mod posting;
pub mod sort;

use crate::amount::{style_from_sample, AmountCtx, Places};
use crate::lex::{
    closes_comment_block, directive_arg, is_blank, is_indented_non_blank, opens_comment_block,
    opens_txn, rstrip,
};
use posting::{account_of, parse_posting, render, restyle_with, Posting};

/// What a formatting run should do beyond the canonical layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Options {
    /// Stably sort transactions by date, within directive-bounded runs.
    pub sort: bool,
    /// Spell out what the journal leaves implied: fill in each transaction's
    /// one inferred amount, and pad every restyled amount to its commodity's
    /// declared number of decimal places.
    ///
    /// This rewrites amounts rather than only their layout, so it is never a
    /// default and never configurable — it happens because the invocation
    /// asked for it.
    pub explicit: bool,
}

impl Options {
    /// Sorting only: the shape of a plain `fmt --sort`.
    #[must_use]
    pub const fn sorted(sort: bool) -> Self {
        Self {
            sort,
            explicit: false,
        }
    }

    /// The precision policy restyling runs under.
    const fn places(self) -> Places {
        if self.explicit {
            Places::AtLeastDeclared
        } else {
            Places::AsWritten
        }
    }
}

/// Format a whole file's contents, restyling amounts to the styles declared
/// in the text itself.
///
/// Output always ends in a newline (empty input yields empty output).
/// Idempotent: `format(format(x)) == format(x)`.
#[must_use]
pub fn format(s: &str) -> String {
    format_with(s, &scan_ctx(&lines(s)))
}

/// Like [`format`], with styles supplied by the caller — the whole include
/// tree's, typically. The text's own directives are *not* rescanned: the
/// caller's context is authoritative.
#[must_use]
pub fn format_with(s: &str, ctx: &AmountCtx) -> String {
    format_opts(s, ctx, Options::default())
}

/// Like [`format`], but also stably sorts transactions by date.
///
/// Sorting is directive-bounded: transactions reorder only within runs between
/// directives and standalone comment blocks, which act as barriers, so
/// positional directives keep their scope. Equal dates keep their source order.
#[must_use]
pub fn format_sorted(s: &str) -> String {
    format_sorted_with(s, &scan_ctx(&lines(s)))
}

/// [`format_sorted`] with caller-supplied styles.
#[must_use]
pub fn format_sorted_with(s: &str, ctx: &AmountCtx) -> String {
    format_opts(s, ctx, Options::sorted(true))
}

/// The one formatting entry point every other one funnels into: caller-supplied
/// styles, and everything optional named in [`Options`].
#[must_use]
pub fn format_opts(s: &str, ctx: &AmountCtx, opts: Options) -> String {
    let ls = lines(s);
    let ls = if opts.sort {
        sort::sort_entries(&ls)
    } else {
        ls
    };
    unlines(&format_lines(&blank::normalize(&ls), ctx, opts))
}

/// [`format_opts`] against the styles declared in the text itself.
#[must_use]
pub fn format_opts_scanned(s: &str, opts: Options) -> String {
    format_opts(s, &scan_ctx(&lines(s)), opts)
}

/// Whether the input is already a fixed point of [`format`]. Drives `--check`.
#[must_use]
pub fn is_formatted(s: &str) -> bool {
    format(s) == s
}

/// [`is_formatted`] with caller-supplied styles.
#[must_use]
pub fn is_formatted_with(s: &str, ctx: &AmountCtx) -> bool {
    format_with(s, ctx) == s
}

/// Whether the input is already a fixed point of [`format_sorted`]. Drives
/// `--check --sort`.
#[must_use]
pub fn is_formatted_sorted(s: &str) -> bool {
    format_sorted(s) == s
}

/// [`is_formatted_sorted`] with caller-supplied styles.
#[must_use]
pub fn is_formatted_sorted_with(s: &str, ctx: &AmountCtx) -> bool {
    format_sorted_with(s, ctx) == s
}

/// Whether the input is already a fixed point of [`format_opts`]. Drives
/// `--check` under any combination of flags.
#[must_use]
pub fn is_formatted_opts(s: &str, ctx: &AmountCtx, opts: Options) -> bool {
    format_opts(s, ctx, opts) == s
}

/// Collect declared commodity display styles from the text alone: `commodity`
/// directive samples, their indented `format` subdirectives, and `D`.
///
/// Verified against hledger 1.99: styles apply journal-wide regardless of
/// position, the last declaration of a commodity wins, and a `commodity`
/// style beats a `D` style even when `D` comes later. A bare `commodity EUR`
/// declares no style.
#[must_use]
pub fn scan_ctx(ls: &[&str]) -> AmountCtx {
    let mut ctx = AmountCtx::default();
    let mut declared: Vec<(String, crate::amount::DisplayStyle)> = Vec::new();
    // The commodity whose indented subdirective block we are inside, waiting
    // for a `format` line.
    let mut block: Option<String> = None;
    // Inside a `comment` block nothing is journal syntax, so no directive in
    // it may be read.
    let mut opaque = false;
    for l in ls {
        if opaque {
            opaque = !closes_comment_block(l);
            continue;
        }
        if opens_comment_block(l) {
            opaque = true;
            block = None;
            continue;
        }
        if is_indented_non_blank(l) {
            if let Some(name) = &block {
                if let Some(sample) = directive_arg(l.trim_start(), "format") {
                    if let Some((_, style)) = style_from_sample(sample) {
                        declared.push((name.clone(), style));
                    }
                }
            }
            continue;
        }
        block = None;
        if let Some(sample) = directive_arg(l, "commodity") {
            if let Some((name, style)) = style_from_sample(sample) {
                block = Some(name.clone());
                declared.push((name, style));
            } else {
                block = Some(sample.to_owned());
            }
        } else if let Some(sample) = directive_arg(l, "D") {
            // `D` styles rank below `commodity` styles: they land in the map
            // first and the `declared` pass below overwrites them.
            if let Some((name, style)) = style_from_sample(sample) {
                ctx.styles.insert(name, style);
            }
        }
    }
    for (name, style) in declared {
        ctx.styles.insert(name, style);
    }
    ctx
}

/// Split into lines with Haskell `lines` semantics.
///
/// `""` yields no lines, a trailing newline does not produce a final empty
/// line, and `\r` is kept (it is content, not a terminator).
#[must_use]
pub fn lines(s: &str) -> Vec<&str> {
    if s.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<&str> = s.split('\n').collect();
    if s.ends_with('\n') {
        out.pop();
    }
    out
}

/// Join with Haskell `unlines` semantics.
///
/// A newline goes after *every* line, so output always ends in one and a file
/// lacking a trailing newline gains one.
#[must_use]
pub fn unlines<S: AsRef<str>>(ls: &[S]) -> String {
    let mut out = String::new();
    for l in ls {
        out.push_str(l.as_ref());
        out.push('\n');
    }
    out
}

/// The file-wide alignment widths.
///
/// The longest account name over every posting in the file, and the longest
/// number field over every amount posting.
///
/// These are deliberately **not** per-transaction. `add` calls this directly so
/// its preview and its writes align exactly as `fmt` would.
#[must_use]
pub fn widths(ls: &[&str]) -> (usize, usize) {
    widths_with(ls, &scan_ctx(ls))
}

/// [`widths`] with caller-supplied styles: the widths of the *restyled*
/// postings, which is what [`format_with`] aligns to.
#[must_use]
pub fn widths_with(ls: &[&str], ctx: &AmountCtx) -> (usize, usize) {
    widths_of(&styled_lines(ls, ctx, Options::default()))
}

/// What a physical line is, once classified.
enum Class {
    /// A posting line, parsed and restyled: the only kind that is reflowed.
    ///
    /// Normally one posting, and always one without `--explicit`. Filling in
    /// an inferred amount that spans commodities turns the line into several,
    /// which is what `hledger print -x` does with the same input.
    Post(Vec<Posting>),
    /// Anything else the formatter may touch — see [`format_other`].
    Other,
    /// A line inside a `comment` block, or one of its delimiters: passed
    /// through byte-for-byte, since its contents are not journal syntax.
    Opaque,
}

/// The alignment widths of an already-classified line list.
fn widths_of(styled: &[Class]) -> (usize, usize) {
    let posts = styled.iter().flat_map(|c| match c {
        Class::Post(ps) => ps.as_slice(),
        Class::Other | Class::Opaque => &[],
    });
    let acc_w = posts
        .clone()
        .filter_map(|p| account_of(p).map(|a| a.chars().count()))
        .max()
        .unwrap_or(0);
    let num_w = posts
        .filter_map(|p| match p {
            Posting::Amount { num, .. } => Some(num.chars().count()),
            Posting::Comment(_) | Posting::Bare(_, _) => None,
        })
        .max()
        .unwrap_or(0);
    (acc_w, num_w)
}

/// Classify each physical line: a parsed-and-restyled posting, or a
/// pass-through line.
///
/// A posting line is an indented, non-blank line that follows a transaction
/// header. An indented run that follows anything else — an `account` or
/// `commodity` directive, say — is not postings. A `decimal-mark` directive
/// changes how amounts *parse* from that point on (the declared style still
/// governs display — verified against hledger 1.99). A `comment` block is
/// opaque throughout: a posting-looking line inside it is prose.
fn styled_lines(ls: &[&str], ctx: &AmountCtx, opts: Options) -> Vec<Class> {
    let mut cur = ctx.clone();
    let mut in_txn = false;
    let mut opaque = false;
    let mut out: Vec<Class> = Vec::with_capacity(ls.len());
    // Where the posting run of the transaction being classified starts, and
    // the styles in effect there: `--explicit` needs the whole transaction at
    // once, so the fill waits until the run ends.
    let mut txn: Option<(usize, AmountCtx)> = None;
    for l in ls {
        if opaque {
            opaque = !closes_comment_block(l);
            out.push(Class::Opaque);
        } else if opens_comment_block(l) {
            opaque = true;
            in_txn = false;
            close_txn(&mut out, &mut txn, opts);
            out.push(Class::Opaque);
        } else if in_txn && is_indented_non_blank(l) {
            out.push(Class::Post(vec![restyle_with(
                parse_posting(l),
                &cur,
                opts.places(),
            )]));
        } else {
            if let Some(arg) = directive_arg(l, "decimal-mark") {
                cur.decimal_mark = arg.chars().next().filter(|c| matches!(c, '.' | ','));
            }
            close_txn(&mut out, &mut txn, opts);
            out.push(Class::Other);
            in_txn = opens_txn(l);
            if in_txn {
                txn = Some((out.len(), cur.clone()));
            }
        }
    }
    close_txn(&mut out, &mut txn, opts);
    out
}

/// End the transaction whose postings start at the recorded index, filling in
/// its inferred amounts when `--explicit` asked for them.
fn close_txn(out: &mut [Class], txn: &mut Option<(usize, AmountCtx)>, opts: Options) {
    let Some((start, ctx)) = txn.take() else {
        return;
    };
    if !opts.explicit {
        return;
    }
    if let Some(run) = out.get_mut(start..) {
        explicit::fill(run, &ctx);
    }
}

/// Reflow a list of physical lines against the file-wide widths.
fn format_lines(ls: &[&str], ctx: &AmountCtx, opts: Options) -> Vec<String> {
    let styled = styled_lines(ls, ctx, opts);
    let (acc_w, num_w) = widths_of(&styled);
    ls.iter()
        .zip(styled)
        .flat_map(|(l, c)| match c {
            Class::Post(ps) => ps.iter().map(|p| render(acc_w, num_w, p)).collect(),
            Class::Other => vec![format_other(l)],
            Class::Opaque => vec![(*l).to_owned()],
        })
        .collect()
}

/// Format a non-posting line.
///
/// Blank lines collapse to empty — how *many* there are is [`blank`]'s
/// business, decided before this point; transaction headers get
/// trailing whitespace trimmed; everything else — directives, top-level
/// comments, `include`, `P` price lines — passes through verbatim.
fn format_other(s: &str) -> String {
    if is_blank(s) {
        String::new()
    } else if opens_txn(s) {
        rstrip(s).to_owned()
    } else {
        s.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn haskell_line_semantics() {
        assert_eq!(lines(""), Vec::<&str>::new());
        assert_eq!(lines("a\n"), vec!["a"]);
        assert_eq!(lines("a"), vec!["a"]);
        assert_eq!(lines("a\n\n"), vec!["a", ""]);
        assert_eq!(unlines::<&str>(&[]), "");
        assert_eq!(unlines(&["a", "b"]), "a\nb\n");
    }

    #[test]
    fn empty_input_yields_empty_output() {
        assert_eq!(format(""), "");
    }

    #[test]
    fn a_missing_trailing_newline_is_added() {
        assert_eq!(format("2025-01-01 x"), "2025-01-01 x\n");
    }

    #[test]
    fn whitespace_only_lines_are_blank_lines() {
        // Trailing blanks go entirely; an interior run collapses to one.
        assert_eq!(format("2025-01-01 x\n   \t \n\n"), "2025-01-01 x\n");
        assert_eq!(
            format("2025-01-01 x\n   \t \n\naccount a:b\n"),
            "2025-01-01 x\n\naccount a:b\n"
        );
    }

    #[test]
    fn transactions_are_separated_by_exactly_one_blank_line() {
        let dense = "2025-01-01 a\n    e:x  1\n2025-01-02 b\n    e:x  2\n";
        let spaced = "2025-01-01 a\n    e:x  1\n\n\n2025-01-02 b\n    e:x  2\n";
        let want = "2025-01-01 a\n    e:x  1\n\n2025-01-02 b\n    e:x  2\n";
        assert_eq!(format(dense), want);
        assert_eq!(format(spaced), want);
    }

    #[test]
    fn a_comment_block_passes_through_byte_for_byte() {
        // Everything inside is prose: the posting-looking lines keep their
        // sloppy indentation and spacing, the blank line survives, the
        // trailing whitespace on the delimiters survives, and the `commodity`
        // directive in there declares nothing.
        let src = "commodity USD\n    format 1,000.00 USD\n\ncomment  \n2025-01-01 not a transaction\n   e:x   1234USD  \n\ncommodity EUR\n    format 1.000,00 EUR\nend comment\t\n\n2025-02-02 real\n    e:x  1234EUR\n";
        let want = "commodity USD\n    format 1,000.00 USD\n\ncomment  \n2025-01-01 not a transaction\n   e:x   1234USD  \n\ncommodity EUR\n    format 1.000,00 EUR\nend comment\t\n\n2025-02-02 real\n    e:x  1234EUR\n";
        assert_eq!(format(src), want);
    }

    #[test]
    fn an_unterminated_comment_block_is_opaque_to_eof() {
        let src = "2025-01-01 a\n    e:x  1 USD\n\ncomment\n2025-01-02 prose\n    sloppy   text\n";
        assert_eq!(format(src), src);
    }

    #[test]
    fn a_comment_block_is_one_barrier_and_never_reordered() {
        let src = "comment\n2025-09-09 prose\nend comment\n\n2025-02-02 b\n    e:x  2 USD\n\n2025-01-01 a\n    e:x  1 USD\n";
        let want = "comment\n2025-09-09 prose\nend comment\n\n2025-01-01 a\n    e:x  1 USD\n\n2025-02-02 b\n    e:x  2 USD\n";
        assert_eq!(format_sorted(src), want);
    }

    #[test]
    fn transaction_headers_lose_only_trailing_whitespace() {
        assert_eq!(format("2025-01-01   payee   \n"), "2025-01-01   payee\n");
    }

    #[test]
    fn directives_and_price_lines_pass_through_verbatim() {
        let src = "# a comment  \ninclude other.journal  \nP 2025-01-01 USD   1.00 EUR  \n";
        assert_eq!(format(src), src);
    }

    #[test]
    fn indented_sub_directives_are_left_alone() {
        let src = "account Assets:Bank\n    ; sub-directive comment\n    note Checking account\n";
        assert_eq!(format(src), src);
    }

    #[test]
    fn widths_are_file_wide_not_per_transaction() {
        let src =
            "2025-01-01 a\n    A:B  1.00 USD\n\n2025-01-02 b\n    Expenses:Long  -7485978.18 USD\n";
        assert_eq!(
            format(src),
            "2025-01-01 a\n    A:B                   1.00 USD\n\n2025-01-02 b\n    Expenses:Long  -7485978.18 USD\n"
        );
    }

    #[test]
    fn a_blank_line_ends_a_transaction_so_a_later_indented_run_is_not_postings() {
        let src = "2025-01-01 a\n    A:B  1 USD\n\n    stray indented line\n";
        assert_eq!(
            format(src),
            "2025-01-01 a\n    A:B  1 USD\n\n    stray indented line\n"
        );
    }

    #[test]
    fn a_file_of_only_directives_is_unchanged() {
        let src = "account Assets:Bank\ncommodity USD\ninclude other.journal\nD 1000.00 USD\n";
        assert_eq!(format(src), src);
    }

    #[test]
    fn a_price_only_file_is_unchanged() {
        let src = "P 2025-01-01 USD 1.00 EUR\nP 2025-01-02 USD 1.01 EUR\n";
        assert_eq!(format(src), src);
    }

    #[test]
    fn commodity_on_the_left_is_not_split_into_number_and_commodity() {
        // `$100` is not number-like, so it stays one token in the number field
        // and is right-aligned as a whole.
        let src = "2025-01-01 x\n    A:B  $100\n    C:D  -$100\n";
        assert_eq!(
            format(src),
            "2025-01-01 x\n    A:B   $100\n    C:D  -$100\n"
        );
    }

    #[test]
    fn accounts_containing_single_spaces_survive() {
        let src = "2025-01-01 x\n    Assets:Bank Account   100.00 USD\n";
        assert_eq!(
            format(src),
            "2025-01-01 x\n    Assets:Bank Account  100.00 USD\n"
        );
    }

    #[test]
    fn tabs_are_separators_and_indentation() {
        let src = "2025-01-01 x\n\tA:B\t\t1 USD\n";
        assert_eq!(format(src), "2025-01-01 x\n    A:B  1 USD\n");
    }

    #[test]
    fn an_amount_less_posting_carrying_only_an_assertion_reserves_the_columns() {
        let src = "2025-01-01 x\n    Expenses:Unknown\n    Assets:Cash   = 0 RSD\n";
        assert_eq!(
            format(src),
            "2025-01-01 x\n    Expenses:Unknown\n    Assets:Cash            = 0 RSD\n"
        );
    }

    // ---- restyling to declared commodity styles ----

    #[test]
    fn declared_styles_restyle_amounts_like_hledger_print() {
        // Verified against hledger 1.99: side, spacing and grouping are
        // normalized; decimals are never padded.
        let src = "commodity 1_000.00 EUR\n\n2025-01-01 x\n    a  10EUR\n    b  EUR10\n    c  -1234.5 EUR\n";
        assert_eq!(
            format(src),
            "commodity 1_000.00 EUR\n\n2025-01-01 x\n    a        10 EUR\n    b        10 EUR\n    c  -1_234.5 EUR\n"
        );
    }

    #[test]
    fn restyling_reaches_cost_and_assertion_tails() {
        let src = "commodity 1_000.00 EUR\ncommodity 1,000.00 USD\n\n2025-01-01 x\n    a  10EUR @ 1.1USD\n    b  -11 USD = -11USD\n";
        assert_eq!(
            format(src),
            "commodity 1_000.00 EUR\ncommodity 1,000.00 USD\n\n2025-01-01 x\n    a   10 EUR @ 1.1 USD\n    b  -11 USD = -11 USD\n"
        );
    }

    #[test]
    fn styles_apply_journal_wide_regardless_of_position() {
        // Verified: hledger applies a directive declared after the txn.
        let src = "2025-01-01 x\n    a  10EUR\n\ncommodity 1_000.00 EUR\n";
        assert_eq!(
            format(src),
            "2025-01-01 x\n    a  10 EUR\n\ncommodity 1_000.00 EUR\n"
        );
    }

    #[test]
    fn undeclared_commodities_and_unitless_amounts_pass_through() {
        let src = "commodity 1_000.00 EUR\n\n2025-01-01 x\n    a  10USD\n    b  1_0.5\n";
        assert_eq!(format(src), src);
    }

    #[test]
    fn a_bare_commodity_directive_declares_no_style() {
        let src = "commodity EUR\n\n2025-01-01 x\n    a  10EUR\n    b\n";
        assert_eq!(format(src), src);
    }

    #[test]
    fn the_format_subdirective_declares_the_style() {
        let src = "commodity EUR\n    format 1.000,00 EUR\n\n2025-01-01 x\n    a  10EUR\n";
        assert_eq!(
            format(src),
            "commodity EUR\n    format 1.000,00 EUR\n\n2025-01-01 x\n    a  10 EUR\n"
        );
    }

    #[test]
    fn the_last_declaration_wins_and_commodity_beats_d() {
        // Both verified against hledger 1.99.
        let src =
            "commodity 1_000.00 EUR\ncommodity 1.000,00 EUR\n\n2025-01-01 x\n    a  1234,5 EUR\n";
        assert_eq!(
            format(src),
            "commodity 1_000.00 EUR\ncommodity 1.000,00 EUR\n\n2025-01-01 x\n    a  1.234,5 EUR\n"
        );
        let src = "D 1.000,00 EUR\ncommodity 1_000.00 EUR\n\n2025-01-01 x\n    a  1234.5 EUR\n";
        assert_eq!(
            format(src),
            "D 1.000,00 EUR\ncommodity 1_000.00 EUR\n\n2025-01-01 x\n    a  1_234.5 EUR\n"
        );
    }

    #[test]
    fn d_declares_a_style_when_no_commodity_directive_does() {
        let src = "D 1_000.00 EUR\n\n2025-01-01 x\n    a  10EUR\n    b\n";
        assert_eq!(
            format(src),
            "D 1_000.00 EUR\n\n2025-01-01 x\n    a  10 EUR\n    b\n"
        );
    }

    #[test]
    fn decimal_mark_governs_parsing_and_stays_in_the_output() {
        // The forced mark must survive rendering — converting `10,5` to the
        // style's `10.5` would re-read as 105 under `decimal-mark ,`. Only
        // the grouping is normalized.
        let src = "decimal-mark ,\ncommodity 1_000.00 EUR\n\n2025-01-01 x\n    a  1000,5 EUR\n";
        assert_eq!(
            format(src),
            "decimal-mark ,\ncommodity 1_000.00 EUR\n\n2025-01-01 x\n    a  1_000,5 EUR\n"
        );
    }

    #[test]
    fn restyled_widths_drive_the_alignment() {
        // The grouped form is wider than the typed form; alignment must be
        // computed on the restyled number.
        let src = "commodity 1_000.00 EUR\n\n2025-01-01 x\n    a  1234567 EUR\n    bb  -1 EUR\n";
        assert_eq!(
            format(src),
            "commodity 1_000.00 EUR\n\n2025-01-01 x\n    a   1_234_567 EUR\n    bb         -1 EUR\n"
        );
    }

    #[test]
    fn restyling_is_idempotent() {
        let src = "commodity 1.000,00 EUR\ndecimal-mark .\n\n2025-01-01 x\n    a  1234.56 EUR @ 1.1USD\n    b  10EUR\n";
        let once = format(src);
        assert_eq!(format(&once), once);
    }

    #[test]
    fn format_with_takes_the_callers_styles() {
        let ctx = scan_ctx(&["commodity 1_000.00 EUR"]);
        let src = "2025-01-01 x\n    a  10EUR\n";
        assert_eq!(format_with(src, &ctx), "2025-01-01 x\n    a  10 EUR\n");
        assert!(!is_formatted_with(src, &ctx));
        assert!(is_formatted(src)); // no styles in the text itself
    }

    #[test]
    fn format_is_idempotent() {
        let src = "; c\n2025/01/01 x\n  A:B\t\t1 USD  ;  n\n   C:D  -1 USD\n";
        let once = format(src);
        assert_eq!(format(&once), once);
    }

    #[test]
    fn is_formatted_reports_fixed_points() {
        assert!(is_formatted("2025-01-01 x\n    A:B  1 USD\n"));
        assert!(!is_formatted("2025-01-01 x\n  A:B  1 USD\n"));
        assert!(is_formatted(""));
    }

    // ---- --explicit ----

    fn explicit(s: &str) -> String {
        format_opts_scanned(
            s,
            Options {
                sort: false,
                explicit: true,
            },
        )
    }

    #[test]
    fn explicit_fills_in_the_one_inferred_amount() {
        assert_eq!(
            explicit("2026-01-01 x\n    a  10 EUR\n    b\n"),
            "2026-01-01 x\n    a   10 EUR\n    b  -10 EUR\n"
        );
    }

    #[test]
    fn explicit_infers_through_a_cost() {
        // hledger 1.99 `print -x` fills `b` with the cost side, -11 USD.
        assert_eq!(
            explicit("2026-01-01 x\n    a  10 EUR @ 1.1 USD\n    b\n"),
            "2026-01-01 x\n    a     10 EUR @ 1.1 USD\n    b  -11.0 USD\n"
        );
    }

    #[test]
    fn explicit_fills_a_posting_that_carries_only_an_assertion() {
        assert_eq!(
            explicit("2026-01-01 x\n    a  10 EUR\n    b   = -10 EUR\n"),
            "2026-01-01 x\n    a   10 EUR\n    b  -10 EUR = -10 EUR\n"
        );
    }

    #[test]
    fn explicit_writes_a_zero_remainder_rather_than_nothing() {
        assert_eq!(
            explicit("2026-01-01 x\n    a  10 EUR\n    b  -10 EUR\n    c\n"),
            "2026-01-01 x\n    a   10 EUR\n    b  -10 EUR\n    c    0 EUR\n"
        );
    }

    #[test]
    fn explicit_splits_a_multi_commodity_remainder_into_one_posting_each() {
        assert_eq!(
            explicit("2026-01-01 x\n    a  10 EUR\n    a  5 USD\n    b\n"),
            "2026-01-01 x\n    a   10 EUR\n    a    5 USD\n    b  -10 EUR\n    b   -5 USD\n"
        );
    }

    #[test]
    fn explicit_leaves_a_transaction_it_cannot_resolve_alone() {
        // Two amount-less postings: an hledger error, and not `fmt`'s to
        // guess at.
        let src = "2026-01-01 x\n    a  10 EUR\n    b\n    c\n";
        assert_eq!(explicit(src), src);
        // An unparseable amount makes the remainder unknown.
        let src = "2026-01-01 x\n    a  wat EUR\n    b\n";
        assert_eq!(explicit(src), src);
        // Nothing to infer from at all.
        let src = "2026-01-01 x\n    a\n";
        assert_eq!(explicit(src), src);
    }

    #[test]
    fn explicit_balances_the_three_posting_kinds_separately() {
        // `(v)` contributes to nothing; `[v]`/`[w]` balance among themselves.
        assert_eq!(
            explicit(
                "2026-01-01 x\n    a  10 EUR\n    (v)  3 EUR\n    b\n    [c]  4 EUR\n    [d]\n"
            ),
            "2026-01-01 x\n    a     10 EUR\n    (v)    3 EUR\n    b    -10 EUR\n    [c]    4 EUR\n    [d]   -4 EUR\n"
        );
    }

    #[test]
    fn explicit_pads_amounts_to_the_declared_decimal_places() {
        assert_eq!(
            explicit("commodity 1_000.00 EUR\n\n2026-01-01 x\n    a  1234 EUR @ 1.1 USD\n    b  -4 EUR\n"),
            "commodity 1_000.00 EUR\n\n2026-01-01 x\n    a  1_234.00 EUR @ 1.1 USD\n    b     -4.00 EUR\n"
        );
        // A floor, never a ceiling: more precision than declared survives.
        assert_eq!(
            explicit(
                "commodity 1_000.00 EUR\n\n2026-01-01 x\n    a  4.001 EUR\n    b  -4.001 EUR\n"
            ),
            "commodity 1_000.00 EUR\n\n2026-01-01 x\n    a   4.001 EUR\n    b  -4.001 EUR\n"
        );
    }

    #[test]
    fn explicit_leaves_undeclared_commodities_unpadded() {
        let src = "2026-01-01 x\n    a   1 USD\n    b  -1 USD\n";
        assert_eq!(explicit(src), src);
    }

    #[test]
    fn an_inferred_amount_of_an_undeclared_commodity_copies_its_siblings() {
        // Nothing declares these commodities, so the postings being balanced
        // against settle how the generated one is written. Every expectation
        // here is `hledger print -x`'s own output, verified against 1.99.
        for (src, want) in [
            // Symbol attached on the left, and the sign hledger puts between
            // symbol and digits.
            ("    a  $10\n    b\n", "    a   $10\n    b  $-10\n"),
            ("    a  $-10\n    b\n", "    a  $-10\n    b   $10\n"),
            // Attached on the right, and symbol-first with a space.
            ("    a  10€\n    b\n", "    a   10€\n    b  -10€\n"),
            ("    a  USD 10\n    b\n", "    a   USD 10\n    b  USD -10\n"),
            // Digit grouping the author typed is kept.
            (
                "    a  1,234.50 USD\n    b\n",
                "    a   1,234.50 USD\n    b  -1,234.50 USD\n",
            ),
            // A zero remainder still renders in the sibling's style.
            (
                "    a  $10\n    b  $-10\n    c\n",
                "    a   $10\n    b  $-10\n    c    $0\n",
            ),
            // The style may come from a cost tail rather than a face.
            (
                "    a  10 EUR @ $1.1\n    b\n",
                "    a      10 EUR @ $1.1\n    b  $-11.0\n",
            ),
        ] {
            let src = format!("2026-01-01 x\n{src}");
            assert_eq!(explicit(&src), format!("2026-01-01 x\n{want}"), "{src}");
        }
    }

    #[test]
    fn a_unitless_inferred_amount_stays_unitless() {
        // Invariant 3: never infer a commodity.
        assert_eq!(
            explicit("2026-01-01 x\n    a  10\n    b\n"),
            "2026-01-01 x\n    a   10\n    b  -10\n"
        );
    }

    #[test]
    fn a_declared_style_beats_the_siblings() {
        // The directive is the author's statement about the commodity; a
        // sibling amount is only what someone happened to type.
        assert_eq!(
            explicit("commodity 1_000.00 EUR\n\n2026-01-01 x\n    a  EUR1234\n    b\n"),
            "commodity 1_000.00 EUR\n\n2026-01-01 x\n    a   1_234.00 EUR\n    b  -1_234.00 EUR\n"
        );
    }

    #[test]
    fn explicit_never_touches_periodic_or_auto_transactions() {
        let src = "~ monthly\n    a  10 EUR\n    b\n\n= a\n    c  *2\n    d\n";
        assert_eq!(explicit(src), src);
    }

    #[test]
    fn explicit_keeps_the_comment_on_the_posting_it_was_written_for() {
        assert_eq!(
            explicit("2026-01-01 x\n    ; note\n    a  10 EUR\n    b  ; why\n"),
            "2026-01-01 x\n    ; note\n    a   10 EUR\n    b  -10 EUR  ; why\n"
        );
    }

    #[test]
    fn explicit_fills_through_every_cost_and_assertion_operator() {
        // hledger has four assertion operators and allows the amount to be
        // attached to any operator. Each of these forms used to make the
        // whole amount unparseable, so nothing was filled in.
        for (tail, want) in [
            ("= 10 USD", "-10 USD"),
            ("== 10 USD", "-10 USD"),
            ("=* 10 USD", "-10 USD"),
            ("==* 10 USD", "-10 USD"),
            ("=10USD", "-10 USD"),
            // The sibling is attached, so the fill is too — as hledger does.
            ("@1.1EUR", "-11.0EUR"),
            ("@@13EUR", "-13EUR"),
        ] {
            let src = format!("2026-01-01 x\n    a  10 USD {tail}\n    b\n");
            let out = explicit(&src);
            let filled = out.lines().last().unwrap_or_default().trim();
            assert_eq!(filled, format!("b  {want}"), "tail `{tail}`");
        }
    }

    #[test]
    fn explicit_sees_through_a_status_flag_on_a_virtual_posting() {
        // `* (v)` is an unbalanced virtual posting that contributes to
        // nothing. Counting its 3 EUR into the real balance filled `b` with
        // -13 EUR, which hledger refuses to read.
        assert_eq!(
            explicit("2026-01-01 x\n    a  10 EUR\n    * (v)  3 EUR\n    b\n"),
            "2026-01-01 x\n    a       10 EUR\n    * (v)    3 EUR\n    b      -10 EUR\n"
        );
        assert_eq!(
            explicit("2026-01-01 x\n    a  10 EUR\n    b\n    ! [c]  4 EUR\n    ! [d]\n"),
            "2026-01-01 x\n    a       10 EUR\n    b      -10 EUR\n    ! [c]    4 EUR\n    ! [d]   -4 EUR\n"
        );
    }

    #[test]
    fn explicit_is_idempotent() {
        let src = "commodity 1_000.00 EUR\n\n2026-01-01 x\n    a  1234EUR\n    a  5 USD\n    b\n\n2026-01-02 y\n    c  1 EUR\n    d\n";
        let once = explicit(src);
        assert_eq!(explicit(&once), once);
    }

    #[test]
    fn explicit_is_off_by_default() {
        let src = "commodity 1_000.00 EUR\n\n2026-01-01 x\n    a  1234 EUR\n    b\n";
        assert_eq!(
            format(src),
            "commodity 1_000.00 EUR\n\n2026-01-01 x\n    a  1_234 EUR\n    b\n"
        );
    }

    #[test]
    fn explicit_composes_with_sorting() {
        let opts = Options {
            sort: true,
            explicit: true,
        };
        assert_eq!(
            format_opts_scanned("2026-02-02 b\n    a  2 EUR\n    b\n\n2026-01-01 a\n    a  1 EUR\n    b\n", opts),
            "2026-01-01 a\n    a   1 EUR\n    b  -1 EUR\n\n2026-02-02 b\n    a   2 EUR\n    b  -2 EUR\n"
        );
    }

    #[test]
    fn explicit_check_is_the_same_fixed_point_question() {
        let ctx = AmountCtx::default();
        let opts = Options {
            sort: false,
            explicit: true,
        };
        let src = "2026-01-01 x\n    a  10 EUR\n    b\n";
        assert!(!is_formatted_opts(src, &ctx, opts));
        assert!(is_formatted(src)); // already formatted without --explicit
        assert!(is_formatted_opts(&explicit(src), &ctx, opts));
    }
}
