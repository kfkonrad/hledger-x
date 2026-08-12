//! The inline raw-mode terminal frontend.
//!
//! No alternate screen: normal scrollback is preserved and the program
//! controls only the lines around the cursor — the log of completed
//! transactions, the live transaction block, a separator carrying the running
//! imbalance, the prompt, and either the ghost suggestion or the completion
//! menu. The log lives in the live frame rather than in scrollback, so that
//! undoing a transaction takes it back off the screen; it is printed
//! permanently into scrollback only when the session ends.

use std::io::{self, Write};

use crossterm::cursor::{MoveToColumn, MoveUp};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::style::{Attribute, Print, SetAttribute};
use crossterm::terminal::{self, Clear, ClearType};
use crossterm::QueueableCommand;

use super::plain::{render_done, rewrite_recovery};
use super::{Session, Submit};
use crate::add::write::{NewTransaction, Recovery};
use crate::amount::render_amount_like;

/// Open completion menu state.
struct Menu {
    candidates: Vec<String>,
    selected: usize,
    offset: usize,
}

/// A transient message under the prompt.
enum Note {
    None,
    Info(String),
    Error(String),
    /// Waiting for y/n on a new account.
    Confirm(String),
}

/// RAII raw-mode guard: always restores the terminal.
struct RawMode;

impl RawMode {
    fn enter() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        Ok(Self)
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
    }
}

/// Run the interactive session until Ctrl-D. Returns the completed
/// transactions.
///
/// # Errors
///
/// Terminal I/O.
pub fn run(session: &mut Session, recovery: &Recovery) -> io::Result<Vec<NewTransaction>> {
    let mut out = io::stdout();
    replay(session, recovery, &mut out)?;
    let _raw = RawMode::enter()?;
    let mut ui = Ui {
        menu: None,
        note: Note::None,
        drawn: 0,
        cursor_row: 0,
        menu_rows: 8,
    };
    loop {
        ui.draw(session, &mut out)?;
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match ui.handle(session, recovery, key, &mut out)? {
            Flow::Continue => {}
            Flow::Quit => break,
        }
    }
    ui.retire(session, &mut out)?;
    Ok(session.completed.clone())
}

/// Print recovered transactions, outside raw mode.
fn replay(session: &mut Session, recovery: &Recovery, out: &mut impl Write) -> io::Result<()> {
    let pending = recovery.pending();
    if pending.is_empty() {
        return Ok(());
    }
    writeln!(
        out,
        "recovered {} transaction(s) from an interrupted session",
        pending.len()
    )?;
    for t in pending {
        session.completed.push(t);
    }
    Ok(())
}

enum Flow {
    Continue,
    Quit,
}

struct Ui {
    menu: Option<Menu>,
    note: Note,
    /// Lines drawn in the last frame.
    drawn: usize,
    /// Row (within the frame) the terminal cursor was left on.
    cursor_row: usize,
    /// Menu entries that fit below the prompt, recomputed each draw from
    /// the terminal height.
    menu_rows: usize,
}

impl Ui {
    fn handle(
        &mut self,
        session: &mut Session,
        recovery: &Recovery,
        key: KeyEvent,
        out: &mut impl Write,
    ) -> io::Result<Flow> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        // A pending new-account confirmation swallows y/n/Esc.
        if matches!(self.note, Note::Confirm(_)) {
            match key.code {
                KeyCode::Char('y' | 'Y') => {
                    self.note = Note::None;
                    let r = session.submit_confirmed();
                    return Ok(self.apply_submit(session, recovery, r));
                }
                KeyCode::Char('n' | 'N') | KeyCode::Esc => self.note = Note::None,
                _ => {}
            }
            return Ok(Flow::Continue);
        }

