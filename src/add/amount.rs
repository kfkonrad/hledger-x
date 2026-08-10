//! Exact-decimal amount parsing and rendering.
//!
//! Numbers are parsed in exactly one place in the whole tool: the transaction
//! currently being entered, to compute the running imbalance and the
//! balancing amount. Historical amounts are never interpreted. On any parse
//! failure the imbalance is *unknown*, never wrong.
//!
//! Decimal-mark rules, verified against hledger 1.99:
//!
//! - both `.` and `,` present → the last one is the decimal mark
//! - a single mark occurring once → it is the decimal mark (`1,234` is
//!   1.234, not 1234)
//! - a single mark occurring more than once → digit grouping
//! - `_` is always a digit group mark
//! - an explicit `decimal-mark` directive, or the commodity's declared style,
//!   overrides the guess
//!
//! Cost tails contribute at cost (also verified): `N C @ P C2` adds `N*P C2`,
//! `N C @@ T C2` adds `sign(N)*T C2`.

use std::collections::HashMap;

use rust_decimal::Decimal;

use crate::lex::{is_number_like, split_amount};

/// Which side of the number a commodity symbol sits on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// `$100`, `USD 100`
    Left,
    /// `100 EUR`, `100€`
    Right,
}

/// A commodity's display style, learned from a `commodity` directive sample
/// such as `1_000.00 EUR`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayStyle {
    /// Decimal mark.
    pub decimal_mark: char,
    /// Digit group separator, if grouping is used.
    pub group_sep: Option<char>,
    /// Group sizes from the right; the last repeats. Typically `[3]`.
    pub group_sizes: Vec<usize>,
    /// Number of decimal places in the sample.
    pub decimal_places: u32,
    /// Symbol placement.
    pub symbol_side: Side,
    /// Whether a space separates symbol and number.
    pub symbol_space: bool,
}

impl Default for DisplayStyle {
    fn default() -> Self {
        Self {
            decimal_mark: '.',
            group_sep: None,
            group_sizes: vec![3],
            decimal_places: 2,
            symbol_side: Side::Right,
            symbol_space: true,
        }
    }
}

/// Everything amount parsing needs to know about its surroundings: the
/// `decimal-mark` in effect at the insertion point, and declared commodity
/// styles.
#[derive(Debug, Clone, Default)]
pub struct AmountCtx {
    /// `decimal-mark` directive in effect, if any. Takes precedence.
    pub decimal_mark: Option<char>,
    /// Declared display styles by commodity symbol.
    pub styles: HashMap<String, DisplayStyle>,
}

/// A parsed entered amount: exact value, commodity (may be empty — a
/// unitless amount is valid), and its cost-converted contribution to the
/// balance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedAmount {
    /// The face value.
    pub value: Decimal,
    /// The face commodity (`""` for unitless).
    pub commodity: String,
    /// What this posting contributes to the transaction balance: the face
    /// value, or the cost when an `@`/`@@` tail converts it.
    pub contributes: (Decimal, String),
}

