//! `--explicit`: write out the amounts hledger would otherwise infer.
//!
//! One posting per transaction may leave its amount off; hledger works it out
//! from the others, and `hledger print -x` shows it. This module does the same
//! thing in the journal itself, so the file says what it means.
//!
//! The rules were checked against hledger 1.99:
//!
//! - Real postings, balanced-virtual (`[...]`) postings and unbalanced-virtual
//!   (`(...)`) postings balance separately. Only the first two infer anything;
//!   an unbalanced virtual posting contributes to no sum and is skipped.
//! - Exactly one posting in a group may be missing its amount. Zero means
//!   there is nothing to fill in; two or more is an hledger error, and `fmt`
//!   is not a validator, so the transaction is left as it stands.
//! - A posting carrying only a balance assertion (`Assets:Cash  = 0 EUR`) is
//!   missing its amount too, and gets one in front of the assertion.
//! - When the remainder spans several commodities the inferred posting is
//!   split into one posting per commodity, which is what `print -x` does.
//! - A remainder of zero still gets written: `0 EUR`, not nothing.
//!
//! Anything the arithmetic cannot reach — an unparseable amount elsewhere in
//! the transaction, an already-unbalanced transaction, a periodic or auto
//! transaction (neither opens a transaction as far as `fmt` is concerned) —
//! is passed through untouched. Filling in a wrong amount would be far worse
//! than filling in none.
//!
//! Costs are deliberately *not* inferred: `print -x` turns `10 EUR` /
//! `-11 USD` into `10 EUR @@ 11 USD`, which is a claim about the transaction
//! rather than a value already implied by it.

use rust_decimal::Decimal;

use super::posting::Posting;
use super::Class;
use crate::amount::{balance, render_amount_like, split_rendered, AmountCtx};

/// Which set of postings a posting balances against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Group {
    /// An ordinary posting.
    Real,
    /// `[Account]` — balances among its own kind.
    Balanced,
    /// `(Account)` — balances against nothing.
    Unbalanced,
}

impl Group {
    fn of(account: &str) -> Self {
        match account.chars().next() {
            Some('[') => Self::Balanced,
            Some('(') => Self::Unbalanced,
            _ => Self::Real,
        }
    }
}

/// Fill in the inferred amounts of one transaction's posting lines.
///
/// `txn` is the classified line run following a transaction header; lines that
/// are not postings are ignored.
pub(super) fn fill(txn: &mut [Class], ctx: &AmountCtx) {
    fill_group(txn, ctx, Group::Real);
    fill_group(txn, ctx, Group::Balanced);
}

fn fill_group(txn: &mut [Class], ctx: &AmountCtx, group: Group) {
    let members: Vec<usize> = txn
        .iter()
        .enumerate()
        .filter(|(_, c)| postings_of(c).iter().any(|p| in_group(p, group)))
        .map(|(i, _)| i)
        .collect();

    // Exactly one posting may be waiting for an amount.
    let mut waiting = members.iter().copied().filter(|i| {
        txn.get(*i)
            .is_some_and(|c| postings_of(c).iter().any(needs_amount))
    });
    let (Some(target), None) = (waiting.next(), waiting.next()) else {
        return;
    };

    let texts: Vec<String> = members
        .iter()
        .filter(|i| **i != target)
        .flat_map(|i| txn.get(*i).map_or(&[][..], postings_of))
        .filter_map(amount_text)
        .collect();
    let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
    let Some(sums) = balance(&refs, ctx) else {
        return;
    };
    let Some(filled) = remainder(&sums, ctx, &refs) else {
        return;
    };

    if let Some(Class::Post(ps)) = txn.get_mut(target) {
        *ps = apply(ps, &filled);
    }
}

/// The amounts the inferred posting must carry: the negated sums, rendered.
///
/// Zero sums drop out, since a commodity that already balances needs no
/// posting — except when *everything* balances, where hledger still writes an
/// explicit zero in the first commodity seen.
///
/// `siblings` are the amounts being balanced against, which supply the
/// display style for any commodity that declares none.
fn remainder(
    sums: &[(String, Decimal)],
    ctx: &AmountCtx,
    siblings: &[&str],
) -> Option<Vec<String>> {
    let (first, _) = sums.first()?;
    let negated: Vec<(String, Decimal)> = sums
        .iter()
        .filter(|(_, v)| !v.is_zero())
        .map(|(c, v)| Decimal::ZERO.checked_sub(*v).map(|n| (c.clone(), n)))
        .collect::<Option<_>>()?;
    if negated.is_empty() {
        return Some(vec![render_amount_like(
            Decimal::ZERO,
            first,
            ctx,
            siblings,
        )]);
    }
    Some(
        negated
            .iter()
            .map(|(c, v)| render_amount_like(*v, c, ctx, siblings))
            .collect(),
    )
}

