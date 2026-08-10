# rledger — implementation plan

Companion to `DESIGN.md` (what and why). This file is how and in what order.

Epic 1 (`fmt`) has no dependencies and no open questions. Build it first.

## Crate layout

```
Cargo.toml
src/
  main.rs          CLI dispatch
  lex.rs           shared lexical layer  (epic 1, used by both)
  fmt/
    mod.rs         format, format_sorted, is_formatted, is_formatted_sorted
    posting.rs     Posting parse + render, width computation
    sort.rs        Entry parse, directive-bounded stable sort
  add/             (epic 2)
    mod.rs
    parser.rs      include walk, directive extraction, scope stack
    index.rs       frecency indices
    amount.rs      decimal parse/render, commodity display styles
    ui.rs          reedline integration, field state machine, live preview
    write.rs       insertion, write modes, recovery journal
  config.rs        (epic 2)
tests/
  golden/          fixtures ported from hledger-fmt/test/testdata
```

Dependencies: `clap` (CLI). Epic 2 adds `reedline`, `rust_decimal`, `glob`,
`toml` + `serde`, `chrono`, `directories`.

Distribution is GitHub releases; no crates.io publish, no CI yet.

---

# Epic 1 — `rledger fmt`

Port of github.com/mikluko/hledger-fmt `src/Hledger/Fmt.hs`.
Read it — it is 341 lines and the algorithm below is a transcription, not an
adaptation. The goldens in its `test/testdata/` are the acceptance criteria.

## `lex.rs` — shared primitives

Exact semantics, all operating on a single line:

| Function | Rule |
| --- | --- |
| `is_blank(s)` | all chars whitespace |
| `is_comment(s)` | first char is `;`, `#` or `*` (column 0, not indented) |
| `opens_txn(s)` | first char `is_ascii_digit` — this is the *only* test for a transaction header |
| `is_indented_non_blank(s)` | first char is whitespace and not `is_blank` |
| `rstrip(s)` | drop trailing whitespace |
| `split_comment(s)` | split at the **first** `;`. Returns `(rstrip(before), Option<comment_including_semicolon>)`. Accounts and amounts never contain `;`, so the first one is the boundary. |
| `split_account_amount(s)` | split at the first run of **2 or more** whitespace chars (space or tab). Returns `(account, rest_after_dropping_the_run)`. No such run → whole string is the account, rest is empty. A *single* space or tab is not a separator — account names may contain single spaces. |
| `is_rest_start(tok)` | token starts with `@` or `=` |
| `is_number_like(tok)` | first char in `+-.0123456789` **and** the token contains at least one digit. Deliberately rejects `$100` and bare `AMD`. |
| `split_amount(tokens)` | see below |

`split_amount(tokens) -> (num, commodity, rest)`:

```
(amt, rest) = tokens.split at first is_rest_start
match amt:
  []                        -> ("",             "",  rest)
  [t]                       -> (t,              "",  rest)
  [t0, t1, ...more]:
    if is_number_like(t0)   -> (t0,             t1,  more ++ rest)
    if is_number_like(t1) && more.is_empty()
                            -> (join(t0,t1),    "",  rest)
    else                    -> (join(all amt),  "",  rest)
```

`rest` is the cost (`@`/`@@`) and balance assertion (`=`/`==`) tail. It is kept
verbatim and never column-aligned.

## `fmt/posting.rs`

```rust
enum Posting {
    Comment(String),                                    // indented `;` line
    Bare(String, Option<String>),                       // account, comment
    Amount { account: String, num: String, commodity: String,
             rest: Vec<String>, comment: Option<String> },
}
```

`parse_posting(raw)`:
1. `s = rstrip(raw.trim_start())`
2. if `s` starts with `;` → `Comment(s)`
3. `(body, comment) = split_comment(s)`
4. `(account, amt) = split_account_amount(body)`
5. `amt` empty → `Bare(account, comment)`
6. else `(num, commodity, rest) = split_amount(amt.split_whitespace())`

`render(acc_w, num_w, posting)`, with `INDENT = "    "` (4 spaces):

