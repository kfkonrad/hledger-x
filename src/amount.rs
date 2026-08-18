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
            let tail: String = t.get(num_end.saturating_add(1)..).unwrap_or("").to_owned();
            // The number part may carry marks *inside* digits_on only; a
            // leading sign belongs to the number, not the symbol.
            // The sign belongs to the number on either side of the symbol:
            // both `-$10` and `$-10` sample the commodity `$`.
            let symbol_left = head.trim_matches(['+', '-']).to_owned();
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
        marks
            .iter()
            .rev()
            .find(|(_, c)| *c == forced)
            .map(|_| forced)
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
        // A sign may sit *between* a left-side symbol and its digits: `$-10`
        // is how hledger writes a negative dollar amount, and it is one
        // commodity `$`, not a commodity `$-` (verified against 1.99). The
        // sign belongs to the number wherever it was written.
        let symbol = leading.trim_end_matches(['+', '-']);
        let inner: String = leading.chars().skip(symbol.chars().count()).collect();
        return (format!("{sign}{inner}{rest}"), symbol.to_owned());
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
        let keep = body
            .chars()
            .count()
            .saturating_sub(trailing.chars().count());
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
    let Some(first) = rest.first() else {
        return Some((value, commodity.to_owned()));
    };
    // The operator may have its amount attached (`@1.1EUR`, `=5USD`), so it
    // is a prefix of the token rather than the whole of it. Matched against
    // the shared [`OPS`] list, longest first — hledger has four assertion
    // operators, not two, and reading `=*` as an unknown tail used to make
    // the whole amount unparseable.
    let op = OPS.iter().find(|o| first.starts_with(**o))?;
    // A balance assertion says what the account holds afterwards; it does
    // not change what this posting contributes.
    if op.starts_with('=') {
        return Some((value, commodity.to_owned()));
    }
    // The price: the operator's attached operand, if any, then tokens up to
    // a further tail.
    let attached = first.get(op.len()..).unwrap_or("");
    let mut price_toks: Vec<&str> = if attached.is_empty() {
        Vec::new()
    } else {
        vec![attached]
    };
    price_toks.extend(
        rest.iter()
            .skip(1)
            .take_while(|t| !crate::lex::is_rest_start(t))
            .copied(),
    );
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
    sum_by_commodity(amounts, ctx, false)
}

/// Per-commodity sums at *face* value.
///
/// What a posting says, not what it costs: the sum hledger balances when no
/// cost is being interpreted, and so the sum equity conversion postings have
/// to cancel.
#[must_use]
pub fn face_balance(amounts: &[&str], ctx: &AmountCtx) -> Option<Vec<(String, Decimal)>> {
    sum_by_commodity(amounts, ctx, true)
}

