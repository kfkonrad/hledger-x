//! Interactive entry: the field state machine, completion, live preview,
//! and the two frontends (raw-mode terminal, plain line mode for pipes).
//!
//! Everything in this module except `term` is pure and testable headlessly.
//! The state machine is the *only* place that knows a posting is two fields —
//! uniting account and amount into one journal-syntax line later must not
//! touch the completers or the pre-fill logic.

pub mod complete;
pub mod dates;
pub mod plain;
pub mod term;

use std::collections::HashSet;
use std::path::PathBuf;

use chrono::NaiveDate;
use rust_decimal::Decimal;

use super::index::Index;
use super::parser::{FileMap, Journal, Transaction};
use super::write::{comment_suffix, NewTransaction, Posting};
use crate::amount::{imbalance, parse_amount, render_amount_like, AmountCtx};
use crate::config::{Completion, Config};
use crate::fmt::posting::{parse_posting, render};

/// The bare comment line written between conversion groups. A posting with
/// this account and no amount renders as `    ;`, which both `hledger` and
/// `hledger-x fmt` carry through untouched.
const GROUP_SEPARATOR: &str = ";";

/// The field currently being edited.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Field {
    /// The transaction date.
    #[default]
    Date,
    /// The description / payee.
    Description,
    /// Account of posting `i` (0-based).
    Account(usize),
    /// Amount of posting `i`.
    Amount(usize),
}

impl Field {
    /// Linear position, for navigation order.
    const fn ordinal(self) -> usize {
        match self {
            Self::Date => 0,
            Self::Description => 1,
            Self::Account(i) => i.saturating_mul(2).saturating_add(2),
            Self::Amount(i) => i.saturating_mul(2).saturating_add(3),
        }
    }

    const fn from_ordinal(n: usize) -> Self {
        match n {
            0 => Self::Date,
            1 => Self::Description,
            _ => {
                let i = n.saturating_sub(2).saturating_div(2);
                // Even ordinals are accounts, odd ones amounts.
                if n & 1 == 0 {
                    Self::Account(i)
                } else {
                    Self::Amount(i)
                }
            }
        }
    }

    /// Whether this field's buffer can carry a trailing `; comment`.
    ///
    /// Every field of a journal line can, which is the point — the comment is
    /// not a separate prompt, it is the tail of the line you are already
    /// typing.
    const fn takes_comment(self) -> bool {
        !matches!(self, Self::Date)
    }

    /// The prompt label shown to the user.
    #[must_use]
    pub fn label(self) -> String {
        match self {
            Self::Date => "date".to_owned(),
            Self::Description => "description".to_owned(),
            Self::Account(i) => format!("account {}", i.saturating_add(1)),
            Self::Amount(i) => format!("amount {}", i.saturating_add(1)),
        }
    }
}

/// Split a field buffer into its value and the comment a `;` introduces.
///
/// This is what makes comments cost nothing: `Rewe ; trip: berlin` in the
/// description field is a description and a comment, exactly as the journal
/// line reads, with no extra prompt for the overwhelmingly common case of no
/// comment at all. The returned comment has no `;` and is trimmed; `None`
/// means the buffer carried no comment.
fn split_field_comment(buffer: &str) -> (&str, Option<&str>) {
    let (body, comment) = crate::lex::split_comment(buffer);
    (
        body.trim(),
        comment.map(|c| c.trim_start().trim_start_matches(';').trim()),
    )
}

/// The inverse: put a field's value and comment back into one buffer, so
/// navigating back to a field shows what was typed there.
fn join_field_comment(value: &str, comment: &str) -> String {
    if comment.trim().is_empty() {
        value.to_owned()
    } else if value.is_empty() {
        format!("; {}", comment.trim())
    } else {
        format!("{value} ; {}", comment.trim())
    }
}

/// What submitting a field produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Submit {
    /// Field accepted; the draft has advanced (or moved) and the buffer is
    /// loaded for the new current field.
    Advanced,
    /// The transaction is complete.
    Done(Box<NewTransaction>),
    /// Input rejected; message explains why. The field is unchanged.
    Invalid(String),
    /// Strict mode: the entered name is not declared; ask before using it.
    /// Answer yes by calling [`Session::submit_confirmed`]; anything else
    /// leaves the field as it was.
    Confirm {
        /// The full question, including any "did you mean …?" near-miss.
        question: String,
    },
    /// Accepted, but with a note (e.g. an account new to the journal).
    AdvancedWithNote(String),
    /// `u` at the date prompt: undo the last completed transaction.
    Undo,
    /// `q` at the date prompt: save everything completed and exit.
    Quit,
}

/// Session-wide immutable context assembled at startup.
pub struct SessionCtx {
    /// The parsed journal.
    pub journal: Journal,
    /// Frecency indices.
    pub index: Index,
    /// Resolved configuration.
    pub config: Config,
    /// Amount parsing context at the insertion point.
    pub amount_ctx: AmountCtx,
    /// Today, fixed at startup.
    pub today: NaiveDate,
    /// The write-target file.
    pub target: PathBuf,
    /// File-wide widths of the target, for the live preview.
    pub acc_w: usize,
    /// See `acc_w`.
    pub num_w: usize,
    /// Account declarations visible at the insertion point — what `hledger
    /// check accounts` would accept there. The strict check tests against
    /// this set.
    pub declared_accounts_visible: HashSet<String>,
    /// Every account used in a transaction anywhere in the tree. An account
    /// in here is not *new*, even if undeclared.
    pub used_accounts: HashSet<String>,
    /// Commodity declarations visible at the insertion point, for the strict
    /// commodity check.
    pub declared_commodities_visible: HashSet<String>,
    /// Every commodity seen in a posting.
    pub used_commodities: HashSet<String>,
    /// Payee declarations visible at the insertion point — what `hledger
    /// check payees` would accept there.
    pub declared_payees_visible: HashSet<String>,
    /// Every *payee* used by a transaction anywhere in the tree — the payee
    /// half of its description. A payee in here is not new, even if
    /// undeclared.
    pub used_payees: HashSet<String>,
    /// Declared payees (full pool, for completion).
    pub declared_payees: Vec<String>,
    /// Declared tag names (full pool, for completion inside comments).
    pub declared_tags: Vec<String>,
    /// Tag names seen in comments anywhere in the tree.
    pub used_tags: Vec<String>,
    /// Declared accounts (full pool — offering a name is harmless even when
    /// it is declared below the insertion point).
    pub declared_accounts: Vec<String>,
    /// Strict mode: prompt before using undeclared accounts/commodities.
    pub strict: bool,
    /// Default commodity sample: the target file's `D` directive, falling
    /// back to the configured default.
    pub default_commodity: Option<String>,
}

impl SessionCtx {
    /// Assemble the context for a target file within a parsed journal.
    #[must_use]
    pub fn new(
        journal: Journal,
        config: Config,
        today: NaiveDate,
        target: PathBuf,
        target_map: &FileMap,
    ) -> Self {
        let index = Index::build(&journal, today, crate::add::index::DEFAULT_HALF_LIFE_DAYS);

        let target_file = journal.file(&target).cloned();
        let (insertion_pos, state) =
            target_file.map_or((usize::MAX, None), |f| (f.eof_pos, Some(f.state_at_eof)));
        // The styles in effect at the insertion point — everything the tree
        // declares ahead of it, and nothing declared after.
        let mut amount_ctx = journal.amount_ctx_at(insertion_pos);
        amount_ctx.decimal_mark = state.as_ref().and_then(|s| s.decimal_mark);

        let declared_accounts_visible: HashSet<String> = journal
            .accounts_visible_at(insertion_pos)
            .into_iter()
            .map(ToOwned::to_owned)
            .collect();
        let declared_commodities_visible: HashSet<String> = journal
            .commodities
            .iter()
            .filter(|c| c.pos < insertion_pos)
            .map(|c| c.name.clone())
            .collect();
        let declared_payees_visible: HashSet<String> = journal
            .payees_visible_at(insertion_pos)
            .into_iter()
            .map(ToOwned::to_owned)
            .collect();
        let mut used_accounts = HashSet::new();
        let mut used_commodities = HashSet::new();
        let mut used_payees = HashSet::new();
        let mut used_tags: Vec<String> = Vec::new();
        for t in &journal.transactions {
            if !t.description.is_empty() {
                let (payee, _note) = crate::lex::split_payee_note(&t.description);
                used_payees.insert(payee.to_owned());
            }
            for name in tags_in(t.comment.as_deref()) {
                if !used_tags.contains(&name) {
                    used_tags.push(name);
                }
            }
            for p in &t.postings {
                used_accounts.insert(p.account.clone());
                if let Some(c) = &p.commodity {
                    used_commodities.insert(c.clone());
                }
                for name in tags_in(p.comment.as_deref()) {
                    if !used_tags.contains(&name) {
                        used_tags.push(name);
                    }
                }
            }
        }
        let declared_payees: Vec<String> = journal.payees.iter().map(|d| d.name.clone()).collect();
        let declared_tags: Vec<String> = journal.tags.iter().map(|d| d.name.clone()).collect();
        let declared_accounts: Vec<String> =
            journal.accounts.iter().map(|d| d.name.clone()).collect();
        let strict = config.strict;

        // The configuration alone, deliberately: a journal's `D` directive is
        // not consulted here. `add` materializes its default into an amount
        // the user typed, and taking that from the file meant a bare `12.50`
        // silently acquiring a commodity nobody asked for. Spelling `D` out
        // is `fmt --explicit`'s job, where it is what was asked for
        // (the user's call, 2026-08-18).
        let _ = state;
        let default_commodity = config.default_commodity.clone();

        Self {
            journal,
            index,
            config,
            amount_ctx,
            today,
            target,
            acc_w: target_map.acc_w,
            num_w: target_map.num_w,
            declared_accounts_visible,
            used_accounts,
            declared_commodities_visible,
            used_commodities,
            declared_payees_visible,
            used_payees,
            declared_payees,
            declared_tags,
            used_tags,
            declared_accounts,
            strict,
            default_commodity,
        }
    }

    /// Ranked description candidates for the description field: whole
    /// descriptions as the journal writes them — `payee | note` included,
    /// since re-entering one wholesale is the point — then declared payees
    /// that appear in no transaction yet.
    #[must_use]
    pub fn description_pool(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .index
            .ranked_descriptions()
            .into_iter()
            .map(ToOwned::to_owned)
            .collect();
        for p in &self.declared_payees {
            if !out.iter().any(|x| x == p) {
                out.push(p.clone());
            }
        }
        out
    }

    /// Ranked *payee* candidates — the payee halves the journal uses, plus
    /// every declared payee. This is what the near-miss hint searches, so
    /// that `bahn` is answered with `Deutsche Bahn` and not with some whole
    /// description that happens to contain it.
    #[must_use]
    pub fn payee_pool(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .index
            .ranked_payees()
            .into_iter()
            .map(ToOwned::to_owned)
            .collect();
        for p in &self.declared_payees {
            if !out.iter().any(|x| x == p) {
                out.push(p.clone());
            }
        }
        out
    }

    /// The tag completion pool: declared tags first, then tags seen in
    /// comments anywhere in the tree.
    #[must_use]
    pub fn tag_pool(&self) -> Vec<String> {
        let mut out = self.declared_tags.clone();
        for t in &self.used_tags {
            if !out.iter().any(|x| x == t) {
                out.push(t.clone());
            }
        }
        out
    }

    /// Ranked account candidates, conditioned on a description, with
    /// declared-but-unused accounts appended.
    #[must_use]
    pub fn account_pool(&self, payee: Option<&str>) -> Vec<String> {
        let mut out: Vec<String> = self
            .index
            .ranked_accounts(payee)
            .into_iter()
            .map(ToOwned::to_owned)
            .collect();
        for a in &self.declared_accounts {
            if !out.iter().any(|x| x == a) {
                out.push(a.clone());
            }
        }
        out
    }

    /// The commodity of the journal's `D` sample (or configured default):
    /// what gets attached to generated unitless balancing amounts as
    /// editable pre-fill.
    #[must_use]
    pub fn default_commodity_symbol(&self) -> Option<String> {
        let sample = self.default_commodity.as_deref()?;
        parse_amount(sample, &self.amount_ctx)
            .map(|p| p.commodity)
            .filter(|c| !c.is_empty())
            .or_else(|| {
                // The sample may be a bare symbol.
                let t = sample.trim();
                (!t.is_empty() && !t.chars().any(|c| c.is_ascii_digit())).then(|| t.to_owned())
            })
    }
}

/// The transaction being entered.
#[derive(Debug, Clone, Default)]
pub struct Draft {
    /// Raw date input and its resolution.
    pub date_input: String,
    /// Resolved date once the date field was accepted.
    pub date: Option<NaiveDate>,
    /// Accepted description.
    pub description: String,
    /// The transaction's comment text, without the leading `;`.
    pub comment: String,
    /// Committed postings.
    pub postings: Vec<Posting>,
    /// The current field.
    pub field: Field,
    /// The current field's edit buffer.
    pub buffer: String,
    /// Cursor position in `buffer`, in chars.
    pub cursor: usize,
    /// Whether the buffer still holds untouched pre-filled text. A printable
    /// keystroke then replaces it wholesale (Enter still accepts it, and
    /// editing keys keep it for editing).
    pub pristine: bool,
    /// Template postings pre-filling this draft, from the description's most
    /// recent transaction. Comments are deliberately not pre-filled: they
    /// describe the occasion, not the shape of the transaction.
    template: Vec<(String, String)>,
    /// Highest field reached, as an ordinal — the navigation frontier.
    frontier: usize,
    /// The suggested account has been refused. Cleared on every move, so it
    /// only ever applies to the prompt it was asked for.
    ///
    /// **Accounts only, on purpose.** Refusing means something there and
    /// nowhere else: an empty account prompt has two readings — accept the
    /// suggestion, or finish the transaction — and this is how you reach the
    /// second while the first is on offer. At the date and amount prompts an
    /// empty Enter does the same thing whether or not a ghost is showing, so
    /// hiding the ghost there would change the display and nothing else.
    account_refused: bool,
}