- `Comment(c)` → `INDENT + c`
- `Bare(a, c)` → `INDENT + a + comment_part(c)`
- `Amount{..}` → `INDENT + pad_right(account, acc_w) + "  " + amount_field
                 + (rest empty ? "" : " " + rest.join(" "))
                 + comment_part(comment)`

where

```
amount_field =
  if num.is_empty()  ->  " " * num_w + phantom_commodity_pad(rest)
  else               ->  pad_left(num, num_w)
                         + (commodity.is_empty() ? "" : " " + commodity)

phantom_commodity_pad(rest) =
  let c = tail_commodity(rest)
  c.is_empty() ? "" : " " + " " * c.len()

tail_commodity(rest) = first token that is neither is_rest_start nor
                       is_number_like; "" if none

comment_part(None) = ""
comment_part(Some(c)) = "  " + c        // exactly two spaces
```

Note `pad_right`/`pad_left` must not truncate when the string exceeds the width
(`n - len` can go negative — saturate at 0).

## `fmt/mod.rs` — the two-pass format

```
format(s) = lines(s) |> format_lines |> unlines
```

**Match Haskell `lines`/`unlines` exactly**, because the goldens depend on it:
- `lines("")  == []`, `unlines([]) == ""` → empty input yields empty output
- `unlines` appends `\n` after *every* element → output always ends in a
  newline, and a file lacking a trailing newline gains one
- `lines("a\n") == ["a"]` (no trailing empty element)

`format_lines(lines)`:

1. **Pass 1 — widths.** Collect `posting_runs(lines)`: maximal runs of
   `is_indented_non_blank` lines that follow a transaction header. Parse each
   line as a `Posting`.
   - `acc_w = max(len(account))` over all postings that have an account
     (i.e. `Bare` and `Amount`; `Comment` has none), else 0
   - `num_w = max(len(num))` over `Amount` postings only, else 0
   - **These are file-wide, not per-transaction.** This is the single most
     consequential property of the formatter.
2. **Pass 2 — render.** Walk with an `in_txn: bool`:
   - if `in_txn && is_indented_non_blank(line)` → consume the maximal run of
     such lines, render each with `(acc_w, num_w)`, stay `in_txn = true`
   - otherwise → emit `format_other(line)`, set `in_txn = opens_txn(line)`

`format_other(s)`:
- `is_blank(s)` → `""`
- `opens_txn(s)` → `rstrip(s)`
- otherwise → `s` **verbatim**

That last branch is what keeps directives, top-level comments, `include`, `P`
lines and indented sub-directives untouched. The `in_txn` guard is what stops
`account`/`commodity` sub-directive blocks from being treated as postings.

`is_formatted(s) = format(s) == s` (drives `--check`).

## `fmt/sort.rs` — `--sort`

```
format_sorted(s) = lines(s) |> sort_entries |> format_lines |> unlines
```

```rust
enum Entry {
    Blank,                       // exactly one blank line, stays in place
    Anchor(Vec<String>),         // directive or standalone comment block — a barrier
    Txn((i32,u32,u32), Vec<String>),
}
```

`parse_entries` walks with a `pending: Vec<String>` of leading comment lines:
- blank → flush pending as an `Anchor`, emit `Blank`
- `is_comment` → push to pending
- `opens_txn` → take the following `is_indented_non_blank` run; emit
  `Txn(date_key(header), pending ++ [header] ++ run)`; clear pending
- otherwise (a directive) → take the following `is_indented_non_blank` run
  (its sub-directives); emit `Anchor(pending ++ [line] ++ run)`; clear pending
- at EOF, flush any pending as an `Anchor`

So a comment line **directly above** a transaction travels with it, while a
comment followed by a blank line becomes a standalone `Anchor` barrier.

`sort_runs`: split the entry list at every `Anchor`; sort each maximal
anchor-free run independently.

`sort_run`: **stable** sort of only the `Txn` entries by their date key, then
*refill* — walk the original run and replace each `Txn` slot in order with the
next sorted transaction, leaving `Blank` entries in their original positions.

