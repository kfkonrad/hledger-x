//! Shared lexical layer.
//!
//! Every function here operates on a single line (or its tokens) and knows
//! nothing about what a line *means*. `fmt` is built entirely out of these;
//! `add`'s parser will reuse them so the two agree on where an account ends
//! and an amount begins.

/// All characters are whitespace (an empty line is blank).
#[must_use]
pub fn is_blank(s: &str) -> bool {
    s.chars().all(char::is_whitespace)
}

/// A line-start comment (column 0, not indented): `;`, `#` or `*`.
#[must_use]
pub fn is_comment(s: &str) -> bool {
    matches!(s.chars().next(), Some(';' | '#' | '*'))
}

/// Whether a line opens a `comment` block, whose contents are opaque: not
/// journal syntax at all, and never read or rewritten.
///
/// Verified against hledger 1.99: the keyword must stand alone at column 0.
/// `comment some words` is a parse error, an indented `comment` is not an
/// opener, and the keyword is lowercase only. Trailing whitespace is fine.
#[must_use]
pub fn opens_comment_block(s: &str) -> bool {
    rstrip(s) == "comment"
}

/// Whether a line closes a `comment` block. Same shape as the opener, and
/// just as strict: an *indented* `end comment` does not close the block —
/// verified, hledger swallows the rest of the file.
#[must_use]
pub fn closes_comment_block(s: &str) -> bool {
    rstrip(s) == "end comment"
}

/// How many lines a `comment` block occupies *after* its opener: its
/// contents, plus the `end comment` line if it has one.
///
/// An unterminated block runs to the end of the file, which hledger accepts
/// without complaint (verified).
#[must_use]
pub fn comment_block_len(after_opener: &[&str]) -> usize {
    let inner = after_opener
        .iter()
        .take_while(|l| !closes_comment_block(l))
        .count();
    after_opener.len().min(inner.saturating_add(1))
}

/// Whether a line opens a transaction.
///
/// A leading ASCII digit is the *only* test, which is what excludes periodic
/// (`~`) and auto (`=`) transactions.
#[must_use]
pub fn opens_txn(s: &str) -> bool {
    s.chars().next().is_some_and(|c| c.is_ascii_digit())
}

/// Whether a line opens a periodic (`~`) or auto (`=`) transaction *rule*.
///
/// A rule is not a transaction — hledger only expands one under `--forecast`
/// or `--auto` — so nothing in it is balanced, filled in or restyled. Its
/// postings are laid out and nothing more, which is why this is a separate
/// question from [`opens_txn`].
#[must_use]
pub fn opens_rule(s: &str) -> bool {
    matches!(s.chars().next(), Some('~' | '='))
}

/// An indented, non-blank line: a posting or an in-transaction comment.
#[must_use]
pub fn is_indented_non_blank(s: &str) -> bool {
    s.chars().next().is_some_and(char::is_whitespace) && !is_blank(s)
}

/// Drop trailing whitespace.
#[must_use]
pub fn rstrip(s: &str) -> &str {
    s.trim_end_matches(char::is_whitespace)
}

/// Split off a trailing inline comment beginning at the first `;`.
///
/// Accounts and amounts never contain `;`, so the first one is the boundary.
#[must_use]
pub fn split_comment(s: &str) -> (&str, Option<&str>) {
    s.find(';').map_or_else(
        || (rstrip(s), None),
        |i| (rstrip(s.get(..i).unwrap_or_default()), s.get(i..)),
    )
}

/// Split a posting body into account and amount.
///
/// The separator is the first run of two or more whitespace characters. A
/// *single* space or tab is not a separator — account names may contain single
/// spaces. No separator means the whole body is the account.
#[must_use]
pub fn split_account_amount(s: &str) -> (&str, &str) {
    let is_sep = |c: char| c == ' ' || c == '\t';
    let mut prev: Option<(usize, char)> = None;
    for (i, c) in s.char_indices() {
        if let Some((pi, pc)) = prev {
            if is_sep(pc) && is_sep(c) {
                let account = s.get(..pi).unwrap_or_default();
                let amount = s.get(pi..).unwrap_or_default().trim_start_matches(is_sep);
                return (account, amount);
            }
        }
        prev = Some((i, c));
    }
    (s, "")
}

