//! Directive-bounded stable sort of transactions by date.
//!
//! Directives and standalone comment blocks are barriers: transactions
//! reorder only within the runs between them, which keeps positional
//! directives in scope. A comment line directly above a transaction (no blank
//! line between) travels with it.

use crate::lex::{is_blank, is_comment, is_indented_non_blank, opens_txn};

/// A transaction's sort key: its primary date as `(year, month, day)`.
type DateKey = (i64, i64, i64);

/// One chunk of the file for sorting purposes.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Entry<'a> {
    /// A single blank line: a separator, kept in place.
    Blank,
    /// A directive or standalone comment block: a barrier.
    Anchor(Vec<&'a str>),
    /// A transaction: its date key, and its lines (any leading comment lines
    /// attached above the header, the header, and its posting lines).
    Txn(DateKey, Vec<&'a str>),
}

/// Stably sort transactions by date within each directive-bounded run, then
/// flatten back to lines.
#[must_use]
pub fn sort_entries<'a>(ls: &[&'a str]) -> Vec<&'a str> {
    let entries = sort_runs(parse_entries(ls));
    entries.into_iter().flat_map(entry_lines).collect()
}

fn entry_lines(e: Entry<'_>) -> Vec<&str> {
    match e {
        Entry::Blank => vec![""],
        Entry::Anchor(ls) | Entry::Txn(_, ls) => ls,
    }
}

/// Split the file into entries, attaching leading comment lines to the
/// transaction they head. A comment followed by a blank line is standalone,
/// and therefore a barrier.
fn parse_entries<'a>(ls: &[&'a str]) -> Vec<Entry<'a>> {
    let mut out: Vec<Entry<'a>> = Vec::new();
    // Buffered leading comment lines awaiting the transaction they head.
    let mut pending: Vec<&'a str> = Vec::new();
    let mut rest = ls;

    while let Some((line, tail)) = rest.split_first() {
        if is_blank(line) {
            flush(&mut pending, &mut out);
            out.push(Entry::Blank);
            rest = tail;
        } else if is_comment(line) {
            pending.push(line);
            rest = tail;
        } else {
            let run_len = tail.iter().take_while(|l| is_indented_non_blank(l)).count();
            let (run, after) = tail.split_at(run_len);
            let mut block = std::mem::take(&mut pending);
            block.push(line);
            block.extend_from_slice(run);
            out.push(if opens_txn(line) {
                Entry::Txn(date_key(line), block)
            } else {
                Entry::Anchor(block)
            });
            rest = after;
        }
    }
    flush(&mut pending, &mut out);
    out
}

fn flush<'a>(pending: &mut Vec<&'a str>, out: &mut Vec<Entry<'a>>) {
    if !pending.is_empty() {
        out.push(Entry::Anchor(std::mem::take(pending)));
    }
}