/// Shared body of [`balance`] and [`face_balance`]: sums in first-appearance
/// order, taking either the face value or the cost contribution.
fn sum_by_commodity(
    amounts: &[&str],
    ctx: &AmountCtx,
    face: bool,
) -> Option<Vec<(String, Decimal)>> {
    let mut sums: Vec<(String, Decimal)> = Vec::new();
    for text in amounts {
        if text.trim().is_empty() {
            continue;
        }
        let parsed = parse_amount(text, ctx)?;
        let (value, commodity) = if face {
            (parsed.value, parsed.commodity)
        } else {
            parsed.contributes
        };
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

/// As [`imbalance`], at face value.
#[must_use]
pub fn face_imbalance(amounts: &[&str], ctx: &AmountCtx) -> Option<Vec<(String, Decimal)>> {
    face_balance(amounts, ctx).map(|sums| sums.into_iter().filter(|(_, v)| !v.is_zero()).collect())
}

/// The amounts of the equity conversion postings a transaction needs.
///
/// The negated face imbalance, one per commodity, in the order the
/// commodities first appear. Posting these makes the transaction sum to zero
/// without interpreting the cost — what `hledger print --infer-equity`
/// generates, in a flat account rather than per-commodity subaccounts.
///
/// Empty when nothing is converted, and equally when the face imbalance is
/// unknown (an unparseable or elided amount) — never guess.
#[must_use]
pub fn equity_conversions(amounts: &[&str], ctx: &AmountCtx) -> Vec<String> {
    if amounts.iter().any(|a| a.trim().is_empty()) {
        return Vec::new();
    }
    let Some(sums) = face_imbalance(amounts, ctx) else {
        return Vec::new();
    };
    sums.into_iter()
        .filter_map(|(commodity, value)| {
            let negated = Decimal::ZERO.checked_sub(value)?;
            Some(render_amount_like(negated, &commodity, ctx, amounts))
        })
        .collect()
}

/// The cost and assertion operators, longest first so a prefix match picks
/// the right one.
const OPS: [&str; 6] = ["==*", "=*", "==", "=", "@@", "@"];

/// Split an amount field into the amount samples it contains: the face, and
/// each amount in its cost/assertion tail. `10 EUR @ 1.1 USD` yields
/// `["10 EUR", "1.1 USD"]`, and an amount attached to its operator (`=5EUR`)
/// joins that operator's group.
fn amount_groups(text: &str) -> Vec<String> {
    let mut groups: Vec<String> = Vec::new();
    let mut cur: Vec<&str> = Vec::new();
    for tok in text.split_whitespace() {
        if !crate::lex::is_rest_start(tok) {
            cur.push(tok);
            continue;
        }
        if !cur.is_empty() {
            groups.push(cur.join(" "));
            cur = Vec::new();
        }
        if let Some(op) = OPS.iter().find(|o| tok.starts_with(**o)) {
            let attached = tok.get(op.len()..).unwrap_or("");
            if !attached.is_empty() {
                cur.push(attached);
            }
        }
    }
    if !cur.is_empty() {
        groups.push(cur.join(" "));
    }
    groups
}

/// Learn a display style for `commodity` from amounts as they are written
/// nearby — the other postings of the transaction being balanced.
///
/// The first amount in that commodity wins, matching how hledger picks up a
/// style from the first amount it sees. `None` for a unitless amount, for a
/// commodity that appears in none of the samples, and for anything that does
/// not parse.
///
/// This is **not** the journal-wide style inference `DESIGN.md` rules out.
/// That rule protects amounts a person wrote from being reflowed by someone
/// else's sloppy entry; nothing here restyles an existing amount. It only
/// decides how to render an amount we are creating from nothing, where the
/// alternative is not "leave it alone" but a hardcoded guess.
#[must_use]
pub fn style_from_amounts(amounts: &[&str], commodity: &str) -> Option<DisplayStyle> {
    if commodity.is_empty() {
        return None;
    }
    amounts
        .iter()
        .flat_map(|t| amount_groups(t))
        .filter_map(|g| style_from_sample(&g))
        .find(|(name, _)| name == commodity)
        .map(|(_, style)| style)
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
    render_amount_like(value, commodity, ctx, &[])
}

/// [`render_amount`], falling back to the style of the amounts it is
/// balancing against when the commodity declares none.
///
/// A generated amount has to be rendered *somehow*, and a hardcoded guess
/// puts `$10` and `-10 $` side by side in one transaction. So when no
/// `commodity` directive settles it, the sibling postings do: `$10` balances
/// with `$-10`, `10€` with `-10€`, and a typed `1,234.50` keeps its digit
/// grouping. A declared style still wins over anything observed.
#[must_use]
pub fn render_amount_like(
    value: Decimal,
    commodity: &str,
    ctx: &AmountCtx,
    siblings: &[&str],
) -> String {
    let style = ctx
        .styles
        .get(commodity)
        .cloned()
        .or_else(|| style_from_amounts(siblings, commodity))
        .map_or_else(
            || DisplayStyle {
                decimal_mark: ctx.decimal_mark.unwrap_or('.'),
                group_sep: None,
                decimal_places: value.scale(),
                ..DisplayStyle::default()
            },
            |s| effective_style(&s, ctx),
        );
    // `value` is computed, so its scale is an artefact of the arithmetic that
    // produced it: 10.00 × 1.105 lands at scale 5 and would print
    // `11.05000`. Trailing zeros carry no information here — unlike in text a
    // person typed, where they are a deliberate statement of precision and
    // are preserved — so the floor is applied to the normalized scale.
    let places = style.decimal_places.max(value.normalize().scale());
    render_styled(value, commodity, &style, places)
}

/// Render `value` in `style` with exactly `places` fraction digits.
fn render_styled(value: Decimal, commodity: &str, style: &DisplayStyle, places: u32) -> String {
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

/// Re-render a face amount in its commodity's declared style, mirroring
/// `hledger print`.
///
/// Symbol side, spacing, digit grouping and decimal mark are normalized; the
/// typed precision is kept exactly — no padding, no rounding. `None` when
/// there is nothing to restyle: a unitless amount, a commodity without a
/// declared style, or text that does not parse.
#[must_use]
pub fn restyle_face_text(field: &str, ctx: &AmountCtx) -> Option<String> {
    restyle_face_text_with(field, ctx, Places::AsWritten)
}

fn restyle_face_text_with(field: &str, ctx: &AmountCtx, places: Places) -> Option<String> {
    let toks: Vec<&str> = field.split_whitespace().collect();
    let (num_field, commodity, rest) = split_amount(&toks);
    if !rest.is_empty() {
        return None;
    }
    restyle_pair(&num_field, &commodity, ctx, places)
}

/// The style actually used for rendering under `ctx`.
///
/// A `decimal-mark` directive forces how amounts in its file *parse*, so a
/// rendered amount must keep that mark or it would re-read as a different
/// value (`10.5` under `decimal-mark ,` is 105). This deliberately deviates
/// from `hledger print`, whose output drops the directives and so can
/// switch marks safely — a formatter writing back into the file cannot.
/// When the style's group separator collides with the forced mark, the two
/// marks swap: `1.000,00` under `decimal-mark .` renders as `1,000.00`.
fn effective_style(style: &DisplayStyle, ctx: &AmountCtx) -> DisplayStyle {
    let mut s = style.clone();
    if let Some(mark) = ctx.decimal_mark {
        if s.decimal_mark != mark {
            if s.group_sep == Some(mark) {
                s.group_sep = Some(s.decimal_mark);
            }
            s.decimal_mark = mark;
        }
    }
    s
}

/// How much precision a restyle should produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Places {
    /// Exactly what was written. `fmt`'s default: changing the precision of
    /// an amount already in the journal changes what `hledger print` emits,
    /// which the semantic invariant forbids.
    AsWritten,
    /// The commodity's declared places, or the amount's own if it carries
    /// more. A floor, never a ceiling — see [`restyle_entered`]. `add` always
    /// uses this; `fmt` only under `--explicit`, where the user has asked for
    /// exactly that rewrite.
    AtLeastDeclared,
}

impl Places {
    /// The number of fraction digits to render `value` with under this
    /// policy, given its commodity's declared style.
    const fn of(self, value: Decimal, style: &DisplayStyle) -> u32 {
        match self {
            Self::AsWritten => value.scale(),
            Self::AtLeastDeclared => {
                let declared = style.decimal_places;
                let written = value.scale();
                if declared > written {
                    declared
                } else {
                    written
                }
            }
        }
    }
}

fn restyle_pair(
    num_field: &str,
    commodity: &str,
    ctx: &AmountCtx,
    places: Places,
) -> Option<String> {
    let (value, commodity) = parse_face(num_field, commodity, ctx)?;
    if commodity.is_empty() {
        return None;
    }
    let style = effective_style(ctx.styles.get(&commodity)?, ctx);
    let places = places.of(value, &style);
    Some(render_styled(value, &commodity, &style, places))
}

/// Restyle a posting's (number field, commodity token) pair for column
/// rendering.
///
/// A right-side spaced style keeps the commodity in its own column; attached
/// and left-side styles put the whole rendered amount in the number field,
/// the way `$100` is aligned today. `None` when nothing restyles — the
/// caller keeps the fields as parsed.
#[must_use]
pub fn restyle_face_fields(
    num: &str,
    commodity: &str,
    ctx: &AmountCtx,
) -> Option<(String, String)> {
    restyle_face_fields_with(num, commodity, ctx, Places::AsWritten)
}

/// [`restyle_face_fields`] with an explicit precision policy.
#[must_use]
pub fn restyle_face_fields_with(
    num: &str,
    commodity: &str,
    ctx: &AmountCtx,
    places: Places,
) -> Option<(String, String)> {
    let (value, name) = parse_face(num, commodity, ctx)?;
    if name.is_empty() {
        return None;
    }
    let style = effective_style(ctx.styles.get(&name)?, ctx);
    let places = places.of(value, &style);
    if style.symbol_side == Side::Right && style.symbol_space {
        Some((render_styled(value, "", &style, places), name))
    } else {
        Some((render_styled(value, &name, &style, places), String::new()))
    }
}

/// Split a rendered amount back into the (number field, commodity) pair the
/// column renderer wants — the inverse of [`restyle_face_fields`]'s output
/// shape, for an amount produced by [`render_amount`].
///
/// A right-side spaced style leaves the symbol in its own column; anything
/// attached or symbol-first keeps the whole rendering in the number field,
/// the way `$100` is aligned.
#[must_use]
pub fn split_rendered(rendered: &str) -> (String, String) {
    let toks: Vec<&str> = rendered.split_whitespace().collect();
    let (num, commodity, _rest) = split_amount(&toks);
    (num, commodity)
}

/// Restyle the amounts inside a cost/assertion tail. Operator tokens pass
/// through; each amount group after an operator is restyled like a face.
/// Any group that does not restyle stays exactly as written.
#[must_use]
pub fn restyle_tail(rest: &[&str], ctx: &AmountCtx) -> Vec<String> {
    restyle_tail_with(rest, ctx, Places::AsWritten)
}

/// [`restyle_tail`] with an explicit precision policy.
#[must_use]
pub fn restyle_tail_with(rest: &[&str], ctx: &AmountCtx, places: Places) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut i = 0usize;
    while let Some(tok) = rest.get(i) {
        i = i.saturating_add(1);
        if !crate::lex::is_rest_start(tok) {
            out.push((*tok).to_owned());
            continue;
        }
        let Some(op) = OPS.iter().find(|o| tok.starts_with(**o)) else {
            out.push((*tok).to_owned());
            continue;
        };
        // An amount attached to the operator (`=5EUR`) joins the group.
        let attached = tok.get(op.len()..).unwrap_or("");
        let mut group: Vec<&str> = if attached.is_empty() {
            Vec::new()
        } else {
            vec![attached]
        };
        while let Some(next) = rest.get(i) {
            if crate::lex::is_rest_start(next) {
                break;
            }
            group.push(next);
            i = i.saturating_add(1);
        }
        if let Some(s) = restyle_face_text_with(&group.join(" "), ctx, places) {
            out.push((*op).to_owned());
            out.extend(s.split_whitespace().map(ToOwned::to_owned));
        } else {
            out.push((*tok).to_owned());
            let skip = usize::from(!attached.is_empty());
            out.extend(group.iter().skip(skip).map(|t| (*t).to_owned()));
        }
    }
    out
}

/// Restyle an amount the user has **just entered** — face plus any
/// cost/assertion tail — leaving every part without a declared style, or that
/// does not parse, exactly as typed.
///
/// This is `add`'s path, and it differs from [`restyle_face_fields`] (`fmt`'s)
/// in one way: the face amount is padded out to the commodity's declared
/// number of decimal places. Entering `4 EUR` under `commodity 1_000.00 EUR`
/// stores `4.00 EUR`, which is also what the generated balancing amount would
/// have been, so the two sides of a transaction agree.
///
/// The declared places are a **minimum, never a maximum**: `4.000` and
/// `4.001` are left alone. hledger accepts more precision than a commodity
/// declares — the directive sets the default, not a limit — and rounding
/// `4.001` down would lose value, which no rewrite here is allowed to do.
///
/// Applies to every amount in the field, the face as well as a cost or
/// assertion tail: `10EUR @ 1.1USD` becomes `10.00 EUR @ 1.10 USD`.
///
/// `fmt` must *not* do this to text already in the journal. Verified: under a
/// declared `1_000.00 EUR`, `hledger print` renders a written `1234 EUR` as
/// `1_234.`, so padding it to `1_234.00` changes print output and breaks the
/// semantic invariant. Existing precision is the author's; entered precision
/// is ours to complete.
#[must_use]
pub fn restyle_entered(text: &str, ctx: &AmountCtx) -> String {
    let toks: Vec<&str> = text.split_whitespace().collect();
    let (num_field, commodity, rest) = split_amount(&toks);
    let face_src = if commodity.is_empty() {
        num_field.clone()
    } else {
        format!("{num_field} {commodity}")
    };
    let mut out: Vec<String> = Vec::new();
    match restyle_pair(&num_field, &commodity, ctx, Places::AtLeastDeclared) {
        Some(s) => out.extend(s.split_whitespace().map(ToOwned::to_owned)),
        None => out.extend(face_src.split_whitespace().map(ToOwned::to_owned)),
    }
    out.extend(restyle_tail_with(&rest, ctx, Places::AtLeastDeclared));
    out.join(" ")
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

    #[test]
    fn all_four_assertion_operators_leave_the_contribution_alone() {
        // hledger has `=`, `==` and their subaccount-inclusive `=*`/`==*`
        // forms (verified against 1.99). Reading `=*` as an unknown tail used
        // to make the whole amount unparseable, which silently blocked
        // `fmt --explicit` from filling in the other posting and made `add`
        // report the imbalance as unknown.
        let ctx = AmountCtx::default();
        for tail in ["= 10 USD", "== 10 USD", "=* 10 USD", "==* 10 USD"] {
            let text = format!("10 USD {tail}");
            let parsed = parse_amount(&text, &ctx).unwrap_or_else(|| panic!("{text}"));
            assert_eq!(parsed.contributes.0, Decimal::from(10), "{text}");
            assert_eq!(parsed.contributes.1, "USD", "{text}");
        }
    }

    #[test]
    fn an_operator_with_its_amount_attached_still_parses() {
        // `@1.1EUR` is one token. It used to parse only by luck, when
        // restyling happened to split it first — which needs a declared
        // style, so an undeclared commodity fell through the gap.
        let ctx = AmountCtx::default();
        let parsed = parse_amount("10 USD @1.1EUR", &ctx).unwrap();
        assert_eq!(parsed.contributes, (Decimal::new(11, 0), "EUR".to_owned()));
        let parsed = parse_amount("10 USD @@13EUR", &ctx).unwrap();
        assert_eq!(parsed.contributes, (Decimal::from(13), "EUR".to_owned()));
        let parsed = parse_amount("10 USD =10USD", &ctx).unwrap();
        assert_eq!(parsed.contributes, (Decimal::from(10), "USD".to_owned()));
    }

    #[test]
    fn a_sign_between_symbol_and_digits_belongs_to_the_number() {
        // `$-10` is how hledger writes a negative dollar amount: one
        // commodity `$`, not a commodity `$-` (verified against 1.99).
        assert_eq!(detach_symbol("$-10"), ("-10".to_owned(), "$".to_owned()));
        assert_eq!(detach_symbol("-$10"), ("-10".to_owned(), "$".to_owned()));
        assert_eq!(detach_symbol("$+10"), ("+10".to_owned(), "$".to_owned()));
        assert_eq!(detach_symbol("$10"), ("10".to_owned(), "$".to_owned()));
        assert_eq!(detach_symbol("10€"), ("10".to_owned(), "€".to_owned()));
        assert_eq!(detach_symbol("-10€"), ("-10".to_owned(), "€".to_owned()));

        // So the two forms sum as one commodity rather than cancelling into
        // two bogus ones.
        let ctx = AmountCtx::default();
        assert_eq!(
            balance(&["$10", "$-10"], &ctx),
            Some(vec![("$".to_owned(), Decimal::ZERO)])
        );
        assert_eq!(
            style_from_sample("$-10").map(|(name, _)| name),
            Some("$".to_owned())
        );
    }

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

    // ---- face balance and equity conversions ----

    #[test]
    fn face_balance_ignores_cost_tails() {
        // At cost this balances; at face value it does not.
        let amounts = ["10 USD @@ 9.06 EUR", "-9.06 EUR"];
        assert!(imbalance(&amounts, &ctx()).unwrap().is_empty());
        assert_eq!(
            face_imbalance(&amounts, &ctx()).unwrap(),
            vec![("USD".into(), dec("10")), ("EUR".into(), dec("-9.06"))]
        );
    }

    #[test]
    fn equity_conversions_negate_the_face_imbalance_in_first_appearance_order() {
        assert_eq!(
            equity_conversions(&["10 USD @@ 9.06 EUR", "-9.06 EUR"], &ctx()),
            vec!["-10 USD".to_owned(), "9.06 EUR".to_owned()]
        );
    }

    #[test]
    fn equity_conversions_are_empty_when_nothing_is_converted() {
        // A single-commodity transaction needs none.
        assert!(equity_conversions(&["5 EUR", "-5 EUR"], &ctx()).is_empty());
        // Unknown imbalance: never guess.
        assert!(equity_conversions(&["5 EUR", "wat"], &ctx()).is_empty());
        // An elided amount makes the face imbalance meaningless.
        assert!(equity_conversions(&["5 EUR", ""], &ctx()).is_empty());
    }

    #[test]
    fn equity_conversions_follow_declared_styles() {
        let c = ctx_with("EUR", "1.000,00 EUR");
        assert_eq!(
            equity_conversions(&["10 USD @@ 9,06 EUR", "-9,06 EUR"], &c),
            vec!["-10 USD".to_owned(), "9,06 EUR".to_owned()]
        );
    }

    // ---- styles and rendering ----

    #[test]
    fn tail_commodities_are_extracted_from_costs_and_assertions() {
        assert_eq!(tail_commodities("23.45 EUR"), Vec::<String>::new());
        assert_eq!(tail_commodities("5 EUR @ 1.10 USD"), vec!["USD"]);
        assert_eq!(
            tail_commodities("5 EUR @@ 6 USD = 5 EUR"),
            vec!["USD", "EUR"]
        );
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
        assert_eq!(
            render_amount(dec("1234567.8"), "EUR", &c),
            "1.234.567,80 EUR"
        );
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

    // ---- restyling, mirroring verified `hledger print` behaviour ----

    #[test]
    fn restyle_normalizes_side_space_and_grouping_but_never_pads() {
        // Verified: hledger prints `10 EUR`, not `10.00 EUR`.
        let c = ctx_with("EUR", "1_000.00 EUR");
        assert_eq!(restyle_face_text("10EUR", &c).unwrap(), "10 EUR");
        assert_eq!(restyle_face_text("EUR10", &c).unwrap(), "10 EUR");
        assert_eq!(restyle_face_text("EUR 10", &c).unwrap(), "10 EUR");
        assert_eq!(
            restyle_face_text("-1234.5 EUR", &c).unwrap(),
            "-1_234.5 EUR"
        );
        // Verified: trailing zeros are kept.
        assert_eq!(restyle_face_text("10.500 EUR", &c).unwrap(), "10.500 EUR");
    }

    #[test]
    fn restyle_converts_the_decimal_mark() {
        let c = ctx_with("EUR", "1.000,00 EUR");
        assert_eq!(
            restyle_face_text("1000,50 EUR", &c).unwrap(),
            "1.000,50 EUR"
        );
        // Verified: under a comma-decimal style hledger reads `10.5` as 105.
        assert_eq!(restyle_face_text("10.5 EUR", &c).unwrap(), "105 EUR");
        // A decimal-mark directive governs parsing AND stays in the rendered
        // number: switching to the style's mark would change the value on
        // re-read. (hledger print switches marks, but its output drops the
        // directive; a formatter writing back into the file cannot.)
        let mut c = ctx_with("EUR", "1_000.00 EUR");
        c.decimal_mark = Some(',');
        assert_eq!(restyle_face_text("10,5 EUR", &c).unwrap(), "10,5 EUR");
        assert_eq!(restyle_face_text("1000,5 EUR", &c).unwrap(), "1_000,5 EUR");
        // A colliding group separator swaps with the displaced decimal mark.
        let mut c = ctx_with("EUR", "1.000,00 EUR");
        c.decimal_mark = Some('.');
        assert_eq!(restyle_face_text("1000.5 EUR", &c).unwrap(), "1,000.5 EUR");
    }

    #[test]
    fn restyle_leaves_the_undeclared_and_the_unparseable_alone() {
        let c = ctx_with("EUR", "1_000.00 EUR");
        assert_eq!(restyle_face_text("10USD", &c), None);
        assert_eq!(restyle_face_text("42", &c), None); // unitless
        assert_eq!(restyle_face_text("wat", &c), None);
        assert_eq!(restyle_face_text("", &c), None);
    }

    #[test]
    fn restyle_fields_split_by_symbol_placement() {
        let c = ctx_with("EUR", "1_000.00 EUR");
        assert_eq!(
            restyle_face_fields("10EUR", "", &c).unwrap(),
            ("10".to_owned(), "EUR".to_owned())
        );
        // Attached left symbol: the whole amount is the number field.
        let c = ctx_with("$", "$1000.00");
        assert_eq!(
            restyle_face_fields("5", "$", &c).unwrap(),
            ("$5".to_owned(), String::new())
        );
        assert_eq!(
            restyle_face_fields("-$5.2", "", &c).unwrap(),
            ("$-5.2".to_owned(), String::new())
        );
        // Spaced left symbol: also one field, aligned as a unit.
        let c = ctx_with("USD", "USD 1,000.00");
        assert_eq!(
            restyle_face_fields("100USD", "", &c).unwrap(),
            ("USD 100".to_owned(), String::new())
        );
    }

    #[test]
    fn restyle_tail_reaches_costs_and_assertions() {
        let mut c = ctx_with("EUR", "1_000.00 EUR");
        let (n, s) = style_from_sample("1,000.00 USD").unwrap();
        c.styles.insert(n, s);
        assert_eq!(restyle_tail(&["@", "1.1USD"], &c), vec!["@", "1.1", "USD"]);
        assert_eq!(restyle_tail(&["=", "-11USD"], &c), vec!["=", "-11", "USD"]);
        // Attached operator amounts restyle too.
        assert_eq!(restyle_tail(&["=5EUR"], &c), vec!["=", "5", "EUR"]);
        // Undeclared or unparseable groups stay verbatim.
        assert_eq!(
            restyle_tail(&["@", "1.1", "GBP"], &c),
            vec!["@", "1.1", "GBP"]
        );
        assert_eq!(restyle_tail(&["@", "wat"], &c), vec!["@", "wat"]);
    }

    #[test]
    fn a_generated_amount_sheds_arithmetic_trailing_zeros_but_keeps_the_floor() {
        let c = ctx_with("EUR", "1_000.00 EUR");
        let d = |s: &str| s.parse::<Decimal>().unwrap();
        // 10.00 × 1.105 lands at scale 5; the zeros say nothing.
        assert_eq!(render_amount(d("11.05000"), "EUR", &c), "11.05 EUR");
        // The declared places are still a floor.
        assert_eq!(render_amount(d("5"), "EUR", &c), "5.00 EUR");
        assert_eq!(render_amount(d("5.0"), "EUR", &c), "5.00 EUR");
        // Genuine precision beyond the declaration survives.
        assert_eq!(render_amount(d("11.056"), "EUR", &c), "11.056 EUR");
        assert_eq!(render_amount(d("11.05600"), "EUR", &c), "11.056 EUR");
    }

    #[test]
    fn restyle_entered_covers_face_and_tail() {
        let mut c = ctx_with("EUR", "1_000.00 EUR");
        let (n, s) = style_from_sample("1,000.00 USD").unwrap();
        c.styles.insert(n, s);
        // Face, price and assertion all take their commodity's declared two
        // places — every amount in the field the user just typed.
        assert_eq!(
            restyle_entered("10EUR @ 1.1USD = 5EUR", &c),
            "10.00 EUR @ 1.10 USD = 5.00 EUR"
        );
        // Nothing restylable: byte-identical tokens, single-spaced.
        assert_eq!(restyle_entered("10 GBP", &c), "10 GBP");
    }

    #[test]
    fn restyling_never_changes_the_parsed_value() {
        let mut c = ctx_with("EUR", "1.000,00 EUR");
        let (n, s) = style_from_sample("1,000.00 USD").unwrap();
        c.styles.insert(n, s);
        for src in ["10EUR", "1000,50 EUR", "10.5 EUR", "-5EUR @ 1.2USD"] {
            let restyled = restyle_entered(src, &c);
            assert_eq!(
                parse_amount(src, &c).unwrap(),
                parse_amount(&restyled, &c).unwrap(),
                "value drifted for {src:?} -> {restyled:?}"
            );
        }
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