        if ctrl {
            return self.handle_ctrl(session, key, out);
        }
        match key.code {
            KeyCode::Char(c) => {
                session.draft.insert(c);
                self.menu = None;
                self.note = Note::None;
            }
            KeyCode::Backspace => {
                session.draft.backspace();
                self.menu = None;
            }
            KeyCode::Left => {
                session.draft.cursor = session.draft.cursor.saturating_sub(1);
            }
            KeyCode::Right => {
                let len = session.draft.buffer.chars().count();
                if session.draft.cursor < len {
                    session.draft.cursor = session.draft.cursor.saturating_add(1);
                } else if let Some(s) = session.suggestion() {
                    // → on the empty field picks up the grey suggestion.
                    session.draft.set_buffer(&s);
                    self.menu = None;
                } else if let Some(g) = ghost(session) {
                    // → at end of line accepts the ghost suggestion.
                    session.draft.set_buffer(&g);
                    self.menu = None;
                }
            }
            KeyCode::Up | KeyCode::Down | KeyCode::Tab | KeyCode::BackTab => {
                self.navigate(session, key.code);
            }
            KeyCode::Esc => {
                self.menu = None;
                self.note = Note::None;
            }
            KeyCode::Enter => {
                if let Some(m) = self.menu.take() {
                    if let Some(c) = m.candidates.get(m.selected) {
                        session.draft.set_buffer(c);
                    }
                } else {
                    let r = session.submit();
                    return Ok(self.apply_submit(session, recovery, r));
                }
            }
            _ => {}
        }
        Ok(Flow::Continue)
    }

    /// The navigation keys: within an open menu they move the selection,
    /// otherwise ↑/↓ move between fields and Tab/Shift-Tab pick up the grey
    /// suggestion or open the menu. On an empty buffer that menu is the
    /// field's whole candidate list, so it doubles as history.
    fn navigate(&mut self, session: &mut Session, code: KeyCode) {
        let rows = self.menu_rows;
        if let Some(m) = &mut self.menu {
            let last = m.candidates.len().saturating_sub(1);
            m.selected = match code {
                KeyCode::Up => m.selected.saturating_sub(1),
                KeyCode::Down => m.selected.saturating_add(1).min(last),
                KeyCode::Tab => m
                    .selected
                    .saturating_add(1)
                    .checked_rem(m.candidates.len())
                    .unwrap_or(0),
                _ => m.selected.checked_sub(1).unwrap_or(last),
            };
            m.scroll(rows);
            return;
        }
        match code {
            KeyCode::Up => {
                session.draft.nav_up();
                self.note = Note::None;
            }
            KeyCode::Down => {
                session.draft.nav_down();
                self.note = Note::None;
            }
            KeyCode::Tab | KeyCode::BackTab => {
                if let Some(s) = session.suggestion() {
                    session.draft.set_buffer(&s);
                } else if let Some(t) = session.complete_buffer() {
                    // Tab completes as far as the candidates unanimously
                    // agree. If that settled it, get out of the way;
                    // otherwise show what is left to choose between.
                    session.draft.set_buffer(&t);
                    let candidates = session.candidates();
                    if candidates.len() > 1 {
                        self.open_menu(candidates);
                    } else {
                        self.menu = None;
                    }
                } else {
                    let candidates = session.candidates();
                    self.open_menu(candidates);
                }
            }
            _ => {}
        }
    }

    /// Control-key chords.
    fn handle_ctrl(
        &mut self,
        session: &mut Session,
        key: KeyEvent,
        out: &mut impl Write,
    ) -> io::Result<Flow> {
        match key.code {
            KeyCode::Char('c') => {
                session.reset_draft();
                self.menu = None;
                self.note = Note::Info("transaction aborted".to_owned());
            }
            KeyCode::Char('d') => return Ok(Flow::Quit),
            KeyCode::Char('u') => {
                // Clear what is typed. On an already-empty account prompt
                // there is nothing to clear, so it refuses the suggested
                // account instead — the only way to reach "finish the
                // transaction" while an account is being offered. Elsewhere
                // an empty buffer means Ctrl-U has nothing to do.
                if session.draft.buffer.is_empty() {
                    session.draft.dismiss_suggestion();
                } else {
                    session.draft.set_buffer("");
                }
                self.menu = None;
            }
            KeyCode::Char('w') => {
                let account_mode = matches!(session.draft.field, super::Field::Account(_));
                session.draft.delete_word(account_mode);
                self.menu = None;
            }
            KeyCode::Char('e') => {
                self.menu = None;
                self.edit_in_editor(session, out)?;
            }
            _ => {}
        }
        Ok(Flow::Continue)
    }

    fn apply_submit(&mut self, session: &mut Session, recovery: &Recovery, result: Submit) -> Flow {
        match result {
            Submit::Advanced => self.note = Note::None,
            Submit::AdvancedWithNote(n) => self.note = Note::Info(n),
            Submit::Invalid(m) => self.note = Note::Error(m),
            Submit::Confirm { question } => {
                self.note = Note::Confirm(format!("{question} [y/n]"));
            }
            Submit::Quit => return Flow::Quit,
            Submit::Undo => {
                if let Some(t) = session.undo() {
                    rewrite_recovery(session, recovery);
                    self.note = Note::Info(format!(
                        "undid: {} {}",
                        t.date.format("%Y-%m-%d"),
                        t.description
                    ));
                }
            }
            Submit::Done(txn) => {
                let txn = *txn;
                if let Err(e) = recovery.record(&txn) {
                    self.note = Note::Error(format!(
                        "could not write the recovery journal: {}",
                        crate::errors::io_reason(&e)
                    ));
                }
                // The finished block joins the in-frame log; the next draw
                // paints it above the prompt. Nothing goes to scrollback
                // yet — `u` may still take it back.
                session.complete(txn);
            }
        }
        Flow::Continue
    }

    fn open_menu(&mut self, candidates: Vec<String>) {
        if candidates.is_empty() {
            self.note = Note::Info("no completions".to_owned());
        } else {
            self.menu = Some(Menu {
                candidates,
                selected: 0,
                offset: 0,
            });
        }
    }

    /// Erase the live frame entirely (before printing permanent output).
    fn clear_frame(&mut self, out: &mut impl Write) -> io::Result<()> {
        if self.drawn > 0 {
            let up = u16::try_from(self.cursor_row).unwrap_or(u16::MAX);
            if up > 0 {
                out.queue(MoveUp(up))?;
            }
            out.queue(MoveToColumn(0))?
                .queue(Clear(ClearType::FromCursorDown))?;
        }
        self.drawn = 0;
        self.cursor_row = 0;
        Ok(())
    }

    /// Called on exit: drop the live frame and hand the log — everything
    /// that survived undo — to scrollback, where it stays after the program
    /// is gone.
    fn retire(&mut self, session: &Session, out: &mut impl Write) -> io::Result<()> {
        self.clear_frame(out)?;
        for txn in &session.completed {
            for l in render_done(session, txn) {
                out.queue(Print(l))?.queue(Print("\r\n"))?;
            }
            out.queue(Print("\r\n"))?;
        }
        out.flush()
    }

    /// Redraw the whole live frame.
    fn draw(&mut self, session: &Session, out: &mut impl Write) -> io::Result<()> {
        let (width, height) =
            terminal::size().map_or((80, 24), |(w, h)| (usize::from(w), usize::from(h)));
        let width = width.max(20);
        // A row is one or more (text, dim) spans.
        let mut rows: Vec<Vec<(String, bool)>> = Vec::new();

        let preview = session.preview_lines();
        // What the log may use: whatever the rest of the frame — preview,
        // separator, prompt, one note or menu row, one spare — leaves over.
        let log_budget = height.saturating_sub(preview.len()).saturating_sub(4);
        rows.extend(log_rows(session, log_budget));

        for l in preview {
            rows.push(vec![(format!("  {l}"), false)]);
        }
        rows.push(vec![(separator(session, width), true)]);

        let label = session.draft.field.label();
        let prompt_row = rows.len();
        let prompt_col = format!("  {label} › ")
            .chars()
            .count()
            .saturating_add(session.draft.cursor);
        let mut prompt = vec![(format!("  {label} › {}", session.draft.buffer), false)];
        if self.menu.is_none() {
            // The grey inline suggestion sits after the (empty) buffer.
            if let Some(s) = session.suggestion() {
                prompt.push((s, true));
            }
        }
        rows.push(prompt);

        // The menu gets whatever fits below the prompt, scrolling beyond
        // that; a footer row and one spare line stay reserved.
        let menu_rows = height.saturating_sub(rows.len()).saturating_sub(2).max(1);
        self.menu_rows = menu_rows;

        match (&self.menu, &self.note) {
            (Some(m), _) => {
                let end = m.offset.saturating_add(menu_rows).min(m.candidates.len());
                for (i, c) in m
                    .candidates
                    .get(m.offset..end)
                    .unwrap_or(&[])
                    .iter()
                    .enumerate()
                {
                    let idx = m.offset.saturating_add(i);
                    let marker = if idx == m.selected { "▸" } else { " " };
                    rows.push(vec![(format!("  {marker} {c}"), idx != m.selected)]);
                }
                if m.candidates.len() > menu_rows {
                    rows.push(vec![(
                        format!(
                            "    {}–{} of {}",
                            m.offset.saturating_add(1),
                            end,
                            m.candidates.len()
                        ),
                        true,
                    )]);
                }
            }
            (None, Note::None) => {
                if let Some(g) = ghost(session) {
                    rows.push(vec![(format!("    {g}  ← → to accept"), true)]);
                } else if let Some(h) = session.hint() {
                    rows.push(vec![(format!("  {h}"), true)]);
                }
            }
            (None, Note::Info(n) | Note::Confirm(n)) => {
                rows.push(vec![(format!("  {n}"), true)]);
            }
            (None, Note::Error(e)) => rows.push(vec![(format!("  ✗ {e}"), false)]),
        }

        self.paint(&rows, width, (prompt_row, prompt_col), out)
    }

    /// Paint a built frame: move to its top, clear down, print every row and
    /// park the cursor on the prompt.
    fn paint(
        &mut self,
        rows: &[Vec<(String, bool)>],
        width: usize,
        prompt: (usize, usize),
        out: &mut impl Write,
    ) -> io::Result<()> {
        let (prompt_row, prompt_col) = prompt;
        if self.drawn > 0 {
            let up = u16::try_from(self.cursor_row).unwrap_or(u16::MAX);
            if up > 0 {
                out.queue(MoveUp(up))?;
            }
        }
        out.queue(MoveToColumn(0))?
            .queue(Clear(ClearType::FromCursorDown))?;
        for (i, spans) in rows.iter().enumerate() {
            let mut remaining = width;
            for (text, dim) in spans {
                let clipped: String = text.chars().take(remaining).collect();
                remaining = remaining.saturating_sub(clipped.chars().count());
                if *dim {
                    out.queue(SetAttribute(Attribute::Dim))?;
                }
                out.queue(Print(&clipped))?;
                if *dim {
                    out.queue(SetAttribute(Attribute::Reset))?;
                }
            }
            if i.saturating_add(1) < rows.len() {
                out.queue(Print("\r\n"))?;
            }
        }
        // Park the cursor on the prompt line at the edit position.
        let below = rows.len().saturating_sub(1).saturating_sub(prompt_row);
        if below > 0 {
            out.queue(MoveUp(u16::try_from(below).unwrap_or(u16::MAX)))?;
        }
        out.queue(MoveToColumn(
            u16::try_from(prompt_col.min(width.saturating_sub(1))).unwrap_or(u16::MAX),
        ))?;
        out.flush()?;
        self.drawn = rows.len();
        self.cursor_row = prompt_row;
        Ok(())
    }

    /// Ctrl-E: round-trip the draft through `$EDITOR`.
    fn edit_in_editor(&mut self, session: &mut Session, out: &mut impl Write) -> io::Result<()> {
        let text = editor_text(session);
        let dir = std::env::temp_dir();
        let path = dir.join(format!("hledger-x-edit-{}.journal", std::process::id()));
        std::fs::write(&path, &text)?;

        terminal::disable_raw_mode()?;
        self.clear_frame(out)?;
        out.flush()?;
        let editor = std::env::var("VISUAL")
            .or_else(|_| std::env::var("EDITOR"))
            .unwrap_or_else(|_| "vi".to_owned());
        let status = std::process::Command::new(&editor).arg(&path).status();
        terminal::enable_raw_mode()?;

        match status {
            Ok(s) if s.success() => {
                let edited = std::fs::read_to_string(&path).unwrap_or_default();
                if let Some(txn) = crate::add::write::parse_transactions(&edited)
                    .into_iter()
                    .next()
                {
                    session.load_draft_from(&txn);
                    self.note = Note::Info("reloaded from editor".to_owned());
                } else {
                    self.note = Note::Error("editor result did not parse; draft kept".to_owned());
                }
            }
            Ok(_) => self.note = Note::Info("editor exited nonzero; draft kept".to_owned()),
            // A missing editor is the usual cause here, and `$EDITOR` is what
            // to change; the OS's word for it on its own explains nothing.
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                self.note = Note::Error(format!("no editor called {editor} — check $EDITOR"));
            }
            Err(e) => {
                self.note = Note::Error(format!(
                    "could not run {editor}: {}",
                    crate::errors::io_reason(&e)
                ));
            }
        }
        let _ = std::fs::remove_file(&path);
        Ok(())
    }
}