/// Parse a commodity directive sample (`1_000.00 EUR`, `$1000.00`,
/// `1.000,000 X`) into its symbol and style. `None` when the sample is a
/// bare symbol with no number.
#[must_use]
pub fn style_from_sample(sample: &str) -> Option<(String, DisplayStyle)> {
    let toks: Vec<&str> = sample.split_whitespace().collect();
    match toks.as_slice() {
        [t] => {
            // Symbol attached to the number: $1000.00 or 1000.00€.
            let num_end = t.rfind(|c: char| c.is_ascii_digit())?;
            let num_start = t.find(|c: char| c.is_ascii_digit())?;
            let head: String = t.chars().take_while(|c| !c.is_ascii_digit()).collect();
            let digits_on = t.get(num_start..=num_end)?;
            let tail: String = t
                .get(num_end.saturating_add(1)..)
                .unwrap_or("")
                .to_owned();
            // The number part may carry marks *inside* digits_on only; a
            // leading sign belongs to the number, not the symbol.
            let symbol_left = head.trim_start_matches(['+', '-']).to_owned();
            if !symbol_left.is_empty() {
                let style = style_of_number(digits_on, Side::Left, false, &symbol_left)?;
                return Some((symbol_left, style));
            }
            if !tail.is_empty() {
                let style = style_of_number(digits_on, Side::Right, false, &tail)?;
                return Some((tail, style));
            }
            None
        }
        [a, b] => {
            if is_number_like(a) {
                Some(((*b).to_owned(), style_of_number(a, Side::Right, true, b)?))
            } else if is_number_like(b) {
                Some(((*a).to_owned(), style_of_number(b, Side::Left, true, a)?))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Derive a style from a sample number like `1_000.00` or `1.234,56`.
fn style_of_number(num: &str, side: Side, spaced: bool, _symbol: &str) -> Option<DisplayStyle> {
    let (int_part, frac_part, decimal_mark, group_sep) = split_number(num, None)?;
    let group_sizes = group_sep.map_or_else(Vec::new, |sep| {
        let mut sizes: Vec<usize> = int_part
            .split(sep)
            .skip(1)
            .map(|g| g.chars().count())
            .collect();
        sizes.reverse(); // from the right
        if sizes.is_empty() {
            vec![3]
        } else {
            sizes
        }
    });
    Some(DisplayStyle {
        decimal_mark: decimal_mark.unwrap_or('.'),
        group_sep,
        group_sizes: if group_sizes.is_empty() {
            vec![3]
        } else {
            group_sizes
        },
        decimal_places: u32::try_from(frac_part.chars().count()).ok()?,
        symbol_side: side,
        symbol_space: spaced,
    })
}

/// Split a raw number into integer part (group separators intact), fraction
/// digits, the decimal mark found, and the group separator found. Applies
/// the verified auto-detection rules; `forced_mark` (from a directive or
/// style) overrides them.
///
/// Returns `None` when the text cannot be a number.
fn split_number(
    raw: &str,
    forced_mark: Option<char>,
) -> Option<(String, String, Option<char>, Option<char>)> {
    let num = raw.trim();
    if num.is_empty() || !num.chars().any(|c| c.is_ascii_digit()) {
        return None;
    }
    let marks: Vec<(usize, char)> = num
        .char_indices()
        .filter(|(_, c)| *c == '.' || *c == ',')
        .collect();
    let decimal_mark: Option<char> = if let Some(forced) = forced_mark {
        // The forced mark is decimal; the other char, if present, groups.
        marks.iter().rev().find(|(_, c)| *c == forced).map(|_| forced)
    } else {
        match marks.as_slice() {
            [] => None,
            [(_, only)] => Some(*only), // a single occurrence is the decimal mark
            more => {
                let (_, last) = more.last().copied()?;
                let distinct = more.iter().any(|(_, c)| *c != last);
                let last_count = more.iter().filter(|(_, c)| *c == last).count();
                if distinct || last_count == 1 {
                    // Mixed marks: the last one is decimal. (With mixed marks
                    // the last mark appears once — repeated final marks like
                    // 1.2.3 fall to the grouping arm.)
                    if last_count == 1 {
                        Some(last)
                    } else {
                        return None; // e.g. `1,2.3.4` — not a number
                    }
                } else {
                    None // one mark repeated: pure grouping
                }
            }
        }
    };

    // Everything after the last decimal-mark occurrence is the fraction.
    let (int_raw, frac_raw) = decimal_mark.map_or((num, ""), |mark| {
        num.rfind(mark).map_or((num, ""), |i| {
            (
                num.get(..i).unwrap_or(num),
                num.get(i.saturating_add(1)..).unwrap_or(""),
            )
        })
    });

    // Group separators in the integer part: the non-decimal mark chars + `_`.
    let mut group_sep: Option<char> = None;
    for c in int_raw.chars() {
        if c == '_' || ((c == '.' || c == ',') && decimal_mark != Some(c)) {
            if group_sep.is_some_and(|g| g != c) {
                return None; // two different group separators — not a number
            }
            group_sep = Some(c);
        }
    }
    if frac_raw.contains(['.', ',', '_']) {
        return None;
    }
    Some((
        int_raw.to_owned(),
        frac_raw.to_owned(),
        decimal_mark,
        group_sep,
    ))
}

/// Parse a number with the marks in effect. `forced_mark` comes from a
/// `decimal-mark` directive or the commodity's declared style.
#[must_use]
pub fn parse_number(raw: &str, forced_mark: Option<char>) -> Option<Decimal> {
    let (int_part, frac_part, _mark, group_sep) = split_number(raw, forced_mark)?;
    let mut normalized = String::new();
    for c in int_part.chars() {
        if c.is_ascii_digit() || c == '+' || c == '-' {
            normalized.push(c);
        } else if Some(c) == group_sep {
            // dropped
        } else {
            return None;
        }
    }
    if !frac_part.is_empty() {
        normalized.push('.');
        for c in frac_part.chars() {
            if !c.is_ascii_digit() {
                return None;
            }
            normalized.push(c);
        }
    }
    normalized.parse().ok()
}

/// Parse one typed amount field: `23.45 EUR`, `-5 EUR @ 1.2 USD`, `$100`,
/// `1_000`, or bare `12`. Empty input is `None`; so is anything
/// unintelligible.
#[must_use]
pub fn parse_amount(text: &str, ctx: &AmountCtx) -> Option<ParsedAmount> {
    let toks: Vec<&str> = text.split_whitespace().collect();
    if toks.is_empty() {
        return None;
    }
    let (num_field, commodity, rest) = split_amount(&toks);
    let (value, commodity) = parse_face(&num_field, &commodity, ctx)?;
    let contributes = cost_contribution(value, &commodity, &rest, ctx)?;
    Some(ParsedAmount {
        value,
        commodity,
        contributes,
    })
}

/// Parse the face value: the number token (possibly with an attached symbol)
/// plus the separate commodity token, if any.
fn parse_face(num_field: &str, commodity: &str, ctx: &AmountCtx) -> Option<(Decimal, String)> {
    // `USD 100` style: number field contains the joined pair.
    let (num_tok, commodity) = if commodity.is_empty() {
        let mut parts = num_field.split_whitespace();
        let a = parts.next()?;
        parts.next().map_or_else(
            || (a, String::new()),
            |b| {
                if is_number_like(b) {
                    (b, a.to_owned()) // symbol-first: USD 100
                } else {
                    (a, b.to_owned())
                }
            },
        )
    } else {
        (num_field, commodity.to_owned())
    };

    // An attached symbol: $100, 100€, -$5.
    let (num_tok, commodity) = if commodity.is_empty() {
        detach_symbol(num_tok)
    } else {
        (num_tok.to_owned(), commodity)
    };

    let mark = ctx
        .decimal_mark
        .or_else(|| ctx.styles.get(&commodity).map(|s| s.decimal_mark));
    let value = parse_number(&num_tok, mark)?;
    Some((value, commodity))
}

/// Split an attached symbol off a token: `$100` → (`100`, `$`), `100€` →
/// (`100`, `€`), `-$5` → (`-5`, `$`). No symbol → unchanged.
fn detach_symbol(tok: &str) -> (String, String) {
    let sign: String = tok.chars().take_while(|c| *c == '-' || *c == '+').collect();
    let body: String = tok.chars().skip(sign.chars().count()).collect();
    let is_numchar = |c: char| c.is_ascii_digit() || c == '.' || c == ',' || c == '_';
    let leading: String = body.chars().take_while(|c| !is_numchar(*c)).collect();
    if !leading.is_empty() {
        let rest: String = body.chars().skip(leading.chars().count()).collect();
        return (format!("{sign}{rest}"), leading);
    }
    let trailing: String = body
        .chars()
        .rev()
        .take_while(|c| !is_numchar(*c))
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    if !trailing.is_empty() {
        let keep = body.chars().count().saturating_sub(trailing.chars().count());
        let rest: String = body.chars().take(keep).collect();
        return (format!("{sign}{rest}"), trailing);
    }
    (tok.to_owned(), String::new())
}

/// What a posting contributes to the balance, converting through an `@`/`@@`
/// cost tail when present (verified against hledger: `@` multiplies, `@@` is
/// a total taking the amount's sign). A balance assertion tail (`=`) does not
/// affect the contribution.
fn cost_contribution(
    value: Decimal,
    commodity: &str,
    rest: &[&str],
    ctx: &AmountCtx,
) -> Option<(Decimal, String)> {
    let mut it = rest.iter();
    let Some(op) = it.next() else {
        return Some((value, commodity.to_owned()));
    };
    if *op == "=" || *op == "==" {
        return Some((value, commodity.to_owned()));
    }
    if *op != "@" && *op != "@@" {
        return None;
    }
    // The price: a number token and optional commodity, up to a further tail.
    let price_toks: Vec<&str> = it
        .take_while(|t| !t.starts_with('=') && !t.starts_with('@'))
        .copied()
        .collect();
    let (num_field, price_commodity, _r) = split_amount(&price_toks);
    let (price, price_commodity) = parse_face(&num_field, &price_commodity, ctx)?;
    let contributed = if *op == "@" {
        value.checked_mul(price)?
    } else if value.is_sign_negative() {
        Decimal::ZERO.checked_sub(price.abs())?
    } else {
        price.abs()
    };
    Some((contributed, price_commodity))
}

/// The commodity symbols appearing in an amount's cost/assertion tail
/// (`5 USD @ 1.10 EUR = 5.50 EUR` → `["EUR"]`), attached symbols included
/// (`@ $1.10` → `["$"]`). Deduplicated, in order of appearance.
#[must_use]
pub fn tail_commodities(text: &str) -> Vec<String> {
    let toks: Vec<&str> = text.split_whitespace().collect();
    let (_num, _commodity, rest) = split_amount(&toks);
    let mut out: Vec<String> = Vec::new();
    for t in rest {
        if crate::lex::is_rest_start(t) || is_number_like(t) {
            continue;
        }
        let (_n, sym) = detach_symbol(t);
        let sym = if sym.is_empty() { t.to_owned() } else { sym };
        if !sym.is_empty() && !out.contains(&sym) {
            out.push(sym);
        }
    }
    out
}

/// Per-commodity sums of the postings entered so far. `None` if any
/// non-empty amount fails to parse — the imbalance is then unknown.
#[must_use]
pub fn balance(amounts: &[&str], ctx: &AmountCtx) -> Option<Vec<(String, Decimal)>> {
    let mut sums: Vec<(String, Decimal)> = Vec::new();
    for text in amounts {
        if text.trim().is_empty() {
            continue;
        }
        let parsed = parse_amount(text, ctx)?;
        let (value, commodity) = parsed.contributes;
        if let Some(slot) = sums.iter_mut().find(|(c, _)| *c == commodity) {
            slot.1 = slot.1.checked_add(value)?;
        } else {
            sums.push((commodity, value));
        }
    }
    Some(sums)
}

/// The commodities whose sums are nonzero — the running imbalance display.
#[must_use]
pub fn imbalance(amounts: &[&str], ctx: &AmountCtx) -> Option<Vec<(String, Decimal)>> {
    balance(amounts, ctx).map(|sums| sums.into_iter().filter(|(_, v)| !v.is_zero()).collect())
}

/// Render a value in a commodity's declared style — or, absent a declared
/// style, plainly with `.` (or the file's decimal mark) and the value's own
/// scale.
///
/// The style's decimal places are a *minimum*: a value needing more
/// precision keeps it. We are writing a balancing amount and must never
/// round it.
#[must_use]
pub fn render_amount(value: Decimal, commodity: &str, ctx: &AmountCtx) -> String {
    let style = ctx.styles.get(commodity).cloned().unwrap_or_else(|| DisplayStyle {
        decimal_mark: ctx.decimal_mark.unwrap_or('.'),
        group_sep: None,
        decimal_places: value.scale(),
        ..DisplayStyle::default()
    });
    let places = style.decimal_places.max(value.scale());
    let mut v = value;
    v.rescale(places);
    let plain = v.abs().to_string();
    let (int_part, frac_part) = plain
        .split_once('.')
        .map_or((plain.as_str(), ""), |(a, b)| (a, b));

    let grouped = style.group_sep.map_or_else(
        || int_part.to_owned(),
        |sep| group_digits(int_part, sep, &style.group_sizes),
    );

    let mut num = String::new();
    if value.is_sign_negative() {
        num.push('-');
    }
    num.push_str(&grouped);
    if !frac_part.is_empty() {
        num.push(style.decimal_mark);
        num.push_str(frac_part);
    }
    if commodity.is_empty() {
        return num;
    }
    let space = if style.symbol_space { " " } else { "" };
    match style.symbol_side {
        Side::Left => format!("{commodity}{space}{num}"),
        Side::Right => format!("{num}{space}{commodity}"),
    }
}

/// Insert group separators from the right, sizes repeating the last.
fn group_digits(digits: &str, sep: char, sizes: &[usize]) -> String {
    let chars: Vec<char> = digits.chars().collect();
    let mut boundaries: Vec<usize> = Vec::new(); // positions from the right
    let mut acc = 0usize;
    let mut idx = 0usize;
    loop {
        let size = sizes
            .get(idx)
            .or_else(|| sizes.last())
            .copied()
            .unwrap_or(3)
            .max(1);
        acc = acc.saturating_add(size);
        if acc >= chars.len() {
            break;
        }
        boundaries.push(acc);
        idx = idx.saturating_add(1);
    }
    let n = chars.len();
    let mut out = String::new();
    for (i, c) in chars.iter().enumerate() {
        out.push(*c);
        let from_right = n.saturating_sub(i).saturating_sub(1);
        if boundaries.contains(&from_right) && from_right > 0 {
            out.push(sep);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dec(s: &str) -> Decimal {
        s.parse().unwrap()
    }

    fn ctx() -> AmountCtx {
        AmountCtx::default()
    }

    fn ctx_with(commodity: &str, sample: &str) -> AmountCtx {
        let mut c = AmountCtx::default();
        let (name, style) = style_from_sample(sample).unwrap();
        assert_eq!(name, commodity);
        c.styles.insert(name, style);
        c
    }

    // ---- auto-detection, mirroring the verified hledger behaviour ----

    #[test]
    fn a_single_mark_occurring_once_is_the_decimal_mark() {
        assert_eq!(parse_number("1.234", None), Some(dec("1.234")));
        assert_eq!(parse_number("1,234", None), Some(dec("1.234")));
    }

    #[test]
    fn a_repeated_single_mark_is_grouping() {
        assert_eq!(parse_number("1.234.567", None), Some(dec("1234567")));
        assert_eq!(parse_number("1,234,567", None), Some(dec("1234567")));
    }

    #[test]
    fn with_both_marks_the_last_one_is_decimal() {
        assert_eq!(parse_number("1.234,56", None), Some(dec("1234.56")));
        assert_eq!(parse_number("1,234.56", None), Some(dec("1234.56")));
    }

    #[test]
    fn underscore_is_always_grouping() {
        assert_eq!(parse_number("50_888.56", None), Some(dec("50888.56")));
        assert_eq!(parse_number("1_000", None), Some(dec("1000")));
    }

    #[test]
    fn a_forced_mark_overrides_detection() {
        // With decimal-mark '.', a lone comma groups.
        assert_eq!(parse_number("1,234", Some('.')), Some(dec("1234")));
        assert_eq!(parse_number("1.234", Some(',')), Some(dec("1234")));
        assert_eq!(parse_number("1.234,56", Some(',')), Some(dec("1234.56")));
    }

    #[test]
    fn signs_and_bare_integers() {
        assert_eq!(parse_number("-5", None), Some(dec("-5")));
        assert_eq!(parse_number("+5.0", None), Some(dec("5.0")));
        assert_eq!(parse_number("0", None), Some(dec("0")));
    }

    #[test]
    fn garbage_is_none_not_wrong() {
        assert_eq!(parse_number("", None), None);
        assert_eq!(parse_number("abc", None), None);
        assert_eq!(parse_number("1,2.3.4", None), None);
        assert_eq!(parse_number("1.2,3_4", None), None);
    }

    // ---- typed amount fields ----

    #[test]
    fn amount_with_right_commodity() {
        let p = parse_amount("23.45 EUR", &ctx()).unwrap();
        assert_eq!(p.value, dec("23.45"));
        assert_eq!(p.commodity, "EUR");
        assert_eq!(p.contributes, (dec("23.45"), "EUR".into()));
    }

    #[test]
    fn unitless_amounts_are_valid() {
        let p = parse_amount("42", &ctx()).unwrap();
        assert_eq!(p.commodity, "");
        assert_eq!(p.contributes, (dec("42"), String::new()));
    }

    #[test]
    fn attached_and_left_symbols() {
        let p = parse_amount("$100", &ctx()).unwrap();
        assert_eq!((p.value, p.commodity.as_str()), (dec("100"), "$"));
        let p = parse_amount("-$5.20", &ctx()).unwrap();
        assert_eq!((p.value, p.commodity.as_str()), (dec("-5.20"), "$"));
        let p = parse_amount("100€", &ctx()).unwrap();
        assert_eq!((p.value, p.commodity.as_str()), (dec("100"), "€"));
        let p = parse_amount("USD 100", &ctx()).unwrap();
        assert_eq!((p.value, p.commodity.as_str()), (dec("100"), "USD"));
    }

    #[test]
    fn unit_cost_converts_the_contribution() {
        let p = parse_amount("-5 EUR @ 1.2 USD", &ctx()).unwrap();
        assert_eq!(p.value, dec("-5"));
        assert_eq!(p.commodity, "EUR");
        assert_eq!(p.contributes, (dec("-6.0"), "USD".into()));
    }

    #[test]
    fn total_cost_takes_the_amount_sign() {
        // Verified: -5 EUR @@ 6 USD contributes -6 USD.
        let p = parse_amount("-5 EUR @@ 6 USD", &ctx()).unwrap();
        assert_eq!(p.contributes, (dec("-6"), "USD".into()));
        let p = parse_amount("5 EUR @@ 6 USD", &ctx()).unwrap();
        assert_eq!(p.contributes, (dec("6"), "USD".into()));
    }

    #[test]
    fn assertion_tails_do_not_affect_the_contribution() {
        let p = parse_amount("10 EUR = 100 EUR", &ctx()).unwrap();
        assert_eq!(p.contributes, (dec("10"), "EUR".into()));
    }

    #[test]
    fn commodity_style_supplies_the_decimal_mark() {
        // Style says comma-decimal, so typed 1.234 groups: 1234.
        let c = ctx_with("EUR", "1.000,00 EUR");
        let p = parse_amount("1.234 EUR", &c).unwrap();
        assert_eq!(p.value, dec("1234"));
    }

    #[test]
    fn directive_mark_beats_commodity_style() {
        let mut c = ctx_with("EUR", "1.000,00 EUR");
        c.decimal_mark = Some('.');
        let p = parse_amount("1.234 EUR", &c).unwrap();
        assert_eq!(p.value, dec("1.234"));
    }

    // ---- balance and imbalance ----

    #[test]
    fn balance_sums_per_commodity() {
        let sums = balance(&["23.45 EUR", "-20 EUR", "5 USD"], &ctx()).unwrap();
        assert_eq!(
            sums,
            vec![("EUR".into(), dec("3.45")), ("USD".into(), dec("5"))]
        );
    }

    #[test]
    fn empty_amounts_are_skipped_but_garbage_poisons_the_balance() {
        assert_eq!(
            balance(&["1 EUR", "", "  "], &ctx()).unwrap(),
            vec![("EUR".into(), dec("1"))]
        );
        assert_eq!(balance(&["1 EUR", "wat"], &ctx()), None);
    }

    #[test]
    fn imbalance_drops_zeroed_commodities() {
        let imb = imbalance(&["5 EUR", "-5 EUR", "1 USD"], &ctx()).unwrap();
        assert_eq!(imb, vec![("USD".into(), dec("1"))]);
    }

    #[test]
    fn cost_postings_balance_in_the_priced_commodity() {
        let imb = imbalance(&["-5 EUR @ 1.2 USD", "6.0 USD"], &ctx()).unwrap();
        assert!(imb.is_empty());
    }

    // ---- styles and rendering ----

    #[test]
    fn tail_commodities_are_extracted_from_costs_and_assertions() {
        assert_eq!(tail_commodities("23.45 EUR"), Vec::<String>::new());
        assert_eq!(tail_commodities("5 EUR @ 1.10 USD"), vec!["USD"]);
        assert_eq!(tail_commodities("5 EUR @@ 6 USD = 5 EUR"), vec!["USD", "EUR"]);
        assert_eq!(tail_commodities("5 EUR ==* 5 EUR"), vec!["EUR"]);
        // Attached symbols count too.
        assert_eq!(tail_commodities("5 EUR @ $1.10"), vec!["$"]);
        // Unitless tails have none.
        assert_eq!(tail_commodities("5 @ 1.10"), Vec::<String>::new());
    }

    #[test]
    fn style_from_right_symbol_sample() {
        let (name, s) = style_from_sample("1_000.00 EUR").unwrap();
        assert_eq!(name, "EUR");
        assert_eq!(s.decimal_mark, '.');
        assert_eq!(s.group_sep, Some('_'));
        assert_eq!(s.group_sizes, vec![3]);
        assert_eq!(s.decimal_places, 2);
        assert_eq!(s.symbol_side, Side::Right);
        assert!(s.symbol_space);
    }

    #[test]
    fn style_from_attached_left_symbol_sample() {
        let (name, s) = style_from_sample("$1000.00").unwrap();
        assert_eq!(name, "$");
        assert_eq!(s.symbol_side, Side::Left);
        assert!(!s.symbol_space);
        assert_eq!(s.group_sep, None);
    }

    #[test]
    fn style_from_comma_decimal_sample() {
        let (_, s) = style_from_sample("1.000,00 EUR").unwrap();
        assert_eq!(s.decimal_mark, ',');
        assert_eq!(s.group_sep, Some('.'));
    }

    #[test]
    fn bare_symbol_samples_have_no_style() {
        assert_eq!(style_from_sample("USD"), None);
    }

    #[test]
    fn render_respects_the_declared_style() {
        let c = ctx_with("EUR", "1_000.00 EUR");
        assert_eq!(render_amount(dec("23.4"), "EUR", &c), "23.40 EUR");
        assert_eq!(render_amount(dec("-51234.5"), "EUR", &c), "-51_234.50 EUR");
    }

    #[test]
    fn render_comma_style_groups_with_dots() {
        let c = ctx_with("EUR", "1.000,00 EUR");
        assert_eq!(render_amount(dec("1234567.8"), "EUR", &c), "1.234.567,80 EUR");
    }

    #[test]
    fn render_never_rounds_a_balancing_amount() {
        let c = ctx_with("EUR", "1_000.00 EUR");
        assert_eq!(render_amount(dec("23.456"), "EUR", &c), "23.456 EUR");
    }

    #[test]
    fn render_left_symbol() {
        let c = ctx_with("$", "$1000.00");
        assert_eq!(render_amount(dec("-5.2"), "$", &c), "$-5.20");
    }

    #[test]
    fn render_without_a_style_keeps_the_value_scale() {
        assert_eq!(render_amount(dec("23.45"), "EUR", &ctx()), "23.45 EUR");
        assert_eq!(render_amount(dec("7"), "", &ctx()), "7");
        let mut c = ctx();
        c.decimal_mark = Some(',');
        assert_eq!(render_amount(dec("23.45"), "EUR", &c), "23,45 EUR");
    }
}
