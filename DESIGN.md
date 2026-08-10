# rledger — design notes

A Rust CLI for plain text accounting journals: ergonomic interactive data entry
(a better `hledger add`) plus a formatter.

Status: design discussion. No code yet.

## Name and CLI surface

The tool is **`rledger`**, with two subcommands:

| Command | Purpose |
| --- | --- |
| `rledger add` | interactive data entry |
| `rledger fmt` | formatting, equivalent to `hledger-fmt` |

Formatting is exposed as a first-class subcommand because the write path
implements it anyway; the marginal cost is a CLI surface, not an engine.

**Deliberately not an hledger add-on.** hledger dispatches to `hledger-*`
executables on `PATH` only for *unknown* subcommands — built-ins win. Verified
against hledger 1.99 with a stub binary on `PATH`: `hledger add` ran the
built-in and never reached the stub, while `hledger jot` reached
`hledger-jot`. So `hledger-add` could never have been invoked as `hledger add`.
We forgo dispatch entirely rather than pick a name around it, since any single
name is ambiguous for a tool that both enters and formats data. A multi-call
binary was considered and rejected as overkill.

**crates.io note:** `rledger` is taken — a single 0.1.0 published 2024-12-11
("Accounting platform", `github.com/mmkaram/hledger`), untouched since, 919
downloads. Abandoned but claimed. The *binary* can still be `rledger`; publish
the crate under a different name with `[[bin]] name = "rledger"`.

### `rledger fmt`

Matches hledger-fmt's surface so it is a drop-in substitute:

```
rledger fmt [--check] [--sort] [FILE|-]...
```

- `FILE...` — format each file in place
- `--check` — write nothing, exit non-zero if any file is not already
  formatted, listing offenders (CI and pre-commit)
- `--sort` — also sort transactions by date
- `-` or no arguments — stdin to stdout
- no `include` following: one file at a time, like a linter

## Motivation — what's wrong with `hledger add`

Observed by driving `hledger 1.99` with piped input:

1. **Typo'd accounts are silently accepted.** Typing `exp:trav` writes a brand
   new account into the journal with no warning and no "did you mean". Typing
   `bahn` matches `Deutsche Bahn` for the purpose of picking defaults but then
   writes the literal string `bahn`, quietly forking the payee.
2. **Output formatting ignores the file's existing style.** It appends its own
   wide-column alignment regardless of how the file is written.
3. **No real navigation.** `<` steps back exactly one prompt at a time. You
   cannot jump to a field or fix posting 1 after typing posting 3 without
   backing through everything in between.
4. **Only one similar transaction is offered**, with no way to reach the others.
5. **The transaction is invisible while you build it.** No live preview, no
   running imbalance.
6. **Weak completion.** Prefix-only, no visible candidate list, no ranking.

## Scope

CLI first — an inline raw-mode interface, not a full-screen TUI. Normal
scrollback, no alternate screen; the program controls a few lines around the
cursor and redraws them. Same model as `fish`'s completion menu or
`fzf --height`. A full TUI stays on the table only if the CLI proves
insufficient.

Likely library: **reedline** (custom completers, columnar/list menus,
ghost-text autosuggestion, rebindable keys, multi-line prompts). Falling back
to raw `crossterm` is a contained rewrite of one module.

## Epic 1 — core

### Interface

The live transaction is rendered above the input line and redrawn on every
keystroke, formatted exactly as it will be written to disk:

```
  2026-02-01 Rewe
      expenses:groceries                23.45 EUR
      liabilities:creditcard           -23.45 EUR
  ───────────────────────────────────────────────
  posting 3 › ass:ba:che
                assets:bank:checking          ← ghost text, → to accept
```

Running imbalance (per commodity) is displayed while nonzero.

### Posting entry

One line per posting in journal syntax — `account<2+ spaces>amount COMMODITY` —
rather than separate account and amount prompts. The completer switches mode on
cursor position: before the whitespace gap it completes accounts, after it
completes commodities.

