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

use crate::amount::{imbalance, parse_amount, render_amount, AmountCtx};
use super::index::Index;
use super::parser::{FileMap, Journal, Transaction};
use super::write::NewTransaction;
use crate::config::{Config, Matching};
use crate::fmt::posting::{parse_posting, render};

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

    fn from_ordinal(n: usize) -> Self {
        match n {
            0 => Self::Date,
            1 => Self::Description,
            _ => {
                let i = n.saturating_sub(2).saturating_div(2);
                if n.checked_rem(2) == Some(0) {
                    Self::Account(i)
                } else {
                    Self::Amount(i)
                }
            }
        }
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
        let index = Index::build(&journal, today, config.half_life_days);

        let mut amount_ctx = journal.amount_ctx();
        let target_file = journal.file(&target).cloned();
        let (insertion_pos, state) = target_file.map_or((usize::MAX, None), |f| {
            (f.eof_pos, Some(f.state_at_eof))
        });
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
        let mut used_accounts = HashSet::new();
        let mut used_commodities = HashSet::new();
        for t in &journal.transactions {
            for p in &t.postings {
                used_accounts.insert(p.account.clone());
                if let Some(c) = &p.commodity {
                    used_commodities.insert(c.clone());
                }
            }
        }
        let declared_accounts: Vec<String> =
            journal.accounts.iter().map(|d| d.name.clone()).collect();
        let strict = config.strict;

        let default_commodity = state
            .and_then(|s| s.default_commodity)
            .or_else(|| config.default_commodity.clone());

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
            declared_accounts,
            strict,
            default_commodity,
        }
    }

    /// Ranked account candidates, conditioned on a description, with
    /// declared-but-unused accounts appended.
    #[must_use]
    pub fn account_pool(&self, description: Option<&str>) -> Vec<String> {
        let mut out: Vec<String> = self
            .index
            .ranked_accounts(description)
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
                (!t.is_empty() && !t.chars().any(|c| c.is_ascii_digit()))
                    .then(|| t.to_owned())
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
    /// Committed postings: (account, raw amount text).
    pub postings: Vec<(String, String)>,
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
    /// recent transaction.
    template: Vec<(String, String)>,
    /// Highest field reached, as an ordinal — the navigation frontier.
    frontier: usize,
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

    /// Number of postings committed so far.
    #[must_use]
    pub const fn committed_postings(&self) -> usize {
        self.postings.len()
    }

    /// Save the buffer into the current field's slot without validation
    /// (used when navigating away).
    fn stash(&mut self) {
        let text = self.buffer.trim().to_owned();
        match self.field {
            Field::Date => self.date_input = text,
            Field::Description => self.description = text,
            Field::Account(i) => {
                ensure_len(&mut self.postings, i.saturating_add(1));
                if let Some(p) = self.postings.get_mut(i) {
                    p.0 = text;
                }
            }
            Field::Amount(i) => {
                ensure_len(&mut self.postings, i.saturating_add(1));
                if let Some(p) = self.postings.get_mut(i) {
                    p.1 = text;
                }
            }
        }
    }

    /// The stored text of a field (for loading on navigation).
    fn stored(&self, field: Field) -> String {
        match field {
            Field::Date => self.date_input.clone(),
            Field::Description => self.description.clone(),
            Field::Account(i) => self.postings.get(i).map(|p| p.0.clone()).unwrap_or_default(),
            Field::Amount(i) => self.postings.get(i).map(|p| p.1.clone()).unwrap_or_default(),
        }
    }

    /// Move to a field, stashing the buffer and loading the target's text.
    /// The loaded text is pristine: typing replaces it.
    fn goto(&mut self, field: Field) {
        self.stash();
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
        self.postings.iter().map(|(_, a)| a.as_str()).collect()
    }
}

fn ensure_len(v: &mut Vec<(String, String)>, n: usize) {
    while v.len() < n {
        v.push((String::new(), String::new()));
    }
}

fn char_to_byte(s: &str, chars: usize) -> usize {
    s.char_indices().nth(chars).map_or(s.len(), |(b, _)| b)
}

/// The running session: completed transactions plus the current draft.
pub struct Session {
    /// Immutable context.
    pub ctx: SessionCtx,
    /// Completed, not-yet-written transactions.
    pub completed: Vec<NewTransaction>,
    /// The draft being edited.
    pub draft: Draft,
}

