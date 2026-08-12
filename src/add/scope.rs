//! Account-name rewriting: `apply account` and `alias`.
//!
//! Both are scope model 1 (parse state) — in effect from their line to the end
//! of their file, inherited by included files, discarded when the include
//! returns. An unclosed region is legal and runs to end of file.
//!
//! Behaviour verified against hledger 1.99 (see `DESIGN.md` § Epic 3):
//!
//! - nested `apply account`s **stack**, joined with `:`; `end apply account`
//!   pops exactly one level
//! - the join is blind, so a prefix written `assets:` yields `assets::checking`
//! - `apply account` prefixes `account` *declarations* too, not only postings
//! - `alias OLD=NEW` matches the whole name or a **segment-bounded leading
//!   prefix**: `bank` and `bank:sub` match `alias bank=X`; `mybank`, `bankish`
//!   and `a:bank` do not
//! - **aliases apply in reverse declaration order**, each to the running
//!   result — the most recently declared goes first
//! - `apply account` is applied **first**; aliases see the prefixed name
//!
//! Going the other way — from the name the user picked to the text that
//! belongs in the file — is **search and verify**, never inversion: a regex
//! alias is not generally invertible, so [`Scope::spell`] generates candidate
//! spellings and runs each back through [`Scope::resolve`], returning only one
//! that demonstrably reads back as the intended account.

use regex::Regex;

/// Where a directive was opened, for error messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Origin {
    /// Index into [`crate::add::parser::Journal::files`].
    pub file: usize,
    /// 1-based line within that file.
    pub line: usize,
}

/// One `alias` directive.
#[derive(Debug, Clone)]
pub struct Alias {
    /// The directive's argument as written — the identity used for equality,
    /// since a compiled [`Regex`] has none.
    raw: String,
    kind: Kind,
    /// Where it was declared.
    pub origin: Origin,
}

#[derive(Debug, Clone)]
enum Kind {
    /// `alias OLD=NEW`.
    Exact { old: String, new: String },
    /// `alias /REGEX/=REPLACEMENT`.
    Regex {
        re: Box<Regex>,
        repl: String,
        /// The pattern's literal text, when the pattern is nothing but a
        /// literal (optionally anchored). Real-world regex aliases are
        /// overwhelmingly literal prefixes, and this is what lets [`spell`]
        /// propose a candidate for them. Only ever a *hint* — the caller
        /// verifies it by resolving forwards.
        ///
        /// [`spell`]: Scope::spell
        literal: Option<String>,
    },
}

/// The literal text of a pattern that is nothing but a literal, with `^` and
/// `$` anchors stripped. `None` for anything with real regex syntax in it.
fn pattern_literal(pattern: &str) -> Option<String> {
    let body = pattern.strip_prefix('^').unwrap_or(pattern);
    let body = body.strip_suffix('$').unwrap_or(body);
    (!body.is_empty() && !body.chars().any(|c| r"\.[]{}()*+?|^$".contains(c)))
        .then(|| body.to_owned())
}

impl PartialEq for Alias {
    fn eq(&self, other: &Self) -> bool {
        self.raw == other.raw && self.origin == other.origin
    }
}

impl Eq for Alias {}

impl Alias {
    /// Parse an `alias` directive argument. `None` when it has no `=`, or the
    /// regex does not compile — an unusable alias is ignored rather than
    /// fatal, since `add` is an entry tool, not a validator.
    #[must_use]
    pub fn parse(arg: &str, origin: Origin) -> Option<Self> {
        let raw = arg.trim().to_owned();
        let kind = if raw.starts_with('/') {
            // `/REGEX/=REPLACEMENT` — the pattern ends at the last `/` that is
            // followed by `=`, so a `/` inside the pattern is not a boundary.
            let body = raw.get(1..)?;
            let cut = body.rfind("/=")?;
            let pattern = body.get(..cut)?;
            let repl = body.get(cut.saturating_add(2)..)?;
            Kind::Regex {
                re: Box::new(Regex::new(pattern).ok()?),
                repl: backrefs(repl),
                literal: pattern_literal(pattern),
            }
        } else {
            let (old, new) = raw.split_once('=')?;
            Kind::Exact {
                old: old.trim().to_owned(),
                new: new.trim().to_owned(),
            }
        };
        Some(Self { raw, kind, origin })
    }