/// A token that begins the cost (`@`, `@@`) or assertion (`=`, `==`) tail.
#[must_use]
pub fn is_rest_start(t: &str) -> bool {
    t.starts_with('@') || t.starts_with('=')
}

/// Whether a token reads as a bare, right-alignable number.
///
/// Deliberately rejects commodity-on-left tokens like `$100` and bare
/// commodities like `AMD`.
#[must_use]
pub fn is_number_like(t: &str) -> bool {
    t.chars()
        .next()
        .is_some_and(|c| "+-.0123456789".contains(c))
        && t.chars().any(|c| c.is_ascii_digit())
}

/// The argument of a top-level `keyword` directive line, comment and 2+
/// space-separated annotation stripped. `None` when the line is not that
/// directive.
#[must_use]
pub fn directive_arg<'a>(line: &'a str, keyword: &str) -> Option<&'a str> {
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

/// Whether a token continues a space-grouped number: it begins with a digit.
///
/// That is the whole test, and it is safe because an unquoted hledger
/// commodity symbol cannot contain a digit at all, let alone start with one —
/// so a token after a number that starts with a digit can only be the rest of
/// that number. The trailing part is left unconstrained on purpose: the last
/// group may carry an attached symbol, as in `1 234 567.00£`. Anything that
/// then fails to parse as a number simply does not restyle, which is the same
/// safe outcome as any other unreadable amount.
#[must_use]
pub fn is_digit_group(t: &str) -> bool {
    t.starts_with(|c: char| c.is_ascii_digit())
}

