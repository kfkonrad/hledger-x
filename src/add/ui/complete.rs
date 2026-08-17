//! Completion: matching, and the text Tab inserts.
//!
//! Three styles (`prefix`, `substring`, `fuzzy`) over two candidate shapes.
//! Account names are colon-segmented and every style matches *within* a
//! segment — never across a `:`. Descriptions and commodities are plain
//! strings and the same styles apply to the whole of them.
//!
//! `prefix` is anchored: query segment *i* against candidate segment *i*,
//! no gaps, so `gro` never reaches `expenses:groceries` and `a:s` never
//! reaches `assets:bank:savings`. `substring` and `fuzzy` match segments in
//! order but allow gaps, which is what lets a bare `check` find
//! `assets:bank:checking`.
//!
//! Candidate lists arrive already frecency-ranked. For `prefix` and
//! `substring` that order is preserved; `fuzzy` re-ranks by match quality
//! first (that is its point), with frecency as the tie-break. Ranking never
//! drives Tab insertion — see [`complete`], where only unanimity does.

use crate::config::Completion;

/// What a candidate list is shaped like.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// Colon-segmented account names.
    Account,
    /// Plain strings: descriptions, commodities.
    Plain,
}

/// Whether `candidate` matches `query` under `style`, with a match quality
/// (higher is better; only `fuzzy` produces meaningful gradations).
#[must_use]
pub fn match_quality(style: Completion, shape: Shape, query: &str, candidate: &str) -> Option<i64> {
    let q = query.to_lowercase();
    let c = candidate.to_lowercase();
    match shape {
        Shape::Plain => segment_quality(style, &q, &c),
        Shape::Account => account_quality(style, &q, &c),
    }
}

/// One query segment against one candidate segment — the whole string, for
/// [`Shape::Plain`].
fn segment_quality(style: Completion, q: &str, c: &str) -> Option<i64> {
    match style {
        Completion::Prefix => c.starts_with(q).then_some(0),
        Completion::Substring => c.contains(q).then_some(0),
        Completion::Fuzzy => fuzzy_quality(q, c),
    }
}

/// Segment-wise matching; see the module docs for why `prefix` is anchored
/// and the other two are not.
fn account_quality(style: Completion, q: &str, c: &str) -> Option<i64> {
    let qs: Vec<&str> = q.split(':').collect();
    let cs: Vec<&str> = c.split(':').collect();
    if style == Completion::Prefix {
        if qs.len() > cs.len() {
            return None;
        }
        let mut score: i64 = 0;
        for (i, qp) in qs.iter().enumerate() {
            let cp = cs.get(i)?;
            score = score.saturating_add(segment_quality(style, qp, cp)?);
        }
        return Some(score);
    }
    // In order, gaps allowed, earliest match wins.
    let mut ci = 0usize;
    let mut score: i64 = 0;
    for qp in &qs {
        let mut hit = None;
        while let Some(cp) = cs.get(ci) {
            ci = ci.saturating_add(1);
            if let Some(s) = segment_quality(style, qp, cp) {
                hit = Some(s);
                break;
            }
        }
        score = score.saturating_add(hit?);
    }
    Some(score)
}

/// Subsequence match within one segment; quality favours contiguity and
/// early matches.
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
    let start_penalty = i64::try_from(first_hit.unwrap_or(0))
        .unwrap_or(i64::MAX)
        .min(20);
    want.is_none()
        .then_some(score.saturating_sub(start_penalty))
}

/// Filter and order `ranked` (already frecency-ordered, best first) for
/// `query`.
#[must_use]
pub fn filter_ranked<'a>(
    style: Completion,
    shape: Shape,
    query: &str,
    ranked: &[&'a str],
) -> Vec<&'a str> {
    let mut hits: Vec<(usize, i64, &str)> = ranked
        .iter()
        .enumerate()
        .filter_map(|(i, c)| match_quality(style, shape, query, c).map(|s| (i, s, *c)))
        .collect();
    if style == Completion::Fuzzy {
        hits.sort_by(|(ia, sa, _), (ib, sb, _)| sb.cmp(sa).then_with(|| ia.cmp(ib)));
    }
    hits.into_iter().map(|(_, _, c)| c).collect()
}