impl Draft {
    /// A fresh draft at the date prompt, pre-filled with today.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the buffer (accepting a candidate, or a test typing a whole
    /// value). The result counts as the user's own text.
    pub fn set_buffer(&mut self, text: &str) {
        text.clone_into(&mut self.buffer);
        self.cursor = text.chars().count();
        self.pristine = false;
    }

    /// Insert a char at the cursor. On a pristine pre-fill this replaces the
    /// whole buffer — typing over a suggestion starts fresh.
    pub fn insert(&mut self, c: char) {
        if self.pristine {
            self.buffer.clear();
            self.cursor = 0;
            self.pristine = false;
        }
        let byte = char_to_byte(&self.buffer, self.cursor);
        self.buffer.insert(byte, c);
        self.cursor = self.cursor.saturating_add(1);
    }

    /// Delete the word before the cursor. In account mode the boundary is
    /// the colon: deletion stops just short of the previous `:`, so
    /// `expenses:groceries` becomes `expenses:` — one segment at a time. A
    /// cursor sitting right after a colon consumes that colon along with the
    /// segment before it.
    pub fn delete_word(&mut self, account_mode: bool) {
        self.pristine = false;
        let chars: Vec<char> = self.buffer.chars().collect();
        let mut i = self.cursor.min(chars.len());
        let at = |j: usize| j.checked_sub(1).and_then(|k| chars.get(k)).copied();
        if account_mode {
            if at(i) == Some(':') {
                i = i.saturating_sub(1);
            }
            while i > 0 && at(i).is_some_and(|c| c != ':') {
                i = i.saturating_sub(1);
            }
        } else {
            while i > 0 && at(i).is_some_and(char::is_whitespace) {
                i = i.saturating_sub(1);
            }
            while i > 0 && at(i).is_some_and(|c| !c.is_whitespace()) {
                i = i.saturating_sub(1);
            }
        }
        let head: String = chars.get(..i).unwrap_or(&[]).iter().collect();
        let tail: String = chars
            .get(self.cursor.min(chars.len())..)
            .unwrap_or(&[])
            .iter()
            .collect();
        self.buffer = format!("{head}{tail}");
        self.cursor = i;
    }

    /// Delete the char before the cursor.
    pub fn backspace(&mut self) {
        self.pristine = false;
        if self.cursor == 0 {
            return;
        }
        self.cursor = self.cursor.saturating_sub(1);
        let byte = char_to_byte(&self.buffer, self.cursor);
        self.buffer.remove(byte);
    }

    /// Refuse the suggested account, so that Enter finishes the transaction
    /// instead of accepting it. No effect on the other prompts — see
    /// [`Self::account_refused`].
    pub const fn dismiss_suggestion(&mut self) {
        self.account_refused = true;
    }

    /// Number of postings committed so far.
    #[must_use]
    pub const fn committed_postings(&self) -> usize {
        self.postings.len()
    }

    /// The payee half of the description — everything before the first `|`,
    /// or the whole thing when there is none. This is the transaction's
    /// identity: what the payee checks test and what the template keys on.
    #[must_use]
    pub fn payee(&self) -> &str {
        crate::lex::split_payee_note(&self.description).0
    }

    /// The note half — everything after the first `|`. Equal to the payee
    /// when the description carries no `|`, as in hledger.
    #[must_use]
    pub fn note(&self) -> &str {
        crate::lex::split_payee_note(&self.description).1
    }

    /// Save the buffer into the current field's slot without validation
    /// (used when navigating away).
    /// A posting's comment belongs to the *line*, so it can be typed on
    /// either of the line's two fields. The amount field owns it — it is the
    /// end of the line, and the one place clearing it makes sense; the
    /// account field can only add one, which the amount field then carries.
    fn stash(&mut self) {
        let (value, comment) = split_field_comment(&self.buffer);
        let value = value.to_owned();
        match self.field {
            Field::Date => self.date_input = value,
            Field::Description => {
                self.description = value;
                self.comment = comment.unwrap_or_default().to_owned();
            }
            Field::Account(i) => {
                ensure_len(&mut self.postings, i.saturating_add(1));
                if let Some(p) = self.postings.get_mut(i) {
                    p.account = value;
                    if let Some(c) = comment {
                        c.clone_into(&mut p.comment);
                    }
                }
            }
            Field::Amount(i) => {
                ensure_len(&mut self.postings, i.saturating_add(1));
                if let Some(p) = self.postings.get_mut(i) {
                    p.amount = value;
                    comment.unwrap_or_default().clone_into(&mut p.comment);
                }
            }
        }
    }

    /// The stored text of a field (for loading on navigation), comment and
    /// all, so navigating back shows what was typed.
    fn stored(&self, field: Field) -> String {
        match field {
            Field::Date => self.date_input.clone(),
            Field::Description => join_field_comment(&self.description, &self.comment),
            Field::Account(i) => self
                .postings
                .get(i)
                .map(|p| p.account.clone())
                .unwrap_or_default(),
            Field::Amount(i) => self
                .postings
                .get(i)
                .map(|p| join_field_comment(&p.amount, &p.comment))
                .unwrap_or_default(),
        }
    }

    /// Move to a field, stashing the buffer and loading the target's text.
    /// The loaded text is pristine: typing replaces it.
    fn goto(&mut self, field: Field) {
        self.stash();
        self.account_refused = false;
        self.field = field;
        self.frontier = self.frontier.max(field.ordinal());
        let text = self.stored(field);
        self.set_buffer(&text);
        self.pristine = !text.is_empty();
    }

    /// Navigate up (earlier field). Returns whether anything moved.
    pub fn nav_up(&mut self) -> bool {
        let ord = self.field.ordinal();
        if ord == 0 {
            return false;
        }
        self.goto(Field::from_ordinal(ord.saturating_sub(1)));
        true
    }

    /// Navigate down (later field), bounded by the frontier.
    pub fn nav_down(&mut self) -> bool {
        let ord = self.field.ordinal();
        if ord >= self.frontier {
            return false;
        }
        self.goto(Field::from_ordinal(ord.saturating_add(1)));
        true
    }

    /// The amounts committed so far, as raw strings.
    fn committed_amounts(&self) -> Vec<&str> {
        self.postings.iter().map(|p| p.amount.as_str()).collect()
    }
}

fn ensure_len(v: &mut Vec<Posting>, n: usize) {
    while v.len() < n {
        v.push(Posting::default());
    }
}

fn char_to_byte(s: &str, chars: usize) -> usize {
    s.char_indices().nth(chars).map_or(s.len(), |(b, _)| b)
}

/// The running session: completed transactions plus the current draft.
/// The completion problem for one field: which style and candidate shape
/// apply, the part of the buffer being completed, and the text that has to
/// go back in front of the result (only the amount field, which completes
/// the trailing commodity of `12.00 EUR @ USD`).
struct CompletionQuery {
    style: Completion,
    shape: complete::Shape,
    text: String,
    /// Literal text preceding the completed part, including any separator.
    head: String,
    /// Literal text appended after it — a tag name's `:`.
    tail: String,
    pool: Vec<String>,
}

impl CompletionQuery {
    /// Put a completed candidate back into buffer terms.
    fn rejoin(&self, candidate: &str) -> String {
        format!("{}{candidate}{}", self.head, self.tail)
    }
}

pub struct Session {
    /// Immutable context.
    pub ctx: SessionCtx,
    /// Completed, not-yet-written transactions.
    pub completed: Vec<NewTransaction>,
    /// The draft being edited.
    pub draft: Draft,
    /// Accounts the user has committed to during this session, oldest
    /// first. They complete like any other account, and they stop counting
    /// as new — having accepted `expenses:coffee` once, the user should not
    /// be asked about it again in the same sitting. Undo does not retract
    /// them: the acceptance was deliberate either way.
    entered_accounts: Vec<String>,
    /// Descriptions committed to during this session, for the same reason:
    /// having accepted a new payee once, do not ask again in the same
    /// sitting.
    entered_payees: Vec<String>,
}

impl Session {
    /// Start a session.
    #[must_use]
    pub fn new(ctx: SessionCtx) -> Self {
        let mut s = Self {
            ctx,
            completed: Vec::new(),
            draft: Draft::new(),
            entered_accounts: Vec::new(),
            entered_payees: Vec::new(),
        };
        s.reset_draft();
        s
    }

    /// The account completion pool: the journal's, with accounts introduced
    /// in this session and not yet in the index put in front — they are the
    /// freshest thing the user could want.
    #[must_use]
    pub fn account_pool(&self) -> Vec<String> {
        let payee = self.draft.payee();
        let mut pool = self.ctx.account_pool((!payee.is_empty()).then_some(payee));
        for a in self.entered_accounts.iter().rev() {
            if !pool.iter().any(|x| x == a) {
                pool.insert(0, a.clone());
            }
        }
        pool
    }

    /// Whether `account` is already known: declared, used in the journal, or
    /// entered earlier in this session.
    fn account_known(&self, account: &str) -> bool {
        self.ctx.declared_accounts_visible.contains(account)
            || self.ctx.used_accounts.contains(account)
            || self.entered_accounts.iter().any(|a| a == account)
    }

    /// Whether `payee` is already known — the same three sources.
    fn payee_known(&self, payee: &str) -> bool {
        self.ctx.declared_payees_visible.contains(payee)
            || self.ctx.used_payees.contains(payee)
            || self.entered_payees.iter().any(|p| p == payee)
    }

    /// A close existing payee, for "did you mean". Descriptions are plain
    /// text, so this is edit distance only — there are no segments to match.
    #[must_use]
    pub fn near_payee(&self, name: &str) -> Option<String> {
        let pool = self.ctx.payee_pool();
        let lower = name.to_lowercase();
        if lower.is_empty() {
            return None;
        }
        // Containment is checked *first*, and deliberately so: `bahn` sits at
        // edit distance 9 from `Deutsche Bahn` — further than several
        // unrelated payees — so ranking by distance alone would answer with
        // whichever short name happened to be nearest and miss the one case
        // this hint exists for. The pool is frecency-ordered, so the first
        // containing candidate is the likeliest.
        if let Some(hit) = pool.iter().find(|c| c.to_lowercase().contains(&lower)) {
            return Some(hit.clone());
        }
        let (d, candidate) = pool
            .iter()
            .map(|c| (levenshtein(&lower, &c.to_lowercase()), c))
            .min_by(|(da, ca), (db, cb)| da.cmp(db).then_with(|| ca.cmp(cb)))?;
        (d <= name.chars().count().saturating_div(2).max(2)).then(|| candidate.clone())
    }

    /// Rebuild the indices over the journal plus everything completed so
    /// far, so a description or commodity entered this session completes in
    /// the next transaction. Cheap enough to redo wholesale, which keeps
    /// undo correct for free.
    fn reindex(&mut self) {
        let half_life = crate::add::index::DEFAULT_HALF_LIFE_DAYS;
        let today = self.ctx.today;
        let mut index = Index::build(&self.ctx.journal, today, half_life);
        for txn in &self.completed {
            let postings: Vec<(String, Option<String>)> = txn
                .postings
                .iter()
                .map(|p| {
                    let commodity = parse_amount(&p.amount, &self.ctx.amount_ctx)
                        .map(|a| a.commodity)
                        .filter(|c| !c.is_empty());
                    (p.account.clone(), commodity)
                })
                .collect();
            index.bump_session(txn.date, &txn.description, &postings, today, half_life);
        }
        self.ctx.index = index;
    }

    /// Reset to a fresh draft at the date prompt (today ghost-suggested).
    pub fn reset_draft(&mut self) {
        self.draft = Draft::new();
    }

    /// The ghost suggestion for the current (empty) field, shown dimmed
    /// after the cursor. Tab or `→` copies it into the buffer for editing;
    /// Enter submits the empty buffer instead — which itself means the
    /// suggested date at the date prompt, and the explicit balancing amount
    /// at a final amount prompt.
    #[must_use]
    pub fn suggestion(&self) -> Option<String> {
        if !self.draft.buffer.is_empty() {
            return None;
        }
        match self.draft.field {
            Field::Date => Some(
                self.draft
                    .date
                    .unwrap_or(self.ctx.today)
                    .format("%Y-%m-%d")
                    .to_string(),
            ),
            Field::Amount(i) => {
                let s = self.amount_prefill(i);
                (!s.is_empty()).then_some(s)
            }
            // The template's account for this posting. It used to be
            // written into the buffer as pre-filled text, which read as
            // something already entered while behaving like a suggestion:
            // white, yet wiped wholesale by the first keystroke. Ghosting it
            // makes appearance and behaviour agree — dim text is a proposal,
            // buffer text is yours.
            Field::Account(i) if !self.draft.account_refused => self
                .draft
                .template
                .get(i)
                .map(|(a, _)| a.clone())
                .filter(|a| !a.is_empty()),
            // A refused account, and the description, suggest nothing.
            Field::Account(_) | Field::Description => None,
        }
    }

    /// Template amount when the account matches; otherwise the negated
    /// running sum (the balancing amount), when it is known and nonzero.
    fn amount_prefill(&self, i: usize) -> String {
        let account = self
            .draft
            .postings
            .get(i)
            .map(|p| p.account.as_str())
            .unwrap_or_default();
        let template_match = self
            .draft
            .template
            .get(i)
            .and_then(|(a, amt)| (a == account && !amt.is_empty()).then(|| amt.clone()));
        let balancing = self.balancing_prefill(i);
        // On the template's *last* posting the balancing sum is the better
        // prediction ("the final posting's amount is pre-filled with the
        // negated running sum"); earlier template postings keep their
        // amounts verbatim.
        let last_template_slot = i.saturating_add(1) >= self.draft.template.len();
        if last_template_slot {
            balancing.or(template_match).unwrap_or_default()
        } else {
            template_match.or(balancing).unwrap_or_default()
        }
    }