    /// Rewrite one account name through this alias.
    #[must_use]
    pub fn apply(&self, name: &str) -> String {
        match &self.kind {
            Kind::Exact { old, new } => {
                if name == old {
                    return new.clone();
                }
                // Segment-bounded leading prefix only.
                name.strip_prefix(old.as_str())
                    .filter(|rest| rest.starts_with(':'))
                    .map_or_else(|| name.to_owned(), |rest| format!("{new}{rest}"))
            }
            Kind::Regex { re, repl, .. } => re.replace_all(name, repl.as_str()).into_owned(),
        }
    }

    /// Candidate names that might produce `target` through this alias.
    ///
    /// Hints only — every one is verified against [`Scope::resolve`] before it
    /// can be written, so a wrong guess here costs a failed check, never a
    /// wrong account in the file. Regex patterns are inverted only when they
    /// are plain literals; anything else contributes nothing.
    fn invert(&self, target: &str) -> Vec<String> {
        match &self.kind {
            Kind::Exact { old, new } => {
                if target == new {
                    return vec![old.clone()];
                }
                target
                    .strip_prefix(new.as_str())
                    .filter(|rest| rest.starts_with(':'))
                    .map(|rest| vec![format!("{old}{rest}")])
                    .unwrap_or_default()
            }
            Kind::Regex { repl, literal, .. } => {
                // A replacement carrying backreferences cannot be undone by
                // substitution; `$$` is the escaped literal `$` and is fine.
                let Some(lit) = literal else {
                    return Vec::new();
                };
                if repl.is_empty() || repl.replace("$$", "").contains('$') {
                    return Vec::new();
                }
                let all = target.replace(repl.as_str(), lit);
                let first = target.replacen(repl.as_str(), lit, 1);
                if all == first {
                    vec![all]
                } else {
                    vec![all, first]
                }
            }
        }
    }
}

/// Translate hledger's `\1` backreferences into the `${1}` form the `regex`
/// crate expects. A doubled `\\` escapes a literal backslash.
fn backrefs(repl: &str) -> String {
    let mut out = String::with_capacity(repl.len());
    let mut chars = repl.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            // `$` is a substitution sigil for `regex` but a literal for
            // hledger, so it has to be escaped on the way through.
            if c == '$' {
                out.push_str("$$");
            } else {
                out.push(c);
            }
            continue;
        }
        match chars.peek() {
            Some(d) if d.is_ascii_digit() => {
                out.push_str("${");
                while chars.peek().is_some_and(char::is_ascii_digit) {
                    if let Some(d) = chars.next() {
                        out.push(d);
                    }
                }
                out.push('}');
            }
            Some('\\') => {
                chars.next();
                out.push('\\');
            }
            _ => out.push('\\'),
        }
    }
    out
}

/// The `apply account` / `alias` state in effect at a point in the journal.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scope {
    /// Nested `apply account` prefixes, outermost first.
    apply: Vec<(String, Origin)>,
    /// Aliases in declaration order. Applied in reverse.
    aliases: Vec<Alias>,
}

impl Scope {
    /// Push an `apply account` prefix.
    pub fn push_apply(&mut self, prefix: &str, origin: Origin) {
        self.apply.push((prefix.trim().to_owned(), origin));
    }

    /// `end apply account` — pops exactly one level (verified, AA2).
    pub fn pop_apply(&mut self) {
        self.apply.pop();
    }

    /// Add an alias.
    pub fn push_alias(&mut self, alias: Alias) {
        self.aliases.push(alias);
    }

    /// `end aliases` — clears them all.
    pub fn clear_aliases(&mut self) {
        self.aliases.clear();
    }

    /// Whether anything here rewrites account names.
    #[must_use]
    pub const fn is_active(&self) -> bool {
        !self.apply.is_empty() || !self.aliases.is_empty()
    }

    /// The combined `apply account` prefix, if any.
    #[must_use]
    pub fn prefix(&self) -> Option<String> {
        if self.apply.is_empty() {
            return None;
        }
        let parts: Vec<&str> = self.apply.iter().map(|(p, _)| p.as_str()).collect();
        Some(parts.join(":"))
    }