Rationale: halves the prompt count and uses syntax the user already knows.
**Unproven** — the user is willing to try it but not sold. Evaluate against the
two-prompt alternative once it can be felt in practice.

Postings are pre-filled from the ranked match, so accepting is one keypress
either way. The final posting is pre-filled with the negated running sum.

### Dates

- Default is **today**, for every transaction — not the previous entry's date
  (which is what `hledger add` does).
- Partial and smart dates accepted (`30`, `8/30`, `yesterday`), resolved against
  today.
- The fully resolved date is rendered in the live block as you type, so an
  invalid or misunderstood date is visible before it is committed.

### Navigation

| Key | Action |
| --- | --- |
| `Enter` | accept field, advance; on an empty posting line, finish the transaction |
| `↑` / `↓` | move between fields, loading existing text for editing |
| `Tab` / `Shift-Tab` | open completion menu / cycle candidates |
| `→` | accept ghost-text autosuggestion |
| `Ctrl-R` | history search (Up/Down are field navigation, not history) |
| `Ctrl-E` | open the whole transaction in `$EDITOR`, reparse on close |
| `Ctrl-C` | abort the current transaction, not the program |
| `Ctrl-D` | quit |

No "save this transaction? [y]" confirmation — redundant when the transaction
has been visible the whole time. `u` at the date prompt undoes the last
completed transaction.

**Unproven** — the user wants to see this in practice before signing off.

### Completion

**Matching**, configurable per field, all four available:

| Strategy | Behaviour |
| --- | --- |
| `prefix` | classic |
| `substring` | middle-of-string |
| `segment` | split query on `:`, prefix-match each account component — `ex:gro` → `expenses:groceries` |
| `fuzzy` | subsequence, ranked by match quality |

Default for **accounts**: `substring`, switching to `segment` as soon as the
query contains a colon. Default for **descriptions**: `substring`.

**Ranking** — frecency (usage count x recency decay) *conditioned on the
description already entered*. This is the answer to "how do I reach the second
similar transaction" without a separate template-selection step: at Rewe, both
real grocery patterns are the top two candidates, one `Tab` apart. No cost when
there is only one obvious answer.

**Presentation** — both at once, fish-style:
- ghost text of the top candidate, `→` accepts (zero extra keystrokes in the
  common case)
- `Tab` opens the menu, `Tab`/`Shift-Tab` cycle, arrows navigate; each entry
  carries a hint (last-used date, amount from the matching transaction)

### Amounts and commodities

- **Never infer a commodity.** A unitless amount is valid even in strict mode.
- A configured default commodity, and the journal's own `D` directive, may
  supply one — but always as **editable pre-filled text in the buffer**, never
  applied silently at write time. The user sees `23.45 EUR` and can delete the
  `EUR`.
- No arithmetic, no percentage or split helpers. Would collide with a
  percentage commodity.
- Balancing amounts are always written **explicitly**, never elided.

### New-account and new-payee guard

Config `new_account = confirm | warn | allow | error`. Defaults to `confirm`
when the journal declares accounts via `account` directives, `warn` otherwise.
Not a block — a prompt that surfaces near-misses: "`exp:trav` is new — create
it? (did you mean `expenses:travel:train`?)". Same treatment for descriptions
that near-miss an existing payee.

### Output formatting

Follows **hledger-fmt** conventions. The live preview and the writer share one
formatter — the block on screen is the bytes that will be written.

Reference: github.com/mikluko/hledger-fmt
(`src/Hledger/Fmt.hs`, 341 lines of Haskell, `base`-only; goldens in
`test/testdata`). To be reimplemented in Rust — roughly 120 lines of real
logic. It is line-oriented and builds **no semantic model**, so it specifies
our formatter and contributes nothing to our parser; the semantic parser
(accounts, commodities, decimal marks, include graph) is independent work.