    /// The negated sum over the *other* postings, rendered — the balancing
    /// amount. `None` when it is unknown, zero, or spans commodities.
    fn balancing_prefill(&self, i: usize) -> Option<String> {
        let amounts: Vec<&str> = self
            .draft
            .postings
            .iter()
            .enumerate()
            .filter(|(j, _)| *j != i)
            .map(|(_, p)| p.amount.as_str())
            .collect();
        let sums = imbalance(&amounts, &self.ctx.amount_ctx)?;
        let [(commodity, value)] = sums.as_slice() else {
            return None;
        };
        let negated = Decimal::ZERO.checked_sub(*value)?;
        let commodity = if commodity.is_empty() {
            self.ctx.default_commodity_symbol().unwrap_or_default()
        } else {
            commodity.clone()
        };
        // The other postings settle how this commodity is written when no
        // `commodity` directive does, so `$10` balances with `$-10`.
        Some(render_amount_like(
            negated,
            &commodity,
            &self.ctx.amount_ctx,
            &amounts,
        ))
    }

    /// Ranked completion candidates for the current field and buffer.
    #[must_use]
    pub fn candidates(&self) -> Vec<String> {
        let Some(q) = self.completion_query() else {
            return Vec::new();
        };
        let refs: Vec<&str> = q.pool.iter().map(String::as_str).collect();
        complete::filter_ranked(q.style, q.shape, &q.text, &refs)
            .into_iter()
            .map(|c| q.rejoin(c))
            .collect()
    }

    /// Tag-name completion for the comment tail of the buffer, when there is
    /// one. Only the tag *name* completes: a value is free text, and offering
    /// candidates for it would be guessing.
    fn tag_query(&self) -> Option<CompletionQuery> {
        let at = self.draft.buffer.find(';')?;
        let head = self.draft.buffer.get(..=at)?;
        let rest = self.draft.buffer.get(at.saturating_add(1)..)?;
        let (tag_head, partial) = split_tag_query(rest)?;
        Some(CompletionQuery {
            style: self.ctx.config.description_completion,
            shape: complete::Shape::Plain,
            text: partial.to_owned(),
            head: format!("{head}{tag_head}"),
            // Completing a tag name commits to it being a tag.
            tail: ":".to_owned(),
            pool: self.ctx.tag_pool(),
        })
    }

    /// What Tab should put in the buffer, or `None` when completion makes no
    /// progress and the menu should open instead.
    #[must_use]
    pub fn complete_buffer(&self) -> Option<String> {
        let q = self.completion_query()?;
        let refs: Vec<&str> = q.pool.iter().map(String::as_str).collect();
        let completed = complete::complete(q.style, q.shape, &q.text, &refs)?;
        let out = q.rejoin(&completed);
        (out != self.draft.buffer).then_some(out)
    }

    /// The completion problem posed by the current field.
    fn completion_query(&self) -> Option<CompletionQuery> {
        // Past a `;` the buffer is comment text whatever field it belongs to,
        // and what completes there is a tag name.
        if self.draft.field.takes_comment() {
            if let Some(q) = self.tag_query() {
                return Some(q);
            }
        }
        let query = self.draft.buffer.trim();
        match self.draft.field {
            Field::Date => None,
            Field::Description => Some(CompletionQuery {
                style: self.ctx.config.description_completion,
                // A `:` in a description is ordinary text, never a segment
                // break — hledger's payee separator is `|`.
                shape: complete::Shape::Plain,
                text: query.to_owned(),
                head: String::new(),
                tail: String::new(),
                pool: self.ctx.description_pool(),
            }),
            Field::Account(_) => Some(CompletionQuery {
                style: self.ctx.config.account_completion,
                shape: complete::Shape::Account,
                text: query.to_owned(),
                head: String::new(),
                tail: String::new(),
                pool: self.account_pool(),
            }),
            Field::Amount(_) => {
                // Commodities — the face commodity and equally the second
                // one after a cost or assertion tail.
                let (head, tail) = split_commodity_query(query)?;
                let mut pool: Vec<String> = self
                    .ctx
                    .index
                    .ranked_commodities()
                    .into_iter()
                    .map(ToOwned::to_owned)
                    .collect();
                for c in &self.ctx.journal.commodities {
                    if !pool.iter().any(|x| x == &c.name) {
                        pool.push(c.name.clone());
                    }
                }
                Some(CompletionQuery {
                    style: Completion::Prefix,
                    shape: complete::Shape::Plain,
                    text: tail.to_owned(),
                    head: if head.is_empty() {
                        String::new()
                    } else {
                        format!("{head} ")
                    },
                    tail: String::new(),
                    pool,
                })
            }
        }
    }

    /// Submit the current buffer for the current field.
    pub fn submit(&mut self) -> Submit {
        match self.draft.field {
            Field::Date => self.submit_date(),
            Field::Description => self.submit_description(false),
            Field::Account(i) => self.submit_account(i, false),
            Field::Amount(i) => self.submit_amount(i, false),
        }
    }

    /// Re-submit the current field after the user answered a
    /// [`Submit::Confirm`] question with yes.
    pub fn submit_confirmed(&mut self) -> Submit {
        match self.draft.field {
            Field::Description => self.submit_description(true),
            Field::Account(i) => self.submit_account(i, true),
            Field::Amount(i) => self.submit_amount(i, true),
            Field::Date => Submit::Invalid("nothing to confirm".to_owned()),
        }
    }

    /// The current buffer split into the field's value and its comment.
    fn buffer_parts(&self) -> (String, String) {
        let (value, comment) = split_field_comment(&self.draft.buffer);
        (value.to_owned(), comment.unwrap_or_default().to_owned())
    }

    fn submit_date(&mut self) -> Submit {
        let text = self.draft.buffer.trim().to_owned();
        if text == "u" {
            return if self.completed.is_empty() {
                Submit::Invalid("nothing to undo".to_owned())
            } else {
                Submit::Undo
            };
        }
        if text == "q" {
            return Submit::Quit;
        }
        // Empty accepts the ghost suggestion: the already-accepted date
        // when navigating back, today otherwise.
        let resolved = if text.is_empty() {
            Some(self.draft.date.unwrap_or(self.ctx.today))
        } else {
            dates::resolve(&text, self.ctx.today)
        };
        let Some(date) = resolved else {
            return Submit::Invalid(format!("cannot understand date {text:?}"));
        };
        self.draft.date = Some(date);
        self.draft.date_input = text;
        self.advance_to(Field::Description);
        Submit::Advanced
    }

    fn submit_description(&mut self, confirmed: bool) -> Submit {
        let (text, comment) = self.buffer_parts();
        self.draft.comment = comment;
        // The description is `payee | note`; everything that treats it as an
        // *identity* — the checks, the near-miss, the template — uses the
        // payee half, exactly as hledger does. Only the file gets the whole
        // string.
        let (payee, _note) = crate::lex::split_payee_note(&text);
        let payee = payee.to_owned();
        // The payee checks mirror the account ones exactly: `strict` asks
        // before using an undeclared payee (as `hledger check payees` would),
        // otherwise a name new to the journal gets a passing note. This is
        // the fix for hledger `add` matching `bahn` against `Deutsche Bahn`
        // to pick defaults and then writing the literal `bahn`.
        let mut note = None;
        if !confirmed && !payee.is_empty() {
            if self.ctx.strict && !self.ctx.declared_payees_visible.contains(&payee) {
                let hint = self
                    .near_payee(&payee)
                    .map_or_else(String::new, |s| format!(" — did you mean {s}?"));
                return Submit::Confirm {
                    question: format!("{payee} is not a declared payee — use it anyway?{hint}"),
                };
            }
            if !self.payee_known(&payee) {
                let hint = self
                    .near_payee(&payee)
                    .map_or_else(String::new, |s| format!(" — did you mean {s}?"));
                note = Some(format!("{payee} is new to this journal{hint}"));
            }
        }
        self.draft.description.clone_from(&text);
        if !payee.is_empty() && !self.entered_payees.iter().any(|p| p == &payee) {
            self.entered_payees.push(payee.clone());
        }
        // Load the template once, from the payee's most recent transaction.
        // Predictability: most recent, not highest scoring.
        if self.draft.postings.is_empty() && self.draft.template.is_empty() {
            if let Some(tpl) = self.ctx.index.template(&self.ctx.journal, &payee) {
                self.draft.template = template_postings(tpl);
            }
        }
        self.advance_to(Field::Account(0));
        note.map_or(Submit::Advanced, Submit::AdvancedWithNote)
    }

    fn submit_account(&mut self, i: usize, confirmed: bool) -> Submit {
        // A `;` here is a comment for this posting's line, not part of the
        // account name — writing it into the name would produce a corrupt
        // posting line.
        let (mut text, comment) = self.buffer_parts();
        if !comment.is_empty() {
            ensure_len(&mut self.draft.postings, i.saturating_add(1));
            if let Some(p) = self.draft.postings.get_mut(i) {
                p.comment = comment;
            }
        }
        if text.is_empty() {
            // Enter on an empty account accepts the suggested one, the way it
            // does at the date and amount prompts. With nothing suggested —
            // the template is exhausted, or the suggestion was dismissed —
            // an empty account is the end of the transaction.
            match self.suggestion() {
                Some(suggested) => {
                    // Into the buffer, not just into `text`: advancing stashes
                    // the buffer into this field's slot, so an accepted
                    // suggestion that stayed out of it would be stashed away
                    // again as empty.
                    self.draft.set_buffer(&suggested);
                    text = suggested;
                }
                None => return self.finish(i),
            }
        }
        // Strict mode mirrors `hledger check accounts`: only declarations
        // visible at the insertion point count. hledger-x never declares
        // anything itself — the question is whether to *use* the name.
        //
        // An account accepted earlier in this session counts as settled:
        // asking about `expenses:coffee` on every posting would be noise.
        if !confirmed
            && self.ctx.strict
            && !self.ctx.declared_accounts_visible.contains(&text)
            && !self.entered_accounts.iter().any(|a| a == &text)
        {
            let hint = self
                .near_miss(&text)
                .map_or_else(String::new, |s| format!(" — did you mean {s}?"));
            return Submit::Confirm {
                question: format!("{text} is not a declared account — use it anyway?{hint}"),
            };
        }
        // Not strict: accept, but surface names that are genuinely new to
        // the journal (neither declared nor ever used) — once.
        let brand_new = !self.account_known(&text);
        self.commit_account(i, &text);
        if brand_new && !confirmed {
            return Submit::AdvancedWithNote(format!("{text} is new to this journal"));
        }
        Submit::Advanced
    }

    fn commit_account(&mut self, i: usize, text: &str) {
        // Record it before it is written, so later postings of *this*
        // transaction can complete it too.
        if !self.entered_accounts.iter().any(|a| a == text) {
            self.entered_accounts.push(text.to_owned());
        }
        ensure_len(&mut self.draft.postings, i.saturating_add(1));
        if let Some(p) = self.draft.postings.get_mut(i) {
            text.clone_into(&mut p.account);
        }
        self.advance_to(Field::Amount(i));
    }

    fn submit_amount(&mut self, i: usize, confirmed: bool) -> Submit {
        // The amount field owns this posting's comment: it is the end of the
        // line, so what is typed here is the whole truth about it.
        let (mut text, comment) = self.buffer_parts();
        ensure_len(&mut self.draft.postings, i.saturating_add(1));
        if let Some(p) = self.draft.postings.get_mut(i) {
            p.comment = comment;
        }
        if text.is_empty() {
            let Some(suggested) = self.suggestion() else {
                return Submit::Invalid(if i == 0 {
                    "an amount is required on the first posting".to_owned()
                } else {
                    "an amount is needed here — the balancing amount cannot be worked out yet"
                        .to_owned()
                });
            };
            // Enter accepts the ghost, as at every prompt. One deliberate
            // extra: when that ghost is the *balancing* amount and no later
            // posting is waiting, accepting it also ends the transaction —
            // the shortcut that keeps a routine entry at one keystroke per
            // field. A template amount never finishes; only arithmetic that
            // has nothing left to balance does.
            //
            // The strict commodity check is skipped here for the same reason
            // the equity conversion postings skip it: hledger-x generated
            // this amount rather than the user typing it, and its commodity
            // is the negation of one already vetted on the posting it
            // balances.
            if !self.has_later_postings(i)
                && self.balancing_prefill(i).as_deref() == Some(suggested.as_str())
            {
                ensure_len(&mut self.draft.postings, i.saturating_add(1));
                if let Some(p) = self.draft.postings.get_mut(i) {
                    p.amount.clone_from(&suggested);
                }
                self.draft.set_buffer(&suggested);
                self.draft.postings.truncate(i.saturating_add(1));
                return self.complete_draft();
            }
            self.draft.set_buffer(&suggested);
            text = suggested;
        }
        let Some(parsed) = parse_amount(&text, &self.ctx.amount_ctx) else {
            return Submit::Invalid(format!("cannot parse amount {text:?}"));
        };
        // A bare number gets the default commodity written into the
        // transaction being built — visibly, and still editable via ↑.
        // Otherwise this posting and a balancing prefill carrying the
        // default would read as two different commodities and the
        // transaction could never balance.
        let mut commodity = parsed.commodity;
        if commodity.is_empty() && !text.contains(['@', '=']) {
            if let Some(symbol) = self.ctx.default_commodity_symbol() {
                text = attach_commodity(&text, &symbol, &self.ctx.amount_ctx);
                commodity = symbol;
            }
        }
        // Normalize what was typed to the declared display style — the form
        // `fmt` (and `hledger print`) would write. Amounts of undeclared
        // commodities stay as typed.
        text = crate::amount::restyle_entered(&text, &self.ctx.amount_ctx);
        // Strict mode mirrors `hledger check commodities`, covering the face
        // commodity and any second commodity in a cost/assertion tail. A
        // unitless amount is always valid, even in strict mode.
        if !confirmed {
            let mut names: Vec<String> = Vec::new();
            if !commodity.is_empty() {
                names.push(commodity);
            }
            for c in crate::amount::tail_commodities(&text) {
                if !names.contains(&c) {
                    names.push(c);
                }
            }
            if self.ctx.strict {
                let undeclared = names
                    .iter()
                    .find(|c| !self.ctx.declared_commodities_visible.contains(*c));
                if let Some(c) = undeclared {
                    let hint = closest(c, self.ctx.declared_commodities_visible.iter())
                        .map_or_else(String::new, |s| format!(" — did you mean {s}?"));
                    return Submit::Confirm {
                        question: format!("{c} is not a declared commodity — use it anyway?{hint}"),
                    };
                }
            } else if let Some(c) = names.iter().find(|c| {
                !self.ctx.declared_commodities_visible.contains(*c)
                    && !self.ctx.used_commodities.contains(*c)
            }) {
                let note = format!("{c} is a commodity new to this journal");
                self.accept_amount(i, &text);
                return Submit::AdvancedWithNote(note);
            }
        }
        self.accept_amount(i, &text);
        Submit::Advanced
    }

