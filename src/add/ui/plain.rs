//! Plain line-based entry, used when stdin is not a terminal (pipes,
//! scripts, tests). Same state machine as the interactive mode, no raw-mode
//! anything.

use std::io::{BufRead, Write};

use super::{Session, Submit};
use crate::add::write::{NewTransaction, Recovery};

/// Drive a session over plain line I/O until EOF. Returns the completed
/// transactions.
///
/// Protocol per prompt: the pre-fill is shown in brackets; an empty line
/// accepts it, a line of a single `.` clears it (entering "empty"), anything
/// else replaces it. EOF ends the session (like Ctrl-D).
///
/// # Errors
///
/// Only I/O errors on the streams.
pub fn run<R: BufRead, W: Write>(
    session: &mut Session,
    recovery: &Recovery,
    input: &mut R,
    out: &mut W,
) -> std::io::Result<Vec<NewTransaction>> {
    replay_recovery(session, recovery, out)?;
    loop {
        let field = session.draft.field;
        // What Enter would take: the text already in the buffer, or the ghost
        // suggestion offered for an empty one.
        let offered = if session.draft.buffer.is_empty() {
            session.suggestion().unwrap_or_default()
        } else {
            session.draft.buffer.clone()
        };
        let hint = session
            .hint()
            .map_or_else(String::new, |h| format!(" ({h})"));
        if offered.is_empty() {
            write!(out, "{}{hint}: ", field.label())?;
        } else {
            write!(out, "{} [{offered}]{hint}: ", field.label())?;
        }
        out.flush()?;
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            break; // EOF — save everything completed
        }
        let line = line.trim_end_matches(['\n', '\r']);
        match line {
            "" => {} // accept whatever is offered
            // `.` is this frontend's Ctrl-U: refuse what is offered.
            "." => {
                session.draft.set_buffer("");
                session.draft.dismiss_suggestion();
            }
            other => session.draft.set_buffer(other),
        }
        match session.submit() {
            Submit::Advanced => {}
            Submit::AdvancedWithNote(note) => writeln!(out, "note: {note}")?,
            Submit::Invalid(msg) => writeln!(out, "error: {msg}")?,
            Submit::Quit => break,
            Submit::Confirm { question } => {
                write!(out, "{question} [y/N]: ")?;
                out.flush()?;
                let mut answer = String::new();
                if input.read_line(&mut answer)? == 0 {
                    break;
                }
                if answer.trim().eq_ignore_ascii_case("y") {
                    match session.submit_confirmed() {
                        Submit::Invalid(msg) => writeln!(out, "error: {msg}")?,
                        Submit::Done(txn) => {
                            finish_txn(session, recovery, *txn, out)?;
                        }
                        _ => {}
                    }
                } else {
                    writeln!(out, "kept the field for editing")?;
                }
            }
            Submit::Undo => {
                if let Some(t) = session.undo() {
                    rewrite_recovery(session, recovery);
                    writeln!(
                        out,
                        "undid: {} {}",
                        t.date.format("%Y-%m-%d"),
                        t.description
                    )?;
                }
            }
            Submit::Done(txn) => finish_txn(session, recovery, *txn, out)?,
        }
    }
    Ok(session.completed.clone())
}

/// Record, echo and bank one finished transaction.
fn finish_txn<W: Write>(
    session: &mut Session,
    recovery: &Recovery,
    txn: NewTransaction,
    out: &mut W,
) -> std::io::Result<()> {
    if let Err(e) = recovery.record(&txn) {
        writeln!(
            out,
            "warning: could not write the recovery journal: {}",
            crate::errors::io_reason(&e)
        )?;
    }
    for l in render_done(session, &txn) {
        writeln!(out, "{l}")?;
    }
    session.complete(txn);
    Ok(())
}

/// Replay transactions a dead session left in the recovery journal.
fn replay_recovery<W: Write>(
    session: &mut Session,
    recovery: &Recovery,
    out: &mut W,
) -> std::io::Result<()> {
    let pending = recovery.pending();
    if pending.is_empty() {
        return Ok(());
    }
    writeln!(
        out,
        "recovered {} transaction(s) from an interrupted session ({})",
        pending.len(),
        recovery.path().display()
    )?;
    for t in pending {
        session.completed.push(t);
    }
    Ok(())
}

/// Rewrite the recovery journal to match `session.completed` (after undo).
pub fn rewrite_recovery(session: &Session, recovery: &Recovery) {
    recovery.clear();
    for t in &session.completed {
        let _ = recovery.record(t);
    }
}

