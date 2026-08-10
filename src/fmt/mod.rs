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
//! Amounts are aligned to a single file-wide column: the account field is
//! padded past the longest account name in the file, and every first-amount
//! number is right-aligned to one shared column across all transactions.

pub mod posting;
pub mod sort;

use crate::amount::{style_from_sample, AmountCtx};
use crate::lex::{directive_arg, is_blank, is_indented_non_blank, opens_txn, rstrip};
use posting::{account_of, parse_posting, render, restyle, Posting};

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
    unlines(&format_lines(&lines(s), ctx))
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
    unlines(&format_lines(&sort::sort_entries(&lines(s)), ctx))
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
    for l in ls {
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
    widths_of(&styled_lines(ls, ctx))
}

/// The alignment widths of an already-classified line list.
fn widths_of(styled: &[Option<Posting>]) -> (usize, usize) {
    let posts = styled.iter().flatten();
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
/// governs display — verified against hledger 1.99).
fn styled_lines(ls: &[&str], ctx: &AmountCtx) -> Vec<Option<Posting>> {
    let mut cur = ctx.clone();
    let mut in_txn = false;
    let mut out = Vec::with_capacity(ls.len());
    for l in ls {
        if in_txn && is_indented_non_blank(l) {
            out.push(Some(restyle(parse_posting(l), &cur)));
        } else {
            if let Some(arg) = directive_arg(l, "decimal-mark") {
                cur.decimal_mark = arg.chars().next().filter(|c| matches!(c, '.' | ','));
            }
            out.push(None);
            in_txn = opens_txn(l);
        }
    }
    out
}

/// Reflow a list of physical lines against the file-wide widths.
fn format_lines(ls: &[&str], ctx: &AmountCtx) -> Vec<String> {
    let styled = styled_lines(ls, ctx);
    let (acc_w, num_w) = widths_of(&styled);
    ls.iter()
        .zip(styled)
        .map(|(l, p)| p.map_or_else(|| format_other(l), |p| render(acc_w, num_w, &p)))
        .collect()
}

/// Format a non-posting line.
///
/// Blank lines collapse to empty (grouping preserved); transaction headers get
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
    fn blank_lines_collapse_but_are_not_removed() {
        assert_eq!(format("2025-01-01 x\n   \t \n\n"), "2025-01-01 x\n\n\n");
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
        let src = "commodity 1_000.00 EUR\ncommodity 1.000,00 EUR\n\n2025-01-01 x\n    a  1234,5 EUR\n";
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
}
