# hledger-x

A Rust CLI for plain text accounting (hledger) journals:

- `hledger-x fmt` — a formatter; `hledger-fmt`'s output, behind a
  project-aware CLI (`DESIGN.md` § CLI ergonomics)
- `hledger-x add` — ergonomic interactive data entry, a better `hledger add`

The name is load-bearing: `x` is not an hledger built-in, so with `hledger-x` on
`PATH` hledger dispatches `hledger x add` / `hledger x fmt` to it (verified
against hledger 1.99). Renaming the binary breaks that.

**Status: epics 1 (`fmt`), 2 (`add`) and 3 (the remaining directives —
`payee`, `tag`, `Y`, `apply account`, `alias`) are implemented and green.**
Read `DESIGN.md` for what and why, `IMPLEMENTATION.md` for how. One deliberate
deviation from the plan: `ui` is built directly on `crossterm`, not
`reedline` — the design sanctioned this fallback, and the live preview block
plus field-navigation keys fight reedline's line-editor repaint model. `ui`
splits into submodules (`dates`, `complete`, `plain`, `term`); everything
except `term` is terminal-free and unit-tested. The `add` navigation scheme and
the account/amount split were signed off by the user on 2026-08-12 after using
the shipped tool; `DESIGN.md` has no open questions left.

Epic 3 decisions, all settled with the user on 2026-08-12: `apply
account`/`alias` get **full support** (not detect-and-refuse); the payee checks
**mirror the account ones exactly** — a passing note by default, a confirmation
under `strict`; and comments are entered **inline**, as the `; …` tail of the
description or amount field.

That last one was decided *after* building the alternative: dedicated comment
fields in the Enter flow were implemented, tried, and rejected because they
lengthened the common no-comment path. Do not reintroduce them. The field
count must stay date → description → account → amount.

`strict` was made **per check** on 2026-08-19: `true`/`false` still work, and a
list picks individual checks by hledger's own plural names — `accounts`,
`commodities`, `payees`. Only those spellings, no aliases, and an unrecognised
one is an error, since tolerating it would silently switch a guard off. See
`DESIGN.md` § Strict mode.

Four epic 3 facts that are easy to get wrong and are pinned by tests:

- **Aliases apply in reverse declaration order**, each to the running result.
  Four plausible rules each fit part of the evidence; only this one fits all
  of it (`DESIGN.md` § Epic 3, tests AL11/AL13/AL14/AL16/AL17).
- **`apply account` prefixes `account` declarations**, not only postings.
- **A description is `payee | note`**, split at the first `|`. Anything using a
  description as an identity — the payee checks, the near-miss, the template,
  the account conditioning — must use the payee half; only the file gets the
  whole string.
- Writing an account into a directive region is **search-and-verify**, never
  inversion: candidate spellings are run back through the forward resolver and
  only a verified round-trip is written. `spell` returning `None` is a
  refusal, and the write path must keep it one.

## Start here

1. `DESIGN.md` — decisions and their rationale, plus empirically verified
   hledger behaviour. Everything in it was settled with the user; do not
   relitigate it without asking.
2. `IMPLEMENTATION.md` — module layout, data structures, ordered build plan,
   test strategy.

Epic 2's build order is `parser.rs` → `index.rs` → `amount.rs` → `write.rs` →
`ui/`; keep the first four free of any terminal dependency. Epic 3 added
`scope.rs` at the front of that chain, likewise terminal-free.

## How to work here

- **TDD.** Write the failing test first, then the implementation. Every module
  in epic 1 was built this way and the tests are the spec.
- **Lints are strict and non-negotiable**, taken verbatim from
  <https://www.namtao.com/rust/>: `clippy::pedantic` and `clippy::nursery` at
  `deny`, plus the no-panic set (`unwrap_used`, `expect_used`,
  `indexing_slicing`, `arithmetic_side_effects`, `panic`, `todo`,
  `string_slice`, `as_conversions`, …). `cargo clippy --all-targets` must be
  clean. Panicking constructs are allowed in tests only — via `clippy.toml` for
  `#[cfg(test)]` modules and a crate-level `#![allow]` in each `tests/*.rs`.
- **Formatting is stock `rustfmt`** — no `rustfmt.toml`, nothing to argue
  about. CI gates on `cargo fmt --all --check`, so run `cargo fmt` before
  committing. It reformats the whole crate, so run it on a clean tree: if it
  touches files you did not, that churn does not belong in your change.
- **Prefer that page's dependencies** when a new one is needed: `clap`,
  `chrono`, `color-eyre`, `criterion`, `itertools`, `rayon`, `serde`.
- **This file is committed and public. Keep it free of anything local or
  private.** No machine-specific paths, no details of the user's actual
  journals — not their location, their accounts, their commodities, their
  balances, nor anything derived from them. If a real journal is useful as a
  regression corpus, use it read-only in the session and say nothing about it
  here.
