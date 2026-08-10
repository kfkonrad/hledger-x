//! The inline raw-mode terminal frontend.
//!
//! No alternate screen: normal scrollback is preserved and the program
//! controls only a few lines around the cursor — the live transaction block,
//! a separator carrying the running imbalance, the prompt, and either the
//! ghost suggestion or the completion menu. Every completed transaction is
//! printed permanently into scrollback.

use std::io::{self, Write};

use crossterm::cursor::{MoveToColumn, MoveUp};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::style::{Attribute, Print, SetAttribute};
use crossterm::terminal::{self, Clear, ClearType};
use crossterm::QueueableCommand;

use super::plain::{render_done, rewrite_recovery};
use super::{Session, Submit};
use crate::add::amount::render_amount;
use crate::add::write::{NewTransaction, Recovery};

/// Maximum completion-menu entries shown at once.
const MENU_ROWS: usize = 8;

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
    ui.retire(&mut out)?;
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
                    let r = session.submit_confirmed_account();
                    self.apply_submit(session, recovery, r, out)?;
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
                } else if let Some(g) = ghost(session) {
                    // → at end of line accepts the ghost suggestion.
                    session.draft.set_buffer(&g);
                    self.menu = None;
                }
            }
            KeyCode::Up => {
                if let Some(m) = &mut self.menu {
                    m.selected = m.selected.saturating_sub(1);
                    m.scroll();
                } else {
                    session.draft.nav_up();
                    self.note = Note::None;
                }
            }
            KeyCode::Down => {
                if let Some(m) = &mut self.menu {
                    m.selected = m
                        .selected
                        .saturating_add(1)
                        .min(m.candidates.len().saturating_sub(1));
                    m.scroll();
                } else {
                    session.draft.nav_down();
                    self.note = Note::None;
                }
            }
            KeyCode::Tab => {
                if let Some(m) = &mut self.menu {
                    m.selected = m
                        .selected
                        .saturating_add(1)
                        .checked_rem(m.candidates.len())
                        .unwrap_or(0);
                    m.scroll();
                } else {
                    let candidates = session.candidates();
                    self.open_menu(candidates);
                }
            }
            KeyCode::BackTab => {
                if let Some(m) = &mut self.menu {
                    m.selected = m
                        .selected
                        .checked_sub(1)
                        .unwrap_or_else(|| m.candidates.len().saturating_sub(1));
                    m.scroll();
                }
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
                    self.apply_submit(session, recovery, r, out)?;
                }
            }
            _ => {}
        }
        Ok(Flow::Continue)
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
                session.draft.set_buffer("");
                self.menu = None;
            }
            KeyCode::Char('w') => {
                delete_word(session);
                self.menu = None;
            }
            KeyCode::Char('r') => {
                // History list: all candidates for the field, unfiltered.
                let saved = session.draft.buffer.clone();
                session.draft.set_buffer("");
                let all = session.candidates();
                session.draft.set_buffer(&saved);
                self.open_menu(all);
            }
            KeyCode::Char('e') => {
                self.menu = None;
                self.edit_in_editor(session, out)?;
            }
            _ => {}
        }
        Ok(Flow::Continue)
    }

    fn apply_submit(
        &mut self,
        session: &mut Session,
        recovery: &Recovery,
        result: Submit,
        out: &mut impl Write,
    ) -> io::Result<()> {
        match result {
            Submit::Advanced => self.note = Note::None,
            Submit::AdvancedWithNote(n) => self.note = Note::Info(n),
            Submit::Invalid(m) => self.note = Note::Error(m),
            Submit::ConfirmNewAccount { name, suggestion } => {
                let hint = suggestion
                    .map_or_else(String::new, |s| format!(" — did you mean {s}?"));
                self.note = Note::Confirm(format!("{name} is new — create it? [y/n]{hint}"));
            }
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
                    self.note = Note::Error(format!("recovery journal: {e}"));
                }
                // Retire the live area and print the final block into
                // scrollback.
                self.clear_frame(out)?;
                for l in render_done(session, &txn) {
                    out.queue(Print(l))?.queue(Print("\r\n"))?;
                }
                out.queue(Print("\r\n"))?;
                out.flush()?;
                session.complete(txn);
            }
        }
        Ok(())
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

    /// Called on exit: drop the live frame.
    fn retire(&mut self, out: &mut impl Write) -> io::Result<()> {
        self.clear_frame(out)?;
        out.flush()
    }

    /// Redraw the whole live frame.
    fn draw(&mut self, session: &Session, out: &mut impl Write) -> io::Result<()> {
        let width = terminal::size().map_or(80, |(w, _)| usize::from(w)).max(20);
        let mut rows: Vec<(String, bool)> = Vec::new(); // (text, dim)

        for l in session.preview_lines() {
            rows.push((format!("  {l}"), false));
        }
        rows.push((separator(session, width), true));

        let label = session.draft.field.label();
        let prompt = format!("  {label} › {}", session.draft.buffer);
        let prompt_row = rows.len();
        let prompt_col = format!("  {label} › ")
            .chars()
            .count()
            .saturating_add(session.draft.cursor);
        rows.push((prompt, false));

        match (&self.menu, &self.note) {
            (Some(m), _) => {
                let end = m.offset.saturating_add(MENU_ROWS).min(m.candidates.len());
                for (i, c) in m
                    .candidates
                    .get(m.offset..end)
                    .unwrap_or(&[])
                    .iter()
                    .enumerate()
                {
                    let idx = m.offset.saturating_add(i);
                    let marker = if idx == m.selected { "▸" } else { " " };
                    let hint = session.candidate_hint(c);
                    let hint = if hint.is_empty() {
                        String::new()
                    } else {
                        format!("    {hint}")
                    };
                    rows.push((format!("  {marker} {c}{hint}"), idx != m.selected));
                }
                if m.candidates.len() > MENU_ROWS {
                    rows.push((
                        format!(
                            "    … {} more (Tab to cycle)",
                            m.candidates.len().saturating_sub(MENU_ROWS)
                        ),
                        true,
                    ));
                }
            }
            (None, Note::None) => {
                if let Some(g) = ghost(session) {
                    rows.push((format!("    {g}  ← → to accept"), true));
                }
            }
            (None, Note::Info(n) | Note::Confirm(n)) => rows.push((format!("  {n}"), true)),
            (None, Note::Error(e)) => rows.push((format!("  ✗ {e}"), false)),
        }

        // Paint: move to the frame top, clear down, print every row.
        if self.drawn > 0 {
            let up = u16::try_from(self.cursor_row).unwrap_or(u16::MAX);
            if up > 0 {
                out.queue(MoveUp(up))?;
            }
        }
        out.queue(MoveToColumn(0))?
            .queue(Clear(ClearType::FromCursorDown))?;
        for (i, (text, dim)) in rows.iter().enumerate() {
            let clipped: String = text.chars().take(width).collect();
            if *dim {
                out.queue(SetAttribute(Attribute::Dim))?;
            }
            out.queue(Print(&clipped))?;
            if *dim {
                out.queue(SetAttribute(Attribute::Reset))?;
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
        let path = dir.join(format!("rledger-edit-{}.journal", std::process::id()));
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
                if let Some(txn) = crate::add::write::parse_transactions(&edited).into_iter().next()
                {
                    session.load_draft_from(&txn);
                    self.note = Note::Info("reloaded from editor".to_owned());
                } else {
                    self.note = Note::Error("editor result did not parse; draft kept".to_owned());
                }
            }
            Ok(_) => self.note = Note::Info("editor exited nonzero; draft kept".to_owned()),
            Err(e) => self.note = Note::Error(format!("could not run {editor}: {e}")),
        }
        let _ = std::fs::remove_file(&path);
        Ok(())
    }
}

