# hledger-x — implementation plan

Companion to `DESIGN.md` (what and why). This file is how and in what order.

Epic 1 (`fmt`) has no dependencies and no open questions. Build it first.

## Crate layout

```
Cargo.toml
src/
  main.rs          CLI dispatch
  lex.rs           shared lexical layer  (epic 1, used by both)
  amount.rs        decimal parse/render, display styles, restyling
                   (crate-level: fmt restyles amounts but must not depend
                   on add)
  fmt/
    mod.rs         format[_with], sorted/check variants, scan_ctx, widths
    posting.rs     Posting parse + restyle + render, width computation
    sort.rs        Entry parse, directive-bounded stable sort
    blank.rs       top-level block split, blank-line normalization
  add/             (epic 2)
    mod.rs
    parser.rs      include walk, directive extraction, scope stack
    index.rs       frecency indices
    ui/            field state machine, completion, live preview
      mod.rs       Draft/Session state machine, pre-fill, suggestions
      complete.rs  the three completion styles, and what Tab inserts
      dates.rs     smart date resolution
      plain.rs     line-based frontend (stdin is not a tty)
      term.rs      raw-mode crossterm frontend
    write.rs       insertion, write modes, recovery journal
  config.rs        (epic 2)
tests/
  golden/          fixtures ported from hledger-fmt/test/testdata
```

Everything under `ui/` except `term.rs` is terminal-free and unit-tested.

Dependencies: `clap` (CLI), `color-eyre` (errors), `similar` (`fmt --diff`),
and `tempfile` as a dev-dependency. Epic 2 adds `crossterm`, `rust_decimal`,
`glob`, `toml` + `serde`, `chrono`.

Distribution is GitHub releases, built by goreleaser (`.goreleaser.yaml`) from
a `v*` tag via `.github/workflows/release.yml`, whose notes come from the
matching `CHANGELOG.md` section. `.github/workflows/ci.yml` runs `cargo test`
(with hledger installed, so the semantic-equivalence tests do not skip
themselves) and `cargo clippy --all-targets -- -D warnings` on pull requests.

---

# Epic 1 — `hledger-x fmt`

