# hledger-x — design notes

A Rust CLI for plain text accounting journals: ergonomic interactive data entry
(a better `hledger add`) plus a formatter.

Status: epics 1 (`fmt`) and 2 (`add`) are implemented. Epic 3 (deferred
directives) is not started. Where the implementation deviates from what is
written here, the deviation is recorded inline.

## Name and CLI surface

The tool is **`hledger-x`**, with two subcommands:

| Command | Purpose |
| --- | --- |
| `hledger-x fmt` | formatting, equivalent to `hledger-fmt` |
| `hledger-x add` | interactive data entry |

Formatting is exposed as a first-class subcommand because the write path
implements it anyway; the marginal cost is a CLI surface, not an engine.

**The name is chosen so hledger dispatches to it.** hledger dispatches to
`hledger-*` executables on `PATH` only for *unknown* subcommands — built-ins
win. Verified against hledger 1.99 with a stub binary on `PATH`: `hledger add`
ran the built-in and never reached the stub, while `hledger jot` reached
`hledger-jot`. So the earlier candidate `hledger-add` could never have been
invoked as `hledger add`, and a neutral name like `rledger` gave up dispatch
altogether.

`x` is not a built-in, so **`hledger x …` reaches `hledger-x`** and the tool is
usable either way:

| Direct | Via hledger |
| --- | --- |
| `hledger-x add` | `hledger x add` |
| `hledger-x fmt` | `hledger x fmt` |

Verified against hledger 1.99 with a stub on `PATH`: `hledger x add --to
foo.journal` reached the stub with `add --to foo.journal` — arguments after the
subcommand are passed through verbatim, hledger consumes none of them, and the
stub's own exit status is propagated. Standard streams are inherited (checked
with a pipe on stdin), so the raw-mode `add` UI works under dispatch as well.
Bare `hledger x` reaches the stub with no arguments, so `hledger-x`'s own
help/usage is what the user sees.

The one-letter name is deliberately meaningless: any descriptive single name is
ambiguous for a tool that both enters and formats data, and the subcommands
carry the meaning. A multi-call binary was considered and rejected as overkill.

**Distribution:** GitHub releases. Not published to crates.io for now, so the
crate name is a non-issue. (For the record: the earlier candidate name
`rledger` is taken on crates.io — a single abandoned 0.1.0 from 2024-12-11.
`hledger-x` has not been checked. If we publish later and the name is taken,
use a different crate name with `[[bin]] name = "hledger-x"`.) CI config is
deferred.

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

## Architecture — two engines, one dependency direction

`fmt` is line-oriented and builds **no semantic model**. That is not a
limitation, it is where its safety comes from: because directives pass through
byte-for-byte, price-only and include-only files are correct by construction.
(One deliberate carve-out, added later with the user: `fmt` *reads* declared
commodity display styles — `commodity`, its `format` subdirective, `D`,
`decimal-mark` — to restyle amounts the way `hledger print` does. It still
never writes or otherwise interprets a directive. See § Amount restyling.)

`add` needs a full semantic model. It is built **on top of** `fmt`, never the
other way around. `fmt` must never learn what any other directive means; the
shared amount machinery lives in the crate-level `amount` module so `fmt`
still does not depend on `add`.

```
        ┌─────────────────────────────────────┐
        │ lexical layer (shared)              │
        │  classify_line, split_account_amount│
        │  split_comment, split_amount, …     │
        └─────────────────────────────────────┘
              ▲                        ▲
              │                        │
      ┌───────┴────────┐      ┌────────┴─────────┐
      │ fmt            │◀─────│ add              │
      │ line-oriented  │ uses │ semantic parser  │
      │ width + render │      │ + entry UI       │
      └────────────────┘      └──────────────────┘
```

The shared lexical layer is the part that is genuinely identical: the 2+
whitespace account/amount separator rule, comment splitting, the
number/commodity/cost-tail split, "does this line open a transaction".

`add` also calls `fmt`'s width computation directly — it needs the file-wide
`accW`/`numW` for both the live preview and the write path.

### Interface style (`add`)

CLI first — an inline raw-mode interface, not a full-screen TUI. Normal
scrollback, no alternate screen; the program controls a few lines around the
cursor and redraws them. Same model as `fish`'s completion menu or
`fzf --height`. A full TUI stays on the table only if the CLI proves
insufficient.