impl Menu {
    /// Keep the selection within a `rows`-tall visible window.
    const fn scroll(&mut self, rows: usize) {
        let rows = if rows == 0 { 1 } else { rows };
        if self.selected < self.offset {
            self.offset = self.selected;
        }
        let bottom = self.offset.saturating_add(rows);
        if self.selected >= bottom {
            self.offset = self.selected.saturating_sub(rows).saturating_add(1);
        }
    }
}

/// The completed transactions, as frame rows: newest last, each block
/// followed by a blank line. Only the newest ones that fit in `budget` rows
/// are kept; the rest are stood in for by a dim elision line. Rendering this
/// from `session.completed` on every draw is what makes an undone
/// transaction disappear.
fn log_rows(session: &Session, budget: usize) -> Vec<Vec<(String, bool)>> {
    if budget == 0 || session.completed.is_empty() {
        return Vec::new();
    }
    // Take blocks newest-first for as long as they fit.
    let mut blocks: Vec<Vec<String>> = Vec::new();
    let mut used = 0usize;
    for txn in session.completed.iter().rev() {
        let mut block = render_done(session, txn);
        block.push(String::new());
        let next = used.saturating_add(block.len());
        if next > budget {
            break;
        }
        used = next;
        blocks.push(block);
    }
    let mut hidden = session.completed.len().saturating_sub(blocks.len());
    // The elision line needs a row of its own; buy it back from the oldest
    // block shown.
    if hidden > 0 && used.saturating_add(1) > budget && blocks.pop().is_some() {
        hidden = hidden.saturating_add(1);
    }
    let mut rows: Vec<Vec<(String, bool)>> = Vec::new();
    if hidden > 0 {
        rows.push(vec![(format!("  … {hidden} earlier transaction(s)"), true)]);
    }
    for block in blocks.iter().rev() {
        for l in block {
            rows.push(vec![(format!("  {l}").trim_end().to_owned(), false)]);
        }
    }
    rows
}