/// Split amount tokens into (number field, commodity, remaining tokens).
///
/// The remainder is the cost/assertion tail, kept verbatim and never aligned.
///
/// A space between two digit groups is part of the number, not the boundary
/// with the commodity: `1 234.00 USD` is one amount, the way hledger reads and
/// prints it under `commodity 1 000.00 USD`.
#[must_use]
pub fn split_amount<'a>(toks: &[&'a str]) -> (String, String, Vec<&'a str>) {
    let cut = toks
        .iter()
        .position(|t| is_rest_start(t))
        .unwrap_or(toks.len());
    let (amt, rest) = toks.split_at(cut);
    if let Some((first, tail)) = amt.split_first() {
        if is_number_like(first) {
            let groups = tail.iter().take_while(|t| is_digit_group(t)).count();
            let (extra, after) = tail.split_at(groups);
            let mut num = (*first).to_owned();
            for g in extra {
                num.push(' ');
                num.push_str(g);
            }
            let (commodity, more) = after
                .split_first()
                .map_or_else(|| (String::new(), &[][..]), |(c, m)| ((*c).to_owned(), m));
            let mut tail_toks: Vec<&str> = more.to_vec();
            tail_toks.extend_from_slice(rest);
            return (num, commodity, tail_toks);
        }
    }
    match amt {
        [] => (String::new(), String::new(), rest.to_vec()),
        [t] => ((*t).to_string(), String::new(), rest.to_vec()),
        [t0, t1, more @ ..] => {
            if is_number_like(t1) && more.is_empty() {
                (format!("{t0} {t1}"), String::new(), rest.to_vec())
            } else {
                (amt.join(" "), String::new(), rest.to_vec())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blankness() {
        assert!(is_blank(""));
        assert!(is_blank("   \t "));
        assert!(!is_blank("  x"));
    }

    #[test]
    fn comments_are_column_zero_only() {
        assert!(is_comment("; hi"));
        assert!(is_comment("# hi"));
        assert!(is_comment("* hi"));
        assert!(!is_comment("    ; indented"));
        assert!(!is_comment(""));
        assert!(!is_comment("account Assets:Bank"));
    }

    #[test]
    fn a_space_between_digit_groups_belongs_to_the_number() {
        // `1 234.00 USD` is one amount, not the number 1 with a commodity
        // `234.00`. Safe because an unquoted commodity symbol cannot contain
        // a digit, so nothing else can follow a number starting with one.
        assert_eq!(
            split_amount(&["1", "234.00", "USD"]),
            ("1 234.00".to_owned(), "USD".to_owned(), vec![])
        );
        assert_eq!(
            split_amount(&["-1", "234", "567.00", "USD"]),
            ("-1 234 567.00".to_owned(), "USD".to_owned(), vec![])
        );
        // The last group may carry an attached symbol.
        assert_eq!(
            split_amount(&["1", "234.00£"]),
            ("1 234.00£".to_owned(), String::new(), vec![])
        );
        // A unitless grouped number.
        assert_eq!(
            split_amount(&["1", "000"]),
            ("1 000".to_owned(), String::new(), vec![])
        );
        // The tail is still cut first, and grouping applies inside it too.
        assert_eq!(
            split_amount(&["1", "234", "USD", "@", "1", "500", "EUR"]),
            (
                "1 234".to_owned(),
                "USD".to_owned(),
                vec!["@", "1", "500", "EUR"]
            )
        );
    }

    #[test]
    fn ordinary_amounts_are_unaffected_by_group_spaces() {
        assert_eq!(
            split_amount(&["100", "USD"]),
            ("100".to_owned(), "USD".to_owned(), vec![])
        );
        // Symbol-first: the first token is not a number, so nothing is joined.
        assert_eq!(
            split_amount(&["USD", "100"]),
            ("USD 100".to_owned(), String::new(), vec![])
        );
        assert_eq!(
            split_amount(&["$100"]),
            ("$100".to_owned(), String::new(), vec![])
        );
        assert_eq!(
            split_amount(&["100", "USD", "extra"]),
            ("100".to_owned(), "USD".to_owned(), vec!["extra"])
        );
    }

    #[test]
    fn rules_are_recognized_but_are_not_transactions() {
        assert!(opens_rule("~ monthly  rent"));
        assert!(opens_rule("= expenses:food"));
        assert!(!opens_rule("2026-01-01 x"));
        assert!(!opens_rule("    indented"));
        assert!(!opens_rule("commodity EUR"));
        // And a rule still does not open a transaction.
        assert!(!opens_txn("~ monthly  rent"));
        assert!(!opens_txn("= expenses:food"));
    }

    #[test]
    fn only_a_leading_digit_opens_a_transaction() {
        assert!(opens_txn("2025-01-01 payee"));
        assert!(opens_txn("2025/01/01"));
        assert!(!opens_txn(" 2025-01-01 indented"));
        assert!(!opens_txn("~ monthly"));
        assert!(!opens_txn("= expenses:food"));
        assert!(!opens_txn("P 2025-01-01 USD 1 EUR"));
        assert!(!opens_txn(""));
    }

    #[test]
    fn comment_block_delimiters_stand_alone_at_column_zero() {
        assert!(opens_comment_block("comment"));
        assert!(opens_comment_block("comment \t"));
        assert!(!opens_comment_block("  comment"));
        assert!(!opens_comment_block("comment some words"));
        assert!(!opens_comment_block("Comment"));
        assert!(closes_comment_block("end comment  "));
        assert!(!closes_comment_block("  end comment"));
        assert!(!closes_comment_block("end comment now"));
    }

    #[test]
    fn a_comment_block_runs_to_its_terminator_or_to_eof() {
        // Contents plus the `end comment` line.
        assert_eq!(comment_block_len(&["a", "b", "end comment", "c"]), 3);
        // Unterminated: everything that is left.
        assert_eq!(comment_block_len(&["a", "b"]), 2);
        assert_eq!(comment_block_len(&[]), 0);
        // An indented terminator does not terminate.
        assert_eq!(comment_block_len(&["a", "  end comment", "b"]), 3);
    }

    #[test]
    fn indented_non_blank() {
        assert!(is_indented_non_blank("    Assets:Bank"));
        assert!(is_indented_non_blank("\tAssets:Bank"));
        assert!(!is_indented_non_blank("Assets:Bank"));
        assert!(!is_indented_non_blank("    "));
        assert!(!is_indented_non_blank(""));
    }

    #[test]
    fn rstrip_drops_only_trailing_whitespace() {
        assert_eq!(rstrip("a b  \t "), "a b");
        assert_eq!(rstrip("  a"), "  a");
        assert_eq!(rstrip("   "), "");
        assert_eq!(rstrip(""), "");
    }

    #[test]
    fn comment_split_is_at_the_first_semicolon() {
        assert_eq!(split_comment("a  ; one ; two"), ("a", Some("; one ; two")));
        assert_eq!(split_comment("a b"), ("a b", None));
        assert_eq!(split_comment("; whole line"), ("", Some("; whole line")));
    }

    #[test]
    fn account_amount_separator_is_two_or_more_spaces() {
        assert_eq!(
            split_account_amount("Assets:Bank  100 USD"),
            ("Assets:Bank", "100 USD")
        );
        assert_eq!(
            split_account_amount("Assets:Bank Account   100.00 USD"),
            ("Assets:Bank Account", "100.00 USD")
        );
        // A single space is not a separator.
        assert_eq!(
            split_account_amount("Assets:Bank Account"),
            ("Assets:Bank Account", "")
        );
        // Tabs count, and mixed runs count.
        assert_eq!(
            split_account_amount("Assets:Checking\t\t100.00 USD"),
            ("Assets:Checking", "100.00 USD")
        );
        assert_eq!(
            split_account_amount("Assets:Checking \t100.00 USD"),
            ("Assets:Checking", "100.00 USD")
        );
        // A single tab is not a separator either.
        assert_eq!(
            split_account_amount("Assets:Checking\t100.00 USD"),
            ("Assets:Checking\t100.00 USD", "")
        );
    }

    #[test]
    fn rest_start_tokens() {
        assert!(is_rest_start("@"));
        assert!(is_rest_start("@@"));
        assert!(is_rest_start("="));
        assert!(is_rest_start("=="));
        assert!(!is_rest_start("100"));
        assert!(!is_rest_start(""));
    }

    #[test]
    fn number_like_needs_a_leading_sign_or_digit_and_a_digit() {
        assert!(is_number_like("100"));
        assert!(is_number_like("-7485978.18"));
        assert!(is_number_like("+1"));
        assert!(is_number_like(".5"));
        assert!(!is_number_like("$100"));
        assert!(!is_number_like("AMD"));
        assert!(!is_number_like("-"));
        assert!(!is_number_like(""));
    }

    #[test]
    fn amount_splitting() {
        fn split(s: &str) -> (String, String, Vec<&str>) {
            let toks: Vec<&str> = s.split_whitespace().collect();
            split_amount(&toks)
        }

        // Empty.
        assert_eq!(split(""), (String::new(), String::new(), vec![]));
        // Bare number, no commodity.
        assert_eq!(split("100"), ("100".into(), String::new(), vec![]));
        // Number then commodity.
        assert_eq!(split("100.00 USD"), ("100.00".into(), "USD".into(), vec![]));
        // Number, commodity, cost tail.
        assert_eq!(
            split("100.00 EUR @ 1.20 USD"),
            ("100.00".into(), "EUR".into(), vec!["@", "1.20", "USD"])
        );
        // Assertion tail with no amount at all.
        assert_eq!(
            split("= 0 RSD"),
            (String::new(), String::new(), vec!["=", "0", "RSD"])
        );
        // Commodity on the left of the number: joined, no commodity column.
        assert_eq!(split("USD 100"), ("USD 100".into(), String::new(), vec![]));
        // Three non-number-leading tokens: all joined into the number field.
        assert_eq!(split("a b c"), ("a b c".into(), String::new(), vec![]));
        // Number first wins even with more tokens following.
        assert_eq!(
            split("100 USD extra"),
            ("100".into(), "USD".into(), vec!["extra"])
        );
    }
}
