//! Blank-line normalization.
//!
//! The file is split into top-level blocks — a transaction (header plus its
//! indented run), or anything else (a directive plus its indented
//! subdirectives, a price line, an `include`) — and the blank lines *between*
//! blocks are rewritten. Blanks inside a block are impossible by construction:
//! a block's indented run is consumed whole, so a blank line can never land
//! between a header and its postings, nor inside a directive's subdirective
//! block, where it would end that block.
//!
//! Three rules:
//!
//! 1. A run of blank lines collapses to exactly one — never to none, so an
//!    existing separation is never lost.
//! 2. Leading blank lines are dropped, and so are trailing ones (`unlines`
//!    then supplies the single final newline).
//! 3. A blank line is *inserted* where a boundary has none and either side is
//!    a transaction. Consecutive directives, `P` lines and `include`s stay
//!    dense, which is how they are written.
//!
//! Comments attach downward: a comment block directly above another block is
//! part of it, so the blank goes above the comment, never between the comment
//! and the transaction it heads. This is the same attachment [`super::sort`]
//! uses, so a comment that heads a transaction travels with it under `--sort`
//! and keeps its blank line here.
//!
//! A `comment` … `end comment` block counts as a single block and is taken
//! whole, so its contents — blank lines included — are never rewritten.
//!
//! Verified against hledger 1.99: transactions written back-to-back with no
//! blank line parse identically, and `hledger print` itself separates
//! transactions with exactly one blank line.

use crate::lex::{
    comment_block_len, is_blank, is_comment, is_indented_non_blank, opens_comment_block, opens_txn,
};

/// What a block is, for spacing purposes. Only "is it a transaction" matters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Txn,
    Other,
}

/// A top-level block: its lines, and whether a blank line preceded it in the
/// input (which is what a boundary between two non-transactions preserves).
#[derive(Debug)]
struct Block<'a> {
    kind: Kind,
    lines: Vec<&'a str>,
    blank_before: bool,
}

/// Rewrite the blank lines between top-level blocks.
#[must_use]
pub fn normalize<'a>(ls: &[&'a str]) -> Vec<&'a str> {
    let blocks = parse(ls);
    let mut out: Vec<&'a str> = Vec::with_capacity(ls.len());
    let mut prev: Option<Kind> = None;
    for b in blocks {
        if let Some(p) = prev {
            if b.blank_before || p == Kind::Txn || b.kind == Kind::Txn {
                out.push("");
            }
        }
        prev = Some(b.kind);
        out.extend(b.lines);
    }
    out
}