impl Menu {
    /// Keep the selection within the visible window.
    const fn scroll(&mut self) {
        if self.selected < self.offset {
            self.offset = self.selected;
        }
        let bottom = self.offset.saturating_add(MENU_ROWS);
        if self.selected >= bottom {
            self.offset = self.selected.saturating_sub(MENU_ROWS).saturating_add(1);
        }
    }
}

/// The separator line: a rule, with the running imbalance surfaced when it
/// is nonzero (or unknown).
fn separator(session: &Session, width: usize) -> String {
    let status = session.running_imbalance().map_or_else(
        || " imbalance unknown ".to_owned(),
        |sums| {
            if sums.is_empty() {
                String::new()
            } else {
                let parts: Vec<String> = sums
                    .iter()
                    .map(|(c, v)| render_amount(*v, c, &session.ctx.amount_ctx))
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

/// Delete the whitespace-delimited word before the cursor.
fn delete_word(session: &mut Session) {
    let draft = &mut session.draft;
    let chars: Vec<char> = draft.buffer.chars().collect();
    let mut i = draft.cursor.min(chars.len());
    while i > 0 && chars.get(i.saturating_sub(1)).is_some_and(|c| c.is_whitespace()) {
        i = i.saturating_sub(1);
    }
    while i > 0 && chars.get(i.saturating_sub(1)).is_some_and(|c| !c.is_whitespace()) {
        i = i.saturating_sub(1);
    }
    let head: String = chars.get(..i).unwrap_or(&[]).iter().collect();
    let tail: String = chars.get(draft.cursor.min(chars.len())..).unwrap_or(&[]).iter().collect();
    draft.buffer = format!("{head}{tail}");
    draft.cursor = i;
}
