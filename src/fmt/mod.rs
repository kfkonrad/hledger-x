//! Format-preserving hledger journal formatter.
//!
//! Line-oriented: never builds a semantic model. Each physical line is
//! classified and only posting lines are reflowed; everything else passes
//! through (directives and comments verbatim, transaction headers with
//! trailing whitespace trimmed). Because directives pass through untouched,
//! price-only and include-only files are safe by construction.
//!
//! Amounts are aligned to a single file-wide column: the account field is
//! padded past the longest account name in the file, and every first-amount
//! number is right-aligned to one shared column across all transactions.

pub mod posting;
pub mod sort;

use crate::lex::{is_blank, is_indented_non_blank, opens_txn, rstrip};
use posting::{account_of, parse_posting, render, Posting};

/// Format a whole file's contents. Output always ends in a newline (empty
/// input yields empty output). Idempotent: `format(format(x)) == format(x)`.
#[must_use]
pub fn format(s: &str) -> String {
    unlines(&format_lines(&lines(s)))
}

/// Like [`format`], but also stably sorts transactions by date.
///
/// Sorting is directive-bounded: transactions reorder only within runs between
/// directives and standalone comment blocks, which act as barriers, so
/// positional directives keep their scope. Equal dates keep their source order.
#[must_use]
pub fn format_sorted(s: &str) -> String {
    unlines(&format_lines(&sort::sort_entries(&lines(s))))
}

/// Whether the input is already a fixed point of [`format`]. Drives `--check`.
#[must_use]
pub fn is_formatted(s: &str) -> bool {
    format(s) == s
}

/// Whether the input is already a fixed point of [`format_sorted`]. Drives
/// `--check --sort`.
#[must_use]
pub fn is_formatted_sorted(s: &str) -> bool {
    format_sorted(s) == s
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
    let posts: Vec<Posting> = posting_runs(ls)
        .into_iter()
        .flatten()
        .map(parse_posting)
        .collect();
    let acc_w = posts
        .iter()
        .filter_map(|p| account_of(p).map(|a| a.chars().count()))
        .max()
        .unwrap_or(0);
    let num_w = posts
        .iter()
        .filter_map(|p| match p {
            Posting::Amount { num, .. } => Some(num.chars().count()),
            Posting::Comment(_) | Posting::Bare(_, _) => None,
        })
        .max()
        .unwrap_or(0);
    (acc_w, num_w)
}

/// Reflow a list of physical lines against the file-wide widths.
fn format_lines(ls: &[&str]) -> Vec<String> {
    let (acc_w, num_w) = widths(ls);
    let mut out = Vec::with_capacity(ls.len());
    let mut in_txn = false;
    for l in ls {
        if in_txn && is_indented_non_blank(l) {
            out.push(render(acc_w, num_w, &parse_posting(l)));
        } else {
            out.push(format_other(l));
            in_txn = opens_txn(l);
        }
    }
    out
}

/// The maximal runs of posting lines in the file.
///
/// A posting line is an indented, non-blank line that follows a transaction
/// header. An indented run that follows anything else — an `account` or
/// `commodity` directive, say — is not postings.
fn posting_runs<'a>(ls: &[&'a str]) -> Vec<Vec<&'a str>> {
    let mut runs: Vec<Vec<&'a str>> = Vec::new();
    let mut current: Vec<&'a str> = Vec::new();
    let mut in_txn = false;
    for l in ls {
        if in_txn && is_indented_non_blank(l) {
            current.push(l);
        } else {
            if !current.is_empty() {
                runs.push(std::mem::take(&mut current));
            }
            in_txn = opens_txn(l);
        }
    }
    if !current.is_empty() {
        runs.push(current);
    }
    runs
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