    /// Commit an amount and advance to the next account prompt.
    fn accept_amount(&mut self, i: usize, text: &str) {
        // Put the (possibly commodity-completed) text back in the buffer:
        // advancing stashes the buffer into this field's slot, so the comment
        // has to travel with it or stashing would drop it.
        ensure_len(&mut self.draft.postings, i.saturating_add(1));
        let comment = self
            .draft
            .postings
            .get(i)
            .map(|p| p.comment.clone())
            .unwrap_or_default();
        self.draft.set_buffer(&join_field_comment(text, &comment));
        self.advance_to(Field::Account(i.saturating_add(1)));
    }

    /// Whether an empty Enter at amount `i` would write the balancing
    /// amount and end the transaction, rather than merely accept a ghost.
    #[must_use]
    pub fn balancing_finishes(&self, i: usize) -> bool {
        !self.has_later_postings(i)
            && self.balancing_prefill(i).is_some()
            && self.balancing_prefill(i) == self.suggestion()
    }

    /// Whether any posting after `i` has been started. The balancing
    /// shortcut must not finish over the top of one.
    fn has_later_postings(&self, i: usize) -> bool {
        self.draft
            .postings
            .get(i.saturating_add(1)..)
            .is_some_and(|rest| {
                rest.iter()
                    .any(|p| !p.account.trim().is_empty() || !p.amount.trim().is_empty())
            })
    }

    /// Finish the transaction at an empty account prompt for posting `i`.
    fn finish(&mut self, i: usize) -> Submit {
        // Drop any trailing empty posting slots.
        self.draft.postings.truncate(i);
        while self
            .draft
            .postings
            .last()
            .is_some_and(|p| p.account.trim().is_empty())
        {
            self.draft.postings.pop();
        }
        self.complete_draft()
    }

    /// Validate the drafted postings and produce the finished transaction.
    fn complete_draft(&self) -> Submit {
        if self.draft.postings.len() < 2 {
            return Submit::Invalid(
                "a transaction needs at least two postings (Ctrl-C to abort)".to_owned(),
            );
        }
        // Refuse to write a transaction that provably does not balance.
        // Unknown imbalance (unparseable or elided amounts) passes — entry
        // tool, not validator.
        let has_bare = self
            .draft
            .postings
            .iter()
            .any(|p| p.amount.trim().is_empty());
        if !has_bare {
            let amounts = self.draft.committed_amounts();
            if let Some(sums) = imbalance(&amounts, &self.ctx.amount_ctx) {
                if !sums.is_empty() {
                    let detail: Vec<String> = sums
                        .iter()
                        .map(|(c, v)| render_amount_like(*v, c, &self.ctx.amount_ctx, &amounts))
                        .collect();
                    return Submit::Invalid(format!(
                        "transaction does not balance (off by {})",
                        detail.join(", ")
                    ));
                }
            }
        }
        let Some(date) = self.draft.date else {
            return Submit::Invalid("no date".to_owned());
        };
        let postings = self.with_equity_conversions(self.draft.postings.clone());
        let txn = NewTransaction {
            date,
            description: self.draft.description.clone(),
            comment: self.draft.comment.clone(),
            postings,
        };
        Submit::Done(Box::new(txn))
    }

    /// Lay out the finished draft's postings, adding the equity conversion
    /// postings its `@`/`@@` costs need.
    ///
    /// Each conversion is written as a group — its own postings, then the
    /// pair of equity postings cancelling its cost — and groups are
    /// separated by a bare `;` line. A posting belonging to no single
    /// conversion, such as one payment funding two of them, is written after
    /// every group rather than assigned to one arbitrarily. Grouping can
    /// reorder the postings; see [`crate::amount::conversion_layout`].
    ///
    /// Returned unchanged when `equity_conversion` is off, when nothing is
    /// converted, or when any amount does not parse.
    fn with_equity_conversions(&self, postings: Vec<Posting>) -> Vec<Posting> {
        if !self.ctx.config.equity_conversion.is_on() {
            return postings;
        }
        let amounts = self.draft.committed_amounts();
        let Some(layout) = crate::amount::conversion_layout(&amounts, &self.ctx.amount_ctx) else {
            return postings;
        };
        let account = &self.ctx.config.equity_conversion_account;
        let mut out: Vec<Posting> = Vec::new();
        let mut sections: Vec<Vec<Posting>> = Vec::new();
        for group in &layout.groups {
            let mut section: Vec<Posting> = group
                .postings
                .iter()
                .filter_map(|i| postings.get(*i).cloned())
                .collect();
            section.extend(
                group
                    .equity
                    .iter()
                    .map(|amount| Posting::new(account, amount)),
            );
            sections.push(section);
        }
        let trailing: Vec<Posting> = layout
            .unassigned
            .iter()
            .filter_map(|i| postings.get(*i).cloned())
            .collect();
        if !trailing.is_empty() {
            sections.push(trailing);
        }
        for (n, section) in sections.into_iter().enumerate() {
            if n > 0 {
                out.push(Posting::new(GROUP_SEPARATOR, ""));
            }
            out.extend(section);
        }
        out
    }

    /// Move to `field`, loading whatever text it already holds. Proposals
    /// are never loaded — they are offered as ghosts, see
    /// [`Self::suggestion`].
    fn advance_to(&mut self, field: Field) {
        self.draft.goto(field);
    }

    /// A close existing account name, for "did you mean".
    #[must_use]
    pub fn near_miss(&self, name: &str) -> Option<String> {
        let pool = self.ctx.account_pool(None);
        let lower = name.to_lowercase();
        let mut best: Option<(usize, &String)> = None;
        for candidate in &pool {
            let d = levenshtein(&lower, &candidate.to_lowercase());
            if best.is_none_or(|(bd, _)| d < bd) {
                best = Some((d, candidate));
            }
        }
        let (d, candidate) = best?;
        // Also accept segment near-misses like `exp:trav` for
        // `expenses:travel:train`, which levenshtein alone would miss.
        let segmentish = complete::match_quality(
            Completion::Prefix,
            complete::Shape::Account,
            name,
            candidate,
        )
        .is_some();
        let close_enough = d <= name.chars().count().saturating_div(2).max(2);
        (segmentish || close_enough).then(|| candidate.clone())
    }

    /// A discoverability hint for the current field, shown dimmed under the
    /// prompt when nothing more urgent (menu, ghost, note) is on screen.
    #[must_use]
    pub fn hint(&self) -> Option<String> {
        let empty = self.draft.buffer.trim().is_empty();
        // One rule, said the same way everywhere: a ghost is accepted by
        // Enter and edited with Tab or →. The only field that adds anything
        // is the account, where refusing the ghost is how you finish the
        // transaction — that difference is forced by the account prompt
        // meaning two things, and is the only one.
        if empty && self.suggestion().is_some() {
            return Some(match self.draft.field {
                Field::Account(_) => {
                    "Enter accepts · Tab or → edits it · Ctrl-U dismisses it".to_owned()
                }
                // Say which Enter this is: on the balancing amount it also
                // ends the transaction, and a hint that promised only
                // "accepts" would be lying about the one prompt that does
                // something extra.
                Field::Amount(i) if self.balancing_finishes(i) => {
                    "Enter writes it and finishes · Tab or → edits it".to_owned()
                }
                _ => "Enter accepts · Tab or → edits it".to_owned(),
            });
        }
        match self.draft.field {
            Field::Date => {
                Some("Enter accepts · u undoes the last transaction · q saves and quits".to_owned())
            }
            Field::Account(i) if i >= 1 && empty => {
                Some("Enter on the empty account finishes the transaction".to_owned())
            }
            _ => None,
        }
    }

    /// The running imbalance of the committed postings, for display.
    /// `None` when some amount is unparseable ("imbalance unknown").
    #[must_use]
    pub fn running_imbalance(&self) -> Option<Vec<(String, Decimal)>> {
        imbalance(&self.draft.committed_amounts(), &self.ctx.amount_ctx)
    }

    /// The live preview block, formatted exactly as it will be written:
    /// rendered against the file's widths so an entry that will reflow the
    /// file visibly shifts the preview.
    #[must_use]
    pub fn preview_lines(&self) -> Vec<String> {
        let mut draft = self.draft.clone();
        draft.stash(); // include the in-progress buffer
        let date_text = draft.date.map_or_else(
            || {
                dates::resolve(&draft.date_input, self.ctx.today).map_or_else(
                    || format!("{}?", draft.date_input),
                    |d| d.format("%Y-%m-%d").to_string(),
                )
            },
            |d| d.format("%Y-%m-%d").to_string(),
        );
        let mut acc_w = self.ctx.acc_w;
        let mut num_w = self.ctx.num_w;
        let rendered_postings: Vec<String> = draft
            .postings
            .iter()
            .filter(|p| !p.account.trim().is_empty())
            .map(|p| {
                let body = if p.amount.trim().is_empty() {
                    format!("    {}", p.account)
                } else {
                    format!("    {}  {}", p.account, p.amount.trim())
                };
                format!("{body}{}", comment_suffix(&p.comment))
            })
            .collect();
        for line in &rendered_postings {
            let p = parse_posting(line);
            if let Some(a) = crate::fmt::posting::account_of(&p) {
                acc_w = acc_w.max(a.chars().count());
            }
            if let crate::fmt::posting::Posting::Amount { num, .. } = &p {
                num_w = num_w.max(num.chars().count());
            }
        }
        let header = format!("{date_text} {}", draft.description)
            .trim_end()
            .to_owned();
        let mut out = vec![format!("{header}{}", comment_suffix(&draft.comment))];
        for line in &rendered_postings {
            out.push(render(acc_w, num_w, &parse_posting(line)));
        }
        out
    }

    /// Replace the draft's content from an edited transaction (the Ctrl-E
    /// editor round-trip), leaving the user at a fresh account prompt.
    pub fn load_draft_from(&mut self, txn: &NewTransaction) {
        self.draft = Draft::new();
        self.draft.date = Some(txn.date);
        self.draft.date_input = txn.date.format("%Y-%m-%d").to_string();
        self.draft.description.clone_from(&txn.description);
        self.draft.postings.clone_from(&txn.postings);
        let next = Field::Account(txn.postings.len());
        self.draft.frontier = next.ordinal();
        self.draft.field = next;
        self.draft.set_buffer("");
    }

    /// Record a finished transaction and reset for the next one.
    pub fn complete(&mut self, txn: NewTransaction) {
        self.completed.push(txn);
        self.reindex();
        self.reset_draft();
    }

    /// Undo the last completed transaction. Returns it, if any.
    pub fn undo(&mut self) -> Option<NewTransaction> {
        let t = self.completed.pop();
        self.reindex();
        self.reset_draft();
        t
    }
}

/// Where tag-name completion applies in a comment buffer.
///
/// hledger comment tags are `name: value`, comma-separated. Only a name
/// completes, so this returns `(text before the name, the partial name)` when
/// the cursor is in name position, and `None` once a `:` has been typed —
/// after that the user is writing a value, which is free text.
fn split_tag_query(buffer: &str) -> Option<(&str, &str)> {
    // The tag being typed starts after the last comma.
    let start = buffer.rfind(',').map_or(0, |i| i.saturating_add(1));
    let current = buffer.get(start..)?;
    // A `:` means the name is settled and this is its value.
    if current.contains(':') {
        return None;
    }
    let name = current.trim_start();
    // Tag names are single words; a space means this is prose, not a tag.
    if name.contains(char::is_whitespace) {
        return None;
    }
    let split = buffer.len().saturating_sub(name.len());
    Some((buffer.get(..split)?, name))
}

/// The tag names in a comment.
///
/// hledger's comment tag syntax is `name: value`, comma-separated; the name is
/// the word before a `:`. Only names are collected — values are free text and
/// completing them would be guessing.
fn tags_in(comment: Option<&str>) -> Vec<String> {
    let Some(text) = comment else {
        return Vec::new();
    };
    let body = text.trim_start().trim_start_matches(';');
    let mut out = Vec::new();
    for part in body.split(',') {
        let Some((name, _value)) = part.split_once(':') else {
            continue;
        };
        // A tag name is the last whitespace-separated word before the colon.
        let name = name.split_whitespace().next_back().unwrap_or_default();
        if !name.is_empty() && !out.iter().any(|x| x == name) {
            out.push(name.to_owned());
        }
    }
    out
}

/// The template postings of a historical transaction: (account, raw amount).
fn template_postings(t: &Transaction) -> Vec<(String, String)> {
    t.postings
        .iter()
        .map(|p| (p.account.clone(), p.amount.clone()))
        .collect()
}