#### Layout rules

1. Posting indent is exactly 4 spaces.
2. Account and amount are separated by a run of 2+ whitespace characters
   (hledger's rule — account names may contain single spaces).
3. Account field padded right to `accW`, then **2 spaces**, then the number
   right-aligned in `numW`, then **1 space**, then the commodity.
4. `@`/`@@` cost and `=`/`==` assertion tails follow with single-space
   separators and are **never** column-aligned.
5. Amount-less postings: account only, trailing whitespace trimmed. If such a
   posting carries only an assertion or cost, the number and commodity columns
   are reserved with blanks so the tail lines up as if a zero amount stood in
   front of it.
6. Inline posting comments normalized to **2 spaces** before `;`.
7. Numbers are never restyled — digit grouping, decimal places and sign spacing
   copied through unchanged. (Note: this is hledger-fmt's rule for *existing*
   text. Amounts *we* generate must still respect `commodity` /
   `decimal-mark` directives.)
8. Blank lines collapse to empty. Transaction headers get trailing whitespace
   trimmed, nothing more. Directives, top-level comments, `include`, `P` lines
   and indented sub-directives pass through verbatim.
9. Output always ends in a newline. `format (format x) == format x`.

An indented run is treated as postings only when it follows a transaction
header (a line starting at column 0 with a digit), which is what keeps
`account` / `commodity` sub-directives untouched.

#### Alignment is file-wide — and it forces whole-file rewrites

`accW` and `numW` are computed over **every posting in the file**, not per
transaction. Verified in `postings.golden`: the `12.50` in the second
transaction is aligned to a column set by `-7485978.18` in the first.

Therefore adding one transaction whose account or number is longer than the
current maximum **reflows every posting line in the file**.

**Default: always emit a fully formatted file.** Run the formatter over the
whole result on write. When the file is already formatted and widths do not
grow, this is byte-identical to a pure append (zero diff). When widths grow, it
produces exactly the reflow `hledger-fmt` would produce anyway. One code path,
always idempotent.

Consequences:
- Warn on first write if the file was not already formatted, since the tool
  will reformat it.
- The **live preview must render against the file's widths**, not the
  transaction's own. When typing pushes a width past the current maximum the
  preview visibly shifts — a free signal that this entry will reflow the file.

#### Write modes (configurable)

| Setting | Default | Behaviour |
| --- | --- | --- |
| `format_file` | `true` | reformat the whole file on write |
| `sort` | `false` | also sort transactions by date, `--sort` semantics |

With `format_file = false`, only the lines we add are written, but they are
rendered **as if the whole file were correctly formatted**: `accW` / `numW` are
still computed over the entire file *including* the new transactions, so our
lines match what `hledger-fmt` would emit.

The exact guarantee this gives, stated precisely because it is narrower than it
sounds:
- widths unchanged and file already formatted → the file is a fixed point; a
  later `hledger-fmt` run is a no-op.
- widths grew → **our** lines are already correct and a later `hledger-fmt` run
  will not touch them, but the pre-existing lines are now stale at the old
  widths and *will* be reflowed. Warn when this happens; we cannot avoid it
  without rewriting the file, which is precisely what this mode forbids.

Interactions:
- `sort = true` inherently rewrites the whole file, so it is only coherent with
  `format_file = true`. Reject the combination `format_file = false` +
  `sort = true` at config load rather than silently picking one.
- `sort = true` makes `insertion = chronological` redundant — sorting subsumes
  it. Not an error, just noise; prefer one or the other.

#### Chronological insertion follows `--sort` semantics

hledger-fmt's `formatSorted` already defines where a date belongs: stable
(equal dates keep source order) and **directive-bounded** — directives and
standalone comment blocks are barriers, transactions reorder only within the
runs between them, and a comment line directly above a transaction (no blank
line between) travels with it. Our `chronological` mode matches this rather
than inventing a second notion of ordering.

### Writing

- **Buffer everything; write once on exit.** Both `Ctrl-C` and `Ctrl-D` save all
  completed transactions, like `hledger add` does.
- A recovery journal is written to `$XDG_STATE_HOME/rledger/` as the session
  progresses, replayed on the next launch if the process dies without writing.
  Invisible when nothing goes wrong.
- Insertion position: `append` (default) or `chronological`.
  - `chronological` inserts after the last transaction with date <= the new one.
  - If the file is not sorted, **warn and proceed anyway**. Tell the user the
    file is unsorted.
- Write target: the main file from `-f` or `$LEDGER_FILE`. Overridable per
  invocation; **error** if the override file is not reachable through the
  include graph. Nested includes must resolve.

### Journal directives honoured (epic 1)

Parsed by rledger itself. No shelling out to `hledger` — the write path
needs file positions and raw text, which resolved `hledger` output does not
carry. `hledger` may later serve as an optional cross-check.

| Directive | Use |
| --- | --- |
| `account` | completion pool (incl. accounts never used in a transaction); triggers `new_account = confirm` |
| `commodity` | valid commodities; **display style** — decimal mark, digit grouping, decimal places, symbol placement and spacing |
| `decimal-mark` | file-scoped parsing and rendering of typed input |
| `D` | default commodity, surfaced as editable pre-filled text |
| `include` | file graph, nested |

Commodity display style is not optional: if the journal declares
`commodity 1.000,00 EUR`, we must write `23,45 EUR` and parse the user's typed
`23,45` with a comma decimal mark. Getting it wrong produces a file that
reformats on the next hledger-fmt pass.

### Configuration

- `~/.config/rledger/config.toml`
- overridden by a local `.rledger.toml`, discovered by **walking up from
  cwd**, the same way hledger discovers its own config.

## Epic 2 — deferred

Deliberately out of scope until the core is right. Recorded here so they are not
lost.

### `payee` directive

Declared payees join the description completion pool and make the near-miss
warning trustworthy (a payee can then be known-good even if it appears in no
transaction yet). Until then, description candidates come only from
transactions actually present in the journal.

### `tag` directive

Declared tags drive completion inside `; comments`. Requires comment entry to
be a first-class field with its own completer, which epic 1 does not have.

### `Y` / `year` directive

Sets the default year for dates in a file. Affects both parsing (a `01-15` date
in that file means the declared year) and writing (dates we emit into such a
file may legally omit the year, and hledger-fmt's convention for that must be
matched). Epic 1 assumes full `YYYY-MM-DD` dates throughout.

### `apply account` and `alias`

The user does not use these, so they are not required for core functionality.
Epic 1 ignores them entirely — no parsing, no detection.

The trap they represent: inside an `apply account assets:bank` block, the text
written to the file is the *remainder* of the account name, not the full name.
Writing the fully-qualified name there silently produces
`assets:bank:assets:bank:checking`. `alias` has the same shape of problem in the
opposite direction. The first step of epic 2 is therefore detection — refuse to
insert into a region where either is active, with a clear error — followed by
full support.

Full support means resolving both directions — display the resolved account name
in completion and the live preview, write the unresolved remainder to the file —
and tracking the lexical scope of each directive across the include graph
(`apply account` is scoped to the rest of the file or until `end apply account`;
`alias` until `end aliases`, and both propagate into included files).

## Open questions

- Is the one-line posting entry actually better than two prompts? Decide by
  feel, once it exists.
- Does the navigation scheme hold up in practice?
- Should the `D` directive be honoured at all, or ignored in favour of config
  only? Currently: honoured, but only as visible pre-filled text.
- Confirm the whole-file-rewrite decision above. The alternative — append at
  current widths and accept that the file may stop being a fixed point of
  `hledger-fmt` — avoids ever touching lines the user did not add, at the cost
  of `hledger-fmt --check` failing afterwards.