Port of [`hledger-fmt`](https://github.com/mikluko/hledger-fmt)'s
`src/Hledger/Fmt.hs`. Read it — it is 341 lines and the algorithm below is a
transcription, not an adaptation. The goldens in its `test/testdata/` are the
acceptance criteria; they are ported into `tests/golden/`.

## `lex.rs` — shared primitives

Exact semantics, all operating on a single line — except
`comment_block_len`, which needs a slice to find a construct's extent:

| Function | Rule |
| --- | --- |
| `is_blank(s)` | all chars whitespace |
| `is_comment(s)` | first char is `;`, `#` or `*` (column 0, not indented) |
| `opens_txn(s)` | first char `is_ascii_digit` — this is the *only* test for a transaction header |
| `is_indented_non_blank(s)` | first char is whitespace and not `is_blank` |
| `opens_comment_block(s)` | `rstrip(s) == "comment"` — the keyword alone at column 0 |
| `closes_comment_block(s)` | `rstrip(s) == "end comment"`, same strictness |
| `comment_block_len(after_opener)` | lines the block spans past its opener: contents plus the terminator, or the whole slice if unterminated |
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
format(s) = lines(s) |> blank::normalize |> format_lines |> unlines
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

### `comment` blocks

Three `lex.rs` helpers carry it: `opens_comment_block(s)` (`rstrip(s) ==
"comment"`), `closes_comment_block(s)` (`rstrip(s) == "end comment"`), and
`comment_block_len(after_opener)` — the contents plus the terminator, or the
rest of the slice if unterminated. See `DESIGN.md` for the verified delimiter
rules; the strictness is the point, since `comment` with an argument is a
parse error and an *indented* `end comment` does not close anything.

Two shapes use them. Index walks (`scan_ctx`, `styled_lines`, both of
`parser.rs`'s walks) carry an `opaque: bool` and `continue` while it is set.
Block walks (`sort::parse_entries`, `blank::parse`) call `comment_block_len`
to take the block whole, which is what makes it one barrier and keeps its
interior blank lines out of reach.

`styled_lines` therefore returns a three-way `Class` rather than
`Option<Posting>`:

```rust
enum Class { Post(Posting), Other, Opaque }
```

`Opaque` lines skip `format_other` entirely — they are emitted byte-for-byte,
keeping even their trailing whitespace — and contribute nothing to
`widths_of`, so prose inside a block cannot widen the alignment columns.

## `fmt/blank.rs` — blank-line normalization

Runs on the line list *before* `format_lines` (and after `sort_entries`), so
it can insert and delete lines while `format_lines` stays a 1:1 line map.
Blank lines are `""` literals, so it returns `Vec<&'a str>` like its input.

```rust
enum Kind { Txn, Other }
struct Block<'a> {
    kind: Kind,
    lines: Vec<&'a str>,
    blank_before: bool,   // was there a blank line above it in the input?
}
```

`parse` is `sort::parse_entries` without the dates and without `Blank`
entries: it buffers leading comment lines in `pending` and attaches them to
the block below (a comment followed by a blank stands alone), consuming each
head line's `is_indented_non_blank` run with it. Blank lines are not emitted;
they only set `blank_before` on the next block. Because a block owns its whole
indented run, no rule below can put a blank inside one.

Emit, for each adjacent pair: a single `""` iff
`b.blank_before || prev.kind == Txn || b.kind == Txn`. Nothing before the
first block or after the last, which drops leading and trailing blanks.

Idempotent by construction: after one pass `blank_before` is exactly what the
rules produced. See `DESIGN.md` § Blank lines for the rationale, and
`tests/golden/blanks.*` for the end-to-end fixture.

## `fmt/sort.rs` — `--sort`

```
format_sorted(s) = lines(s) |> sort_entries |> blank::normalize
                 |> format_lines |> unlines
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

Revised 2026-08; `DESIGN.md` § CLI has the shape and the rationale.

```
hledger-x fmt [--check] [--diff] [--sort|--no-sort] [-q] [-f|--follow ROOT]... [FILE|-]...
```

`main.rs` resolves the operands into a `Plan` — a list of `Job`s (path,
display name, index into the plan's `AmountCtx` list) deduplicated by canonical
path — and then walks it:

- no arguments → the configured `ledger_file`, then `$LEDGER_FILE`, as a
  `--follow` root; usage error (2) when neither resolves
- `--follow ROOT` → `parse_journal(ROOT)`, then a job per file in the tree, all
  sharing the tree's `amount_ctx()`. A cycle falls back to formatting the root
  alone; an I/O failure drops the root entirely (there is nothing to fall back
  to, and the read error is reported once)
- `FILE...` → one job each, styled by the file's own include tree, exactly as
  before
- `-` → stdin to stdout; rejected (2) alongside any other operand
- `--check` → write nothing; `would reformat: PATH` per file on stderr; exit 1.
  With sorting on, check against `format_sorted`
- `--diff` → `write_diff` renders `similar::TextDiff::from_lines(..)`'s
  `unified_diff()` with `context_radius(3)` and `a/PATH` `b/PATH` headers, to
  stdout, in place of the file list. It does not imply `--check`
- writes list changed paths on stdout unless `-q` or `--diff`; an unchanged
  file is never written, so its mtime does not move
- sorting comes from the config, overridden by `--sort` / `--no-sort`
  (`SortFlags::resolve`)

Exit codes are a `Status` enum ordered worst-last (`Ok` < `Unformatted` <
`Usage` < `Error`) merged across files, so `0`/`1`/`2`/`3` fall out of
`Status::code`.

## Tests

1. **Golden fixtures.** Copy `*.in.ledger` / `*.golden` from
   `hledger-fmt/test/testdata/` into `tests/golden/`. Byte-for-byte equality.
   `sort.in.ledger` / `sort.sorted.golden` covers `--sort`, and
   `blanks.*` — ours, not the reference's — covers blank-line normalization.
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

# Epic 2 — `hledger-x add`

Depends on epic 1. See `DESIGN.md` for the full behavioural spec; this is the
build order and the shapes.

## Order

1. `add/parser.rs` — include walk + directives. Testable headlessly.
2. `add/index.rs` — frecency indices. Pure function of the parse.
3. `amount.rs` — decimal parse/render, commodity styles. (Built under
   `add/`, later moved to the crate level so `fmt`'s restyling can use it
   without depending on `add`.)
4. `add/write.rs` — write modes and insertion, driven by fixtures.
5. `add/ui/` — last, because `term.rs` is the only part that needs a terminal.
6. `config.rs` — alongside whatever first needs it.

Keep 1–4 free of any terminal dependency so they stay unit-testable.

## `parser.rs`

**Include walk** (all verified against hledger 1.99 — see `DESIGN.md`):
- start at `-f`, else config `ledger_file`, else `$LEDGER_FILE`
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
  and dates, whether it is already formatted, and its file-wide
  `acc_w`/`num_w` from `fmt`. Sort barriers are not recorded: the write path
  calls `fmt`'s own sort, which recomputes them.

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

`half_life` is fixed at 90 days (`index::DEFAULT_HALF_LIFE_DAYS`), relative to
today at parse time.

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
- Parsing is needed for the transaction being entered — running imbalance
  and the balancing amount — and for restyling amounts of declared-style
  commodities (`restyle_field` / `restyle_face_fields` / `restyle_tail`,
  used by `fmt` and by `add`'s submit path; value-preserving, precision
  kept). On any parse failure nothing is rewritten and the imbalance shows
  as unknown rather than wrong.

## `ui/`

Raw `crossterm`, inline — no alternate screen. (`reedline` was the plan; its
line-editor repaint model fights the live preview block and the
field-navigation keys, so the sanctioned fallback was taken. `term.rs` is the
only module that touches a terminal; a second frontend, `plain.rs`, drives the
same state machine over pipes so entry is testable and scriptable.)

**Field state machine**: date → description → account 1 → amount 1 → account 2
→ … Each field owns its completer. Keep the state machine as the *only* thing
that knows a posting is two fields; the completers and the pre-fill logic must
not depend on it. (The two fields are permanent — the single-line alternative
was settled against on 2026-08-12; see `DESIGN.md` § Posting entry. This is
plain modularity now, not preparation for a merge.)

The live preview is rendered by `fmt` against the **file's** widths, not the
transaction's own, so it is byte-accurate and visibly shifts when an entry will
reflow the file.

Keys, no-confirmation save, and undo: see `DESIGN.md`.

## `write.rs`

- Buffer everything, write once on exit. `Ctrl-C` and `Ctrl-D` both save all
  completed transactions.
- Recovery journal under `$XDG_STATE_HOME/hledger-x/`, written as the session
  progresses, replayed on next launch if the process died without writing.
- `format_file` (default true), `sort` (default false), `insertion`
  (default `append`). Reject `format_file = false` + `sort = true` at config
  load — sorting inherently rewrites the file.
- The rest of the settings live in `config.rs` and are consumed by `ui/`, not
  here: `strict`, `account_completion` / `description_completion`,
  `default_commodity`, `equity_conversion` and `equity_conversion_account`.
  `ledger_file` and `sort` sit at the config's top level (both subcommands read
  them); everything `add` alone governs is in the `[add]` table, listed in
  `config::ADD_SETTINGS` so a misplaced key can name its section.
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