/// The pool entry closest to `name` (case-insensitive edit distance ≤ 2),
/// for "did you mean …?" hints.
fn closest<'a, I: Iterator<Item = &'a String>>(name: &str, pool: I) -> Option<String> {
    let lower = name.to_lowercase();
    pool.map(|c| (levenshtein(&lower, &c.to_lowercase()), c))
        .min_by(|(da, ca), (db, cb)| da.cmp(db).then_with(|| ca.cmp(cb)))
        .filter(|(d, _)| *d <= 2)
        .map(|(_, c)| c.clone())
}

/// Attach a commodity symbol to a bare typed number, following the
/// commodity's declared style for side and spacing (right with a space when
/// no style is declared). The typed number itself is never restyled.
fn attach_commodity(number: &str, symbol: &str, ctx: &AmountCtx) -> String {
    use crate::amount::Side;
    let (side, spaced) = ctx
        .styles
        .get(symbol)
        .map_or((Side::Right, true), |s| (s.symbol_side, s.symbol_space));
    let space = if spaced { " " } else { "" };
    match side {
        Side::Left => format!("{symbol}{space}{number}"),
        Side::Right => format!("{number}{space}{symbol}"),
    }
}

/// Where commodity completion applies in an amount buffer, classified by
/// the last whitespace-separated token. This makes the *second* commodity —
/// after an `@`/`@@` cost or `=`/`==`/`=*`/`==*` assertion tail —
/// completable exactly like the first.
///
/// - partial commodity (`23.45 EU`, `5 USD @ 1.10 E`) → `(head, partial)`:
///   complete the token
/// - number (`23.45`, `5 USD @ 1.10`) → `(buffer, "")`: offer commodities
///   to append
/// - operator (`5 USD @`) → `None`: a price/assertion number must come
///   first, so nothing to offer
fn split_commodity_query(buffer: &str) -> Option<(&str, &str)> {
    if buffer.is_empty() {
        return Some(("", ""));
    }
    let (head, last) = buffer.rfind(char::is_whitespace).map_or(("", buffer), |i| {
        (
            buffer.get(..i).unwrap_or_default(),
            buffer.get(i.saturating_add(1)..).unwrap_or_default(),
        )
    });
    if crate::lex::is_rest_start(last) {
        return None;
    }
    if last.chars().any(|c| c.is_ascii_digit()) {
        return Some((buffer, ""));
    }
    Some((head, last))
}

/// Levenshtein distance over chars (two-row DP, no indexing).
fn levenshtein(a: &str, b: &str) -> usize {
    let bv: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=bv.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur: Vec<usize> = Vec::with_capacity(bv.len().saturating_add(1));
        cur.push(i.saturating_add(1));
        for (j, cb) in bv.iter().enumerate() {
            let del = prev
                .get(j.saturating_add(1))
                .copied()
                .unwrap_or(usize::MAX)
                .saturating_add(1);
            let ins = cur.last().copied().unwrap_or(usize::MAX).saturating_add(1);
            let sub = prev
                .get(j)
                .copied()
                .unwrap_or(usize::MAX)
                .saturating_add(usize::from(ca != *cb));
            cur.push(del.min(ins).min(sub));
        }
        prev = cur;
    }
    prev.last().copied().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::add::parser::parse_journal;
    use std::fs;
    use tempfile::TempDir;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    const TODAY: fn() -> NaiveDate = || d(2026, 8, 10);

    /// A session over a small journal on disk.
    fn session(journal_src: &str) -> (Session, TempDir) {
        session_with(journal_src, Config::default())
    }

    /// A config whose `add.default_commodity` is `sample`. `add` takes its
    /// default from the configuration alone; a journal's `D` directive is
    /// `fmt --explicit`'s business, not `add`'s.
    fn config_with_default(sample: &str) -> Config {
        Config {
            default_commodity: Some(sample.to_owned()),
            ..Config::default()
        }
    }

    fn session_with(journal_src: &str, config: Config) -> (Session, TempDir) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("main.journal");
        fs::write(&path, journal_src).unwrap();
        let journal = parse_journal(&path).unwrap();
        let map = FileMap::build(journal_src);
        let ctx = SessionCtx::new(journal, config, TODAY(), path, &map);
        (Session::new(ctx), dir)
    }

    const JOURNAL: &str = "\
2026-08-01 Rewe
    expenses:groceries        23.45 EUR
    liabilities:creditcard   -23.45 EUR

2026-08-05 Rewe
    expenses:groceries        12.00 EUR
    assets:bank:checking     -12.00 EUR

2026-08-06 Deutsche Bahn
    expenses:travel:train     49.00 EUR
    assets:bank:checking     -49.00 EUR