/// Reorder transactions within each maximal run bounded by anchors.
fn sort_runs(entries: Vec<Entry<'_>>) -> Vec<Entry<'_>> {
    let mut out: Vec<Entry<'_>> = Vec::with_capacity(entries.len());
    let mut run: Vec<Entry<'_>> = Vec::new();
    for e in entries {
        if matches!(e, Entry::Anchor(_)) {
            out.append(&mut sort_run(std::mem::take(&mut run)));
            out.push(e);
        } else {
            run.push(e);
        }
    }
    out.append(&mut sort_run(run));
    out
}

/// Stably sort the transactions in a run by date, leaving blank separators in
/// their original positions.
fn sort_run(run: Vec<Entry<'_>>) -> Vec<Entry<'_>> {
    let mut sorted: Vec<Entry<'_>> = run
        .iter()
        .filter(|e| matches!(e, Entry::Txn(_, _)))
        .cloned()
        .collect();
    sorted.sort_by_key(|e| match e {
        Entry::Txn(k, _) => *k,
        Entry::Blank | Entry::Anchor(_) => (0, 0, 0),
    });
    let mut filled = sorted.into_iter();
    run.into_iter()
        .map(|e| match e {
            Entry::Txn(_, _) => filled.next().unwrap_or(e),
            other => other,
        })
        .collect()
}

/// The sort key from a transaction header: its primary date parsed to
/// `(year, month, day)`. Unparseable dates sort first; the stable sort then
/// preserves their source order.
fn date_key(line: &str) -> DateKey {
    let token = line.split_whitespace().next().unwrap_or_default();
    // Drop any =secondary-date.
    let primary = token.split('=').next().unwrap_or_default();
    let parts: Vec<i64> = primary
        .split(['/', '-', '.'])
        .map(|p| p.parse::<i64>().unwrap_or(0))
        .collect();
    match parts.as_slice() {
        [y, m, d] => (*y, *m, *d),
        [m, d] => (0, *m, *d),
        _ => (0, 0, 0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sorted(src: &str) -> String {
        let ls: Vec<&str> = src.split('\n').collect();
        sort_entries(&ls).join("\n")
    }

    #[test]
    fn date_keys_from_transaction_headers() {
        assert_eq!(date_key("2025-03-02 later"), (2025, 3, 2));
        assert_eq!(date_key("2025/03/02"), (2025, 3, 2));
        assert_eq!(date_key("2025.03.02 payee"), (2025, 3, 2));
        // A secondary date is dropped.
        assert_eq!(date_key("2025-03-02=2025-03-09 payee"), (2025, 3, 2));
        // Two components mean no year.
        assert_eq!(date_key("03-02 payee"), (0, 3, 2));
        // Unparseable components become zero; unparseable shapes sort first.
        assert_eq!(date_key("2025-XX-02 payee"), (2025, 0, 2));
        assert_eq!(date_key("garbage"), (0, 0, 0));
    }

    #[test]
    fn transactions_sort_by_date_within_a_run() {
        let src = "2025-03-02 b\n    A:B  1 USD\n\n2025-01-05 a\n    A:B  2 USD\n";
        assert_eq!(
            sorted(src),
            "2025-01-05 a\n    A:B  2 USD\n\n2025-03-02 b\n    A:B  1 USD\n"
        );
    }

    #[test]
    fn equal_dates_keep_their_source_order() {
        let src = "2025-01-05 B\n\n2025-01-05 A\n";
        assert_eq!(sorted(src), "2025-01-05 B\n\n2025-01-05 A\n");
    }

    #[test]
    fn directives_are_barriers() {
        // The February transaction sits after a price directive and must stay
        // there, even though it is older than the January one above it.
        let src = "2025-03-02 b\n\nP 2025-06-01 USD 1 EUR\n\n2025-02-01 a\n";
        assert_eq!(sorted(src), src);
    }

    #[test]
    fn a_comment_directly_above_a_transaction_travels_with_it() {
        let src = "; about b\n2025-03-02 b\n\n2025-01-05 a\n";
        assert_eq!(sorted(src), "2025-01-05 a\n\n; about b\n2025-03-02 b\n");
    }

    #[test]
    fn a_comment_followed_by_a_blank_line_is_a_barrier() {
        let src = "2025-03-02 b\n\n; standalone\n\n2025-01-05 a\n";
        assert_eq!(sorted(src), src);
    }

    #[test]
    fn blank_lines_stay_in_their_original_positions() {
        // Three transactions, uneven blank separation: the blanks do not move.
        let src = "2025-03-01 c\n\n\n2025-01-01 a\n\n2025-02-01 b\n";
        assert_eq!(
            sorted(src),
            "2025-01-01 a\n\n\n2025-02-01 b\n\n2025-03-01 c\n"
        );
    }

    #[test]
    fn posting_lines_travel_with_their_header() {
        let src = "2025-03-02 b\n    A:B  1 USD\n    C:D\n2025-01-05 a\n    E:F  2 USD\n";
        assert_eq!(
            sorted(src),
            "2025-01-05 a\n    E:F  2 USD\n2025-03-02 b\n    A:B  1 USD\n    C:D\n"
        );
    }
}