/// The separator line: a rule, with the running imbalance surfaced when it
/// is nonzero (or unknown).
fn separator(session: &Session, width: usize) -> String {
    // The same amounts the balancing prefill is computed from, so the status
    // line and the amount it will offer are written the same way.
    let amounts = session.draft.committed_amounts();
    let status = session.running_imbalance().map_or_else(
        || " imbalance unknown ".to_owned(),
        |sums| {
            if sums.is_empty() {
                String::new()
            } else {
                let parts: Vec<String> = sums
                    .iter()
                    .map(|(c, v)| render_amount_like(*v, c, &session.ctx.amount_ctx, &amounts))
                    .collect();
                format!(" imbalance {} ", parts.join(", "))
            }
        },
    );
    let mut line = String::from("  ");
    line.push_str(&"─".repeat(6));
    line.push_str(&status);
    let used = line.chars().count();
    line.push_str(&"─".repeat(width.saturating_sub(used).saturating_sub(2)));
    line
}

/// The draft as editor text (Ctrl-E).
fn editor_text(session: &Session) -> String {
    let mut lines = session.preview_lines();
    for l in &mut lines {
        *l = l.trim_start().to_owned();
    }
    // Postings need their indent back.
    let mut out = String::new();
    for (i, l) in lines.iter().enumerate() {
        if i > 0 {
            out.push_str("    ");
        }
        out.push_str(l);
        out.push('\n');
    }
    out
}