";

    fn type_in(s: &mut Session, text: &str) {
        s.draft.set_buffer(text);
    }

    #[test]
    fn typed_amounts_are_restyled_to_the_declared_style() {
        let src = "commodity 1_000.00 EUR\n\n2026-08-01 Rewe\n    expenses:groceries   23.45 EUR\n    assets:bank         -23.45 EUR\n";
        let (mut s, _t) = session(src);
        s.submit(); // date
        type_in(&mut s, "Ikea");
        s.submit(); // description
        type_in(&mut s, "expenses:household");
        s.submit();
        type_in(&mut s, "1234EUR");
        assert_eq!(s.submit(), Submit::Advanced);
        // Grouped and spaced per the declaration, and filled out to its two
        // decimal places — the same form the generated balancing amount
        // would take, so the two sides of the transaction match.
        assert_eq!(s.draft.postings[0].amount, "1_234.00 EUR");
        // Undeclared commodities stay as typed.
        type_in(&mut s, "liabilities:cc");
        s.submit();
        type_in(&mut s, "-10USD");
        assert_eq!(
            s.submit(),
            Submit::AdvancedWithNote("USD is a commodity new to this journal".to_owned())
        );
        assert_eq!(s.draft.postings[1].amount, "-10USD");
    }

    #[test]
    fn declared_decimal_places_are_a_floor_not_a_ceiling() {
        // The `commodity` directive states the default precision, so an
        // entered amount is filled out to it — but hledger accepts more, and
        // rounding down would lose value, so extra places survive untouched.
        let src =
            "commodity 1_000.00 EUR\n\n2026-08-01 Rewe\n    a:b   1.00 EUR\n    c:d  -1.00 EUR\n";
        for (typed, written) in [
            ("4 EUR", "4.00 EUR"),
            ("4.0 EUR", "4.00 EUR"),
            ("4.00 EUR", "4.00 EUR"),
            // More precision than declared: kept exactly.
            ("4.000 EUR", "4.000 EUR"),
            ("4.001 EUR", "4.001 EUR"),
            // Undeclared commodity: nothing is assumed about it at all.
            ("4 USD", "4 USD"),
            // Unitless amounts have no style to follow.
            ("4", "4"),
            // Prices and assertions are amounts too, and take their own
            // commodity's declared places.
            ("4 EUR @ 1.1 EUR", "4.00 EUR @ 1.10 EUR"),
            ("4 EUR = 9 EUR", "4.00 EUR = 9.00 EUR"),
            // …but only where a style is declared.
            ("4 EUR @ 1.1 USD", "4.00 EUR @ 1.1 USD"),
        ] {
            let (mut s, _t) = session(src);
            s.submit();
            type_in(&mut s, "Ikea");
            s.submit();
            type_in(&mut s, "a:b");
            s.submit();
            type_in(&mut s, typed);
            s.submit();
            assert_eq!(s.draft.postings[0].amount, written, "typed {typed:?}");
        }
    }

    #[test]
    fn padding_an_entered_amount_never_changes_its_value() {
        let src =
            "commodity 1_000.00 EUR\n\n2026-08-01 Rewe\n    a:b   1.00 EUR\n    c:d  -1.00 EUR\n";
        let (s, _t) = session(src);
        for typed in [
            "4 EUR",
            "4.0 EUR",
            "4.000 EUR",
            "4.001 EUR",
            "1234EUR",
            "4 EUR @ 1.1 EUR",
        ] {
            let written = crate::amount::restyle_entered(typed, &s.ctx.amount_ctx);
            assert_eq!(
                parse_amount(typed, &s.ctx.amount_ctx),
                parse_amount(&written, &s.ctx.amount_ctx),
                "{typed:?} -> {written:?} changed value"
            );
        }
    }

    #[test]
    fn the_happy_path_enters_a_balanced_transaction() {
        let (mut s, _t) = session(JOURNAL);
        // Date empty, today ghost-suggested; Enter accepts it.
        assert_eq!(s.draft.buffer, "");
        assert_eq!(s.suggestion().as_deref(), Some("2026-08-10"));
        assert_eq!(s.submit(), Submit::Advanced);
        assert_eq!(s.draft.field, Field::Description);

        type_in(&mut s, "Rewe");
        assert_eq!(s.submit(), Submit::Advanced);
        // Template from the most recent Rewe: account 1 suggested, not
        // entered. Enter accepts it.
        assert_eq!(s.draft.field, Field::Account(0));
        assert_eq!(s.draft.buffer, "");
        assert_eq!(s.suggestion().as_deref(), Some("expenses:groceries"));
        assert_eq!(s.submit(), Submit::Advanced);
        // Template amount ghost-suggested, never pre-filled.
        assert_eq!(s.draft.field, Field::Amount(0));
        assert_eq!(s.draft.buffer, "");
        assert_eq!(s.suggestion().as_deref(), Some("12.00 EUR"));
        type_in(&mut s, "18.20 EUR");
        assert_eq!(s.submit(), Submit::Advanced);
        // Account 2 from template, likewise suggested.
        assert_eq!(s.draft.buffer, "");
        assert_eq!(s.suggestion().as_deref(), Some("assets:bank:checking"));
        assert_eq!(s.submit(), Submit::Advanced);
        // Balancing amount suggested: negated running sum, not the template.
        // Enter accepts it, as at every prompt; the transaction then ends at
        // the account prompt after it, which suggests nothing.
        assert_eq!(s.draft.buffer, "");
        assert_eq!(s.suggestion().as_deref(), Some("-18.20 EUR"));
        let Submit::Done(txn) = s.submit() else {
            panic!("expected the transaction to finish");
        };
        assert_eq!(txn.date, d(2026, 8, 10));
        assert_eq!(txn.description, "Rewe");
        assert_eq!(
            txn.postings,
            vec![
                Posting::new("expenses:groceries", "18.20 EUR"),
                Posting::new("assets:bank:checking", "-18.20 EUR"),
            ]
        );
    }

    // ---- epic 3: inline `; comment` ----

    /// A comment is the tail of the line you are already typing, never a
    /// prompt of its own — entering no comment costs nothing.
    #[test]
    fn a_trailing_semicolon_makes_the_rest_of_the_field_a_comment() {
        let (mut s, _t) = session(JOURNAL);
        assert_eq!(s.submit(), Submit::Advanced); // date

        type_in(&mut s, "Rewe ; trip: berlin");
        assert_eq!(s.submit(), Submit::Advanced);
        // Straight to the first posting: no comment prompt in between.
        assert_eq!(s.draft.field, Field::Account(0));
        assert_eq!(s.draft.description, "Rewe");
        assert_eq!(s.draft.comment, "trip: berlin");

        assert_eq!(s.submit(), Submit::Advanced); // template account
        type_in(&mut s, "18.20 EUR ; receipt: yes");
        assert_eq!(s.submit(), Submit::Advanced);
        assert_eq!(s.draft.field, Field::Account(1));
        assert_eq!(s.draft.postings[0].amount, "18.20 EUR");
        assert_eq!(s.draft.postings[0].comment, "receipt: yes");

        assert_eq!(s.submit(), Submit::Advanced); // template account 2
                                                  // The empty amount still balances and finishes outright.
        let Submit::Done(txn) = s.submit() else {
            panic!("expected Done");
        };
        assert_eq!(txn.comment, "trip: berlin");
        assert_eq!(txn.postings[0].comment, "receipt: yes");
        assert_eq!(txn.postings[1].comment, "");
        assert_eq!(txn.postings[1].amount, "-18.20 EUR");
    }

    #[test]
    fn entering_no_comment_costs_no_keystrokes() {
        // The whole point of the inline form: the field count is exactly what
        // it was before comments existed.
        let (mut s, _t) = session(JOURNAL);
        s.submit(); // date
        type_in(&mut s, "Rewe");
        s.submit(); // description
        assert_eq!(s.draft.field, Field::Account(0));
        s.submit(); // account 1 (template)
        assert_eq!(s.draft.field, Field::Amount(0));
        type_in(&mut s, "18.20 EUR");
        s.submit();
        assert_eq!(s.draft.field, Field::Account(1));
        s.submit(); // account 2 (template)
        assert!(matches!(s.submit(), Submit::Done(_)));
    }

    #[test]
    fn a_comment_survives_navigating_away_and_back() {
        let (mut s, _t) = session(JOURNAL);
        s.submit();
        type_in(&mut s, "Rewe ; trip: berlin");
        s.submit();
        assert!(s.draft.nav_up());
        assert_eq!(s.draft.field, Field::Description);
        // Re-joined for editing, exactly as typed.
        assert_eq!(s.draft.buffer, "Rewe ; trip: berlin");
        // …and deleting it there clears it.
        type_in(&mut s, "Rewe");
        s.submit();
        assert_eq!(s.draft.comment, "");
    }

    #[test]
    fn a_comment_typed_on_the_account_belongs_to_that_posting() {
        // A `;` in an account field would otherwise end up inside the account
        // name and produce a corrupt posting line.
        let (mut s, _t) = session(JOURNAL);
        s.submit();
        type_in(&mut s, "Rewe");
        s.submit();
        type_in(&mut s, "expenses:household ; gift: yes");
        s.submit();
        assert_eq!(s.draft.postings[0].account, "expenses:household");
        assert_eq!(s.draft.postings[0].comment, "gift: yes");
        // The amount field carries it, being the end of the line.
        assert_eq!(s.draft.buffer, "; gift: yes");
    }

    #[test]
    fn comments_are_rendered_into_the_written_lines() {
        let txn = NewTransaction {
            date: d(2026, 8, 10),
            description: "Rewe".into(),
            comment: "trip: berlin".into(),
            postings: vec![
                Posting {
                    account: "expenses:groceries".into(),
                    amount: "18.20 EUR".into(),
                    comment: "receipt: yes".into(),
                },
                Posting::new("assets:bank:checking", "-18.20 EUR"),
            ],
        };
        assert_eq!(
            txn.raw_lines(),
            vec![
                "2026-08-10 Rewe  ; trip: berlin".to_owned(),
                "    expenses:groceries  18.20 EUR  ; receipt: yes".to_owned(),
                "    assets:bank:checking  -18.20 EUR".to_owned(),
            ]
        );
    }

    #[test]
    fn the_preview_shows_comments_as_they_will_be_written() {
        let (mut s, _t) = session(JOURNAL);
        s.submit();
        type_in(&mut s, "Rewe ; trip: berlin");
        s.submit();
        s.submit(); // template account
        type_in(&mut s, "18.20 EUR ; receipt: yes");
        let preview = s.preview_lines();
        assert_eq!(preview[0], "2026-08-10 Rewe  ; trip: berlin");
        assert!(preview[1].ends_with("  ; receipt: yes"), "{}", preview[1]);
    }

    // ---- epic 3: tag completion inside comments ----

    #[test]
    fn tag_names_complete_inside_a_comment() {
        let src = "tag project\ntag receipt\n\n2026-08-01 x  ; client: acme\n    a:b   1 EUR\n    c:d  -1 EUR\n";
        let (mut s, _t) = session(src);
        s.submit();
        // Still on the description field — the `;` is what switches
        // completion over to tag names.
        type_in(&mut s, "x ; ");
        let all = s.candidates();
        assert!(all.contains(&"x ; project:".to_owned()), "{all:?}");
        assert!(all.contains(&"x ; client:".to_owned()), "{all:?}");

        type_in(&mut s, "x ; proj");
        assert_eq!(s.complete_buffer().as_deref(), Some("x ; project:"));
    }

    #[test]
    fn a_tag_value_does_not_complete_but_the_next_name_does() {
        let src = "tag project\ntag receipt\n";
        let (mut s, _t) = session(src);
        s.submit();
        // Past the colon this is a value: free text, nothing to offer.
        type_in(&mut s, "x ; project: ac");
        assert!(s.candidates().is_empty());
        // After a comma a new tag name starts, and completion resumes.
        type_in(&mut s, "x ; project: acme, rec");
        assert_eq!(
            s.complete_buffer().as_deref(),
            Some("x ; project: acme, receipt:")
        );
    }

    #[test]
    fn tag_names_are_read_out_of_posting_comments_too() {
        let src = "2026-08-01 x\n    a:b   1 EUR  ; receipt: 12\n    c:d  -1 EUR\n";
        let (s, _t) = session(src);
        assert_eq!(s.ctx.tag_pool(), vec!["receipt".to_owned()]);
    }

    // ---- epic 3: the payee note and strict check ----

    #[test]
    fn a_description_new_to_the_journal_gets_a_note_with_a_near_miss() {
        // Motivation #1: hledger's `add` matches `bahn` against
        // `Deutsche Bahn` to pick defaults and then writes the literal
        // `bahn`, forking the payee silently.
        let (mut s, _t) = session(JOURNAL);
        s.submit();
        type_in(&mut s, "bahn");
        let Submit::AdvancedWithNote(note) = s.submit() else {
            panic!("expected a note for a description new to the journal");
        };
        assert!(note.contains("bahn is new to this journal"), "{note}");
        assert!(note.contains("did you mean Deutsche Bahn"), "{note}");
    }

    #[test]
    fn a_description_already_in_the_journal_passes_quietly() {
        let (mut s, _t) = session(JOURNAL);
        s.submit();
        type_in(&mut s, "Rewe");
        assert_eq!(s.submit(), Submit::Advanced);
    }

    #[test]
    fn a_declared_payee_is_known_even_with_no_transactions() {
        // The point of the `payee` directive: known-good with no history, so
        // the near-miss hint can be trusted.
        let (mut s, _t) = session("payee Hofpfisterei\n");
        s.submit();
        type_in(&mut s, "Hofpfisterei");
        assert_eq!(s.submit(), Submit::Advanced);
    }

    #[test]
    fn strict_mode_asks_before_using_an_undeclared_payee() {
        let cfg = Config {
            strict: true,
            ..Config::default()
        };
        let (mut s, _t) = session_with("payee Deutsche Bahn\n", cfg);
        s.submit();
        type_in(&mut s, "bahn");
        let Submit::Confirm { question } = s.submit() else {
            panic!("expected Confirm");
        };
        assert!(question.contains("not a declared payee"), "{question}");
        assert!(question.contains("use it anyway"), "{question}");
        assert!(
            question.contains("did you mean Deutsche Bahn"),
            "{question}"
        );
        // Confirming proceeds, and the answer is not asked again this
        // session.
        assert_eq!(s.submit_confirmed(), Submit::Advanced);
        assert!(s.payee_known("bahn"));
    }

    // ---- epic 3: payee | note ----

    #[test]
    fn the_payee_check_tests_the_payee_half_not_the_whole_description() {
        // hledger's own behaviour, verified: `payee Deutsche Bahn` satisfies
        // `check payees` for `Deutsche Bahn | ticket`, and declaring the
        // whole string instead does *not*.
        let cfg = Config {
            strict: true,
            ..Config::default()
        };
        let (mut s, _t) = session_with("payee Deutsche Bahn\n", cfg.clone());
        s.submit();
        type_in(&mut s, "Deutsche Bahn | ticket to Koeln");
        assert_eq!(s.submit(), Submit::Advanced);
        assert_eq!(s.draft.description, "Deutsche Bahn | ticket to Koeln");
        assert_eq!(s.draft.payee(), "Deutsche Bahn");
        assert_eq!(s.draft.note(), "ticket to Koeln");

        // The control: declaring the whole description leaves the payee
        // undeclared, exactly as hledger reports it.
        let (mut s, _t) = session_with("payee Deutsche Bahn | ticket to Koeln\n", cfg);
        s.submit();
        type_in(&mut s, "Deutsche Bahn | ticket to Koeln");
        let Submit::Confirm { question } = s.submit() else {
            panic!("expected Confirm");
        };
        assert!(
            question.starts_with("Deutsche Bahn is not a declared payee"),
            "{question}"
        );
    }

    #[test]
    fn the_new_payee_note_names_the_payee_not_the_description() {
        let (mut s, _t) = session(JOURNAL);
        s.submit();
        type_in(&mut s, "Bahnhof Kiosk | coffee");
        let Submit::AdvancedWithNote(note) = s.submit() else {
            panic!("expected a note");
        };
        assert!(note.starts_with("Bahnhof Kiosk is new"), "{note}");
        // …and the near-miss answers with a payee, never a whole description.
        assert!(!note.contains('|'), "{note}");
    }

    #[test]
    fn a_note_does_not_make_a_known_payee_look_new() {
        // `Rewe` is used in JOURNAL; adding a note must not turn it into an
        // unknown payee.
        let (mut s, _t) = session(JOURNAL);
        s.submit();
        type_in(&mut s, "Rewe | big shop");
        assert_eq!(s.submit(), Submit::Advanced);
    }

    #[test]
    fn the_template_and_account_pool_follow_the_payee_through_a_note() {
        let (mut s, _t) = session(JOURNAL);
        s.submit();
        // JOURNAL's most recent Rewe has no note; this one does, and the
        // template must still be found.
        type_in(&mut s, "Rewe | big shop");
        s.submit();
        assert_eq!(s.draft.field, Field::Account(0));
        // The template account is offered as a ghost, not typed into the
        // buffer.
        assert!(s.draft.buffer.is_empty());
        assert_eq!(s.suggestion().as_deref(), Some("expenses:groceries"));
        // Account ranking is conditioned on the payee.
        assert_eq!(s.account_pool()[0], "expenses:groceries");
    }

    #[test]
    fn a_note_and_a_comment_coexist_on_one_description() {
        let (mut s, _t) = session(JOURNAL);
        s.submit();
        type_in(&mut s, "Rewe | big shop ; trip: berlin");
        s.submit();
        assert_eq!(s.draft.description, "Rewe | big shop");
        assert_eq!(s.draft.payee(), "Rewe");
        assert_eq!(s.draft.note(), "big shop");
        assert_eq!(s.draft.comment, "trip: berlin");
    }

    #[test]
    fn declared_payees_join_the_description_completion_pool() {
        let src = "payee Hofpfisterei\n\n2026-08-01 Rewe\n    a:b   1 EUR\n    c:d  -1 EUR\n";
        let (mut s, _t) = session(src);
        s.submit();
        type_in(&mut s, "Hof");
        assert_eq!(s.complete_buffer().as_deref(), Some("Hofpfisterei"));
    }

    /// Enter the IVPN-shaped conversion transaction and finish it.
    fn conversion_txn(config: Config) -> Submit {
        let (mut s, _t) = session_with(JOURNAL, config);
        s.submit(); // date
        type_in(&mut s, "IVPN");
        s.submit();
        type_in(&mut s, "expenses:subscriptions:services");
        s.submit();
        type_in(&mut s, "10 USD @@ 9.06 EUR");
        s.submit();
        type_in(&mut s, "assets:bank:checking");
        s.submit();
        type_in(&mut s, "-9.06 EUR");
        s.submit();
        // Empty account finishes: the postings already balance at cost.
        type_in(&mut s, "");
        s.submit()
    }

    #[test]
    fn equity_conversions_are_off_by_default() {
        let Submit::Done(txn) = conversion_txn(Config::default()) else {
            panic!("expected Done");
        };
        assert_eq!(txn.postings.len(), 2);
    }

    #[test]
    fn equity_conversions_follow_the_postings_they_convert() {
        let config = Config {
            equity_conversion: crate::config::EquityConversion::On,
            ..Config::default()
        };
        let Submit::Done(txn) = conversion_txn(config) else {
            panic!("expected Done");
        };
        // One conversion: both postings belong to it, so its pair closes the
        // transaction whichever of them was typed first.
        assert_eq!(
            txn.postings,
            vec![
                Posting::new("expenses:subscriptions:services", "10 USD @@ 9.06 EUR"),
                Posting::new("assets:bank:checking", "-9.06 EUR"),
                Posting::new("equity:conversion", "-10 USD"),
                Posting::new("equity:conversion", "9.06 EUR"),
            ]
        );
    }

    /// Lay out `postings` as a finished transaction would be written.
    fn laid_out(postings: &[(&str, &str)]) -> Vec<(String, String)> {
        let config = Config {
            equity_conversion: crate::config::EquityConversion::On,
            ..Config::default()
        };
        let (mut s, _t) = session_with(JOURNAL, config);
        s.draft.postings = postings.iter().map(|(a, m)| Posting::new(a, m)).collect();
        s.with_equity_conversions(s.draft.postings.clone())
            .into_iter()
            .map(|p| (p.account, p.amount))
            .collect()
    }

    #[test]
    fn each_conversion_is_written_as_its_own_group() {
        // The yen posting was typed second but belongs to the second
        // conversion, so it is written beside it.
        assert_eq!(
            laid_out(&[
                ("assets:dollars", "$-135"),
                ("assets:yen", "¥-100"),
                ("assets:euros", "€100 @ $1.35"),
                ("assets:euros", "€1 @@ ¥100"),
            ]),
            vec![
                ("assets:dollars".to_owned(), "$-135".to_owned()),
                ("assets:euros".to_owned(), "€100 @ $1.35".to_owned()),
                ("equity:conversion".to_owned(), "€-100".to_owned()),
                ("equity:conversion".to_owned(), "$135".to_owned()),
                (";".to_owned(), String::new()),
                ("assets:yen".to_owned(), "¥-100".to_owned()),
                ("assets:euros".to_owned(), "€1 @@ ¥100".to_owned()),
                ("equity:conversion".to_owned(), "€-1".to_owned()),
                ("equity:conversion".to_owned(), "¥100".to_owned()),
            ]
        );
    }

    #[test]
    fn a_posting_funding_two_conversions_is_written_last() {
        assert_eq!(
            laid_out(&[
                ("expenses:food", "€20 @ $1.10"),
                ("expenses:tips", "€2 @ $1.10"),
                ("assets:checking", "$-24.20"),
            ]),
            vec![
                ("expenses:food".to_owned(), "€20 @ $1.10".to_owned()),
                ("equity:conversion".to_owned(), "€-20".to_owned()),
                ("equity:conversion".to_owned(), "$22.00".to_owned()),
                (";".to_owned(), String::new()),
                ("expenses:tips".to_owned(), "€2 @ $1.10".to_owned()),
                ("equity:conversion".to_owned(), "€-2".to_owned()),
                ("equity:conversion".to_owned(), "$2.20".to_owned()),
                (";".to_owned(), String::new()),
                ("assets:checking".to_owned(), "$-24.20".to_owned()),
            ]
        );
    }

    #[test]
    fn a_single_conversion_gets_no_separator() {
        let out = laid_out(&[
            ("assets:dollars", "$-135"),
            ("assets:euros", "€100 @ $1.35"),
        ]);
        assert!(out.iter().all(|(a, _)| a != ";"), "{out:?}");
    }

    #[test]
    fn the_equity_conversion_account_is_configurable() {
        let config = Config {
            equity_conversion: crate::config::EquityConversion::On,
            equity_conversion_account: "equity:trading".to_owned(),
            ..Config::default()
        };
        let Submit::Done(txn) = conversion_txn(config) else {
            panic!("expected Done");
        };
        assert!(txn.postings[2..]
            .iter()
            .all(|p| p.account == "equity:trading"));
    }

    #[test]
    fn a_single_commodity_transaction_gets_no_equity_conversions() {
        let config = Config {
            equity_conversion: crate::config::EquityConversion::On,
            ..Config::default()
        };
        let (mut s, _t) = session_with(JOURNAL, config);
        s.submit();
        type_in(&mut s, "Rewe");
        s.submit();
        s.submit(); // templated account
        type_in(&mut s, "18.20 EUR");
        s.submit();
        s.submit(); // templated account 2
        let Submit::Done(txn) = s.submit() else {
            panic!("expected Done");
        };
        assert_eq!(txn.postings.len(), 2);
    }

    #[test]
    fn an_unbalanced_transaction_cannot_finish() {
        let (mut s, _t) = session(JOURNAL);
        s.submit();
        type_in(&mut s, "Test");
        s.submit();
        type_in(&mut s, "expenses:groceries");
        s.submit();
        type_in(&mut s, "10 EUR");
        s.submit();
        type_in(&mut s, "assets:bank:checking");
        s.submit();
        type_in(&mut s, "-9 EUR");
        s.submit();
        type_in(&mut s, "");
        let r = s.submit();
        let Submit::Invalid(msg) = r else {
            panic!("expected Invalid, got {r:?}");
        };
        assert!(msg.contains("balance"), "{msg}");
        assert!(msg.contains('1'), "{msg}");
    }

    #[test]
    fn an_empty_amount_writes_the_balancing_amount_and_finishes() {
        let (mut s, _t) = session(JOURNAL);
        s.submit();
        type_in(&mut s, "Test");
        s.submit();
        type_in(&mut s, "expenses:groceries");
        s.submit();
        type_in(&mut s, "10 EUR");
        s.submit();
        type_in(&mut s, "assets:bank:checking");
        s.submit();
        // The balancing ghost is the one deliberate exception: accepting it
        // also ends the transaction, and the hint says so.
        type_in(&mut s, "");
        assert_eq!(s.suggestion().as_deref(), Some("-10 EUR"));
        assert!(s.balancing_finishes(1));
        assert_eq!(
            s.hint().as_deref(),
            Some("Enter writes it and finishes · Tab or → edits it")
        );
        let r = s.submit();
        let Submit::Done(txn) = r else {
            panic!("expected Done, got {r:?}");
        };
        // Never elided — the balancing amount is written out.
        assert_eq!(txn.postings[1].amount, "-10 EUR");
        assert_eq!(txn.postings.len(), 2);
    }

    #[test]
    fn the_first_posting_requires_an_amount() {
        let (mut s, _t) = session(JOURNAL);
        s.submit();
        type_in(&mut s, "Test");
        s.submit();
        type_in(&mut s, "expenses:groceries");
        s.submit();
        // Nothing is suggested for the first amount of a transaction with
        // no template, and there is nothing to balance against either, so
        // Enter cannot invent one.
        type_in(&mut s, "");
        assert_eq!(s.suggestion(), None);
        assert!(!s.balancing_finishes(0));
        let r = s.submit();
        let Submit::Invalid(msg) = r else {
            panic!("expected Invalid, got {r:?}");
        };
        assert!(msg.contains("first posting"), "{msg}");
        assert_eq!(s.draft.field, Field::Amount(0));
    }

    #[test]
    fn an_empty_amount_with_nothing_to_balance_is_rejected() {
        let (mut s, _t) = session(JOURNAL);
        s.submit();
        type_in(&mut s, "Test");
        s.submit();
        type_in(&mut s, "expenses:groceries");
        s.submit();
        type_in(&mut s, "10 EUR");
        s.submit();
        type_in(&mut s, "assets:bank:checking");
        s.submit();
        type_in(&mut s, "-10 EUR");
        s.submit();
        type_in(&mut s, "expenses:misc");
        s.submit();
        // Already balanced: nothing sensible to write here.
        type_in(&mut s, "");
        let r = s.submit();
        assert!(matches!(r, Submit::Invalid(_)), "{r:?}");
    }

    #[test]
    fn q_at_the_date_prompt_quits() {
        let (mut s, _t) = session(JOURNAL);
        type_in(&mut s, "q");
        assert_eq!(s.submit(), Submit::Quit);
        // Anywhere else `q` is just text — a description new to the journal,
        // so it is accepted with the passing note.
        type_in(&mut s, "today");
        s.submit();
        type_in(&mut s, "q");
        assert!(matches!(s.submit(), Submit::AdvancedWithNote(_)));
        assert_eq!(s.draft.description, "q");
    }

    #[test]
    fn ctrl_w_on_accounts_deletes_one_segment_keeping_the_colon() {
        let mut d = Draft::new();
        d.set_buffer("expenses:travel:train");
        d.delete_word(true);
        assert_eq!(d.buffer, "expenses:travel:");
        // Right after a colon, the colon and its segment go together.
        d.delete_word(true);
        assert_eq!(d.buffer, "expenses:");
        d.delete_word(true);
        assert_eq!(d.buffer, "");
        // Non-account fields keep whitespace-word deletion.
        d.set_buffer("23.45 EUR");
        d.delete_word(false);
        assert_eq!(d.buffer, "23.45 ");
    }

    #[test]
    fn bad_dates_and_bad_amounts_are_rejected_in_place() {
        let (mut s, _t) = session(JOURNAL);
        type_in(&mut s, "not a date");
        assert!(matches!(s.submit(), Submit::Invalid(_)));
        assert_eq!(s.draft.field, Field::Date);
        type_in(&mut s, "yesterday");
        s.submit();
        type_in(&mut s, "X");
        s.submit();
        type_in(&mut s, "expenses:groceries");
        s.submit();
        type_in(&mut s, "one hundred");
        assert!(matches!(s.submit(), Submit::Invalid(_)));
        assert_eq!(s.draft.field, Field::Amount(0));
    }

    #[test]
    fn date_resolution_uses_smart_dates() {
        let (mut s, _t) = session(JOURNAL);
        type_in(&mut s, "5");
        s.submit();
        assert_eq!(s.draft.date, Some(d(2026, 8, 5)));
    }

    #[test]
    fn suggestions_vanish_once_the_user_types() {
        let (mut s, _t) = session(JOURNAL);
        assert_eq!(s.suggestion().as_deref(), Some("2026-08-10"));
        s.draft.insert('5');
        assert_eq!(s.suggestion(), None);
    }

    #[test]
    fn an_empty_date_re_accepts_the_date_shown_as_suggestion() {
        let (mut s, _t) = session(JOURNAL);
        type_in(&mut s, "5");
        s.submit();
        // Back at the date field, the accepted date is the suggestion, and
        // an empty Enter keeps it rather than resetting to today.
        s.draft.nav_up();
        s.draft.set_buffer("");
        assert_eq!(s.suggestion().as_deref(), Some("2026-08-05"));
        assert_eq!(s.submit(), Submit::Advanced);
        assert_eq!(s.draft.date, Some(d(2026, 8, 5)));
    }

    #[test]
    fn new_account_warn_policy_advances_with_note() {
        // Not strict (the default): the undeclared, never-used account is
        // accepted, with a passing note.
        let (mut s, _t) = session(JOURNAL);
        s.submit();
        type_in(&mut s, "X");
        s.submit();
        type_in(&mut s, "expenses:brandnew");
        let r = s.submit();
        assert!(matches!(r, Submit::AdvancedWithNote(_)), "{r:?}");
        assert_eq!(s.draft.field, Field::Amount(0));
    }

    #[test]
    fn strict_mode_asks_before_using_undeclared_accounts() {
        let src = "payee X\naccount expenses:travel:train\naccount expenses:groceries\n";
        let cfg = Config {
            strict: true,
            ..Config::default()
        };
        let (mut s, _t) = session_with(src, cfg);
        s.submit();
        type_in(&mut s, "X");
        s.submit();
        type_in(&mut s, "exp:trav");
        let r = s.submit();
        let Submit::Confirm { question } = r else {
            panic!("expected Confirm, got {r:?}");
        };
        // The wording is about *using* the account — hledger-x declares
        // nothing — and carries the near-miss.
        assert!(question.contains("not a declared account"), "{question}");
        assert!(question.contains("use it anyway"), "{question}");
        assert!(
            question.contains("did you mean expenses:travel:train"),
            "{question}"
        );
        // Confirming proceeds.
        assert_eq!(s.submit_confirmed(), Submit::Advanced);
    }

    #[test]
    fn strict_mode_asks_before_using_undeclared_commodities() {
        let src = "payee X\naccount a:b\naccount c:d\ncommodity 1.00 EUR\n";
        let cfg = Config {
            strict: true,
            ..Config::default()
        };
        let (mut s, _t) = session_with(src, cfg);
        s.submit();
        type_in(&mut s, "X");
        s.submit();
        type_in(&mut s, "a:b");
        s.submit();
        type_in(&mut s, "5 EUX");
        let r = s.submit();
        let Submit::Confirm { question } = r else {
            panic!("expected Confirm, got {r:?}");
        };
        assert!(
            question.contains("EUX is not a declared commodity"),
            "{question}"
        );
        assert!(question.contains("did you mean EUR"), "{question}");
        assert_eq!(s.submit_confirmed(), Submit::Advanced);
        // A declared commodity passes without a question…
        type_in(&mut s, "c:d");
        s.submit();
        type_in(&mut s, "-5 EUR");
        assert_eq!(s.submit(), Submit::Advanced);
    }

    #[test]
    fn strict_mode_checks_tail_commodities_too() {
        let src = "payee X\naccount a:b\naccount c:d\ncommodity 1.00 EUR\n";
        let cfg = Config {
            strict: true,
            ..Config::default()
        };
        let (mut s, _t) = session_with(src, cfg);
        s.submit();
        type_in(&mut s, "X");
        s.submit();
        type_in(&mut s, "a:b");
        s.submit();
        // Face commodity declared, price commodity not: the tail is
        // questioned.
        type_in(&mut s, "5 EUR @ 1.10 USD");
        let r = s.submit();
        let Submit::Confirm { question } = r else {
            panic!("expected Confirm, got {r:?}");
        };
        assert!(
            question.contains("USD is not a declared commodity"),
            "{question}"
        );
        assert_eq!(s.submit_confirmed(), Submit::Advanced);
        // A fully declared assertion tail passes silently.
        s.draft.nav_up();
        type_in(&mut s, "5 EUR = 5 EUR");
        assert_eq!(s.submit(), Submit::Advanced);
    }

    #[test]
    fn strict_mode_never_questions_unitless_amounts() {
        let src = "payee X\naccount a:b\naccount c:d\ncommodity 1.00 EUR\n";
        let cfg = Config {
            strict: true,
            ..Config::default()
        };
        let (mut s, _t) = session_with(src, cfg);
        s.submit();
        type_in(&mut s, "X");
        s.submit();
        type_in(&mut s, "a:b");
        s.submit();
        type_in(&mut s, "5");
        assert_eq!(s.submit(), Submit::Advanced);
    }

    #[test]
    fn known_accounts_include_those_used_in_transactions() {
        let (mut s, _t) = session(JOURNAL);
        s.submit();
        type_in(&mut s, "X");
        s.submit();
        type_in(&mut s, "expenses:travel:train");
        assert_eq!(s.submit(), Submit::Advanced);
    }

    #[test]
    fn description_completion_is_frecency_ranked_and_conditioned() {
        let (mut s, _t) = session(JOURNAL);
        s.submit();
        // Substring matching on descriptions.
        type_in(&mut s, "ew");
        let c = s.candidates();
        assert_eq!(c, vec!["Rewe"]);
        type_in(&mut s, "Rewe");
        s.submit();
        // Account candidates conditioned on Rewe: groceries first.
        type_in(&mut s, "");
        let c = s.candidates();
        assert_eq!(c.first().map(String::as_str), Some("expenses:groceries"));
        // Segment-wise matching: `ex` and `tra` each match their own
        // segment, never across the colon.
        type_in(&mut s, "ex:tra");
        assert_eq!(s.candidates(), vec!["expenses:travel:train"]);
    }

    #[test]
    fn tab_completes_a_unique_account_outright() {
        let (mut s, _t) = session(JOURNAL);
        s.submit();
        type_in(&mut s, "Rewe");
        s.submit();
        // Substring (the default): `roc` is unique mid-segment, so Tab
        // finishes it — no menu, no Enter to select.
        type_in(&mut s, "roc");
        assert_eq!(s.complete_buffer().as_deref(), Some("expenses:groceries"));
        // Ambiguous: nothing unanimous past `assets:bank:`, so Tab stops
        // there and the caller opens the menu.
        type_in(&mut s, "ban");
        assert_eq!(s.complete_buffer().as_deref(), Some("assets:bank:checking"));
        // No match leaves the buffer alone.
        type_in(&mut s, "zzz");
        assert_eq!(s.complete_buffer(), None);
    }

    #[test]
    fn tab_completes_a_commodity_after_the_amount() {
        let (mut s, _t) = session(JOURNAL);
        s.submit();
        type_in(&mut s, "Rewe");
        s.submit();
        type_in(&mut s, "expenses:groceries");
        s.submit();
        type_in(&mut s, "5 E");
        assert_eq!(s.complete_buffer().as_deref(), Some("5 EUR"));
    }

    #[test]
    fn an_account_entered_this_session_completes_in_the_next_transaction() {
        let (mut s, _t) = session(JOURNAL);
        s.submit();
        type_in(&mut s, "Cafe");
        s.submit();
        // Brand new to the journal — noted once, and accepted.
        type_in(&mut s, "expenses:coffee");
        assert!(matches!(s.submit(), Submit::AdvancedWithNote(_)));
        type_in(&mut s, "3.20 EUR");
        s.submit();
        type_in(&mut s, "assets:cash");
        s.submit();
        type_in(&mut s, "");
        let Submit::Done(txn) = s.submit() else {
            panic!("expected the transaction to finish");
        };
        s.complete(*txn);

        // Next transaction: it completes like any other account…
        s.submit();
        type_in(&mut s, "Cafe");
        s.submit();
        type_in(&mut s, "coff");
        assert_eq!(s.candidates(), vec!["expenses:coffee"]);
        let completed = s.complete_buffer().unwrap();
        assert_eq!(completed, "expenses:coffee");
        // …and is no longer announced as new.
        type_in(&mut s, &completed);
        assert_eq!(s.submit(), Submit::Advanced);
    }

    #[test]
    fn an_account_from_an_earlier_posting_completes_in_the_same_transaction() {
        let (mut s, _t) = session(JOURNAL);
        s.submit();
        type_in(&mut s, "Cafe");
        s.submit();
        type_in(&mut s, "expenses:coffee");
        s.submit();
        type_in(&mut s, "3.20 EUR");
        s.submit();
        type_in(&mut s, "coff");
        assert_eq!(s.candidates(), vec!["expenses:coffee"]);
    }

    #[test]
    fn strict_mode_asks_about_a_session_account_only_once() {
        let src = "payee X\naccount a:b\n";
        let cfg = Config {
            strict: true,
            ..Config::default()
        };
        let (mut s, _t) = session_with(src, cfg);
        s.submit();
        type_in(&mut s, "X");
        s.submit();
        type_in(&mut s, "expenses:coffee");
        assert!(matches!(s.submit(), Submit::Confirm { .. }));
        assert_eq!(s.submit_confirmed(), Submit::Advanced);
        // Unitless: strict has no commodity to question here.
        type_in(&mut s, "3.20");
        assert_eq!(s.submit(), Submit::Advanced);
        // Same undeclared account again: already settled this session.
        type_in(&mut s, "expenses:coffee");
        assert_eq!(s.submit(), Submit::Advanced);
    }

    #[test]
    fn undo_retracts_a_session_transaction_from_the_completion_pool() {
        let (mut s, _t) = session(JOURNAL);
        s.submit();
        type_in(&mut s, "Pharmacy");
        s.submit();
        type_in(&mut s, "expenses:health");
        s.submit();
        type_in(&mut s, "9.99 EUR");
        s.submit();
        type_in(&mut s, "assets:cash");
        s.submit();
        type_in(&mut s, "");
        let Submit::Done(txn) = s.submit() else {
            panic!("expected the transaction to finish");
        };
        s.complete(*txn);
        assert!(s.ctx.index.descriptions.contains_key("Pharmacy"));
        s.undo();
        assert!(!s.ctx.index.descriptions.contains_key("Pharmacy"));
    }

    #[test]
    fn declared_but_unused_accounts_complete_too() {
        let src = "account equity:conversion\n2026-08-01 x\n    a:b  1 EUR\n    c:d  -1 EUR\n";
        let (mut s, _t) = session(src);
        s.submit();
        type_in(&mut s, "X");
        s.submit();
        type_in(&mut s, "conv");
        assert_eq!(s.candidates(), vec!["equity:conversion"]);
    }

    #[test]
    fn commodity_completion_completes_the_trailing_token() {
        let (mut s, _t) = session(JOURNAL);
        s.submit();
        type_in(&mut s, "X");
        s.submit();
        type_in(&mut s, "expenses:groceries");
        s.submit();
        type_in(&mut s, "12.50 E");
        let c = s.candidates();
        assert_eq!(c, vec!["12.50 EUR"]);
        // A bare number offers commodities to append.
        type_in(&mut s, "12.50");
        assert_eq!(s.candidates(), vec!["12.50 EUR"]);
    }

    #[test]
    fn commodity_completion_reaches_the_second_commodity_of_a_tail() {
        let (mut s, _t) = session(JOURNAL);
        s.submit();
        type_in(&mut s, "X");
        s.submit();
        type_in(&mut s, "expenses:groceries");
        s.submit();
        // After the price number of @ / @@ and the assertion amount of
        // = / == / =* / ==*, the second commodity completes too.
        for op in ["@", "@@", "=", "==", "=*", "==*"] {
            type_in(&mut s, &format!("5 USD {op} 1.10 E"));
            assert_eq!(
                s.candidates(),
                vec![format!("5 USD {op} 1.10 EUR")],
                "completing after {op}"
            );
            type_in(&mut s, &format!("5 USD {op} 1.10"));
            assert_eq!(
                s.candidates(),
                vec![format!("5 USD {op} 1.10 EUR")],
                "appending after {op}"
            );
            // Right after the bare operator a number must come first.
            type_in(&mut s, &format!("5 USD {op}"));
            assert!(s.candidates().is_empty(), "no candidates after bare {op}");
        }
    }

    #[test]
    fn navigation_moves_between_fields_and_reloads_text() {
        let (mut s, _t) = session(JOURNAL);
        s.submit(); // date
        type_in(&mut s, "Rewe");
        s.submit(); // description
        s.submit(); // account 1 (template)
        assert_eq!(s.draft.field, Field::Amount(0));
        // Up to account 1, edit, back down.
        assert!(s.draft.nav_up());
        assert_eq!(s.draft.field, Field::Account(0));
        assert_eq!(s.draft.buffer, "expenses:groceries");
        assert!(s.draft.nav_up());
        assert_eq!(s.draft.field, Field::Description);
        assert_eq!(s.draft.buffer, "Rewe");
        assert!(s.draft.nav_down());
        assert!(s.draft.nav_down());
        assert_eq!(s.draft.field, Field::Amount(0));
        // The frontier stops downward navigation.
        assert!(!s.draft.nav_down());
    }

    #[test]
    fn preview_renders_at_file_widths_and_resolves_the_date() {
        let (mut s, _t) = session(JOURNAL);
        s.submit();
        type_in(&mut s, "Rewe");
        s.submit();
        s.submit(); // account from template
        type_in(&mut s, "9.99 EUR");
        s.submit();
        let lines = s.preview_lines();
        assert_eq!(lines[0], "2026-08-10 Rewe");
        // Account column padded to the file's widest account
        // (liabilities:creditcard, 22) and number right-aligned to the
        // file-wide numW (-23.45 → 6).
        assert_eq!(lines[1], "    expenses:groceries        9.99 EUR");
    }

    #[test]
    fn preview_shows_unresolved_dates_with_a_question_mark() {
        let (mut s, _t) = session(JOURNAL);
        type_in(&mut s, "wat");
        assert_eq!(s.preview_lines()[0], "wat?");
        type_in(&mut s, "30");
        assert_eq!(s.preview_lines()[0], "2026-08-30");
    }

    #[test]
    fn running_imbalance_reports_per_commodity() {
        let (mut s, _t) = session(JOURNAL);
        s.submit();
        type_in(&mut s, "X");
        s.submit();
        type_in(&mut s, "a:b");
        s.submit_confirmed();
        type_in(&mut s, "10 EUR");
        s.submit();
        assert_eq!(
            s.running_imbalance(),
            Some(vec![("EUR".to_owned(), "10".parse().unwrap())])
        );
    }

    #[test]
    fn undo_pops_the_last_completed_transaction() {
        let (mut s, _t) = session(JOURNAL);
        s.complete(NewTransaction {
            date: d(2026, 8, 10),
            description: "X".into(),
            comment: String::new(),
            postings: vec![Posting::new("a", "1 EUR"), Posting::new("b", "-1 EUR")],
        });
        type_in(&mut s, "u");
        assert_eq!(s.submit(), Submit::Undo);
        assert!(s.undo().is_some());
        assert!(s.completed.is_empty());
        // Nothing left: `u` is now invalid.
        type_in(&mut s, "u");
        assert!(matches!(s.submit(), Submit::Invalid(_)));
    }

    #[test]
    fn default_commodity_attaches_to_unitless_balancing_prefills() {
        let src = "2026-08-01 x\n    a:b  1 EUR\n    c:d  -1 EUR\n";
        let (mut s, _t) = session_with(src, config_with_default("1000.00 EUR"));
        s.submit();
        type_in(&mut s, "X");
        s.submit();
        type_in(&mut s, "a:b");
        s.submit();
        type_in(&mut s, "12.50"); // unitless
        s.submit();
        type_in(&mut s, "c:d");
        s.submit();
        // Balancing suggestion: unitless sum + the configured commodity.
        assert_eq!(s.suggestion().as_deref(), Some("-12.50 EUR"));
    }

    #[test]
    fn default_commodity_is_written_into_bare_amounts() {
        let src = "2026-08-01 x\n    a:b  1 EUR\n    c:d  -1 EUR\n";
        let (mut s, _t) = session_with(src, config_with_default("1000.00 EUR"));
        s.submit();
        type_in(&mut s, "X");
        s.submit();
        type_in(&mut s, "a:b");
        s.submit();
        type_in(&mut s, "12.50"); // bare number
        s.submit();
        // The commodity is materialized in the built transaction…
        assert_eq!(s.draft.postings[0].amount, "12.50 EUR");
        type_in(&mut s, "c:d");
        s.submit();
        // …so the balancing suggestion matches.
        assert_eq!(s.suggestion().as_deref(), Some("-12.50 EUR"));
        let r = s.submit();
        let Submit::Done(txn) = r else {
            panic!("expected Done, got {r:?}");
        };
        assert_eq!(txn.postings[0].amount, "12.50 EUR");
        assert_eq!(txn.postings[1].amount, "-12.50 EUR");
    }

    #[test]
    fn without_a_default_commodity_bare_amounts_stay_unitless() {
        let (mut s, _t) = session(JOURNAL);
        s.submit();
        type_in(&mut s, "X");
        s.submit();
        type_in(&mut s, "expenses:groceries");
        s.submit();
        type_in(&mut s, "12.50");
        s.submit();
        assert_eq!(s.draft.postings[0].amount, "12.50");
    }

    #[test]
    fn default_commodity_follows_the_declared_style_side() {
        let src = "commodity $1000.00\n";
        let (mut s, _t) = session_with(src, config_with_default("$1000.00"));
        s.submit();
        type_in(&mut s, "X");
        s.submit();
        type_in(&mut s, "a:b");
        s.submit_confirmed();
        type_in(&mut s, "-12.50");
        s.submit();
        assert_eq!(s.draft.postings[0].amount, "$-12.50");
    }

    #[test]
    fn amounts_with_tails_are_not_touched_by_the_default_commodity() {
        let src = "D 1000.00 EUR\n";
        let (mut s, _t) = session(src);
        s.submit();
        type_in(&mut s, "X");
        s.submit();
        type_in(&mut s, "a:b");
        s.submit_confirmed();
        type_in(&mut s, "5 USD @ 1.10");
        s.submit();
        assert_eq!(s.draft.postings[0].amount, "5 USD @ 1.10");
    }

    #[test]
    fn a_suggested_account_is_a_ghost_not_buffer_text() {
        let (mut s, _t) = session(JOURNAL);
        s.submit();
        type_in(&mut s, "Rewe");
        s.submit();
        // The template's account is *offered*, not entered: the buffer is
        // empty and the account shows as a dim suggestion. Rendering it as
        // buffer text made it look committed while a keystroke wiped it.
        assert_eq!(s.draft.buffer, "");
        assert_eq!(s.suggestion().as_deref(), Some("expenses:groceries"));
        // Typing therefore just types — nothing to destroy.
        s.draft.insert('l');
        assert_eq!(s.draft.buffer, "l");
        assert_eq!(s.suggestion(), None);
    }

    #[test]
    fn enter_accepts_the_suggested_account_and_tab_picks_it_up_to_edit() {
        let (mut s, _t) = session(JOURNAL);
        s.submit();
        type_in(&mut s, "Rewe");
        s.submit();
        // Enter on the empty prompt takes the suggestion, as it does at the
        // date and amount prompts.
        assert_eq!(s.submit(), Submit::Advanced);
        assert_eq!(s.draft.postings[0].account, "expenses:groceries");
        assert_eq!(s.draft.field, Field::Amount(0));

        // Tab / → instead put it in the buffer for editing — which is what
        // the frontends do with `suggestion()`.
        let (mut s, _t) = session(JOURNAL);
        s.submit();
        type_in(&mut s, "Rewe");
        s.submit();
        let suggested = s.suggestion().unwrap();
        s.draft.set_buffer(&suggested);
        s.draft.backspace();
        assert_eq!(s.draft.buffer, "expenses:grocerie");
    }

    #[test]
    fn dismissing_the_suggested_account_lets_enter_finish_the_transaction() {
        // Using fewer postings than the template needs a way to refuse the
        // suggestion: Ctrl-U in the terminal, `.` over a pipe.
        let (mut s, _t) = session(JOURNAL);
        s.submit();
        type_in(&mut s, "Rewe");
        s.submit();
        type_in(&mut s, "expenses:groceries");
        s.submit();
        type_in(&mut s, "18.20 EUR");
        s.submit();
        type_in(&mut s, "assets:bank:checking");
        s.submit();
        type_in(&mut s, "-18.20 EUR");
        s.submit();
        // Account 3: the template is exhausted, so nothing is suggested and
        // Enter finishes.
        assert_eq!(s.draft.field, Field::Account(2));
        assert_eq!(s.suggestion(), None);
        assert!(matches!(s.submit(), Submit::Done(_)));
    }

    #[test]
    fn a_dismissed_suggestion_comes_back_on_the_next_field() {
        let (mut s, _t) = session(JOURNAL);
        s.submit();
        type_in(&mut s, "Rewe");
        s.submit();
        assert!(s.suggestion().is_some());
        s.draft.dismiss_suggestion();
        assert_eq!(s.suggestion(), None);
        // Dismissal is per field: Enter now finishes rather than accepting,
        // and moving on starts clean.
        type_in(&mut s, "expenses:groceries");
        s.submit();
        type_in(&mut s, "1 EUR");
        s.submit();
        assert_eq!(s.draft.field, Field::Account(1));
        assert_eq!(s.suggestion().as_deref(), Some("assets:bank:checking"));
    }

    #[test]
    fn levenshtein_basics() {
        assert_eq!(levenshtein("", ""), 0);
        assert_eq!(levenshtein("abc", "abc"), 0);
        assert_eq!(levenshtein("abc", "abd"), 1);
        assert_eq!(levenshtein("abc", ""), 3);
        assert_eq!(levenshtein("kitten", "sitting"), 3);
    }
}
