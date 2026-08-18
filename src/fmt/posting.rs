//! Posting parsing and rendering.

use crate::amount::{restyle_face_fields_with, restyle_tail_with, AmountCtx, Places};
use crate::lex::{
    is_number_like, is_rest_start, rstrip, split_account_amount, split_amount, split_comment,
};

/// Posting indent: exactly four spaces.
const INDENT: &str = "    ";

/// A parsed posting line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Posting {
    /// Standalone in-transaction comment line (indented `;` ...).
    Comment(String),
    /// Amount-less posting: account plus optional inline comment.
    Bare(String, Option<String>),
    /// account, number field, commodity, cost/assertion tokens, comment.
    Amount {
        account: String,
        num: String,
        commodity: String,
        rest: Vec<String>,
        comment: Option<String>,
    },
}

/// The account name of a posting that has one (comment lines have none).
#[must_use]
pub fn account_of(p: &Posting) -> Option<&str> {
    match p {
        Posting::Comment(_) => None,
        Posting::Bare(a, _) => Some(a),
        Posting::Amount { account, .. } => Some(account),
    }
}

/// Parse one posting line (leading indent and trailing whitespace ignored).
#[must_use]
pub fn parse_posting(raw: &str) -> Posting {
    let s = rstrip(raw.trim_start_matches(char::is_whitespace));
    if s.starts_with(';') {
        return Posting::Comment(s.to_owned());
    }
    let (body, comment) = split_comment(s);
    let comment = comment.map(ToOwned::to_owned);
    let (account, amt) = split_account_amount(body);
    if amt.is_empty() {
        return Posting::Bare(account.to_owned(), comment);
    }
    let toks: Vec<&str> = amt.split_whitespace().collect();
    let (num, commodity, rest) = split_amount(&toks);
    Posting::Amount {
        account: account.to_owned(),
        num,
        commodity,
        rest: rest.into_iter().map(ToOwned::to_owned).collect(),
        comment,
    }
}

/// Re-render a posting's amounts in their commodities' declared styles
/// (see `amount::restyle_face_fields` / `amount::restyle_tail`).
///
/// Amounts of undeclared commodities, unitless amounts, and anything that
/// does not parse stay exactly as written.
#[must_use]
pub fn restyle(p: Posting, ctx: &AmountCtx) -> Posting {
    restyle_with(p, ctx, Places::AsWritten)
}

/// [`restyle`] under an explicit precision policy.
///
/// [`Places::AsWritten`] is the only one `fmt` may use by default: padding an
/// amount already in the journal changes what `hledger print` emits.
/// [`Places::AtLeastDeclared`] is `fmt --explicit`, where that rewrite is
/// exactly what was asked for.
#[must_use]
pub fn restyle_with(p: Posting, ctx: &AmountCtx, places: Places) -> Posting {
    match p {
        Posting::Amount {
            account,
            num,
            commodity,
            rest,
            comment,
        } => {
            let rest_refs: Vec<&str> = rest.iter().map(String::as_str).collect();
            let rest = restyle_tail_with(&rest_refs, ctx, places);
            let (num, commodity) =
                restyle_face_fields_with(&num, &commodity, ctx, places).unwrap_or((num, commodity));
            Posting::Amount {
                account,
                num,
                commodity,
                rest,
                comment,
            }
        }
        other => other,
    }
}

/// Render a posting against the file-wide widths.
///
/// The account is padded to `acc_w`, then two spaces, then the number
/// right-aligned in `num_w`, then the commodity. The cost/assertion tail
/// follows with single-space separators and is never column-aligned.
#[must_use]
pub fn render(acc_w: usize, num_w: usize, p: &Posting) -> String {
    match p {
        Posting::Comment(c) => format!("{INDENT}{c}"),
        Posting::Bare(account, comment) => {
            format!("{INDENT}{account}{}", comment_part(comment.as_deref()))
        }
        Posting::Amount {
            account,
            num,
            commodity,
            rest,
            comment,
        } => {
            let amount_field = if num.is_empty() {
                format!("{}{}", spaces(num_w), phantom_commodity_pad(rest))
            } else {
                let commodity_part = if commodity.is_empty() {
                    String::new()
                } else {
                    format!(" {commodity}")
                };
                format!("{}{commodity_part}", pad_left(num, num_w))
            };
            let tail = if rest.is_empty() {
                String::new()
            } else {
                format!(" {}", rest.join(" "))
            };
            format!(
                "{INDENT}{}  {amount_field}{tail}{}",
                pad_right(account, acc_w),
                comment_part(comment.as_deref())
            )
        }
    }
}

/// Blank padding standing in for the commodity of an omitted amount, taken
/// from the commodity of the cost/assertion tail (empty if it has none).
fn phantom_commodity_pad(rest: &[String]) -> String {
    tail_commodity(rest).map_or_else(String::new, |c| format!(" {}", spaces(c.chars().count())))
}

/// The commodity of a cost/assertion tail: the first token that is neither an
/// operator (`@`, `@@`, `=`, `==`) nor a number.
fn tail_commodity(rest: &[String]) -> Option<&String> {
    rest.iter()
        .find(|t| !is_rest_start(t) && !is_number_like(t))
}