`date_key(header)`:
1. `token` = header up to the first whitespace
2. `primary` = token up to the first `=` (drops any secondary date)
3. split `primary` on `/`, `-`, `.`
4. 3 parts → `(y, m, d)`; 2 parts → `(0, m, d)`; otherwise `(0, 0, 0)`
5. each part parsed as int, unparseable → 0

Unparseable dates sort first; the stable sort preserves their source order.

## CLI

```
rledger fmt [--check] [--sort] [FILE|-]...
```

- `FILE...` → format each in place
- `--check` → write nothing; exit non-zero if any file is not already
  formatted; list the offenders. With `--sort`, check against `format_sorted`.
- `-` or no arguments → stdin to stdout
- no `include` following — one file at a time, like a linter

## Tests

1. **Golden fixtures.** Copy `*.in.ledger` / `*.golden` from
   `hledger-fmt/test/testdata/` into `tests/golden/`. Byte-for-byte equality.
   `sort.in.ledger` / `sort.sorted.golden` covers `--sort`.
2. **Idempotence.** `format(format(x)) == format(x)` for every fixture, plus
   property-based over generated journals if convenient.
3. **`--check` exit codes** on both formatted and unformatted input.
4. **Semantic equivalence.** `hledger print` output must be byte-identical
   before and after formatting. hledger 1.99 is installed; see
   `hledger-fmt/test/semantic.sh` for the approach.
5. **Edge cases to cover explicitly**, since they are easy to get wrong:
   empty input; no trailing newline; a file of only directives; a file of only
   `P` price lines; an `account` directive with indented sub-directives; an
   amount-less posting carrying only `= 0 RSD`; commodity-on-left amounts
   (`$100`); accounts containing single spaces (`Assets:Bank Account`); tabs
   used as separators.

---

# Epic 2 — `rledger add`

Depends on epic 1. See `DESIGN.md` for the full behavioural spec; this is the
build order and the shapes.

## Order

1. `add/parser.rs` — include walk + directives. Testable headlessly.
2. `add/index.rs` — frecency indices. Pure function of the parse.
3. `add/amount.rs` — decimal parse/render, commodity styles.
4. `add/write.rs` — write modes and insertion, driven by fixtures.
5. `add/ui.rs` — last, because it is the only part that needs a terminal.
6. `config.rs` — alongside whatever first needs it.

Keep 1–4 free of any terminal dependency so they stay unit-testable.

## `parser.rs`

**Include walk** (all verified against hledger 1.99 — see `DESIGN.md`):
- start at `-f` or `$LEDGER_FILE`
- resolve include paths **relative to the including file's directory**, not cwd
- expand globs, in sorted order
- detect cycles via a set of canonicalized absolute paths; error on revisit
- depth-first, in file order — directives take effect in encounter order
- a missing include target is a **loud warning**, not an error

**Two scope models. Do not collapse them into one.**

```rust
// Model 1 — parse state: file-scoped, inherited by includes, DISCARDED on return.
// Push a clone on entering an included file, pop on exit.
struct ParseState { decimal_mark: Option<char>, default_commodity: Option<String> }

// Model 2 — declarations: journal-wide, NOT discarded on return, but visible
// only from their stream position forward.
struct Declaration { name: String, kind: DeclKind, stream_pos: usize }
```

`decimal-mark` and `D` are model 1. `account` and `commodity` are model 2.
Epic 3 adds `Y`/`apply account`/`alias` to model 1 and `payee`/`tag` to model 2.

**Transactions are parsed lexically, not semantically.** Extract date,
description, and for each posting the `split_account_amount` pair — the amount
side stays opaque text. Never interpret a historical number.

Leniency: unknown directives ignored; unparseable transactions skipped with one
warning; existing imbalance ignored; periodic (`~`) and auto (`=`) transactions
skipped (already excluded by `opens_txn` requiring a leading digit).

**Two products:**
- *Journal model* — semantics over the whole tree (declarations with stream
  positions, parse state per file, transactions).
- *File map* — for the **write-target file only**: per-transaction line ranges
  and dates, positions of directives and standalone comment blocks (the sort
  barriers), whether it is already formatted, and its file-wide `acc_w`/`num_w`
  from `fmt`.