impl Session {
    /// Start a session.
    #[must_use]
    pub fn new(ctx: SessionCtx) -> Self {
        let mut s = Self {
            ctx,
            completed: Vec::new(),
            draft: Draft::new(),
        };
        s.reset_draft();
        s
    }

    /// Reset to a fresh draft at the date prompt (today ghost-suggested).
    pub fn reset_draft(&mut self) {
        self.draft = Draft::new();
    }

    /// The pre-fill for the current field, used when its stored text is
    /// empty. (Loading an already-stored field on navigation wins.) Only
    /// accounts pre-fill; dates and amounts are ghost-suggested instead —
    /// see [`Self::suggestion`].
    #[must_use]
    pub fn prefill(&self) -> String {
        match self.draft.field {
            Field::Date | Field::Description | Field::Amount(_) => String::new(),
            Field::Account(i) => self
                .draft
                .template
                .get(i)
                .map(|(a, _)| a.clone())
                .unwrap_or_default(),
        }
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
            Field::Description | Field::Account(_) => None,
        }
    }

    /// Template amount when the account matches; otherwise the negated
    /// running sum (the balancing amount), when it is known and nonzero.
    fn amount_prefill(&self, i: usize) -> String {
        let account = self
            .draft
            .postings
            .get(i)
            .map(|(a, _)| a.as_str())
            .unwrap_or_default();
        let template_match = self.draft.template.get(i).and_then(|(a, amt)| {
            (a == account && !amt.is_empty()).then(|| amt.clone())
        });
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
            .map(|(_, (_, a))| a.as_str())
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
        Some(render_amount(negated, &commodity, &self.ctx.amount_ctx))
    }

    /// Ranked completion candidates for the current field and buffer.
    #[must_use]
    pub fn candidates(&self) -> Vec<String> {
        let query = self.draft.buffer.trim();
        match self.draft.field {
            Field::Date => Vec::new(),
            Field::Description => {
                let ranked = self.ctx.index.ranked_descriptions();
                complete::filter_ranked(self.ctx.config.description_matching, query, &ranked)
                    .into_iter()
                    .map(ToOwned::to_owned)
                    .collect()
            }
            Field::Account(_) => {
                let desc = (!self.draft.description.is_empty())
                    .then_some(self.draft.description.as_str());
                let pool = self.ctx.account_pool(desc);
                let refs: Vec<&str> = pool.iter().map(String::as_str).collect();
                let strategy =
                    complete::account_strategy(self.ctx.config.account_matching, query);
                complete::filter_ranked(strategy, query, &refs)
                    .into_iter()
                    .map(ToOwned::to_owned)
                    .collect()
            }
            Field::Amount(_) => {
                // Commodities — the face commodity and equally the second
                // one after a cost or assertion tail.
                let Some((head, tail)) = split_commodity_query(query) else {
                    return Vec::new();
                };
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
                let refs: Vec<&str> = pool.iter().map(String::as_str).collect();
                complete::filter_ranked(Matching::Prefix, tail, &refs)
                    .into_iter()
                    .map(|c| {
                        if head.is_empty() {
                            c.to_owned()
                        } else {
                            format!("{head} {c}")
                        }
                    })
                    .collect()
            }
        }
    }

    /// Submit the current buffer for the current field.
    pub fn submit(&mut self) -> Submit {
        match self.draft.field {
            Field::Date => self.submit_date(),
            Field::Description => self.submit_description(),
            Field::Account(i) => self.submit_account(i, false),
            Field::Amount(i) => self.submit_amount(i, false),
        }
    }

    /// Re-submit the current field after the user answered a
    /// [`Submit::Confirm`] question with yes.
    pub fn submit_confirmed(&mut self) -> Submit {
        match self.draft.field {
            Field::Account(i) => self.submit_account(i, true),
            Field::Amount(i) => self.submit_amount(i, true),
            Field::Date | Field::Description => {
                Submit::Invalid("nothing to confirm".to_owned())
            }
        }
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

    fn submit_description(&mut self) -> Submit {
        let text = self.draft.buffer.trim().to_owned();
        self.draft.description.clone_from(&text);
        // Load the template once, from the description's most recent
        // transaction. Predictability: most recent, not highest scoring.
        if self.draft.postings.is_empty() && self.draft.template.is_empty() {
            if let Some(tpl) = self.ctx.index.template(&self.ctx.journal, &text) {
                self.draft.template = template_postings(tpl);
            }
        }
        self.advance_to(Field::Account(0));
        Submit::Advanced
    }

    fn submit_account(&mut self, i: usize, confirmed: bool) -> Submit {
        let text = self.draft.buffer.trim().to_owned();
        if text.is_empty() {
            return self.finish(i);
        }
        // Strict mode mirrors `hledger check accounts`: only declarations
        // visible at the insertion point count. hledger-x never declares
        // anything itself — the question is whether to *use* the name.
        if !confirmed && self.ctx.strict && !self.ctx.declared_accounts_visible.contains(&text)
        {
            let hint = self
                .near_miss(&text)
                .map_or_else(String::new, |s| format!(" — did you mean {s}?"));
            return Submit::Confirm {
                question: format!("{text} is not a declared account — use it anyway?{hint}"),
            };
        }
        // Not strict: accept, but surface names that are genuinely new to
        // the journal (neither declared nor ever used).
        let brand_new = !self.ctx.declared_accounts_visible.contains(&text)
            && !self.ctx.used_accounts.contains(&text);
        self.commit_account(i, &text);
        if brand_new && !confirmed {
            return Submit::AdvancedWithNote(format!("{text} is new to this journal"));
        }
        Submit::Advanced
    }

    fn commit_account(&mut self, i: usize, text: &str) {
        ensure_len(&mut self.draft.postings, i.saturating_add(1));
        if let Some(p) = self.draft.postings.get_mut(i) {
            text.clone_into(&mut p.0);
        }
        self.advance_to(Field::Amount(i));
    }

    fn submit_amount(&mut self, i: usize, confirmed: bool) -> Submit {
        let mut text = self.draft.buffer.trim().to_owned();
        if text.is_empty() {
            // An empty amount means: this is the last posting — fill it with
            // the balancing amount, explicitly, and finish.
            return self.finish_via_empty_amount(i);
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
        text = crate::amount::restyle_field(&text, &self.ctx.amount_ctx);
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
                        question: format!(
                            "{c} is not a declared commodity — use it anyway?{hint}"
                        ),
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
        // advancing stashes the buffer into this field's slot.
        self.draft.set_buffer(text);
        ensure_len(&mut self.draft.postings, i.saturating_add(1));
        self.advance_to(Field::Account(i.saturating_add(1)));
    }

    /// An empty amount submits the balancing amount — written explicitly —
    /// and ends the transaction. The first posting must carry an amount.
    fn finish_via_empty_amount(&mut self, i: usize) -> Submit {
        if i == 0 {
            return Submit::Invalid("an amount is required on the first posting".to_owned());
        }
        let has_later = self
            .draft
            .postings
            .get(i.saturating_add(1)..)
            .is_some_and(|rest| {
                rest.iter()
                    .any(|(a, amt)| !a.trim().is_empty() || !amt.trim().is_empty())
            });
        if has_later {
            return Submit::Invalid(
                "an empty amount only finishes on the last posting — later postings exist"
                    .to_owned(),
            );
        }
        let Some(balancing) = self.balancing_prefill(i) else {
            return Submit::Invalid(
                "cannot compute the balancing amount here — enter it explicitly".to_owned(),
            );
        };
        ensure_len(&mut self.draft.postings, i.saturating_add(1));
        if let Some(p) = self.draft.postings.get_mut(i) {
            p.1.clone_from(&balancing);
        }
        self.draft.set_buffer(&balancing);
        self.draft.postings.truncate(i.saturating_add(1));
        self.complete_draft()
    }

    /// Finish the transaction at an empty account prompt for posting `i`.
    fn finish(&mut self, i: usize) -> Submit {
        // Drop any trailing empty posting slots.
        self.draft.postings.truncate(i);
        while self
            .draft
            .postings
            .last()
            .is_some_and(|(a, _)| a.trim().is_empty())
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
            .any(|(_, a)| a.trim().is_empty());
        if !has_bare {
            let amounts = self.draft.committed_amounts();
            if let Some(sums) = imbalance(&amounts, &self.ctx.amount_ctx) {
                if !sums.is_empty() {
                    let detail: Vec<String> = sums
                        .iter()
                        .map(|(c, v)| render_amount(*v, c, &self.ctx.amount_ctx))
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
        let txn = NewTransaction {
            date,
            description: self.draft.description.clone(),
            postings: self.draft.postings.clone(),
        };
        Submit::Done(Box::new(txn))
    }

    /// Move to `field`, loading its stored text or, when empty, its
    /// pre-fill.
    fn advance_to(&mut self, field: Field) {
        self.draft.goto(field);
        if self.draft.buffer.is_empty() {
            let fill = self.prefill();
            self.draft.set_buffer(&fill);
            self.draft.pristine = !fill.is_empty();
        }
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
        let segmentish = complete::match_quality(Matching::Segment, name, candidate).is_some();
        let close_enough = d <= name.chars().count().saturating_div(2).max(2);
        (segmentish || close_enough).then(|| candidate.clone())
    }

    /// A discoverability hint for the current field, shown dimmed under the
    /// prompt when nothing more urgent (menu, ghost, note) is on screen.
    #[must_use]
    pub fn hint(&self) -> Option<String> {
        let empty = self.draft.buffer.trim().is_empty();
        match self.draft.field {
            Field::Date => Some(
                "Enter accepts · u undoes the last transaction · q saves and quits".to_owned(),
            ),
            Field::Account(i) if i >= 1 && empty => {
                Some("Enter on the empty account finishes the transaction".to_owned())
            }
            Field::Amount(i) if i >= 1 && empty => Some(
                "Enter on the empty amount writes the balancing amount and finishes".to_owned(),
            ),
            Field::Amount(0) if empty && self.suggestion().is_some() => {
                Some("Tab or → picks up the suggestion".to_owned())
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
            .filter(|(a, _)| !a.trim().is_empty())
            .map(|(a, amt)| {
                if amt.trim().is_empty() {
                    format!("    {a}")
                } else {
                    format!("    {a}  {}", amt.trim())
                }
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
        let mut out = vec![format!("{date_text} {}", draft.description)
            .trim_end()
            .to_owned()];
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
        self.reset_draft();
    }

    /// Undo the last completed transaction. Returns it, if any.
    pub fn undo(&mut self) -> Option<NewTransaction> {
        let t = self.completed.pop();
        self.reset_draft();
        t
    }
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
    let (side, spaced) = ctx.styles.get(symbol).map_or((Side::Right, true), |s| {
        (s.symbol_side, s.symbol_space)
    });
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
            let del = prev.get(j.saturating_add(1)).copied().unwrap_or(usize::MAX).saturating_add(1);
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
        // Committed as hledger would print it; typed precision kept.
        assert_eq!(s.draft.postings[0].1, "1_234 EUR");
        // Undeclared commodities stay as typed.
        type_in(&mut s, "liabilities:cc");
        s.submit();
        type_in(&mut s, "-10USD");
        assert_eq!(s.submit(), Submit::AdvancedWithNote("USD is a commodity new to this journal".to_owned()));
        assert_eq!(s.draft.postings[1].1, "-10USD");
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
        // Template from the most recent Rewe: account 1 pre-filled.
        assert_eq!(s.draft.field, Field::Account(0));
        assert_eq!(s.draft.buffer, "expenses:groceries");
        assert_eq!(s.submit(), Submit::Advanced);
        // Template amount ghost-suggested, never pre-filled.
        assert_eq!(s.draft.field, Field::Amount(0));
        assert_eq!(s.draft.buffer, "");
        assert_eq!(s.suggestion().as_deref(), Some("12.00 EUR"));
        type_in(&mut s, "18.20 EUR");
        assert_eq!(s.submit(), Submit::Advanced);
        // Account 2 from template.
        assert_eq!(s.draft.buffer, "assets:bank:checking");
        assert_eq!(s.submit(), Submit::Advanced);
        // Balancing amount suggested: negated running sum, not the template.
        // Enter on the empty amount writes it explicitly and finishes.
        assert_eq!(s.draft.buffer, "");
        assert_eq!(s.suggestion().as_deref(), Some("-18.20 EUR"));
        let Submit::Done(txn) = s.submit() else {
            panic!("expected Done, got {:?}", s.submit());
        };
        assert_eq!(txn.date, d(2026, 8, 10));
        assert_eq!(txn.description, "Rewe");
        assert_eq!(
            txn.postings,
            vec![
                ("expenses:groceries".to_owned(), "18.20 EUR".to_owned()),
                ("assets:bank:checking".to_owned(), "-18.20 EUR".to_owned()),
            ]
        );
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
        type_in(&mut s, ""); // empty amount: balance explicitly and finish
        let r = s.submit();
        let Submit::Done(txn) = r else {
            panic!("expected Done, got {r:?}");
        };
        // Never elided — the balancing amount is written out.
        assert_eq!(txn.postings[1].1, "-10 EUR");
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
        type_in(&mut s, "");
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
        // Anywhere else `q` is just text.
        type_in(&mut s, "today");
        s.submit();
        type_in(&mut s, "q");
        assert_eq!(s.submit(), Submit::Advanced);
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
        let src = "account expenses:travel:train\naccount expenses:groceries\n";
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
        let src = "account a:b\naccount c:d\ncommodity 1.00 EUR\n";
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
        assert!(question.contains("EUX is not a declared commodity"), "{question}");
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
        let src = "account a:b\naccount c:d\ncommodity 1.00 EUR\n";
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
        assert!(question.contains("USD is not a declared commodity"), "{question}");
        assert_eq!(s.submit_confirmed(), Submit::Advanced);
        // A fully declared assertion tail passes silently.
        s.draft.nav_up();
        type_in(&mut s, "5 EUR = 5 EUR");
        assert_eq!(s.submit(), Submit::Advanced);
    }

    #[test]
    fn strict_mode_never_questions_unitless_amounts() {
        let src = "account a:b\naccount c:d\ncommodity 1.00 EUR\n";
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
        // Segment matching kicks in on a colon.
        type_in(&mut s, "ex:tra");
        assert_eq!(s.candidates(), vec!["expenses:travel:train"]);
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
            postings: vec![("a".into(), "1 EUR".into()), ("b".into(), "-1 EUR".into())],
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
        let src = "D 1000.00 EUR\n2026-08-01 x\n    a:b  1 EUR\n    c:d  -1 EUR\n";
        let (mut s, _t) = session(src);
        s.submit();
        type_in(&mut s, "X");
        s.submit();
        type_in(&mut s, "a:b");
        s.submit();
        type_in(&mut s, "12.50"); // unitless
        s.submit();
        type_in(&mut s, "c:d");
        s.submit();
        // Balancing suggestion: unitless sum + D commodity.
        assert_eq!(s.suggestion().as_deref(), Some("-12.50 EUR"));
    }

    #[test]
    fn default_commodity_is_written_into_bare_amounts() {
        let src = "D 1000.00 EUR\n2026-08-01 x\n    a:b  1 EUR\n    c:d  -1 EUR\n";
        let (mut s, _t) = session(src);
        s.submit();
        type_in(&mut s, "X");
        s.submit();
        type_in(&mut s, "a:b");
        s.submit();
        type_in(&mut s, "12.50"); // bare number
        s.submit();
        // The commodity is materialized in the built transaction…
        assert_eq!(s.draft.postings[0].1, "12.50 EUR");
        type_in(&mut s, "c:d");
        s.submit();
        // …so the balancing suggestion matches and the empty amount
        // finishes the transaction.
        assert_eq!(s.suggestion().as_deref(), Some("-12.50 EUR"));
        let r = s.submit();
        let Submit::Done(txn) = r else {
            panic!("expected Done, got {r:?}");
        };
        assert_eq!(txn.postings[0].1, "12.50 EUR");
        assert_eq!(txn.postings[1].1, "-12.50 EUR");
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
        assert_eq!(s.draft.postings[0].1, "12.50");
    }

    #[test]
    fn default_commodity_follows_the_declared_style_side() {
        let src = "commodity $1000.00\nD $1000.00\n";
        let (mut s, _t) = session(src);
        s.submit();
        type_in(&mut s, "X");
        s.submit();
        type_in(&mut s, "a:b");
        s.submit_confirmed();
        type_in(&mut s, "-12.50");
        s.submit();
        assert_eq!(s.draft.postings[0].1, "$-12.50");
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
        assert_eq!(s.draft.postings[0].1, "5 USD @ 1.10");
    }

    #[test]
    fn typing_over_a_pristine_prefill_replaces_it() {
        let (mut s, _t) = session(JOURNAL);
        s.submit();
        type_in(&mut s, "Rewe");
        s.submit();
        // Account 1 arrives pre-filled and pristine; typing starts fresh.
        assert_eq!(s.draft.buffer, "expenses:groceries");
        assert!(s.draft.pristine);
        s.draft.insert('l');
        assert_eq!(s.draft.buffer, "l");
        assert!(!s.draft.pristine);
    }

    #[test]
    fn backspace_on_a_pristine_prefill_keeps_the_text_for_editing() {
        let (mut s, _t) = session(JOURNAL);
        s.submit();
        type_in(&mut s, "Rewe");
        s.submit();
        // goto loads pristine; backspace switches to editing.
        assert!(s.draft.pristine);
        s.draft.backspace();
        assert_eq!(s.draft.buffer, "expenses:grocerie");
        s.draft.insert('s');
        assert_eq!(s.draft.buffer, "expenses:groceries");
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