    /// The directives in effect, innermost first, as `(description, origin)` —
    /// for the error raised when a name cannot be written here.
    #[must_use]
    pub fn active_directives(&self) -> Vec<(String, Origin)> {
        let mut out: Vec<(String, Origin)> = self
            .apply
            .iter()
            .rev()
            .map(|(p, o)| (format!("apply account {p}"), *o))
            .collect();
        out.extend(
            self.aliases
                .iter()
                .rev()
                .map(|a| (format!("alias {}", a.raw), a.origin)),
        );
        out
    }

    /// Resolve a name as written in the file into the account hledger sees.
    ///
    /// `apply account` first, then aliases in reverse declaration order, each
    /// applied to the running result.
    #[must_use]
    pub fn resolve(&self, name: &str) -> String {
        let mut out = self
            .prefix()
            .map_or_else(|| name.to_owned(), |p| format!("{p}:{name}"));
        for alias in self.aliases.iter().rev() {
            out = alias.apply(&out);
        }
        out
    }

    /// The text to write into the file so that hledger reads it back as
    /// `target`.
    ///
    /// Candidates are generated by stripping the prefix and inverting exact
    /// aliases, then **each is verified** with [`Self::resolve`]; the first
    /// that round-trips wins. `None` means `target` cannot be expressed at
    /// this point in the file, which is a refusal, never a guess.
    #[must_use]
    pub fn spell(&self, target: &str) -> Option<String> {
        if !self.is_active() {
            return Some(target.to_owned());
        }
        let mut seen: Vec<String> = vec![target.to_owned()];
        // Two rounds is enough to undo "prefix, then one alias", which is the
        // deepest composition that can be inverted without guessing.
        for _ in 0..2u8 {
            for candidate in seen.clone() {
                let mut derived: Vec<String> = Vec::new();
                for alias in &self.aliases {
                    derived.extend(alias.invert(&candidate));
                }
                if let Some(prefix) = self.prefix() {
                    if let Some(rest) = candidate.strip_prefix(&format!("{prefix}:")) {
                        derived.push(rest.to_owned());
                    }
                }
                for d in derived {
                    if !seen.contains(&d) {
                        seen.push(d);
                    }
                }
            }
        }
        seen.into_iter().find(|c| self.resolve(c) == target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(applies: &[&str], aliases: &[&str]) -> Scope {
        let mut s = Scope::default();
        for a in applies {
            s.push_apply(a, Origin::default());
        }
        for a in aliases {
            s.push_alias(Alias::parse(a, Origin::default()).unwrap());
        }
        s
    }

    // ---- apply account (hledger 1.99, DESIGN § Epic 3) ----

    #[test]
    fn nested_applies_stack_and_pop_one_at_a_time() {
        // AA1 / AA2.
        let mut s = scope(&["a", "b"], &[]);
        assert_eq!(s.resolve("checking"), "a:b:checking");
        s.pop_apply();
        assert_eq!(s.resolve("checking"), "a:checking");
        s.pop_apply();
        assert_eq!(s.resolve("checking"), "checking");
        // Popping an empty stack is harmless.
        s.pop_apply();
        assert_eq!(s.resolve("checking"), "checking");
    }

    #[test]
    fn the_prefix_join_is_blind_to_a_trailing_colon() {
        // AA3: hledger produces `assets::checking`, and so must we.
        assert_eq!(
            scope(&["assets:"], &[]).resolve("checking"),
            "assets::checking"
        );
    }

    // ---- alias matching ----

    #[test]
    fn an_exact_alias_matches_the_whole_name_or_a_segment_bounded_prefix() {
        // AL18.
        let s = scope(&[], &["bank=X"]);
        assert_eq!(s.resolve("bank"), "X");
        assert_eq!(s.resolve("bank:sub"), "X:sub");
        assert_eq!(s.resolve("mybank"), "mybank");
        assert_eq!(s.resolve("bankish"), "bankish");
        assert_eq!(s.resolve("a:bank"), "a:bank");
    }

    #[test]
    fn aliases_are_case_sensitive() {
        // AL10.
        assert_eq!(scope(&[], &["Bank=assets:x"]).resolve("bank"), "bank");
    }

    #[test]
    fn a_regex_alias_replaces_globally() {
        // AL2 / AL13.
        assert_eq!(
            scope(&[], &["/^exp/=expenses"]).resolve("exp:food"),
            "expenses:food"
        );
        assert_eq!(scope(&[], &["/x/=TWO"]).resolve("xx"), "TWOTWO");
    }

    #[test]
    fn regex_backreferences_use_hledgers_backslash_form() {
        let s = scope(&[], &[r"/^(\w+):bank/=\1:institution"]);
        assert_eq!(s.resolve("assets:bank:giro"), "assets:institution:giro");
    }

    // ---- the reverse-order rule (AL11/AL13/AL14/AL16/AL17) ----

    #[test]
    fn aliases_apply_in_reverse_declaration_order() {
        // AL16 vs AL17: the identical chain gives `D` declared backwards and
        // `B` declared forwards. This pair is the whole proof.
        assert_eq!(scope(&[], &["/C/=D", "/B/=C", "/A/=B"]).resolve("A"), "D");
        assert_eq!(scope(&[], &["/A/=B", "/B/=C", "/C/=D"]).resolve("A"), "B");
    }

    #[test]
    fn a_later_alias_does_not_see_an_earlier_ones_output() {
        // AL11: forward chaining would give TWO.
        assert_eq!(scope(&[], &["/x/=ONE", "/ONE/=TWO"]).resolve("x"), "ONE");
    }

    #[test]
    fn aliases_matching_different_parts_of_a_name_compose() {
        // AL14/AL15: "last match on the original wins" would give A:b.
        assert_eq!(scope(&[], &["/a/=A", "/b/=B"]).resolve("a:b"), "A:B");
        assert_eq!(scope(&[], &["/b/=B", "/a/=A"]).resolve("a:b"), "A:B");
    }

    #[test]
    fn the_last_declared_of_two_competing_aliases_wins() {
        // AL7.
        assert_eq!(scope(&[], &["/x/=ONE", "/x/=TWO"]).resolve("x"), "TWO");
    }

    // ---- apply account runs before aliases (AL8 / AL9) ----

    #[test]
    fn aliases_see_the_apply_account_prefix_not_the_raw_name() {
        assert_eq!(scope(&["TOP"], &["sub=REPLACED"]).resolve("sub"), "TOP:sub");
        assert_eq!(
            scope(&["TOP"], &["TOP:sub=REPLACED"]).resolve("sub"),
            "REPLACED"
        );
    }

    // ---- spelling: search and verify ----

    #[test]
    fn spelling_is_the_identity_when_nothing_is_active() {
        assert_eq!(
            Scope::default().spell("assets:bank:checking").as_deref(),
            Some("assets:bank:checking")
        );
    }

    #[test]
    fn spelling_strips_the_apply_account_prefix() {
        let s = scope(&["assets:bank"], &[]);
        assert_eq!(s.spell("assets:bank:checking").as_deref(), Some("checking"));
        // And it round-trips, which is the property that matters.
        assert_eq!(s.resolve("checking"), "assets:bank:checking");
    }

    #[test]
    fn the_resolved_name_is_preferred_whenever_it_round_trips() {
        // `assets:checking` does not start with `bank`, so writing it
        // literally reads back unchanged. Writing the *explicit* name is
        // better than rewriting it into the alias, so the identity candidate
        // is tried first.
        let s = scope(&[], &["bank=assets:checking"]);
        assert_eq!(
            s.spell("assets:checking").as_deref(),
            Some("assets:checking")
        );
    }

    #[test]
    fn an_account_the_alias_always_rewrites_is_unreachable_and_refused() {
        // Under `alias x=y` there is no text that reads back as `x` — hledger
        // rewrites every spelling of it. Refusing is the only honest answer;
        // writing `x` would silently enter `y`.
        let s = scope(&[], &["x=y"]);
        assert_eq!(s.spell("x"), None);
        // Its target, by contrast, is writable literally.
        assert_eq!(s.spell("y").as_deref(), Some("y"));
        assert_eq!(s.spell("y:sub").as_deref(), Some("y:sub"));
    }

    #[test]
    fn spelling_undoes_a_prefix_and_an_alias_together() {
        let s = scope(&["TOP"], &["TOP:sub=REPLACED"]);
        // `sub` is the only text that reads back as REPLACED here.
        assert_eq!(s.spell("REPLACED").as_deref(), Some("sub"));
        assert_eq!(s.resolve("sub"), "REPLACED");
    }

    #[test]
    fn spelling_refuses_rather_than_guessing_when_nothing_round_trips() {
        // Under `apply account assets:bank`, no text reads back as a name
        // outside that subtree.
        let s = scope(&["assets:bank"], &[]);
        assert_eq!(s.spell("expenses:groceries"), None);
    }

    #[test]
    fn spelling_never_returns_a_candidate_that_does_not_round_trip() {
        // The exhaustive property: whatever `spell` returns must resolve back.
        let cases = [
            scope(&["a"], &[]),
            scope(&["a", "b"], &[]),
            scope(&[], &["x=y"]),
            scope(&["a"], &["a:x=y"]),
            scope(&[], &["/^q/=Q"]),
            scope(&["p"], &["/^p/=P"]),
        ];
        for s in &cases {
            for target in ["a:b:checking", "y", "Q:z", "P:z", "plain", "a:x"] {
                if let Some(text) = s.spell(target) {
                    assert_eq!(
                        s.resolve(&text),
                        target,
                        "spell({target:?}) = {text:?} did not round-trip in {s:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_literal_regex_alias_is_inverted_and_verified() {
        // `/^exp/=expenses` is self-matching: writing `expenses:food` would
        // read back as `expensesenses:food`, so the identity is rejected and
        // the literal inversion `exp:food` is found and verified instead.
        let s = scope(&[], &["/^exp/=expenses"]);
        assert_eq!(s.resolve("expenses:food"), "expensesenses:food");
        assert_eq!(s.spell("expenses:food").as_deref(), Some("exp:food"));
        assert_eq!(s.resolve("exp:food"), "expenses:food");
    }

    #[test]
    fn a_backreference_alias_leaves_names_it_does_not_touch_writable() {
        // The common case: the alias simply does not match the target, so the
        // resolved name is written literally and reads back unchanged.
        let s = scope(&[], &[r"/^(\w+):bank/=\1:institution"]);
        assert_eq!(
            s.spell("assets:institution:giro").as_deref(),
            Some("assets:institution:giro")
        );
    }

    #[test]
    fn a_non_literal_regex_alias_is_refused_rather_than_guessed() {
        // Self-matching *and* carrying a backreference: the identity fails and
        // substitution cannot undo it, so nothing is proposed. Refusal, not a
        // guess.
        let s = scope(&[], &[r"/^(a+)/=x\1"]);
        assert_eq!(s.resolve("aa"), "xaa");
        assert_eq!(s.spell("aa"), None);
    }

    // ---- parsing ----

    #[test]
    fn alias_directive_forms_parse() {
        let o = Origin::default();
        assert!(Alias::parse("bank=assets:checking", o).is_some());
        assert!(Alias::parse("/^exp/=expenses", o).is_some());
        // No `=` at all.
        assert!(Alias::parse("nonsense", o).is_none());
        // An uncompilable regex is ignored, not fatal.
        assert!(Alias::parse("/[/=x", o).is_none());
    }

    #[test]
    fn exact_alias_operands_are_trimmed() {
        let a = Alias::parse("  bank  =  assets:checking  ", Origin::default()).unwrap();
        assert_eq!(a.apply("bank"), "assets:checking");
    }

    #[test]
    fn a_regex_containing_a_slash_splits_at_the_last_boundary() {
        let a = Alias::parse("/a/b/=X", Origin::default()).unwrap();
        assert_eq!(a.apply("a/b"), "X");
    }

    #[test]
    fn active_directives_report_innermost_first() {
        let s = scope(&["outer", "inner"], &["z=y"]);
        let names: Vec<String> = s.active_directives().into_iter().map(|(d, _)| d).collect();
        assert_eq!(
            names,
            vec!["apply account inner", "apply account outer", "alias z=y"]
        );
    }
}
