//! Frecency indices over the parsed journal.
//!
//! Built in one pass over the transactions of the include tree. Each entry
//! carries a count, the date it was last seen, and a frecency score:
//!
//! ```text
//! score = Σ over occurrences of  0.5 ^ (age_days / half_life)
//! ```
//!
//! evaluated relative to today at parse time.
//!
//! **Pre-fill and ranking deliberately use different rules.** Pre-fill (the
//! `templates` index) takes the *most recent* matching transaction, because
//! predictability matters more than cleverness when text is being put in
//! front of the user. The completion menu orders by *score*, because ranking
//! is what makes the second-best candidate reachable. Do not unify them.

use std::collections::HashMap;

use chrono::NaiveDate;

use super::parser::{Journal, Transaction};
use crate::lex::split_payee_note;

/// Default half-life for the frecency decay, in days.
pub const DEFAULT_HALF_LIFE_DAYS: f64 = 90.0;

/// One index entry.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    /// Number of occurrences.
    pub count: u64,
    /// Date of the most recent occurrence.
    pub last_date: NaiveDate,
    /// Frecency score (see module docs).
    pub score: f64,
}

/// The four frecency indices, plus the commodity pool for the amount field.
#[derive(Debug, Clone, Default)]
pub struct Index {
    /// description → entry. Description completion, over descriptions as
    /// written — `payee | note` and all, since that is what gets typed here.
    pub descriptions: HashMap<String, Entry>,
    /// payee → entry. The payee half only; what the payee checks and the
    /// near-miss hint rank against.
    pub payees: HashMap<String, Entry>,
    /// account → entry. Account completion, unconditioned.
    pub accounts: HashMap<String, Entry>,
    /// (payee, account) → entry. Account completion conditioned on the payee
    /// already entered.
    ///
    /// Keyed on the **payee**, not the whole description: a note is often
    /// unique to one transaction (an invoice number, a trip), and keying on
    /// it would mean the conditioning never fires.
    pub by_payee: HashMap<(String, String), Entry>,
    /// payee → index into the journal's transactions of the most recent
    /// transaction with that payee. Posting pre-fill.
    ///
    /// Keyed on the payee for the same reason as `by_payee`, and it matters
    /// more here: a template that never matches is a feature that silently
    /// does nothing.
    pub templates: HashMap<String, usize>,
    /// commodity → entry. Completion in the amount field.
    pub commodities: HashMap<String, Entry>,
}

impl Index {
    /// Build all indices in one pass.
    #[must_use]
    pub fn build(journal: &Journal, today: NaiveDate, half_life_days: f64) -> Self {
        let mut idx = Self::default();
        for (i, txn) in journal.transactions.iter().enumerate() {
            let weight = decay(today, txn.date, half_life_days);
            let (payee, _note) = split_payee_note(&txn.description);
            let payee = payee.to_owned();
            if !txn.description.is_empty() {
                bump(
                    &mut idx.descriptions,
                    txn.description.clone(),
                    txn.date,
                    weight,
                );
                bump(&mut idx.payees, payee.clone(), txn.date, weight);
                // Later transactions win ties: strictly-greater keeps the
                // earlier one only when it is genuinely newer.
                let slot = idx.templates.entry(payee.clone()).or_insert(i);
                let newest = journal.transactions.get(*slot).map(|t| t.date);
                if newest.is_none_or(|d| txn.date >= d) {
                    *slot = i;
                }
            }
            for p in &txn.postings {
                bump(&mut idx.accounts, p.account.clone(), txn.date, weight);
                if !txn.description.is_empty() {
                    bump(
                        &mut idx.by_payee,
                        (payee.clone(), p.account.clone()),
                        txn.date,
                        weight,
                    );
                }
                if let Some(c) = &p.commodity {
                    bump(&mut idx.commodities, c.clone(), txn.date, weight);
                }
            }
        }
        idx
    }