/// The ghost suggestion: the top candidate, when it extends the buffer.
fn ghost(session: &Session) -> Option<String> {
    if session.draft.buffer.trim().is_empty() {
        return None;
    }
    let top = session.candidates().into_iter().next()?;
    (top != session.draft.buffer).then_some(top)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::add::parser::{parse_journal, FileMap};
    use crate::add::ui::SessionCtx;
    use crate::add::write::Posting;
    use crate::config::Config;
    use chrono::NaiveDate;
    use std::fs;
    use tempfile::TempDir;

    const JOURNAL: &str = "\
2026-08-05 Rewe
    expenses:groceries        12.00 EUR
    assets:bank:checking     -12.00 EUR
";

    fn session() -> (Session, TempDir) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("main.journal");
        fs::write(&path, JOURNAL).unwrap();
        let journal = parse_journal(&path).unwrap();
        let map = FileMap::build(JOURNAL);
        let today = NaiveDate::from_ymd_opt(2026, 8, 10).unwrap();
        let ctx = SessionCtx::new(journal, Config::default(), today, path, &map);
        (Session::new(ctx), dir)
    }

    fn complete(s: &mut Session, description: &str) {
        s.complete(NewTransaction {
            date: NaiveDate::from_ymd_opt(2026, 8, 10).unwrap(),
            description: description.to_owned(),
            comment: String::new(),
            postings: vec![
                Posting::new("expenses:groceries", "1 EUR"),
                Posting::new("assets:bank:checking", "-1 EUR"),
            ],
        });
    }

    fn text(rows: &[Vec<(String, bool)>]) -> String {
        rows.iter()
            .map(|r| r.iter().map(|(t, _)| t.as_str()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn ui() -> Ui {
        Ui {
            menu: None,
            note: Note::None,
            drawn: 0,
            cursor_row: 0,
            menu_rows: 8,
        }
    }

    /// Get to the first account prompt of a transaction.
    fn at_account(s: &mut Session) {
        s.submit();
        s.draft.set_buffer("Rewe");
        s.submit();
    }

    #[test]
    fn tab_inserts_a_unique_completion_and_leaves_the_menu_shut() {
        let (mut s, _t) = session();
        let mut ui = ui();
        at_account(&mut s);
        s.draft.set_buffer("roc");
        ui.navigate(&mut s, KeyCode::Tab);
        assert_eq!(s.draft.buffer, "expenses:groceries");
        assert!(ui.menu.is_none());
    }

    #[test]
    fn tab_on_an_ambiguous_query_completes_as_far_as_it_can_and_opens_the_menu() {
        let (mut s, _t) = session();
        let mut ui = ui();
        at_account(&mut s);
        // Both accounts match `e`, and they agree on nothing, so this is
        // the pure-menu case.
        s.draft.set_buffer("e");
        ui.navigate(&mut s, KeyCode::Tab);
        assert_eq!(s.draft.buffer, "e");
        let menu = ui.menu.as_ref().expect("menu should open");
        assert_eq!(menu.candidates.len(), 2);
        // A second Tab cycles it rather than re-completing.
        ui.navigate(&mut s, KeyCode::Tab);
        assert_eq!(ui.menu.as_ref().map(|m| m.selected), Some(1));
    }

    /// What the user actually sees. The prompt paints the buffer un-dimmed
    /// and the suggestion dimmed, so a suggested account must reach the
    /// screen inside a dim span and *not* inside the buffer span.
    #[test]
    fn a_suggested_account_is_painted_dim_and_not_into_the_buffer() {
        let (mut s, _t) = session();
        let mut ui = ui();
        at_account(&mut s);
        let mut out: Vec<u8> = Vec::new();
        ui.draw(&s, &mut out).unwrap();
        let painted = String::from_utf8_lossy(&out).into_owned();

        let dim = "\u{1b}[2m";
        let reset = "\u{1b}[0m";
        let prompt_at = painted.find("account 1").expect("prompt should be painted");
        let tail = painted.get(prompt_at..).unwrap();
        // The account text appears, and the span it sits in starts dim.
        let account_at = tail
            .find("expenses:groceries")
            .expect("the suggested account should be painted");
        let before = tail.get(..account_at).unwrap();
        let last_dim = before.rfind(dim);
        let last_reset = before.rfind(reset);
        assert!(
            last_dim.is_some() && last_dim > last_reset,
            "the suggested account is painted un-dimmed:\n{}",
            painted.escape_debug()
        );
    }

    #[test]
    fn tab_picks_up_a_suggested_account_rather_than_opening_the_list() {
        let (mut s, _t) = session();
        let mut ui = ui();
        at_account(&mut s);
        // "Rewe" has history, so account 1 is suggested; Tab puts it in the
        // buffer to edit, exactly as it does with the date and amount ghosts.
        assert_eq!(s.suggestion().as_deref(), Some("expenses:groceries"));
        ui.navigate(&mut s, KeyCode::Tab);
        assert_eq!(s.draft.buffer, "expenses:groceries");
        assert!(ui.menu.is_none());
    }

    #[test]
    fn tab_on_an_empty_account_opens_the_whole_list() {
        let (mut s, _t) = session();
        let mut ui = ui();
        // A description with no history suggests nothing, so Tab browses the
        // whole pool — which is how history is reached.
        s.submit();
        s.draft.set_buffer("Brand New Payee");
        s.submit();
        assert_eq!(s.suggestion(), None);
        ui.navigate(&mut s, KeyCode::Tab);
        assert!(s.draft.buffer.is_empty());
        assert!(ui.menu.is_some());
    }

    #[test]
    fn tab_with_no_match_leaves_the_buffer_alone() {
        let (mut s, _t) = session();
        let mut ui = ui();
        at_account(&mut s);
        s.draft.set_buffer("zzz");
        ui.navigate(&mut s, KeyCode::Tab);
        assert_eq!(s.draft.buffer, "zzz");
        assert!(ui.menu.is_none());
        assert!(matches!(ui.note, Note::Info(_)));
    }

    #[test]
    fn an_undone_transaction_leaves_the_log() {
        let (mut s, _t) = session();
        complete(&mut s, "Rewe");
        complete(&mut s, "Edeka");
        let before = text(&log_rows(&s, 40));
        assert!(before.contains("Rewe"), "{before}");
        assert!(before.contains("Edeka"), "{before}");

        assert!(s.undo().is_some());
        let after = text(&log_rows(&s, 40));
        assert!(after.contains("Rewe"), "{after}");
        assert!(!after.contains("Edeka"), "{after}");

        assert!(s.undo().is_some());
        assert!(log_rows(&s, 40).is_empty());
    }

    #[test]
    fn a_log_taller_than_the_budget_keeps_the_newest_and_elides_the_rest() {
        let (mut s, _t) = session();
        for d in ["One", "Two", "Three"] {
            complete(&mut s, d);
        }
        // Each block is 3 lines plus its blank: room for one, and the
        // elision line.
        let rows = log_rows(&s, 5);
        let out = text(&rows);
        assert!(out.starts_with("  … 2 earlier transaction(s)"), "{out}");
        assert!(out.contains("Three"), "{out}");
        assert!(!out.contains("Two"), "{out}");
        assert!(rows.len() <= 5, "{out}");
    }

    #[test]
    fn a_budget_too_small_for_any_block_shows_only_the_elision() {
        let (mut s, _t) = session();
        complete(&mut s, "One");
        assert_eq!(text(&log_rows(&s, 2)), "  … 1 earlier transaction(s)");
        assert!(log_rows(&s, 0).is_empty());
    }
}