fn comment_part(comment: Option<&str>) -> String {
    comment.map_or_else(String::new, |c| format!("  {c}"))
}

fn spaces(n: usize) -> String {
    " ".repeat(n)
}

/// Pad on the right to `n` characters, never truncating.
fn pad_right(s: &str, n: usize) -> String {
    format!("{s}{}", spaces(n.saturating_sub(s.chars().count())))
}

/// Pad on the left to `n` characters, never truncating.
fn pad_left(s: &str, n: usize) -> String {
    format!("{}{s}", spaces(n.saturating_sub(s.chars().count())))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn amount(
        account: &str,
        num: &str,
        commodity: &str,
        rest: &[&str],
        comment: Option<&str>,
    ) -> Posting {
        Posting::Amount {
            account: account.into(),
            num: num.into(),
            commodity: commodity.into(),
            rest: rest.iter().map(|s| (*s).to_owned()).collect(),
            comment: comment.map(Into::into),
        }
    }

    #[test]
    fn indented_semicolon_line_is_a_comment_posting() {
        assert_eq!(
            parse_posting("      ; a note  "),
            Posting::Comment("; a note".into())
        );
    }

    #[test]
    fn amount_less_posting_is_bare() {
        assert_eq!(
            parse_posting("    Expenses:Misc"),
            Posting::Bare("Expenses:Misc".into(), None)
        );
        assert_eq!(
            parse_posting("    Expenses:Misc  ; why"),
            Posting::Bare("Expenses:Misc".into(), Some("; why".into()))
        );
        // A single space inside the account name is not a separator.
        assert_eq!(
            parse_posting("    Assets:Bank Account"),
            Posting::Bare("Assets:Bank Account".into(), None)
        );
    }

    #[test]
    fn amount_posting_splits_number_commodity_and_tail() {
        assert_eq!(
            parse_posting("    Assets:Bank  100 USD"),
            amount("Assets:Bank", "100", "USD", &[], None)
        );
        assert_eq!(
            parse_posting("    Expenses:Groceries:Weekly   100.00 EUR @ 1.20 USD"),
            amount(
                "Expenses:Groceries:Weekly",
                "100.00",
                "EUR",
                &["@", "1.20", "USD"],
                None
            )
        );
        assert_eq!(
            parse_posting("    Assets:Bank Account   100.00 USD = 100.00 USD  ; note here"),
            amount(
                "Assets:Bank Account",
                "100.00",
                "USD",
                &["=", "100.00", "USD"],
                Some("; note here")
            )
        );
        // Assertion with no amount in front of it.
        assert_eq!(
            parse_posting("    Assets:Cash   = 0 RSD"),
            amount("Assets:Cash", "", "", &["=", "0", "RSD"], None)
        );
    }

    #[test]
    fn render_pads_account_then_right_aligns_the_number() {
        let p = amount("A:B", "10.00", "USD", &[], None);
        assert_eq!(render(5, 7, &p), "    A:B      10.00 USD");
    }

    #[test]
    fn render_never_aligns_the_cost_tail() {
        let p = amount("A:B", "100.00", "EUR", &["@", "1.20", "USD"], None);
        assert_eq!(render(3, 6, &p), "    A:B  100.00 EUR @ 1.20 USD");
    }

    #[test]
    fn render_reserves_the_columns_for_a_tail_only_posting() {
        // Number column blanked, plus a blank stand-in for the tail's
        // commodity, so `= 0 RSD` lines up as if a zero amount preceded it.
        let p = amount("Assets:Cash", "", "", &["=", "0", "RSD"], None);
        assert_eq!(render(11, 5, &p), "    Assets:Cash            = 0 RSD");
        // A tail with no commodity at all gets no phantom pad.
        let p = amount("Assets:Cash", "", "", &["=", "0"], None);
        assert_eq!(render(11, 5, &p), "    Assets:Cash        = 0");
    }

    #[test]
    fn render_bare_and_comment_postings() {
        assert_eq!(
            render(20, 8, &Posting::Bare("Expenses:Misc".into(), None)),
            "    Expenses:Misc"
        );
        assert_eq!(
            render(
                20,
                8,
                &Posting::Bare("Expenses:Misc".into(), Some("; why".into()))
            ),
            "    Expenses:Misc  ; why"
        );
        assert_eq!(
            render(20, 8, &Posting::Comment("; note".into())),
            "    ; note"
        );
    }

    #[test]
    fn render_does_not_truncate_when_wider_than_the_column() {
        let p = amount("Assets:Very:Long", "12345.67", "USD", &[], None);
        assert_eq!(render(3, 2, &p), "    Assets:Very:Long  12345.67 USD");
    }

    #[test]
    fn render_uses_character_widths_not_bytes() {
        // "Ausgaben:Bücher" is 15 chars, 16 bytes; padding to 17 must add 2.
        let p = amount("Ausgaben:Bücher", "1", "EUR", &[], None);
        assert_eq!(render(17, 1, &p), "    Ausgaben:Bücher    1 EUR");
    }

    #[test]
    fn inline_comments_are_normalized_to_two_spaces() {
        let p = parse_posting("    A:B  1 USD      ; spaced out");
        assert_eq!(render(3, 1, &p), "    A:B  1 USD  ; spaced out");
    }
}