- **Keep `README.md` current.** It follows the
  [standard-readme](https://github.com/RichardLitt/standard-readme) spec, like
  the user's other repos. Behaviour changes belong in it.
- **Keep `CHANGELOG.md` current, and write it for end users.** It follows
  [Keep a Changelog](https://keepachangelog.com/en/1.0.0/). An entry says what
  changed for someone *using* the tool, not how it was built:
  - No implementation vocabulary — no buffers, parsers, internal module or type
    names, or descriptions of the mechanism behind a fix. Say what the user sees
    now and, for a fix, what they saw before.
  - One short concrete example beats a paragraph of rules. Prefer
    `` typing `4 EUR` writes `4.00 EUR` `` over a precise statement of the
    policy.
  - Summarize a rule to the cases a user would notice; do not enumerate every
    branch. Edge cases that only matter to a maintainer belong in `DESIGN.md`.
  - Group under `### Added` / `### Changed` / `### Fixed`, prefix with the
    affected subcommand (`` `add`: ``, `` `fmt`: ``) when it applies, and keep
    each entry to a sentence or two.
  - No issue numbers, commit hashes or internal task references.

```sh
cargo test                  # unit + golden + CLI + semantic-equivalence tests
cargo clippy --all-targets  # must be clean
cargo fmt --all --check     # what CI enforces; `cargo fmt` to fix
```

## Environment facts

- **This is a colocated `jj` (jujutsu) + git repo.** Both `jj st` and
  `git status` work and agree. **`jj` is authoritative — prefer it** (`jj st`,
  `jj diff`, `jj new`, `jj describe`). `git` is fine for reading; avoid
  committing through git, which desynchronizes the two.
- `hledger` 1.99 is installed and is the reference implementation for
  behavioural questions. **Test against it rather than guessing** — several
  design decisions came from doing exactly that, and two assumptions turned out
  to be wrong. Ask the user where it lives if it is not on `PATH`.
- The reference formatter is [`hledger-fmt`](https://github.com/mikluko/hledger-fmt),
  341 lines of `base`-only Haskell in `src/Hledger/Fmt.hs`. Its
  `test/testdata/*.golden` fixtures are ported into `tests/golden/`. The
  binary itself is not installed; ask the user for a checkout if the source is
  needed again.
- Platform: macOS (darwin). Shell is fish — `cd x && y` can trigger permission
  prompts, prefer absolute paths.

## Invariants that must not be broken

These are load-bearing. Violating any of them is a design regression, not a
style choice.

1. **`fmt` stays line-oriented and passes directives through byte-for-byte.**
   That blindness is why it is safe on price-only and include-only files. The
   one sanctioned semantic *reading* (2026-08, with the user) is declared
   commodity display styles — `commodity`, its `format` subdirective, `D`,
   `decimal-mark` — used to restyle amounts as `hledger print` would (see
   `DESIGN.md` § Amount restyling). The second (2026-08-18, at the user's
   request) is `fmt --explicit`, which reads a transaction as a whole to fill
   in the one amount hledger would infer, and is the only place `fmt` turns
   one line into several (a multi-commodity remainder). It is opt-in per
   invocation and deliberately *not* configurable — see `DESIGN.md`
   § `--explicit`. `fmt` never interprets anything else.
   `add` depends on `fmt`; `fmt` must never depend on `add` (shared amount
   machinery lives in the crate-level `amount` module).
2. **Amounts are only ever rewritten value-preservingly**, and **precision is
   `fmt`'s to preserve but `add`'s to complete.** `fmt` keeps an existing
   amount's decimal places exactly (changing them changes `hledger print`
   output — verified, and the semantic test catches it); `add` pads an amount
   the user just typed up to the commodity's declared places, never down.
   `fmt --explicit` is the one sanctioned way to ask `fmt` for `add`'s policy,
   and only because the invocation said so; the default path must stay
   `Places::AsWritten`.
   `amount::Places` names the two policies so a shared helper cannot silently
   apply the wrong one. An amount is
   re-rendered only when its commodity has a *declared* style, via exact
   decimals, and only into a form hledger reads back to the same value (the
   `decimal-mark` in effect stays in the output for that reason). Unitless
   amounts, undeclared commodities and anything that does not parse pass
   through byte-for-byte — never guess, never a lossy rewrite. Template
   pre-fills in `add` remain the raw text as it stands in the journal.
3. **Never infer a commodity.** A unitless amount is valid. A default may be
   offered as editable pre-filled text, never applied silently at write time.
4. **Balancing amounts are always explicit**, never elided — `add` writes
   every one it generates, and `fmt --explicit` writes out the ones already
   in the journal.
5. **`add` is an entry tool, not a validator.** It must never refuse to run
   because the journal contains something it does not understand. (It may
   refuse to *write* — see 7 — but that is a different thing.)
6. **Exact decimals only.** Never floating point for amounts.
7. **An account name is only written once it has been verified to read back.**
   Under `apply account`/`alias` the text in the file is not the account
   hledger sees. Candidate spellings are always checked by resolving them
   forwards; if none round-trips, hledger-x refuses and says which directive
   is responsible. Never invert a rewrite and trust the result.

## User preferences established in design

- Explicit over implicit.
- Warn and proceed rather than block, except where writing would be wrong.
- Configurability where behaviour is a matter of taste, with a sane default.
- The user enters transactions once a day or two, in small batches.
- The user does **not** use `apply account` or `alias` in their own journals.
  Both are fully supported anyway (settled 2026-08-12), which means their
  behaviour cannot be validated against real usage — the hledger differential
  tests in `tests/semantic.rs` are the only safety net there. Extend them
  rather than reasoning about these two directives from first principles.