/// The text Tab should insert for `query` over `pool`, or `None` when there
/// is nothing to add.
///
/// The insertion is the longest prefix of the matches' common prefix that is
/// both longer than what is typed and still matches exactly the same
/// candidates. That second condition is the important one: completing must
/// never widen the match set, which would silently drop a constraint the
/// user typed (`bank:ing` may resolve to `assets:bank:` only while nothing
/// else lives under `assets:bank`). When no prefix qualifies the caller
/// falls back to opening the menu.
///
/// An empty query never completes — an empty buffer opens the field's whole
/// candidate list instead, which doubles as history.
#[must_use]
pub fn complete(style: Completion, shape: Shape, query: &str, pool: &[&str]) -> Option<String> {
    if query.is_empty() {
        return None;
    }
    let matches = filter_ranked(style, shape, query, pool);
    if matches.is_empty() {
        return None;
    }
    let typed = query.chars().count();
    let mut chars: Vec<char> = common_prefix(&matches).chars().collect();
    while chars.len() > typed {
        let attempt: String = chars.iter().collect();
        if same_set(&filter_ranked(style, shape, &attempt, pool), &matches) {
            return Some(attempt);
        }
        chars.pop();
    }
    None
}

/// The longest common character prefix of `items`.
fn common_prefix(items: &[&str]) -> String {
    let mut iter = items.iter();
    let Some(first) = iter.next() else {
        return String::new();
    };
    let mut len = first.chars().count();
    for item in iter {
        len = first
            .chars()
            .zip(item.chars())
            .take(len)
            .take_while(|(a, b)| a == b)
            .count();
    }
    first.chars().take(len).collect()
}

