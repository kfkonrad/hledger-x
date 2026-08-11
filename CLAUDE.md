# hledger-x

A Rust CLI for plain text accounting (hledger) journals:

- `hledger-x fmt` — a formatter; `hledger-fmt`'s output, behind a
  project-aware CLI (`DESIGN.md` § CLI ergonomics)
- `hledger-x add` — ergonomic interactive data entry, a better `hledger add`

The name is load-bearing: `x` is not an hledger built-in, so with `hledger-x` on
`PATH` hledger dispatches `hledger x add` / `hledger x fmt` to it (verified
against hledger 1.99). Renaming the binary breaks that.

**Status: epics 1 (`fmt`) and 2 (`add`) are implemented and green.** Epic 3
(deferred directives: `payee`, `tag`, `Y`, `apply account`, `alias`) is next;
see the epic 3 sections of `DESIGN.md` and `IMPLEMENTATION.md`. Read
`DESIGN.md` for what and why, `IMPLEMENTATION.md` for how. One deliberate
deviation from the plan: `ui` is built directly on `crossterm`, not
`reedline` — the design sanctioned this fallback, and the live preview block
plus field-navigation keys fight reedline's line-editor repaint model. `ui`
splits into submodules (`dates`, `complete`, `plain`, `term`); everything
except `term` is terminal-free and unit-tested. The `add` navigation scheme is
flagged "unproven" in the design — expect user feedback to reshape it.

## Start here

1. `DESIGN.md` — decisions and their rationale, plus empirically verified
   hledger behaviour. Everything in it was settled with the user; do not
   relitigate it without asking.
2. `IMPLEMENTATION.md` — module layout, data structures, ordered build plan,
   test strategy.

Epic 2's build order is `parser.rs` → `index.rs` → `amount.rs` → `write.rs` →
`ui/`; keep the first four free of any terminal dependency.

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

```sh
cargo test                  # unit + golden + CLI + semantic-equivalence tests
cargo clippy --all-targets  # must be clean
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
   `DESIGN.md` § Amount restyling). `fmt` never interprets anything else.
   `add` depends on `fmt`; `fmt` must never depend on `add` (shared amount
   machinery lives in the crate-level `amount` module).
2. **Amounts are only ever rewritten value-preservingly.** An amount is
   re-rendered only when its commodity has a *declared* style, via exact
   decimals, and only into a form hledger reads back to the same value (the
   `decimal-mark` in effect stays in the output for that reason). Unitless
   amounts, undeclared commodities and anything that does not parse pass
   through byte-for-byte — never guess, never a lossy rewrite. Template
   pre-fills in `add` remain the raw text as it stands in the journal.
3. **Never infer a commodity.** A unitless amount is valid. A default may be
   offered as editable pre-filled text, never applied silently at write time.
4. **Balancing amounts are always explicit**, never elided.
5. **`add` is an entry tool, not a validator.** It must never refuse to run
   because the journal contains something it does not understand.
6. **Exact decimals only.** Never floating point for amounts.

## User preferences established in design

- Explicit over implicit.
- Warn and proceed rather than block, except where writing would be wrong.
- Configurability where behaviour is a matter of taste, with a sane default.
- The user enters transactions once a day or two, in small batches.
- The user does **not** use `apply account` or `alias` (epic 3 material).