## `index.rs`

Four indices, one pass over the parsed transactions, each entry
`{ count, last_date, score }`:

| Index | Key | Used for |
| --- | --- | --- |
| descriptions | description | description completion |
| accounts | account | account completion, unconditioned |
| by_description | (description, account) | account completion conditioned on the entered description |
| templates | description → most recent transaction | posting pre-fill |

```
score = Σ over occurrences of  0.5 ^ (age_days / half_life)
```

`half_life` configurable, default 90 days, relative to today at parse time.

**Pre-fill and ranking use different rules on purpose**: pre-fill takes the
*most recent* match (predictability when text is placed in front of the user);
the completion menu orders by *score* (ranking is what makes the second-best
candidate reachable). Do not unify them.

## `amount.rs`

- `rust_decimal`, never floats.
- Decimal mark: use the `decimal-mark`/`commodity` directive in effect; when
  none, replicate hledger's auto-detection (verified: `1.234,56` reads as
  comma-decimal unprompted; a lone `1.234` reads as dot-decimal).
- Parse a `commodity` directive's format sample (`1.000,00 EUR`) into a display
  style: decimal mark, group separator, group sizes, decimal places, symbol
  side and spacing. Amounts we generate must match it.
- Parsing is needed **only** for the transaction being entered — running
  imbalance and the balancing amount. On failure, show "imbalance unknown"
  rather than writing anything.

## `ui.rs`

`reedline`, inline raw mode — no alternate screen. Fall back to raw `crossterm`
if reedline's prompt model fights the live block; that is a contained rewrite
of this module only.

**Field state machine**: date → description → account 1 → amount 1 → account 2
→ … Each field owns its completer. Keep the state machine as the *only* thing
that knows a posting is two fields — uniting account and amount into one
journal-syntax line later must not touch the completers or the pre-fill logic.

The live preview is rendered by `fmt` against the **file's** widths, not the
transaction's own, so it is byte-accurate and visibly shifts when an entry will
reflow the file.

Keys, no-confirmation save, and undo: see `DESIGN.md`.

## `write.rs`

- Buffer everything, write once on exit. `Ctrl-C` and `Ctrl-D` both save all
  completed transactions.
- Recovery journal under `$XDG_STATE_HOME/rledger/`, written as the session
  progresses, replayed on next launch if the process died without writing.
- `format_file` (default true), `sort` (default false), `insertion`
  (default `append`). Reject `format_file = false` + `sort = true` at config
  load — sorting inherently rewrites the file.
- `chronological` insertion follows `fmt --sort` semantics. If the file is not
  sorted, warn and proceed.
- Write target is the main file; an override must be reachable through the
  include graph or it is an error.

---

# Epic 3 — deferred directives

`payee`, `tag`, `Y`/`year`, `apply account`, `alias`. See `DESIGN.md` for what
each requires. Epics 1 and 2 ignore `apply account`/`alias` entirely — no
parsing, no detection — at the user's explicit direction. Epic 3 starts by
adding detection (refuse to insert into a region where either is active) before
attempting full support.

---

# How to verify hledger behaviour

Several design decisions came from empirical testing, and two initial
assumptions were wrong. When a question about hledger semantics arises, test it
— do not reason from the manual.

The technique that worked: construct a case where the answer flips a
**binary, observable** outcome.

- *Does a directive apply here?* Write a transaction that balances **only if**
  it does — e.g. with `decimal-mark ,` in effect, `1.234` means 1234, so
  postings of `1.234 EUR` and `-1234 EUR` balance iff the directive applied.
  `hledger print` then either succeeds or reports the imbalance.
- *Is a name declared here?* Use `hledger check accounts` /
  `hledger check commodities` and read the exit status. Note that piping into
  `head` masks the exit code — capture it directly.
- *Which binary runs?* Put a stub executable earlier on `PATH` and see whether
  it is reached.

Always include a control case that is expected to fail. Two of the tests run
during design initially "passed" only because the amount was unambiguous and
hledger auto-detected it — the control is what exposed that.