    /// Fold a transaction entered in *this session* into the indices, so the
    /// accounts, description and commodities it introduces complete for the
    /// rest of the session — hledger only requires `account` directives in
    /// strict mode, and even there a name is usable the moment the user has
    /// committed to it once.
    ///
    /// `templates` is deliberately left alone: it indexes into the journal,
    /// which these transactions are not part of until they are written.
    pub fn bump_session(
        &mut self,
        date: NaiveDate,
        description: &str,
        postings: &[(String, Option<String>)],
        today: NaiveDate,
        half_life_days: f64,
    ) {
        let weight = decay(today, date, half_life_days);
        let (payee, _note) = split_payee_note(description);
        if !description.is_empty() {
            bump(&mut self.descriptions, description.to_owned(), date, weight);
            bump(&mut self.payees, payee.to_owned(), date, weight);
        }
        for (account, commodity) in postings {
            bump(&mut self.accounts, account.clone(), date, weight);
            if !description.is_empty() {
                bump(
                    &mut self.by_payee,
                    (payee.to_owned(), account.clone()),
                    date,
                    weight,
                );
            }
            if let Some(c) = commodity {
                bump(&mut self.commodities, c.clone(), date, weight);
            }
        }
    }

    /// The template transaction for a payee, if any. Callers holding a whole
    /// description should pass its payee half.
    #[must_use]
    pub fn template<'a>(&self, journal: &'a Journal, payee: &str) -> Option<&'a Transaction> {
        journal.transactions.get(*self.templates.get(payee)?)
    }

    /// Descriptions ranked by score, best first — as written, notes included.
    #[must_use]
    pub fn ranked_descriptions(&self) -> Vec<&str> {
        ranked(&self.descriptions)
    }

    /// Payees ranked by score, best first.
    #[must_use]
    pub fn ranked_payees(&self) -> Vec<&str> {
        ranked(&self.payees)
    }

    /// Accounts ranked by score, best first. With a payee, accounts seen
    /// under that payee rank first (by their conditioned score), then all
    /// remaining accounts by unconditioned score.
    #[must_use]
    pub fn ranked_accounts(&self, payee: Option<&str>) -> Vec<&str> {
        let conditioned: Vec<&str> = payee.map_or_else(Vec::new, |desc| {
            let mut v: Vec<(&str, &Entry)> = self
                .by_payee
                .iter()
                .filter(|((d, _), _)| d == desc)
                .map(|((_, a), e)| (a.as_str(), e))
                .collect();
            sort_ranked(&mut v);
            v.into_iter().map(|(a, _)| a).collect()
        });
        let mut out = conditioned;
        for a in ranked(&self.accounts) {
            if !out.contains(&a) {
                out.push(a);
            }
        }
        out
    }

    /// Commodities ranked by score, best first.
    #[must_use]
    pub fn ranked_commodities(&self) -> Vec<&str> {
        ranked(&self.commodities)
    }
}

/// One occurrence's contribution to a score.
fn decay(today: NaiveDate, date: NaiveDate, half_life_days: f64) -> f64 {
    // A future-dated transaction counts as today's — no boost.
    let age_days = today.signed_duration_since(date).num_days().max(0);
    let age = i32::try_from(age_days).map_or(f64::MAX, f64::from);
    0.5_f64.powf(age / half_life_days)
}

fn bump<K: std::hash::Hash + Eq>(
    map: &mut HashMap<K, Entry>,
    key: K,
    date: NaiveDate,
    weight: f64,
) {
    let e = map.entry(key).or_insert(Entry {
        count: 0,
        last_date: date,
        score: 0.0,
    });
    e.count = e.count.saturating_add(1);
    e.last_date = e.last_date.max(date);
    e.score += weight;
}

/// Keys ranked by score descending, ties broken by recency then name for
/// determinism.
fn ranked<S: AsRef<str> + std::hash::Hash + Eq>(map: &HashMap<S, Entry>) -> Vec<&str> {
    let mut v: Vec<(&str, &Entry)> = map.iter().map(|(k, e)| (k.as_ref(), e)).collect();
    sort_ranked(&mut v);
    v.into_iter().map(|(k, _)| k).collect()
}