**Built on raw `crossterm`.** `reedline` was the first choice (custom
completers, list menus, ghost-text autosuggestion, rebindable keys), and this
design sanctioned falling back to `crossterm` as a contained rewrite of one
module. That is what happened: the live preview block and the field-navigation
keys fight reedline's line-editor repaint model.

---

# Epic 1 — `hledger-x fmt`

Self-contained and fully specified. Depends on nothing else here.

Reference: [`hledger-fmt`](https://github.com/mikluko/hledger-fmt)
(`src/Hledger/Fmt.hs`, 341 lines of Haskell, `base`-only; goldens in
`test/testdata`). Reimplemented in Rust — roughly 120 lines of real logic.
The binary is not installed; ask the user for a checkout if the source is
needed again.

## CLI

Originally hledger-fmt's surface verbatim, so it was a drop-in substitute.
**Revised 2026-08 with the user** (see § CLI ergonomics below): the *output* is
still hledger-fmt's byte for byte, but the argument model now follows the
mainstream formatters instead.

```
hledger-x fmt [--check] [--diff] [--sort|--no-sort] [-q] [-f|--follow ROOT]... [FILE|-]...
```

- no arguments — the configured `ledger_file`, then `$LEDGER_FILE`, **plus its
  include tree**; a usage error when neither resolves
- `-f`/`--follow ROOT` — that root plus its include tree. Repeatable, and
  combinable with operands
- `FILE...` — format each file in place, **without** following its includes
- `-` — stdin to stdout. Cannot be combined with anything else
- `--check` — write nothing, list what it would reformat on stderr, exit 1
- `--diff` — a unified diff of every change on stdout, in place of the file
  list. On its own it still writes (`terraform fmt -diff`, not `black --diff`);
  pair it with `--check` to write nothing. On stdin it replaces the formatted
  payload, there being no file to write
- `--sort` / `--no-sort` — override the configured `sort`
- `-q`/`--quiet` — do not list the files that changed. `--diff` outranks it:
  asking for a diff is asking for output

Every selected file is deduplicated by canonical path, so a file reachable from
two roots — or named as an operand as well — is formatted once. Roots are
resolved before operands, so a file in a tree keeps the tree's styles.

Exit codes: `0` clean, `1` `--check` found work, `2` usage, `3` a file could
not be read, written, or walked. Worst outcome wins.

### CLI ergonomics — why this shape

Settled with the user 2026-08, after surveying `gofmt`, `cargo fmt`,
`terraform fmt`, `prettier` and `black`. What they share, and what the original
surface was missing:

1. **A bare invocation does the obvious project-wide thing.** `gofmt` is the
   only survivor of the older stdin-by-default convention. `hledger-x` already
   discovers a project root the way `cargo` discovers `Cargo.toml`, so `fmt`
   with no arguments formats it. Stdin now needs an explicit `-`.
2. **The include tree is the project.** It is the journal's analog of a crate's
   module tree. `fmt` already walked it read-only for commodity styles; it now
   formats it too, for roots (not for bare operands, which stay linter-like).
3. **The tool says what it changed** — `terraform fmt` and `black` both do.
4. **Canonical-form decisions live in the config**, not only in flags, so a
   pre-commit hook and a developer's bare `fmt` cannot disagree about what
   "formatted" means. That is what moved `sort` into the config for `fmt`.
5. **`--check` failing is distinguishable from the run breaking**, hence the
   `1` / `3` split.

Deferred, and deliberately so: directory arguments are *not* walked — a
directory is a reported error, which keeps the door open to walking it later
without breaking anyone.

`--diff` renders through the [`similar`](https://docs.rs/similar) crate rather
than a hand-rolled LCS: hunk boundaries and context radius are exactly where
hand-rolled diffs go wrong, and it costs one dependency with no transitives of
its own. Note that a diff is often long out of proportion to the edit, because
alignment is file-wide (see below) — that is the formatter working as designed,
not the diff misreporting.

## Layout rules

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
7. Numbers of a commodity **with a declared display style** are restyled to
   that style, the way `hledger print` shows them (see § Amount restyling).
   Everything else — unitless amounts, undeclared commodities, anything that
   does not parse — is copied through unchanged, byte-for-byte.
8. Blank lines collapse to empty. Transaction headers get trailing whitespace
   trimmed, nothing more. Directives, top-level comments, `include`, `P` lines
   and indented sub-directives pass through verbatim.
9. Output always ends in a newline. `format(format(x)) == format(x)`.

An indented run is treated as postings only when it follows a transaction
header (a line starting at column 0 with a digit), which is what keeps
`account` / `commodity` sub-directives untouched.

## Alignment is file-wide — and it forces whole-file rewrites

`accW` and `numW` are computed over **every posting in the file**, not per
transaction. Verified in `postings.golden`: the `12.50` in the second
transaction is aligned to a column set by `-7485978.18` in the first.

Therefore adding one transaction whose account or number is longer than the
current maximum **reflows every posting line in the file**. This is the single
most consequential fact about the formatter, and it drives the write modes in
epic 2.

## Amount restyling — declared styles only

Added 2026-08 with the user (amending the original "numbers are never
restyled" rule): under `commodity 1_000.00 EUR`, a sloppy `10EUR` or `EUR10`
becomes `10 EUR`, in `fmt` and in the transaction `add` is building alike.

What restyling normalizes — all verified against hledger 1.99's `print`:
symbol side, symbol spacing, digit grouping and the decimal mark. The entered
precision is kept exactly: hledger prints `10 EUR`, not `10.00 EUR`, keeps
trailing zeros (`10.500`), and never rounds. Cost and assertion tails restyle
too. Parsing honours the marks in effect, so under a comma-decimal style a
historical `10.5` genuinely *means* 105 and is rewritten as such — that is
hledger's reading of the file, not ours.

Where styles come from (verified):

- `commodity` directive samples, including the indented `format`
  subdirective. A bare `commodity EUR` declares no style.
- `D` as a fallback: a `commodity` style beats it even when `D` comes later;
  otherwise the last declaration of a commodity wins.
- Styles apply journal-wide regardless of position — a directive after a
  transaction still styles it.
- Styles cross include boundaries. The `fmt` CLI walks the include tree of
  each file argument (read-only; on any parse problem it leniently falls
  back to the text's own directives — `fmt` never refuses to run). Stdin has
  no include tree, so only in-text directives apply there.

Two deliberate deviations from `hledger print`:

1. **No style inference.** hledger also infers a style from the amounts
   themselves (first one seen sets side/spacing, and so on); we restyle only
   commodities with a *declared* style. Inference would let one sloppy entry
   silently reflow a whole journal — explicit over implicit.
2. **A `decimal-mark` directive stays in the rendered number.** `hledger
   print` switches to the style's mark, which is safe only because its
   output drops the directives; rewriting `10,5` as `10.5` inside a
   `decimal-mark ,` file would re-read as 105. The forced mark wins, and a
   colliding group separator swaps with the displaced mark: `1.000,00` under
   `decimal-mark .` renders as `1,000.00`.

Restyling is value-preserving by construction (parse to exact decimal,
re-render; unit tests assert re-parse equality) and `hledger print` output
is byte-identical before and after (semantic tests plus a real-journal
regression). In `add`, a typed amount is normalized at submit time, so the
live preview and the written file agree; generated balancing amounts still
pad to the style's decimal places, while typed amounts keep their typed
precision — exactly what hledger does with such a file.

## Sorting (`--sort`)

Stable (equal dates keep source order) and **directive-bounded**: directives
and standalone comment blocks are barriers, transactions reorder only within
the runs between them, and a comment line directly above a transaction (no
blank line between) travels with it. This keeps positional directives in
scope.

---

# Epic 2 — `hledger-x add`

Requires a semantic parser; epic 1 does not.

## Parser

### Include graph walk

All behaviour below verified against hledger 1.99.

- Start at the main file: `-f`, else the config's `ledger_file`, else
  `$LEDGER_FILE`. The config key sits between the two so a per-directory
  `.hledger-x.toml` can point at its journal without exporting anything, while
  the flag still wins for a one-off. A relative path there resolves against
  the config file's own directory (the same rule `include` follows), a leading
  `~/` against `$HOME`.
- **Include paths resolve relative to the *including* file's directory**, not
  cwd. Verified: `include deep/d.journal` inside `g/sub/mid.journal` resolved to
  `g/sub/deep/d.journal`.
- **Globs are supported** and expanded in sorted order. Verified:
  `include sub/g*.journal` pulled in `g1` and `g2`.
- **Cycles are detected and error.** Verified: mutually including files produce
  an error, not a hang. We do the same — track canonicalized absolute paths,
  error on revisit.
- Depth-first, in file order: directives take effect in encounter order.

### Directive scope model — there are TWO, not one

All verified empirically against hledger 1.99. This was initially assumed to be
a single model; it is not.

#### Model 1 — parse state (`decimal-mark`, `D`)

In effect from its line to the end of its file, **propagates into files that
file includes**, but does **not** escape back to the parent after the include
returns.

Proven with `decimal-mark` and an ambiguous `1.234` amount, in a deliberately
unbalanced transaction so that balancing succeeds iff the directive applied:

| Test | Layout | Result |
| --- | --- | --- |
| A | `decimal-mark` in included file, amount later in parent | **did not apply** — identical to no-directive baseline |
| B | `decimal-mark` and amount in same file | applied |
| C | `decimal-mark` in parent, amount in included file | applied |

Implementation: a scope stack. On entering an included file, push a *copy* of
the current scope. On exit, pop and discard. A directive mutates only the top.

Epic 3 adds `Y`, `apply account`, `alias` to this model.

#### Model 2 — declarations (`account`, `commodity`)

Accumulate into one journal-wide set that is **not** discarded on include
return — but a declaration is only visible **from its position forward** in the
flattened stream. Declarations are *not* collected in a pre-pass.

Verified with `hledger check accounts` / `check commodities`:

| Test | Layout | Result |
| --- | --- | --- |
| A | `account` in included file, used later in parent | **pass** — escapes to parent |
| B | `account` in parent, used in included file | pass |
| C | control, genuinely undeclared | fail |
| D | `account` declared **after** its use, same file | **fail** — order matters |
| E | `include` carrying declarations placed **after** the use | **fail** |
| F | `commodity` in included file, used later in parent | pass |
| G | `commodity` in parent, used in included file | pass |
| H | control, undeclared commodity | fail |

A and D together are the whole story: same-file backward reference fails, but a
declaration inside an include *does* survive into the parent's later content.

Note the direct contradiction with model 1 — round-1 test A and round-2 test A
have identical layout (include at top, use later in parent) and opposite
results. That is what proves the two models are distinct.

Epic 3 adds `payee`, `tag` to this model.

#### Consequence for the new-account guard

Because declarations are position-sensitive, the guard must consider only the
declarations visible **at our insertion point** in the flattened stream — not
every declaration in the tree. Otherwise we can tell the user an account is
declared, write it, and have `hledger check accounts` fail afterwards.

This is not hypothetical: appending to the end of the main file puts our text
after the whole tree only when every `include` sits above the insertion point,
which is conventional but not guaranteed.

Cheap to implement — we already walk the stream in order, so record a stream
position alongside each declaration. The *completion pool* can still use every
declaration in the tree (offering a name is harmless); only the guard needs the
position filter.

### What the parse produces

Two products with different requirements, which lets us parse cheaply where we
can and thoroughly only where we must.

**(1) Journal model** — semantics only, over the whole include tree:
- declared accounts (`account`), each with its **stream position** (needed by
  the new-account guard, see scope model)
- declared commodities and their display styles (`commodity`), likewise
- default commodity (`D`)
- decimal mark in effect per file
- transactions: date, description, postings as (account, raw amount text,
  commodity)

**(2) File map** — for the **write-target file only**, needs raw text and
positions:
- line range and date of each transaction (for chronological insertion)
- whether the file is already formatted
- file-wide `accW` / `numW`

Sort barriers (directives and standalone comment blocks) are deliberately
*not* recorded here: the write path reuses `fmt`'s own sort, which recomputes
them, rather than keeping a second notion of where a barrier is.

### Amounts: parse only what we are about to write

Historical amounts are carried as **raw text** and pre-filled verbatim. This
means we never need to correctly parse the numeric value of an existing
posting, which removes a whole class of failure: a journal using digit grouping
or an unusual decimal mark cannot cause us to pre-fill a wrong number.

We parse numbers in exactly one place: the transaction currently being entered,
to compute the running imbalance and the balancing amount. Those are amounts we
are about to write, so parsing them is unavoidable — and if parsing fails we
show "imbalance unknown" rather than writing something wrong.

Representation: exact decimal, never floating point. `rust_decimal` is
sufficient; `bigdecimal` if we later need unbounded precision.

Number parsing needs the decimal mark in effect. hledger auto-detects when no
directive says otherwise (verified: `1.234,56` is read as comma-decimal
unprompted; a lone `1.234` is read as dot-decimal). `decimal-mark` and
`commodity` directives override the guess.

### Leniency

`add` is an entry tool, not a validator. It must never refuse to run because
the journal contains something it does not understand.

| Situation | Response |
| --- | --- |
| unknown directive | ignore silently |
| unparseable transaction | skip for indexing, warn once |
| unbalanced existing transaction | ignore — not our concern |
| missing `include` target | **warn loudly**, continue (weakens the account pool, so the user must know) |
| periodic (`~`) / auto (`=`) transactions | skip for indexing; they start at column 0 with a non-digit so they are already excluded by the transaction-header rule |

### Frecency index

Built in one pass over the transactions in the include tree.

| Index | Key | Used for |
| --- | --- | --- |
| descriptions | description | description completion |
| accounts | account | account completion, unconditioned |
| by-description | (description, account) | account completion **conditioned on the description already entered** |
| templates | description → most recent transaction | posting pre-fill |

Each entry carries `{ count, last_date, score }`.

Score is a proper frecency — a sum over occurrences rather than a count times a
single decay:

```
score = Σ over occurrences of  0.5 ^ (age_days / half_life)
```

`half_life` configurable, default 90 days, evaluated relative to today at parse
time.

**Pre-fill and ranking deliberately use different rules.** Pre-fill takes the
*most recent* matching transaction, because predictability matters more than
cleverness when text is being put in front of you. The completion menu orders by
*score*, because there ranking is what makes the second-best candidate reachable.

### Performance

Single pass, O(lines). No caching in epic 2 — measure first. If a large journal
turns out to be slow, cache keyed on (path, mtime, size).

## Interface

The live transaction is rendered above the input line and redrawn on every
keystroke, formatted exactly as it will be written to disk:

```
  2026-02-01 Rewe
      expenses:groceries                23.45 EUR
      liabilities:creditcard           -23.45 EUR
  ───────────────────────────────────────────────
  account 3 › ass:ba:che
              assets:bank:checking      ← ghost text, → to accept
```

Running imbalance (per commodity) is displayed while nonzero.

The preview **renders against the file's widths**, not the transaction's own.
When typing pushes a width past the current maximum the preview visibly shifts
— a free signal that this entry will reflow the file.

## Posting entry

**Two prompts per posting** — account, then amount — each its own field with its
own completer. This is the starting point.

A single-line alternative (`account<2+ spaces>amount COMMODITY` in journal
syntax, with the completer switching mode on cursor position) was considered.
It halves the prompt count and uses syntax the user already knows, but it is
unproven and harder to complete against. **Start with two fields; uniting them
later is a change to the field state machine only**, since both designs share
the same completers and the same pre-fill logic. Keep that seam clean.

Accounts are pre-filled from the template, so accepting is one keypress.
Amounts are **never pre-filled into the buffer**: the template amount (or,
on the final posting, the negated running sum) appears as a grey inline
suggestion instead — `Tab`/`→` copies it into the buffer for editing, Enter
submits the field as entered. This keeps the empty buffer's meaning ("balance
and finish") one keypress away without a Ctrl-U first.

**The first posting must carry an amount.** Accepting an **empty amount** on
any later posting means "this is the last posting": the balancing amount is
written into it explicitly and the transaction finishes — the fastest way to
end one. (Balancing amounts are never elided, so `add` cannot produce bare
postings at all; an empty amount either balances-and-finishes or is
rejected.) The empty-account finish remains for the case where every amount
was typed explicitly.

## Dates

- Default is **today**, for every transaction — not the previous entry's date
  (which is what `hledger add` does). It is a grey inline suggestion, not a
  pre-fill: the field starts empty so a smart date can be typed directly,
  Enter accepts the suggestion, `Tab`/`→` copies it in for editing.
- Partial and smart dates accepted (`30`, `8/30`, `yesterday`), resolved against
  today.
- The fully resolved date is rendered in the live block as you type, so an
  invalid or misunderstood date is visible before it is committed.

## Navigation

| Key | Action |
| --- | --- |
| `Enter` | accept field, advance; on an empty amount or an empty account line, finish the transaction |
| `↑` / `↓` | move between fields, loading existing text for editing |
| `Tab` / `Shift-Tab` | pick up the grey suggestion, else open completion menu / cycle candidates |
| `→` | pick up the grey suggestion / accept ghost-text autosuggestion |
| `Ctrl-E` | open the whole transaction in `$EDITOR`, reparse on close |
| `Ctrl-U` | clear the buffer |
| `Ctrl-W` | delete a word; on account fields, one `:`-segment at a time (stopping just short of the previous colon) |
| `Ctrl-C` | abort the current transaction, not the program |
| `Ctrl-D` | quit, saving everything completed |
| `q` at the date prompt | same as `Ctrl-D` — the discoverable exit |

**No separate history key.** A `Ctrl-R` history search was specified and then
dropped: `Tab` on an empty buffer already opens the field's whole candidate
list, frecency-ranked, which is the same thing with one fewer key to know.
Up/Down stay field navigation, never history.

No "save this transaction? [y]" confirmation — redundant when the transaction
has been visible the whole time. `u` at the date prompt undoes the last
completed transaction — and because undo has to be able to take a transaction
back off the screen, the log of completed transactions above the prompt is
part of the redrawn frame, not scrollback. It is handed to scrollback on exit,
so what stays behind is exactly what was written. When the log outgrows the
terminal the newest entries win, with a dim `… N earlier transaction(s)` line
standing in for the rest. The UI surfaces these affordances as inline hints
(dimmed, under the prompt) when the relevant field comes up — how to finish a
transaction and how to leave must be discoverable without documentation.

**Unproven** — needs to be seen in practice before sign-off.

## Completion

**Style**, configurable per field (`account_completion`,
`description_completion`), three available. Segmentation is not one of them —
it is the frame all three run in, revised 2026-08 with the user. An account
query is split on `:` and matched one segment at a time; **no style ever
matches across a colon.**

| Style | Behaviour on accounts |
| --- | --- |
| `prefix` | anchored: query segment *i* against candidate segment *i*, no gaps. `ex:gro` → `expenses:groceries`; `gro` alone matches nothing, and `a:s` does not reach `assets:bank:savings` |
| `substring` | middle-of-segment, segments in order with gaps allowed — `check` → `assets:bank:checking` |
| `fuzzy` | subsequence within a segment, gaps allowed, ranked by match quality — `ckng` → `assets:bank:checking`, but `asbk` does not span `assets:bank` |

`prefix` is anchored and the other two are not, deliberately. Anchoring
`substring` would test `check` against the `assets` segment and never find
`assets:bank:checking`, which is the case the style exists for; leaving
`prefix` unanchored would make it something other than a prefix match.

Default for both fields: `substring`. Descriptions are plain text — a `:` in
one is ordinary (hledger's payee separator is `|`), so they are never
segmented.

**Tab completes**, it does not merely filter (revised 2026-08 — the first
implementation only opened a menu, and selecting from it needed Enter). Tab
inserts the longest prefix of the matches' common prefix that is both longer
than what is typed and still matches *exactly the same candidates*. That
second condition is load-bearing: completing must never widen the match set,
which would silently drop a constraint the user typed. A unique match is
inserted whole; a partial agreement is inserted and the menu opens on what
is left; no match leaves the buffer untouched. Ranking orders the menu but
never decides an insertion — only unanimity does, so a confident-looking
fuzzy top hit can never be committed behind the user's back. An empty buffer
does not complete: there Tab opens the whole candidate list, which is how
history is browsed.

**Enter never completes.** It submits the buffer literally, so `check` +
Enter enters an account named `check` (caught by the new-account note, or by
`strict`'s confirmation). hledger's own `add` completes on Enter; that
implicitness is the thing being rejected, and the settled preference is
explicit over implicit.

**Accounts need no declaration.** hledger only requires `account` directives
under `--strict`, so the completion pool is every account *used* in a posting
anywhere in the include tree, plus every account declared anywhere (offering
a declared-but-unused name is harmless). Accounts entered during a session
join the pool the moment they are committed — available to later postings of
the same transaction and to later transactions — scored at today, so they
rank where a just-used account belongs. They stop counting as new: the
"new to this journal" note fires once, and `strict` asks once. Undo removes
completed transactions from the pool again; it does not un-ask the question,
because the acceptance was deliberate either way.

Fields and their completers: date (none), description (descriptions index),
account (accounts index, conditioned on description), amount (commodities).

**Ranking** — frecency conditioned on the description already entered (see
parser). This is the answer to "how do I reach the second similar transaction"
without a separate template-selection step: at Rewe, both real grocery patterns
are the top two candidates, one `Tab` apart. No cost when there is only one
obvious answer.

**Presentation** — both at once, fish-style:
- ghost text of the top candidate, `→` accepts (zero extra keystrokes in the
  common case)
- `Tab` or `Shift-Tab` completes as far as the candidates agree; if that
  settles it the menu stays shut, otherwise it opens on what is left — on an
  empty buffer that is the whole candidate list — and once open they cycle
  it, arrows navigate. Entries are bare names — no hint columns — and the
  menu is as tall as the screen allows, scrolling beyond that

## Amounts and commodities

- **Never infer a commodity.** A unitless amount is valid even in strict mode.
- A configured default commodity, and the journal's own `D` directive, may
  supply one — but always as **visible, editable text**, never applied
  silently at write time. The user sees `23.45 EUR` and can delete the `EUR`.
  Concretely: submitting a bare number materializes the default commodity
  into the amount (`12.50` becomes `12.50 EUR` in the built transaction and
  the live preview, following the commodity's declared symbol side/spacing;
  `↑` goes back to edit it). Without this the balancing amount — which
  carries the default commodity — and the bare amount would read as two
  different commodities, and the transaction could never balance. Amounts
  that already carry a commodity, or a cost/assertion tail, are never
  touched.
- No arithmetic, no percentage or split helpers. Would collide with a
  percentage commodity.
- Balancing amounts are always written **explicitly**, never elided.
- Amounts we generate respect the `commodity` / `decimal-mark` display style:
  if the journal declares `commodity 1.000,00 EUR` we write `23,45 EUR` and
  parse typed `23,45` with a comma decimal mark. Getting this wrong produces a
  file that reformats on the next `fmt` pass.

## Equity conversion postings (2026-08, with the user)

Config `equity_conversion = true | false`, default `false`, plus
`equity_conversion_account`, default `equity:conversion` — the account hledger's
own `--infer-equity` uses.

A transaction with a cost (`10 USD @@ 9.06 EUR` against `-9.06 EUR`) balances
*at cost* but its face amounts do not sum to zero. hledger accepts either form;
some people prefer the conversion made explicit, as a pair of postings to an
equity account. When the option is on, they are appended to the transaction the
moment it is finished — so they land in the written file and in the block
echoed to the terminal, with no extra prompt.

- The amounts are the **negated face-value imbalance** (`face_balance` in
  `amount`, which reads `ParsedAmount::value` where the ordinary balance reads
  `contributes`). One posting per imbalanced commodity, in the order the
  commodities first appear in the transaction.
- **Never guess.** Nothing is generated when the face imbalance is unknown —
  an unparseable amount, or an elided one — mirroring how an unknown imbalance
  is already handled everywhere else.
- Single-commodity transactions are untouched: their face imbalance is zero.
- This is a flat account, not hledger's per-pair
  `equity:conversion:EUR-USD:USD` subaccounts. Verified against hledger 1.99:
  it reads the flat form back as balanced.
- The generated postings deliberately skip the strict-mode check. hledger-x is
  generating them, not the user typing them; in a strict journal the account
  wants declaring once.

## Strict mode — the undeclared-name guard

Config `strict = true | false`, default `false`. (Supersedes the earlier
`new_account = confirm | warn | allow | error` design: one switch, two
checks, wording fixed.)

- **Wording matters**: hledger-x never declares anything — the question is
  whether to *use* an undeclared name. "`exp:trav` is not a declared account
  — use it anyway? (did you mean `expenses:travel:train`?)"
- **Strict** checks both accounts and commodities against the declarations
  visible at the insertion point (the position-filtered sets — exactly what
  `hledger check accounts` / `check commodities` would accept there), and
  asks before using anything undeclared. The commodity check covers the face
  amount and any second commodity in an `@`/`@@` cost or assertion tail. A
  unitless amount is always valid, even in strict mode.
- **Not strict** (default): everything is accepted; a name that is neither
  declared nor used anywhere in the journal gets a passing, non-blocking
  note, so typos stay visible without a prompt.

## Write modes

| Setting | Default | Behaviour |
| --- | --- | --- |
| `format_file` | `true` | reformat the whole file on write |
| `sort` | `false` | also sort transactions by date, `--sort` semantics |
| `insertion` | `append` | or `chronological` |

**Default — always emit a fully formatted file.** When the file is already
formatted and widths do not grow, this is byte-identical to a pure append (zero
diff). When widths grow, it produces exactly the reflow `fmt` would produce
anyway. One code path, always idempotent. Warn on first write if the file was
not already formatted, since the tool will reformat it.

**With `format_file = false`**, only the lines we add are written, but they are
rendered *as if the whole file were correctly formatted*: `accW`/`numW` are
still computed over the entire file including the new transactions.

The guarantee this gives is narrower than it sounds:
- widths unchanged and file already formatted → the file is a fixed point; a
  later `fmt` run is a no-op.
- widths grew → **our** lines are correct and a later `fmt` run will not touch
  them, but the pre-existing lines are now stale at the old widths and *will* be
  reflowed. Warn when this happens; we cannot avoid it without rewriting the
  file, which is precisely what this mode forbids.

Interactions:
- `sort = true` inherently rewrites the whole file, so it is only coherent with
  `format_file = true`. Reject `format_file = false` + `sort = true` at config
  load rather than silently picking one.
- `sort = true` makes `insertion = chronological` redundant — sorting subsumes
  it. Not an error, just noise; prefer one or the other.

## Writing

- **Buffer everything; write once on exit.** Both `Ctrl-C` and `Ctrl-D` save all
  completed transactions, like `hledger add` does.
- A recovery journal is written to `$XDG_STATE_HOME/hledger-x/` as the session
  progresses, replayed on the next launch if the process dies without writing.
  Invisible when nothing goes wrong.
- `insertion = chronological` inserts after the last transaction with date <=
  the new one, following `fmt --sort` semantics rather than inventing a second
  notion of ordering. If the file is not sorted, **warn and proceed anyway**,
  telling the user the file is unsorted.
- Write target: the main file resolved above. Overridable per
  invocation; **error** if the override file is not reachable through the
  include graph. Nested includes must resolve.

## Journal directives honoured

| Directive | Use | Scope |
| --- | --- | --- |
| `account` | completion pool (incl. accounts never used in a transaction); the set strict mode checks against | declaration — journal-wide, position-sensitive |
| `commodity` | valid commodities; display style — decimal mark, digit grouping, decimal places, symbol placement and spacing | declaration — journal-wide, position-sensitive |
| `decimal-mark` | parsing and rendering of typed input | parse state — file-scoped, inherited by includes |
| `D` | default commodity, surfaced as editable pre-filled text | parse state — file-scoped, inherited by includes |
| `include` | file graph, nested, globs | — |

## Configuration

- `~/.config/hledger-x/config.toml`
- overridden by a local `.hledger-x.toml`, discovered by **walking up from cwd**,
  the same way hledger discovers its own config.

---

# Epic 3 — deferred directives

Deliberately out of scope until the core is right. Recorded so they are not
lost.

## `payee` directive

Declared payees join the description completion pool and make the near-miss
warning trustworthy (a payee can then be known-good even if it appears in no
transaction yet). Until then, description candidates come only from
transactions actually present in the journal.

## `tag` directive

Declared tags drive completion inside `; comments`. Requires comment entry to
be a first-class field with its own completer, which epic 2 does not have.

## `Y` / `year` directive

Sets the default year for dates in a file. Affects both parsing (a `01-15` date
in that file means the declared year) and writing (dates we emit into such a
file may legally omit the year, and the `fmt` convention for that must be
matched). Epic 2 assumes full `YYYY-MM-DD` dates throughout.

## `apply account` and `alias`

Not used by the user, so not required for core functionality. Epics 1 and 2
ignore them entirely — no parsing, no detection.

The trap they represent: inside an `apply account assets:bank` block, the text
written to the file is the *remainder* of the account name, not the full name.
Writing the fully-qualified name there silently produces
`assets:bank:assets:bank:checking`. `alias` has the same shape of problem in the
opposite direction.

The first step of this epic is therefore **detection** — refuse to insert into a
region where either is active, with a clear error — followed by full support.
Full support means resolving both directions (display the resolved account name
in completion and the live preview, write the unresolved remainder to the file)
and tracking lexical scope across the include graph: `apply account` is scoped
to the rest of the file or until `end apply account`, `alias` until
`end aliases`, and both propagate into included files under the scope-stack
model above.

---

# Open questions

Neither blocks epic 1.

- Should account and amount be united into one journal-syntax line? Deferred —
  build two fields first, decide by feel.
- Does the navigation scheme hold up in practice?