/// The finished transaction, rendered as it will be written (at the file's
/// widths, grown by its own content).
#[must_use]
pub fn render_done(session: &Session, txn: &NewTransaction) -> Vec<String> {
    use crate::fmt::posting::{account_of, parse_posting, render, Posting};
    let raw = txn.raw_lines();
    let mut acc_w = session.ctx.acc_w;
    let mut num_w = session.ctx.num_w;
    let parsed: Vec<Posting> = raw.iter().skip(1).map(|l| parse_posting(l)).collect();
    for p in &parsed {
        if let Some(a) = account_of(p) {
            acc_w = acc_w.max(a.chars().count());
        }
        if let Posting::Amount { num, .. } = p {
            num_w = num_w.max(num.chars().count());
        }
    }
    let mut out = vec![raw.first().cloned().unwrap_or_default()];
    for p in &parsed {
        out.push(render(acc_w, num_w, p));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::add::parser::{parse_journal, FileMap};
    use crate::add::ui::SessionCtx;
    use crate::config::Config;
    use chrono::NaiveDate;
    use std::fs;
    use std::io::Cursor;
    use tempfile::TempDir;

    fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 8, 10).unwrap()
    }

    fn run_session(journal_src: &str, input: &str) -> (Vec<NewTransaction>, String, TempDir) {
        run_session_with(journal_src, Config::default(), input)
    }

    fn run_session_with(
        journal_src: &str,
        config: Config,
        input: &str,
    ) -> (Vec<NewTransaction>, String, TempDir) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("main.journal");
        fs::write(&path, journal_src).unwrap();
        let journal = parse_journal(&path).unwrap();
        let map = FileMap::build(journal_src);
        let ctx = SessionCtx::new(journal, config, today(), path, &map);
        let mut session = Session::new(ctx);
        let recovery = Recovery::at(dir.path().join("recovery.journal"));
        let mut out = Vec::new();
        let done = run(
            &mut session,
            &recovery,
            &mut Cursor::new(input.as_bytes()),
            &mut out,
        )
        .unwrap();
        (done, String::from_utf8(out).unwrap(), dir)
    }

    const JOURNAL: &str = "\
2026-08-05 Rewe
    expenses:groceries        12.00 EUR
    assets:bank:checking     -12.00 EUR
";

    #[test]
    fn a_full_transaction_via_pipes() {
        // date(accept today), desc, account(accept template), amount,
        // account 2 (accept), amount (accept balancing), empty account =
        // finish; EOF ends.
        let input = "\n Rewe\n\n18.20 EUR\n\n\n.\n";
        let (done, out, _t) = run_session(JOURNAL, input);
        assert_eq!(done.len(), 1, "output was:\n{out}");
        assert_eq!(done[0].description, "Rewe");
        assert_eq!(
            done[0].postings,
            vec![
                ("expenses:groceries".to_owned(), "18.20 EUR".to_owned()),
                ("assets:bank:checking".to_owned(), "-18.20 EUR".to_owned()),
            ]
        );
        // The finished transaction is echoed, formatted.
        assert!(out.contains("2026-08-10 Rewe"), "{out}");
    }

    #[test]
    fn eof_mid_draft_still_returns_completed_transactions() {
        let input = "\nRewe\n\n18.20 EUR\n\n\n.\n\nEdeka\n";
        let (done, _out, _t) = run_session(JOURNAL, input);
        assert_eq!(done.len(), 1);
    }

    #[test]
    fn invalid_input_reprompts() {
        let input = "banana\n";
        let (done, out, _t) = run_session(JOURNAL, input);
        assert!(done.is_empty());
        assert!(out.contains("error: cannot understand date"), "{out}");
    }

    #[test]
    fn strict_confirmation_via_pipe() {
        let src = "account expenses:groceries\naccount assets:cash\ncommodity 1.00 EUR\n";
        let cfg = Config {
            strict: true,
            ..Config::default()
        };
        // date, desc, undeclared account → y, amount, second account
        // (declared), empty amount writes the balancing amount and finishes.
        let input = "\nX\nexpenses:grocerys\ny\n5 EUR\nassets:cash\n\n";
        let (done, out, _t) = run_session_with(src, cfg, input);
        assert_eq!(done.len(), 1, "{out}");
        assert!(out.contains("not a declared account"), "{out}");
        assert!(out.contains("use it anyway"), "{out}");
        assert!(out.contains("did you mean expenses:groceries"), "{out}");
        assert_eq!(done[0].postings[0].0, "expenses:grocerys");
        // The empty amount was written out explicitly, in the declared style.
        assert_eq!(done[0].postings[1].1, "-5.00 EUR");
    }

    #[test]
    fn declining_an_undeclared_account_stays_on_the_field() {
        let src = "account expenses:groceries\n";
        let cfg = Config {
            strict: true,
            ..Config::default()
        };
        let input = "\nX\nexpenses:zzz\nn\n";
        let (done, out, _t) = run_session_with(src, cfg, input);
        assert!(done.is_empty());
        assert!(out.contains("kept the field"), "{out}");
    }

    #[test]
    fn recovery_is_replayed_and_undo_works() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("main.journal");
        fs::write(&path, JOURNAL).unwrap();
        let recovery = Recovery::at(dir.path().join("recovery.journal"));
        // A dead session left one transaction behind.
        recovery
            .record(&NewTransaction {
                date: today(),
                description: "Ghost".into(),
                postings: vec![("a".into(), "1 EUR".into()), ("b".into(), "-1 EUR".into())],
            })
            .unwrap();

        let journal = parse_journal(&path).unwrap();
        let map = FileMap::build(JOURNAL);
        let ctx = SessionCtx::new(journal, Config::default(), today(), path, &map);
        let mut session = Session::new(ctx);
        let mut out = Vec::new();
        // Undo it via `u` at the date prompt, then EOF.
        let done = run(
            &mut session,
            &recovery,
            &mut Cursor::new(b"u\n".as_slice()),
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("recovered 1 transaction"), "{text}");
        assert!(text.contains("undid: 2026-08-10 Ghost"), "{text}");
        assert!(done.is_empty());
        assert!(recovery.pending().is_empty());
    }
}