/// Split into top-level blocks, attaching leading comment lines to the block
/// below them.
fn parse<'a>(ls: &[&'a str]) -> Vec<Block<'a>> {
    let mut out: Vec<Block<'a>> = Vec::new();
    // Comment lines buffered until we know what they head, and whether a blank
    // line preceded the first of them.
    let mut pending: Vec<&'a str> = Vec::new();
    let mut pending_blank = false;
    // Whether a blank line preceded the buffered comments.
    let mut comments_blank = false;
    let mut rest = ls;

    while let Some((line, tail)) = rest.split_first() {
        rest = tail;
        if is_blank(line) {
            // Comments followed by a blank head nothing: they stand alone.
            if !pending.is_empty() {
                out.push(Block {
                    kind: Kind::Other,
                    lines: std::mem::take(&mut pending),
                    blank_before: comments_blank,
                });
            }
            pending_blank = true;
        } else if is_comment(line) {
            if pending.is_empty() {
                comments_blank = pending_blank;
            }
            pending.push(line);
            pending_blank = false;
        } else {
            // A `comment` block is opaque: it is taken whole, so the blank
            // lines inside it are never seen, let alone rewritten.
            let run_len = if opens_comment_block(line) {
                comment_block_len(tail)
            } else {
                tail.iter().take_while(|l| is_indented_non_blank(l)).count()
            };
            let (run, after) = tail.split_at(run_len);
            let blank_before = if pending.is_empty() {
                pending_blank
            } else {
                comments_blank
            };
            let mut lines = std::mem::take(&mut pending);
            lines.push(line);
            lines.extend_from_slice(run);
            out.push(Block {
                kind: if opens_txn(line) {
                    Kind::Txn
                } else {
                    Kind::Other
                },
                lines,
                blank_before,
            });
            pending_blank = false;
            rest = after;
        }
    }
    if !pending.is_empty() {
        out.push(Block {
            kind: Kind::Other,
            lines: pending,
            blank_before: comments_blank,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Normalize a `\n`-joined fixture, for readable expectations.
    fn norm(s: &str) -> String {
        let ls: Vec<&str> = s.split('\n').collect();
        normalize(&ls).join("\n")
    }

    #[test]
    fn a_run_of_blanks_between_transactions_collapses_to_one() {
        assert_eq!(
            norm("2025-01-01 a\n    e:x  1\n\n\n\n2025-01-02 b\n    e:x  2"),
            "2025-01-01 a\n    e:x  1\n\n2025-01-02 b\n    e:x  2"
        );
    }

    #[test]
    fn dense_transactions_gain_a_blank_line() {
        assert_eq!(
            norm("2025-01-01 a\n    e:x  1\n2025-01-02 b\n    e:x  2"),
            "2025-01-01 a\n    e:x  1\n\n2025-01-02 b\n    e:x  2"
        );
    }

    #[test]
    fn leading_and_trailing_blanks_are_dropped() {
        assert_eq!(
            norm("\n\n2025-01-01 a\n    e:x  1\n\n\n"),
            "2025-01-01 a\n    e:x  1"
        );
        assert_eq!(norm(""), "");
        assert_eq!(norm("\n\n"), "");
    }

    #[test]
    fn a_blank_never_lands_inside_a_transaction() {
        // The header and its postings are one block, so no rule can separate
        // them; the blank line inside the posting run is not restored either.
        assert_eq!(
            norm("2025-01-01 a\n    e:x  1\n\n    a:b  -1"),
            "2025-01-01 a\n    e:x  1\n\n    a:b  -1"
        );
    }

    #[test]
    fn consecutive_directives_stay_dense() {
        assert_eq!(
            norm("account a:b\naccount e:x\nP 2025-01-01 USD 1 EUR\ninclude other.j"),
            "account a:b\naccount e:x\nP 2025-01-01 USD 1 EUR\ninclude other.j"
        );
    }

    #[test]
    fn a_blank_between_directives_is_preserved_but_collapsed() {
        assert_eq!(
            norm("account a:b\n\n\naccount e:x"),
            "account a:b\n\naccount e:x"
        );
    }

    #[test]
    fn a_directive_abutting_a_transaction_gains_a_blank() {
        assert_eq!(
            norm("commodity USD\n    format 1,000.00 USD\n2025-01-01 a\n    e:x  1\nP 2025-06-01 USD 1 EUR"),
            "commodity USD\n    format 1,000.00 USD\n\n2025-01-01 a\n    e:x  1\n\nP 2025-06-01 USD 1 EUR"
        );
    }

    #[test]
    fn a_comment_heading_a_transaction_stays_glued_to_it() {
        assert_eq!(
            norm("2025-01-01 a\n    e:x  1\n; note about b\n2025-01-02 b\n    e:x  2"),
            "2025-01-01 a\n    e:x  1\n\n; note about b\n2025-01-02 b\n    e:x  2"
        );
    }

    #[test]
    fn a_comment_trailing_a_transaction_is_detached() {
        // Comments attach downward, so this one heads nothing and is pushed
        // away from the transaction above it.
        assert_eq!(
            norm("2025-01-01 a\n    e:x  1\n; trailing note"),
            "2025-01-01 a\n    e:x  1\n\n; trailing note"
        );
    }

    #[test]
    fn a_standalone_comment_block_keeps_its_separation() {
        assert_eq!(
            norm("; section header\n\naccount a:b"),
            "; section header\n\naccount a:b"
        );
        assert_eq!(norm("; a comment\naccount a:b"), "; a comment\naccount a:b");
    }

    #[test]
    fn a_comment_heading_a_directive_travels_with_it() {
        assert_eq!(
            norm("2025-01-01 a\n    e:x  1\n; about the price\nP 2025-06-01 USD 1 EUR"),
            "2025-01-01 a\n    e:x  1\n\n; about the price\nP 2025-06-01 USD 1 EUR"
        );
    }

    #[test]
    fn an_orphan_indented_run_is_a_block_of_its_own() {
        assert_eq!(
            norm("    stray line\n2025-01-01 a\n    e:x  1"),
            "    stray line\n\n2025-01-01 a\n    e:x  1"
        );
    }

    #[test]
    fn a_comment_block_is_opaque() {
        // Interior blank lines survive; the block gets one blank line on each
        // side because a transaction is involved.
        assert_eq!(
            norm("2025-01-01 a\n    e:x  1\ncomment\nprose\n\n\nmore prose\nend comment\n2025-01-02 b\n    e:x  2"),
            "2025-01-01 a\n    e:x  1\n\ncomment\nprose\n\n\nmore prose\nend comment\n\n2025-01-02 b\n    e:x  2"
        );
    }

    #[test]
    fn an_unterminated_comment_block_swallows_the_rest_of_the_file() {
        // hledger accepts this and treats everything after `comment` as
        // prose; so must we, trailing blank lines and all.
        assert_eq!(
            norm("2025-01-01 a\n    e:x  1\n\ncomment\nprose\n\n2025-01-02 not a transaction\n\n"),
            "2025-01-01 a\n    e:x  1\n\ncomment\nprose\n\n2025-01-02 not a transaction\n\n"
        );
    }

    #[test]
    fn an_indented_end_comment_does_not_close_the_block() {
        assert_eq!(
            norm("comment\n  end comment\n\n2025-01-01 a\n    e:x  1"),
            "comment\n  end comment\n\n2025-01-01 a\n    e:x  1"
        );
    }

    #[test]
    fn normalization_is_idempotent() {
        for case in [
            "2025-01-01 a\n    e:x  1\n2025-01-02 b\n    e:x  2",
            "\n\n; head\n2025-01-01 a\n    e:x  1\n\n\naccount a:b\naccount e:x\n\n",
            "; only a comment",
            "2025-01-01 a\n    e:x  1\ncomment\n\n\nprose\nend comment\naccount a:b",
            "comment\nunterminated\n\n",
            "",
        ] {
            let once = norm(case);
            assert_eq!(norm(&once), once, "not idempotent: {case:?}");
        }
    }
}