/// Put `filled` into the waiting posting, splitting it into one posting per
/// commodity when there is more than one.
///
/// A posting that carries a balance assertion or a cost cannot be split — the
/// tail belongs to a single commodity, and duplicating it would assert
/// something nobody wrote. Such a posting is left alone rather than mangled.
fn apply(ps: &[Posting], filled: &[String]) -> Vec<Posting> {
    let mut out: Vec<Posting> = Vec::new();
    for p in ps {
        if !needs_amount(p) {
            out.push(p.clone());
            continue;
        }
        match p {
            Posting::Bare(account, comment) => {
                // The comment describes the posting the user wrote, so it
                // stays with the first of the postings replacing it.
                for (n, text) in filled.iter().enumerate() {
                    let (num, commodity) = split_rendered(text);
                    out.push(Posting::Amount {
                        account: account.clone(),
                        num,
                        commodity,
                        rest: Vec::new(),
                        comment: if n == 0 { comment.clone() } else { None },
                    });
                }
            }
            Posting::Amount {
                account,
                rest,
                comment,
                ..
            } => match filled {
                [text] => {
                    let (num, commodity) = split_rendered(text);
                    out.push(Posting::Amount {
                        account: account.clone(),
                        num,
                        commodity,
                        rest: rest.clone(),
                        comment: comment.clone(),
                    });
                }
                _ => out.push(p.clone()),
            },
            Posting::Comment(_) => out.push(p.clone()),
        }
    }
    out
}

/// The postings a classified line renders as (empty for anything that is not
/// a posting line).
const fn postings_of(c: &Class) -> &[Posting] {
    match c {
        Class::Post(ps) => ps.as_slice(),
        Class::Other | Class::Opaque => &[],
    }
}

/// Whether a posting balances in `group`. Comment lines belong to none.
fn in_group(p: &Posting, group: Group) -> bool {
    super::posting::account_of(p).is_some_and(|a| Group::of(a) == group)
}

/// Whether a posting is waiting for an amount: no account-side amount at all,
/// or only a cost/assertion tail.
const fn needs_amount(p: &Posting) -> bool {
    match p {
        Posting::Bare(_, _) => true,
        Posting::Amount { num, commodity, .. } => num.is_empty() && commodity.is_empty(),
        Posting::Comment(_) => false,
    }
}

/// A posting's amount as `amount::balance` wants to read it — face plus tail.
/// `None` for a posting that carries no amount.
fn amount_text(p: &Posting) -> Option<String> {
    match p {
        Posting::Amount {
            num,
            commodity,
            rest,
            ..
        } => {
            let mut parts: Vec<&str> = Vec::new();
            if !num.is_empty() {
                parts.push(num);
            }
            if !commodity.is_empty() {
                parts.push(commodity);
            }
            parts.extend(rest.iter().map(String::as_str));
            (!parts.is_empty()).then(|| parts.join(" "))
        }
        Posting::Bare(_, _) | Posting::Comment(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accounts_are_grouped_by_their_brackets() {
        assert_eq!(Group::of("Assets:Bank"), Group::Real);
        assert_eq!(Group::of("[Assets:Bank]"), Group::Balanced);
        assert_eq!(Group::of("(Assets:Bank)"), Group::Unbalanced);
    }

    #[test]
    fn a_posting_is_waiting_when_it_has_no_face_amount() {
        assert!(needs_amount(&Posting::Bare("a".into(), None)));
        assert!(needs_amount(&Posting::Amount {
            account: "a".into(),
            num: String::new(),
            commodity: String::new(),
            rest: vec!["=".into(), "0".into(), "EUR".into()],
            comment: None,
        }));
        assert!(!needs_amount(&Posting::Amount {
            account: "a".into(),
            num: "1".into(),
            commodity: "EUR".into(),
            rest: Vec::new(),
            comment: None,
        }));
        assert!(!needs_amount(&Posting::Comment("; hi".into())));
    }

    #[test]
    fn amount_text_rejoins_face_and_tail() {
        assert_eq!(
            amount_text(&Posting::Amount {
                account: "a".into(),
                num: "10".into(),
                commodity: "EUR".into(),
                rest: vec!["@".into(), "1.1".into(), "USD".into()],
                comment: None,
            }),
            Some("10 EUR @ 1.1 USD".to_owned())
        );
        assert_eq!(amount_text(&Posting::Bare("a".into(), None)), None);
    }

    #[test]
    fn an_all_zero_remainder_still_names_the_commodity() {
        let ctx = AmountCtx::default();
        let sums = vec![("EUR".to_owned(), Decimal::ZERO)];
        assert_eq!(remainder(&sums, &ctx, &[]), Some(vec!["0 EUR".to_owned()]));
    }
}
