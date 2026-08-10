//! Completion matching.
//!
//! Four strategies (`prefix`, `substring`, `segment`, `fuzzy`), applied over
//! candidate lists that arrive already frecency-ranked. For the first three
//! the frecency order is preserved; `fuzzy` re-ranks by match quality first
//! (that is its point), with frecency as the tie-break.

use crate::config::Matching;

/// Whether `candidate` matches `query` under `strategy`, with a match
/// quality (higher is better; only `fuzzy` produces meaningful gradations).
#[must_use]
pub fn match_quality(strategy: Matching, query: &str, candidate: &str) -> Option<i64> {
    let q = query.to_lowercase();
    let c = candidate.to_lowercase();
    match strategy {
        Matching::Prefix => c.starts_with(&q).then_some(0),
        Matching::Substring => c.contains(&q).then_some(0),
        Matching::Segment => segment_match(&q, &c).then_some(0),
        Matching::Fuzzy => fuzzy_quality(&q, &c),
    }
}

/// `ex:gro` → `expenses:groceries`: split the query on `:`, prefix-match
/// each component against the candidate's components, in order but allowing
/// gaps (`ex:tra` matches `expenses:travel:train`).
fn segment_match(query: &str, candidate: &str) -> bool {
    let q_parts: Vec<&str> = query.split(':').collect();
    let c_parts: Vec<&str> = candidate.split(':').collect();
    let mut ci = 0usize;
    for qp in &q_parts {
        let mut found = false;
        while let Some(cp) = c_parts.get(ci) {
            ci = ci.saturating_add(1);
            if cp.starts_with(qp) {
                found = true;
                break;
            }
        }
        if !found {
            return false;
        }
    }
    true
}

/// Subsequence match; quality favours contiguity and early matches.
fn fuzzy_quality(query: &str, candidate: &str) -> Option<i64> {
    if query.is_empty() {
        return Some(0);
    }
    let mut score: i64 = 0;
    let mut last_hit: Option<usize> = None;
    let mut first_hit: Option<usize> = None;
    let mut qchars = query.chars().filter(|c| !c.is_whitespace());
    let mut want = qchars.next();
    for (i, c) in candidate.chars().enumerate() {
        let Some(w) = want else { break };
        if c == w {
            // Contiguity dominates; an early start is a mild tie-break.
            let adjacency = match last_hit {
                Some(prev) if i == prev.saturating_add(1) => 15,
                _ => 0,
            };
            score = score.saturating_add(10).saturating_add(adjacency);
            first_hit = first_hit.or(Some(i));
            last_hit = Some(i);
            want = qchars.next();
        }
    }
    let start_penalty = i64::try_from(first_hit.unwrap_or(0)).unwrap_or(i64::MAX).min(20);
    want.is_none()
        .then_some(score.saturating_sub(start_penalty))
}

/// Filter and order `ranked` (already frecency-ordered, best first) for
/// `query`. The account default is substring switching to segment on a
/// colon; pass the strategy already resolved.
#[must_use]
pub fn filter_ranked<'a>(strategy: Matching, query: &str, ranked: &[&'a str]) -> Vec<&'a str> {
    let mut hits: Vec<(usize, i64, &str)> = ranked
        .iter()
        .enumerate()
        .filter_map(|(i, c)| match_quality(strategy, query, c).map(|s| (i, s, *c)))
        .collect();
    if strategy == Matching::Fuzzy {
        hits.sort_by(|(ia, sa, _), (ib, sb, _)| sb.cmp(sa).then_with(|| ia.cmp(ib)));
    }
    hits.into_iter().map(|(_, _, c)| c).collect()
}

/// The strategy actually in effect for an account query: `segment` as soon
/// as the query contains a colon, otherwise the configured one.
#[must_use]
pub fn account_strategy(configured: Matching, query: &str) -> Matching {
    if query.contains(':') {
        Matching::Segment
    } else {
        configured
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_and_substring() {
        assert!(match_quality(Matching::Prefix, "exp", "expenses:food").is_some());
        assert!(match_quality(Matching::Prefix, "food", "expenses:food").is_none());
        assert!(match_quality(Matching::Substring, "food", "expenses:food").is_some());
        assert!(match_quality(Matching::Substring, "FOOD", "expenses:food").is_some());
    }

    #[test]
    fn segment_matches_component_prefixes() {
        assert!(match_quality(Matching::Segment, "ex:gro", "expenses:groceries").is_some());
        assert!(match_quality(Matching::Segment, "ex:tra", "expenses:travel:train").is_some());
        // Gaps are allowed.
        assert!(match_quality(Matching::Segment, "ex:tra:tr", "expenses:travel:train").is_some());
        assert!(match_quality(Matching::Segment, "as:che", "assets:bank:checking").is_some());
        // Order matters.
        assert!(match_quality(Matching::Segment, "gro:ex", "expenses:groceries").is_none());
        assert!(match_quality(Matching::Segment, "xx:gro", "expenses:groceries").is_none());
    }

    #[test]
    fn fuzzy_is_a_subsequence_ranked_by_quality() {
        assert!(match_quality(Matching::Fuzzy, "egro", "expenses:groceries").is_some());
        assert!(match_quality(Matching::Fuzzy, "zzz", "expenses:groceries").is_none());
        // Contiguous beats scattered.
        let tight = match_quality(Matching::Fuzzy, "gro", "expenses:groceries").unwrap();
        let loose = match_quality(Matching::Fuzzy, "gro", "gifts:relatives:others").unwrap();
        assert!(tight > loose, "{tight} vs {loose}");
    }

    #[test]
    fn filter_preserves_frecency_order_except_fuzzy() {
        let ranked = vec!["expenses:groceries", "expenses:gifts", "assets:bank"];
        assert_eq!(
            filter_ranked(Matching::Substring, "e", &ranked),
            vec!["expenses:groceries", "expenses:gifts", "assets:bank"]
        );
        assert_eq!(
            filter_ranked(Matching::Substring, "gi", &ranked),
            vec!["expenses:gifts"]
        );
    }

    #[test]
    fn account_strategy_switches_on_colon() {
        assert_eq!(account_strategy(Matching::Substring, "gro"), Matching::Substring);
        assert_eq!(account_strategy(Matching::Substring, "ex:gro"), Matching::Segment);
        assert_eq!(account_strategy(Matching::Fuzzy, "gro"), Matching::Fuzzy);
    }

    #[test]
    fn empty_query_matches_everything() {
        let ranked = vec!["a", "b"];
        assert_eq!(filter_ranked(Matching::Substring, "", &ranked), vec!["a", "b"]);
        assert_eq!(filter_ranked(Matching::Fuzzy, "", &ranked), vec!["a", "b"]);
    }
}