fn sort_ranked(v: &mut [(&str, &Entry)]) {
    v.sort_by(|(ka, ea), (kb, eb)| {
        eb.score
            .partial_cmp(&ea.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| eb.last_date.cmp(&ea.last_date))
            .then_with(|| ka.cmp(kb))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::add::parser::RawPosting;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    fn txn(date: NaiveDate, desc: &str, accounts: &[(&str, &str, Option<&str>)]) -> Transaction {
        Transaction {
            date,
            description: desc.into(),
            comment: None,
            postings: accounts
                .iter()
                .map(|(a, amt, c)| RawPosting {
                    account: (*a).into(),
                    amount: (*amt).into(),
                    commodity: c.map(Into::into),
                    comment: None,
                })
                .collect(),
            file: 0,
            line: 1,
            pos: 0,
        }
    }

    fn journal(txns: Vec<Transaction>) -> Journal {
        Journal {
            transactions: txns,
            ..Journal::default()
        }
    }

    const TODAY: fn() -> NaiveDate = || d(2026, 8, 10);

    #[test]
    fn scores_sum_decayed_occurrences() {
        // Two occurrences, 0 and 90 days old: score = 1.0 + 0.5.
        let j = journal(vec![
            txn(d(2026, 8, 10), "Rewe", &[("e:g", "1 EUR", Some("EUR"))]),
            txn(d(2026, 5, 12), "Rewe", &[("e:g", "1 EUR", Some("EUR"))]),
        ]);
        let idx = Index::build(&j, TODAY(), 90.0);
        let e = &idx.descriptions["Rewe"];
        assert_eq!(e.count, 2);
        assert_eq!(e.last_date, d(2026, 8, 10));
        assert!((e.score - 1.5).abs() < 1e-9, "score = {}", e.score);
    }

    #[test]
    fn frequency_beats_a_single_recent_occurrence() {
        // Five month-old "Edeka"s outrank one fresh "Rewe".
        let mut txns: Vec<Transaction> = (0..5)
            .map(|i| {
                txn(
                    d(2026, 7, 10 + i),
                    "Edeka",
                    &[("e:g", "1 EUR", Some("EUR"))],
                )
            })
            .collect();
        txns.push(txn(
            d(2026, 8, 10),
            "Rewe",
            &[("e:g", "1 EUR", Some("EUR"))],
        ));
        let idx = Index::build(&journal(txns), TODAY(), 90.0);
        assert_eq!(idx.ranked_descriptions(), vec!["Edeka", "Rewe"]);
    }

    #[test]
    fn recency_wins_between_equal_counts() {
        let j = journal(vec![
            txn(d(2025, 1, 1), "Old", &[("a", "1 EUR", None)]),
            txn(d(2026, 8, 1), "New", &[("a", "1 EUR", None)]),
        ]);
        let idx = Index::build(&j, TODAY(), 90.0);
        assert_eq!(idx.ranked_descriptions(), vec!["New", "Old"]);
    }

    #[test]
    fn template_is_most_recent_not_highest_scoring() {
        // "Rewe" appears many times long ago with one posting set, and once
        // recently with another. The template must be the recent one even
        // though the old pattern dominates the score.
        let mut txns: Vec<Transaction> = (0..10)
            .map(|i| {
                txn(
                    d(2026, 1, 1 + i),
                    "Rewe",
                    &[("expenses:groceries", "1 EUR", Some("EUR"))],
                )
            })
            .collect();
        txns.push(txn(
            d(2026, 8, 9),
            "Rewe",
            &[("expenses:household", "5 EUR", Some("EUR"))],
        ));
        let j = journal(txns);
        let idx = Index::build(&j, TODAY(), 90.0);
        let tpl = idx.template(&j, "Rewe").unwrap();
        assert_eq!(tpl.postings[0].account, "expenses:household");
    }

    #[test]
    fn template_ties_on_date_go_to_the_later_occurrence() {
        let j = journal(vec![
            txn(d(2026, 8, 9), "Rewe", &[("first", "1 EUR", None)]),
            txn(d(2026, 8, 9), "Rewe", &[("second", "1 EUR", None)]),
        ]);
        let idx = Index::build(&j, TODAY(), 90.0);
        assert_eq!(
            idx.template(&j, "Rewe").unwrap().postings[0].account,
            "second"
        );
    }

    #[test]
    fn accounts_conditioned_on_description_rank_first() {
        let j = journal(vec![
            // Dominant account overall.
            txn(d(2026, 8, 1), "Salary", &[("assets:bank", "1 EUR", None)]),
            txn(d(2026, 8, 2), "Salary", &[("assets:bank", "1 EUR", None)]),
            txn(d(2026, 8, 3), "Salary", &[("assets:bank", "1 EUR", None)]),
            // At Rewe, groceries is what matters.
            txn(
                d(2026, 8, 5),
                "Rewe",
                &[
                    ("expenses:groceries", "1 EUR", None),
                    ("liabilities:cc", "-1 EUR", None),
                ],
            ),
        ]);
        let idx = Index::build(&j, TODAY(), 90.0);
        let ranked = idx.ranked_accounts(Some("Rewe"));
        assert_eq!(ranked[0], "expenses:groceries");
        assert_eq!(ranked[1], "liabilities:cc");
        // The unconditioned pool follows, without duplicates.
        assert_eq!(ranked[2], "assets:bank");
        assert_eq!(ranked.len(), 3);

        // Unconditioned: the dominant account leads.
        assert_eq!(idx.ranked_accounts(None)[0], "assets:bank");
    }

    #[test]
    fn templates_key_on_the_payee_so_a_unique_note_does_not_defeat_them() {
        // The case that matters: every transaction carries a distinct note,
        // so keying on the whole description would mean no template ever
        // matches and the pre-fill silently does nothing.
        let j = journal(vec![
            txn(
                d(2026, 8, 1),
                "Deutsche Bahn | ticket to Koeln",
                &[("expenses:travel:train", "31 EUR", None)],
            ),
            txn(
                d(2026, 8, 5),
                "Deutsche Bahn | ticket to Hamburg",
                &[("expenses:travel:long", "49 EUR", None)],
            ),
        ]);
        let idx = Index::build(&j, TODAY(), 90.0);
        let tpl = idx.template(&j, "Deutsche Bahn").unwrap();
        // Most recent wins, as always.
        assert_eq!(tpl.postings[0].account, "expenses:travel:long");
        // The whole description is not a key.
        assert!(idx
            .template(&j, "Deutsche Bahn | ticket to Koeln")
            .is_none());
    }

    #[test]
    fn account_conditioning_keys_on_the_payee_too() {
        let j = journal(vec![
            txn(
                d(2026, 8, 1),
                "Salary | july",
                &[("assets:bank", "1 EUR", None)],
            ),
            txn(
                d(2026, 8, 2),
                "Salary | august",
                &[("assets:bank", "1 EUR", None)],
            ),
            txn(
                d(2026, 8, 5),
                "Rewe | big shop",
                &[("expenses:groceries", "1 EUR", None)],
            ),
        ]);
        let idx = Index::build(&j, TODAY(), 90.0);
        // Conditioning on the payee finds both Salary transactions' account
        // first, even though neither note repeats.
        assert_eq!(idx.ranked_accounts(Some("Salary"))[0], "assets:bank");
        assert_eq!(idx.ranked_accounts(Some("Rewe"))[0], "expenses:groceries");
    }

    #[test]
    fn descriptions_and_payees_are_indexed_separately() {
        let j = journal(vec![
            txn(d(2026, 8, 1), "Rewe | big shop", &[("a", "1 EUR", None)]),
            txn(d(2026, 8, 2), "Rewe | small shop", &[("a", "1 EUR", None)]),
        ]);
        let idx = Index::build(&j, TODAY(), 90.0);
        // Descriptions stay whole — that is what gets typed and re-used.
        let mut descs = idx.ranked_descriptions();
        descs.sort_unstable();
        assert_eq!(descs, vec!["Rewe | big shop", "Rewe | small shop"]);
        // Payees collapse to the one counterparty.
        assert_eq!(idx.ranked_payees(), vec!["Rewe"]);
        assert_eq!(idx.payees["Rewe"].count, 2);
    }

    #[test]
    fn commodities_are_indexed_from_posting_tokens() {
        let j = journal(vec![
            txn(d(2026, 8, 1), "x", &[("a", "1 EUR", Some("EUR"))]),
            txn(d(2026, 8, 2), "y", &[("a", "1 USD", Some("USD"))]),
            txn(d(2026, 8, 3), "z", &[("a", "1 EUR", Some("EUR"))]),
        ]);
        let idx = Index::build(&j, TODAY(), 90.0);
        assert_eq!(idx.ranked_commodities(), vec!["EUR", "USD"]);
    }

    #[test]
    fn empty_descriptions_index_nothing_but_accounts_still_count() {
        let j = journal(vec![txn(d(2026, 8, 1), "", &[("a", "1 EUR", None)])]);
        let idx = Index::build(&j, TODAY(), 90.0);
        assert!(idx.descriptions.is_empty());
        assert!(idx.templates.is_empty());
        assert_eq!(idx.accounts.len(), 1);
    }
}
