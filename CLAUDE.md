# rledger

A Rust CLI for plain text accounting (hledger) journals:

- `rledger fmt` — a formatter, drop-in equivalent to `hledger-fmt`
- `rledger add` — ergonomic interactive data entry, a better `hledger add`

**Status: design complete, no code written yet.** Read `DESIGN.md` for what and
why, `IMPLEMENTATION.md` for how and in what order. Both are current.

## Start here

1. `DESIGN.md` — decisions and their rationale, plus empirically verified
   hledger behaviour. Everything in it was settled with the user; do not
   relitigate it without asking.
2. `IMPLEMENTATION.md` — module layout, data structures, ordered build plan,
   test strategy.

Epic 1 (`fmt`) is fully specified with no open questions and no dependencies.
It is the place to start.

## Environment facts

- **This is a colocated `jj` (jujutsu) + git repo.** Both `jj st` and
  `git status` work and agree. **`jj` is authoritative — prefer it** (`jj st`,
  `jj diff`, `jj new`, `jj describe`). `git` is fine for reading; avoid
  committing through git, which desynchronizes the two.
- `hledger 1.99` is installed at `~/.local/bin/hledger` and is the reference
  implementation for behavioural questions. **Test against it rather than
  guessing** — several design decisions came from doing exactly that, and two
  assumptions turned out to be wrong.
- The reference formatter is Haskell:
  github.com/mikluko/hledger-fmt
  - `src/Hledger/Fmt.hs` — 341 lines, `base` only, the whole formatter
  - `test/testdata/*.golden` — the output fixtures to port
- `hledger-fmt` itself is **not** installed as a binary.
- Platform: macOS (darwin). Shell is fish — `cd x && y` can trigger permission
  prompts, prefer absolute paths.

## Invariants that must not be broken

These are load-bearing. Violating any of them is a design regression, not a
style choice.

1. **`fmt` never builds a semantic model.** It is line-oriented and passes
   directives through byte-for-byte. That blindness is why it is safe on
   price-only and include-only files. `add` depends on `fmt`; `fmt` must never
   depend on `add`.
2. **Never interpret a historical amount.** Amounts from existing transactions
   are carried as raw text and pre-filled verbatim. The only numbers ever
   parsed are those in the transaction being entered right now.
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