/// Set equality: `fuzzy` reorders by score, so the same candidates can come
/// back in a different order for a different query.
fn same_set(a: &[&str], b: &[&str]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut a: Vec<&str> = a.to_vec();
    let mut b: Vec<&str> = b.to_vec();
    a.sort_unstable();
    b.sort_unstable();
    a == b
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The corpus the design scenarios were written against.
    const POOL: &[&str] = &[
        "assets:bank:checking",
        "assets:bank:savings",
        "assets:cash",
        "expenses:groceries",
        "expenses:gifts",
        "expenses:travel:train",
        "expenses:travel:taxi",
        "income:salary",
    ];

    fn hits(style: Completion, query: &str) -> Vec<&'static str> {
        let mut v = filter_ranked(style, Shape::Account, query, POOL);
        v.sort_unstable();
        v
    }

    fn tab(style: Completion, query: &str) -> Option<String> {
        complete(style, Shape::Account, query, POOL)
    }

    // ---- prefix: anchored, no gaps ----

    #[test]
    fn prefix_is_anchored_segment_by_segment() {
        assert_eq!(
            hits(Completion::Prefix, "ex:gro"),
            vec!["expenses:groceries"]
        );
        assert_eq!(
            hits(Completion::Prefix, "ass:b"),
            vec!["assets:bank:checking", "assets:bank:savings"]
        );
        // A query may be shorter than the candidate, never longer.
        assert!(hits(Completion::Prefix, "a:b:c:d").is_empty());
    }

    #[test]
    fn prefix_allows_no_gaps_leading_or_otherwise() {
        // `gro` is not a leading segment of anything.
        assert!(hits(Completion::Prefix, "gro").is_empty());
        // `s` would have to skip `bank` to reach `savings`.
        assert!(hits(Completion::Prefix, "a:s").is_empty());
        assert_eq!(
            hits(Completion::Prefix, "a:b:s"),
            vec!["assets:bank:savings"]
        );
    }

    #[test]
    fn prefix_tab_completes() {
        assert_eq!(
            tab(Completion::Prefix, "i").as_deref(),
            Some("income:salary")
        );
        assert_eq!(tab(Completion::Prefix, "ass").as_deref(), Some("assets:"));
        assert_eq!(
            tab(Completion::Prefix, "ass:b").as_deref(),
            Some("assets:bank:")
        );
        assert_eq!(
            tab(Completion::Prefix, "ass:b:c").as_deref(),
            Some("assets:bank:checking")
        );
        // The shared `t` of train/taxi comes along.
        assert_eq!(
            tab(Completion::Prefix, "ex:t").as_deref(),
            Some("expenses:travel:t")
        );
        assert_eq!(
            tab(Completion::Prefix, "ex:g").as_deref(),
            Some("expenses:g")
        );
    }

    // ---- substring: gaps allowed, within a segment ----

    #[test]
    fn substring_completes_a_unique_mid_word_match_outright() {
        assert_eq!(
            tab(Completion::Substring, "check").as_deref(),
            Some("assets:bank:checking")
        );
        assert_eq!(
            tab(Completion::Substring, "ash").as_deref(),
            Some("assets:cash")
        );
        assert_eq!(
            tab(Completion::Substring, "ift").as_deref(),
            Some("expenses:gifts")
        );
    }

    #[test]
    fn substring_fills_in_segments_the_user_never_typed() {
        // `an` matches only the `bank` segment; both matches agree on the
        // path above it.
        assert_eq!(
            tab(Completion::Substring, "an").as_deref(),
            Some("assets:bank:")
        );
        assert_eq!(
            tab(Completion::Substring, "bank:ing").as_deref(),
            Some("assets:bank:")
        );
        assert_eq!(
            tab(Completion::Substring, "sets:cash").as_deref(),
            Some("assets:cash")
        );
    }

    #[test]
    fn substring_never_matches_across_a_colon() {
        assert!(hits(Completion::Substring, "tsbank").is_empty());
        // Segment-wise, the same characters do match.
        assert_eq!(
            hits(Completion::Substring, "ts:ba"),
            vec!["assets:bank:checking", "assets:bank:savings"]
        );
    }

    #[test]
    fn a_typed_segment_that_constrains_nothing_is_dropped() {
        // `as:a` and `assets:` select the same three accounts, so replacing
        // the buffer wholesale loses nothing — the trailing `a` was doing no
        // work.
        assert_eq!(
            hits(Completion::Substring, "as:a"),
            vec!["assets:bank:checking", "assets:bank:savings", "assets:cash"]
        );
        assert_eq!(
            tab(Completion::Substring, "as:a").as_deref(),
            Some("assets:")
        );
    }

    #[test]
    fn gaps_reach_deep_and_a_short_query_can_stay_ambiguous() {
        // `a:a` also picks up `travel:train` and `travel:taxi`, so there is
        // no unanimous prefix and Tab defers to the menu.
        assert_eq!(hits(Completion::Substring, "a:a").len(), 5);
        assert_eq!(tab(Completion::Substring, "a:a"), None);
    }

    // ---- fuzzy: subsequence, within a segment ----

    #[test]
    fn fuzzy_matches_within_a_segment_only() {
        assert_eq!(
            hits(Completion::Fuzzy, "ckng"),
            vec!["assets:bank:checking"]
        );
        assert_eq!(hits(Completion::Fuzzy, "sl"), vec!["income:salary"]);
        // `asbk` spans `assets:bank` — deliberately no longer a match.
        assert!(hits(Completion::Fuzzy, "asbk").is_empty());
        assert_eq!(
            hits(Completion::Fuzzy, "as:bk"),
            vec!["assets:bank:checking", "assets:bank:savings"]
        );
    }

    #[test]
    fn fuzzy_tab_completes() {
        assert_eq!(
            tab(Completion::Fuzzy, "ckng").as_deref(),
            Some("assets:bank:checking")
        );
        assert_eq!(
            tab(Completion::Fuzzy, "bnk").as_deref(),
            Some("assets:bank:")
        );
        assert_eq!(
            tab(Completion::Fuzzy, "exp:trn").as_deref(),
            Some("expenses:travel:train")
        );
    }

    #[test]
    fn fuzzy_ranks_by_quality_but_never_completes_to_the_top_hit() {
        let ranked = filter_ranked(Completion::Fuzzy, Shape::Account, "ss", POOL);
        assert!(ranked.len() > 1);
        // Ambiguous: the menu opens, nothing is inserted.
        assert_eq!(tab(Completion::Fuzzy, "ss"), None);
    }

    // ---- shared Tab rules ----

    #[test]
    fn tab_makes_no_progress_when_there_is_nothing_to_add() {
        // Already exactly a candidate.
        assert_eq!(tab(Completion::Prefix, "assets:cash"), None);
        // Ambiguous with no shared characters beyond what is typed.
        assert_eq!(tab(Completion::Prefix, "assets:"), None);
        // No match at all — the buffer is never destroyed.
        assert_eq!(tab(Completion::Prefix, "zzz"), None);
    }

    #[test]
    fn an_empty_query_opens_the_menu_rather_than_completing() {
        assert_eq!(tab(Completion::Substring, ""), None);
        assert_eq!(complete(Completion::Prefix, Shape::Plain, "", POOL), None);
    }

    #[test]
    fn completion_never_widens_the_match_set() {
        // With a third sibling under `assets:bank`, `ing` is load-bearing:
        // resolving to `assets:bank:` would pull `cash` back in.
        let pool: &[&str] = &[
            "assets:bank:checking",
            "assets:bank:savings",
            "assets:bank:cash",
        ];
        let matched = filter_ranked(Completion::Substring, Shape::Account, "bank:ing", pool);
        assert_eq!(matched.len(), 2);
        assert_eq!(
            complete(Completion::Substring, Shape::Account, "bank:ing", pool),
            None
        );
    }

    #[test]
    fn a_candidate_that_is_a_prefix_of_another_completes_without_a_trailing_colon() {
        let pool: &[&str] = &["assets:cash", "assets:cash:wallet"];
        assert_eq!(
            complete(Completion::Substring, Shape::Account, "cash", pool).as_deref(),
            Some("assets:cash")
        );
    }

    #[test]
    fn matching_is_case_insensitive_and_inserts_the_candidates_case() {
        assert_eq!(
            tab(Completion::Substring, "CHECK").as_deref(),
            Some("assets:bank:checking")
        );
    }

    // ---- plain shape: descriptions and commodities ----

    #[test]
    fn plain_candidates_are_never_segmented() {
        let pool: &[&str] = &["Rewe", "Rewe: refund", "Rossmann"];
        // A colon in a description is ordinary text, not a segment break.
        assert_eq!(
            filter_ranked(Completion::Substring, Shape::Plain, "e: ref", pool),
            vec!["Rewe: refund"]
        );
        // …and it completes across one, since there is no segment to stop at.
        assert_eq!(
            complete(Completion::Substring, Shape::Plain, "e: ref", pool).as_deref(),
            Some("Rewe: refund")
        );
    }

    #[test]
    fn plain_completion_stops_at_the_common_prefix() {
        let pool: &[&str] = &["Rewe", "Rewe refund", "Rossmann"];
        assert_eq!(
            complete(Completion::Prefix, Shape::Plain, "Re", pool).as_deref(),
            Some("Rewe")
        );
        assert_eq!(complete(Completion::Prefix, Shape::Plain, "R", pool), None);
    }

    #[test]
    fn filter_preserves_frecency_order_except_fuzzy() {
        let ranked = vec!["expenses:groceries", "expenses:gifts", "assets:bank"];
        assert_eq!(
            filter_ranked(Completion::Substring, Shape::Account, "e", &ranked),
            vec!["expenses:groceries", "expenses:gifts", "assets:bank"]
        );
        assert_eq!(
            filter_ranked(Completion::Substring, Shape::Account, "gi", &ranked),
            vec!["expenses:gifts"]
        );
    }
}
