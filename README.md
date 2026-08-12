# hledger-x

[![standard-readme compliant](https://img.shields.io/badge/standard--readme-OK-green.svg?style=flat-square)](https://github.com/RichardLitt/standard-readme)

Format hledger journals and enter transactions ergonomically

`hledger-x` is a CLI for plain text accounting journals with two subcommands:

- `hledger-x fmt` — a format-preserving journal formatter producing the same
  output as [`hledger-fmt`](https://github.com/mikluko/hledger-fmt), behind a
  project-aware CLI in the shape of `gofmt` and `cargo fmt`
- `hledger-x add` — ergonomic interactive data entry, a better `hledger add`

The formatter is line-oriented. Directives, comments, `include` lines and `P`
price lines pass through byte-for-byte, so price-only and include-only files
are safe by construction. Only posting lines are reflowed — and amounts of a
commodity with a declared display style are restyled to it, the way
`hledger print` shows them.

## Table of Contents

- [Status](#status)
- [Install](#install)
- [Usage](#usage)
- [Formatting rules](#formatting-rules)
- [Interactive entry](#interactive-entry)
- [Configuration](#configuration)
- [Messages](#messages)
- [Testing](#testing)
- [Maintainers](#maintainers)
- [Contributing](#contributing)
- [License](#license)

## Status

`hledger-x fmt` is complete and verified against the reference implementation's
golden fixtures. `hledger-x add` is implemented; its key-navigation scheme is
young and may still change with use. See `DESIGN.md` for the decisions behind
both.

## Install

Build from source with cargo:

```sh
cargo install --path .
```

Binaries are published in the [releases section of this
repo](https://github.com/kfkonrad/hledger-x/releases).

Once `hledger-x` is on your `PATH`, hledger dispatches to it as well, because
`x` is not one of its built-in subcommands. Every invocation below can be
written either way:

```sh
hledger-x add     # or: hledger x add
hledger-x fmt     # or: hledger x fmt
```

Arguments after the subcommand are passed through verbatim, and the exit status
is the one `hledger-x` returns.

## Usage

```sh
hledger-x fmt [--check] [--diff] [--sort|--no-sort] [-q] [-f ROOT]... [FILE|-]...
```

With no arguments, format the whole journal — the configured `ledger_file`, or
`$LEDGER_FILE`, together with every file it includes. This is the everyday
invocation, and the reason `ledger_file` is worth setting:

```sh
hledger-x fmt
```

Format a journal that is not the configured one, again with its includes:

```sh
hledger-x fmt -f 2025/main.journal
```

Format exactly the files named, without following their includes:

```sh
hledger-x fmt main.journal 2025.journal
```

`-f`/`--follow` is repeatable and combines with plain file operands. A file
reachable from more than one root, or named as an operand as well, is formatted
once:

```sh
hledger-x fmt -f main.journal -f archive/2024.journal stray.journal
```

Format standard input to standard output:

```sh
hledger-x fmt - < main.journal
```

Each file that changed is listed on stdout; `-q`/`--quiet` suppresses that
list. Files already in final form are left untouched on disk, so their
modification times do not move.

Check without writing anything. Lists what it would reformat on stderr and
exits 1, which makes it suitable for CI and pre-commit hooks:

```sh
hledger-x fmt --check
```

Show a unified diff of every change on stdout, in place of the file list:

```sh
hledger-x fmt --diff          # format, and show what changed
hledger-x fmt --check --diff  # write nothing, and show what would change
```

Diffs can be long out of proportion to the edit: posting alignment is computed
over every posting in the file, so one account or number wider than the current
maximum reflows every posting line.

Also sort transactions by date. Sorting is stable (equal dates keep their source
order) and directive-bounded: directives and standalone comment blocks act as
barriers, so transactions reorder only within the runs between them and
positional directives keep their scope. A comment line directly above a
transaction travels with it.

```sh
hledger-x fmt --sort main.journal
```

Sorting is part of what "formatted" means, so it belongs in the config
(`sort = true`) rather than in a flag that some invocations remember to pass
and others forget. `--sort` and `--no-sort` override the configured value in
either direction.

A file that cannot be read is reported on stderr and skipped; the remaining
files are still processed. Directories are not walked — a directory argument is
reported as such. Commodity display styles come from the include tree the file
was reached through: a root's tree under `--follow`, and the file's own tree
when it is named as an operand.

| Exit code | Meaning                                                     |
|-----------|-------------------------------------------------------------|
| 0         | success, or `--check` found nothing to fix                  |
| 1         | `--check` found files that need formatting                  |
| 2         | usage error, including no journal to format                 |
| 3         | a file could not be read, written, or walked                |

The worst outcome wins: a run that both finds unformatted files and fails to
read one exits 3.

`hledger-x add` uses the same codes: 0 when it finishes, 2 when the invocation
or the configuration does not make sense (no journal to add to, a `--to` target
the journal does not include, a bad config key), and 3 when something could not
be read or written.

## Formatting rules

1. Posting indent is exactly 4 spaces.
2. Account and amount are separated by a run of 2 or more whitespace characters,
   which is hledger's own rule — account names may contain single spaces.
3. The account field is padded to the width of the longest account name, then 2
   spaces, then the number right-aligned in the number column, then 1 space,
   then the commodity.
4. `@`/`@@` cost and `=`/`==` assertion tails follow with single-space
   separators and are never column-aligned.
5. Amount-less postings keep their account only. One carrying just an assertion
   or cost reserves the number and commodity columns with blanks, so the tail
   lines up as if a zero amount stood in front of it.
6. Inline posting comments are normalized to 2 spaces before the `;`.
7. Amounts of a commodity with a declared display style — a `commodity`
   directive sample such as `commodity 1_000.00 EUR`, its indented `format`
   subdirective, or `D` — are restyled to it, as `hledger print` shows them:
   `10EUR` and `EUR10` become `10 EUR`, `3042.03 EUR` becomes `3_042.03 EUR`.
   Symbol side, spacing, digit grouping and decimal mark are normalized;
   the entered precision is kept (no padding, no rounding), and cost and
   assertion tails restyle too. Styles are collected across the `include`
   tree when formatting files (stdin sees only its own directives). Unitless
   amounts, undeclared commodities and anything unparseable are copied
   through unchanged — a `decimal-mark` directive keeps its mark in the
   output so every rewrite reads back to the same value.
8. Blank lines collapse to empty. Transaction headers lose trailing whitespace
   and nothing more.
9. Output always ends in a newline, and formatting is idempotent.

Alignment is **file-wide**, not per transaction: the account and number columns
are computed over every posting in the file. Adding one transaction with a
longer account name or number therefore reflows every posting line in the file.

## Interactive entry

```sh
hledger-x add [-f FILE] [--to FILE]
```

The journal is `-f FILE`, else the config's `ledger_file`, else
`$LEDGER_FILE`. `--to` writes new transactions
into a different file, which must be reachable through the journal's `include`
graph.

`hledger-x add` walks the whole include tree (nested includes, globs), honours
`account`, `commodity`, `decimal-mark` and `D` directives, and builds a
frecency index over every transaction it finds. Entry is field by field —
date, description, then account/amount pairs — with:

- a **live preview** above the prompt, formatted exactly as it will be
  written, at the file's own alignment widths
- **pre-filled accounts** from the most recent transaction with the same
  description. Typing over a pre-fill replaces it; Enter accepts it as-is
- **grey inline suggestions** for dates and amounts, never pre-filled into
  the buffer: the date field suggests today (type a smart date to override),
  amount fields suggest the template or balancing amount. `Tab` or `→`
  copies the suggestion into the buffer for editing; Enter submits the
  field as entered. The first posting must carry an amount; Enter on an
  **empty amount** on any later posting writes the balancing amount
  explicitly and finishes the transaction — the quickest way to end one
- **completion** everywhere, and `Tab` really completes: it inserts as much
  as every remaining candidate agrees on, exactly like bash or fish. A
  unique match lands in the buffer whole — with the default substring
  style, `check` + `Tab` gives you `assets:bank:checking` outright, no menu
  and no Enter to select. When the candidates only agree part-way, you get
  that much (`an` → `assets:bank:`) and a menu of what is left, as tall as
  the screen allows and scrolling beyond that, ranked by frecency and
  conditioned on the description already entered. On an empty field that
  menu is the whole candidate list, which is how you browse history.
  Completion never widens your query: a `Tab` that would drop a constraint
  you typed opens the menu instead of guessing, and a query that matches
  nothing leaves the buffer untouched. Ranking orders the menu but never
  decides an insertion — only unanimity does
- **account completion is segment-aware** under every style: a query is
  matched one `:`-segment at a time and never across the colon. `prefix` is
  anchored, segment against segment (`ex:gro` → `expenses:groceries`;
  `gro` alone matches nothing). `substring` and `fuzzy` allow gaps, so a
  bare `check` or `ckng` reaches `assets:bank:checking` while `asbk` — which
  would span `assets:bank` — does not. Descriptions are plain text, so a `:`
  in one is never a segment break. In amounts, commodities complete both
  for the face amount and for the second commodity of an `@`/`@@` cost or
  `=`/`==`/`=*`/`==*` assertion tail
- **accounts do not have to be declared.** Anything used in a posting
  anywhere in the include tree completes, declared or not, as does anything
  declared but never used. An account you introduce during a session joins
  the pool immediately — available to the next posting of the same
  transaction and to every later transaction, announced as new only once,
  and asked about only once under `strict`. Undo takes it back out
- an optional **strict mode** (`strict = true`): using an account or
  commodity that is not declared at the insertion point asks first — "…is
  not a declared account — use it anyway?" — surfacing near-misses ("did
  you mean …?") instead of silently forking your account tree. hledger-x
  never declares anything itself; the question is only whether to use the
  name. Off by default: undeclared names are then accepted, with a passing
  note when they are new to the journal
- **smart dates**: `30`, `8/30`, `yesterday`, resolved against today and
  shown resolved in the preview before you commit
- a running per-commodity imbalance in the separator line; a transaction that
  provably does not balance cannot be finished

Keys: `Enter` accepts a field (on an empty amount or an empty account line it
finishes the transaction) as typed — Enter never completes for you, so what
is in the buffer is what you get, `↑`/`↓` move between fields,
`Tab`/`Shift-Tab` complete and then, if the result is still ambiguous, open
the menu and cycle it, `Tab`/`→` pick up the grey suggestion, `→` accepts the ghost suggestion, `Ctrl-U` clears the buffer,
`Ctrl-W` deletes
one word — on account fields one `:`-segment at a time — `Ctrl-E` opens the
draft in `$EDITOR`, `Ctrl-C` aborts the current transaction. To leave, type
`q` at the date prompt or press `Ctrl-D` anywhere; both write everything
completed. `u` at the date prompt undoes the last completed transaction, which
also removes it from the log of finished transactions shown above the prompt.
The UI shows these hints inline as the fields come up, so none of this needs
remembering.

Everything is buffered and written **once, on exit** — both `Ctrl-C` and
`Ctrl-D` keep completed transactions. A recovery journal under
`$XDG_STATE_HOME/hledger-x/` is maintained during the session and replayed on
the next launch if the process dies without writing; it is invisible when
nothing goes wrong.

When stdin is not a terminal, `hledger-x add` falls back to plain line-based
prompts, so it can be scripted and tested through pipes.

Amounts are handled exactly (never floating point), template pre-fills
reproduce the journal's text verbatim, and a commodity is never inferred
invisibly. Typed amounts are normalized to the declared display style as you
accept them — a typed `10EUR` commits as `10 EUR`, precision kept — and
generated balancing amounts follow the same `commodity` / `decimal-mark`
styles, padded to the style's decimal places. A default commodity (from `D` or the config)
is written into bare amounts as you accept them — visible in the preview and
editable like any other text (`↑` to go back), never attached behind your
back at write time. Without it, a bare `12.50` and the `-12.50 EUR`
balancing amount would read as two different commodities and the
transaction could never balance.

## Configuration

`~/.config/hledger-x/config.toml`, overridden key-by-key by the nearest
`.hledger-x.toml` found walking up from the current directory. All keys are
optional; unknown keys are rejected so typos cannot silently disable anything.
A relative `ledger_file` is resolved against the directory of the config file
that set it, and a leading `~/` against `$HOME`.

Both subcommands read `ledger_file` and `sort`; the rest govern `add` alone.

```toml
ledger_file = "main.journal"  # the journal, below -f and above $LEDGER_FILE
format_file = true            # rewrite the whole file, formatted, on write
sort = false                  # keep transactions in date order
insertion = "append"          # or "chronological"
strict = false                # ask before using undeclared accounts/commodities
half_life_days = 90           # frecency decay half-life
account_completion = "substring"     # prefix | substring | fuzzy
description_completion = "substring"
default_commodity = "EUR"     # written into bare amounts as visible, editable text
equity_conversion = false     # append equity postings for multi-commodity transactions
equity_conversion_account = "equity:conversion"
```

With `equity_conversion = true`, a transaction that balances at cost but whose
face amounts do not sum to zero gets the difference posted to
`equity_conversion_account` — one posting per commodity, in the order the
commodities appear. They are appended and shown the moment the transaction is
finished:

```
2026-08-04 IVPN
    expenses:subscriptions:services     10 USD @@ 9.06 EUR
    assets:dkb:giro                  -9.06 EUR
    equity:conversion                  -10 USD
    equity:conversion                 9.06 EUR
```

This is the flat-account form of what `hledger print --infer-equity`
generates. Nothing is generated when the imbalance cannot be computed (an
unparseable or elided amount), and single-commodity transactions are
untouched. In a strict journal, declare the account once — the generated
postings do not prompt.

With `strict = true`, accounts and commodities are checked against the
declarations (`account` / `commodity` directives) visible at the insertion
point — the same set `hledger check accounts` / `check commodities` would
accept there — and anything undeclared asks for confirmation before being
used. The commodity check covers both the face amount and any second
commodity in a cost or assertion tail. Unitless amounts are always valid. `format_file = false` writes only
the new lines (rendered at the would-be file-wide widths) and warns when the
file's existing lines become stale; combining it with `sort = true` is
rejected, since sorting rewrites the whole file anyway.

## Messages

Diagnostics are written for someone keeping books, not for someone reading a
stack trace. An error names the program and subcommand, says what failed and why
in plain language, and — where knowing why is not enough to act on — adds an
indented hint. Non-fatal problems are prefixed `warning:` and the run continues:

```console
$ hledger-x fmt nope.journal
hledger-x fmt: nope.journal: no such file

$ hledger-x add
hledger-x add: no journal file to add to
  (pass -f FILE, set `ledger_file` in .hledger-x.toml, or set $LEDGER_FILE)
```

A mistake in the config is reported against the file and line it is on, with
that line echoed and, where the intent is unambiguous, the setting you meant:

```console
$ hledger-x fmt
hledger-x fmt: config: .hledger-x.toml, line 1: unknown setting `formatfile`
      formatfile = true
  (did you mean `format_file`?)
```

Paths are shown as you would name them — relative to the working directory when
they sit under it, absolute when they do not. Raw OS error numbers, Rust
backtrace instructions, source locations, and serde's type vocabulary are never
part of a message; `tests/cli.rs` asserts that none of them can reach you. A
panic is the one exception, because a panic is a bug in `hledger-x` and its
report exists for whoever fixes it.

If `add` fails after you have entered something, the message says where the
recovery journal is — nothing you typed is lost.

## Testing

```sh
cargo test
cargo clippy --all-targets
```

The suite has four layers:

- **Golden fixtures** ported from `hledger-fmt`, compared byte-for-byte
- **Idempotence**, on every fixture
- **CLI behaviour**, driving the built binary end to end
- **Semantic equivalence** — `hledger print` output must be byte-identical
  before and after formatting, which is the safety net behind every layout rule
  above. Skipped when `hledger` is not on `PATH`.

## Maintainers

[@kfkonrad](https://github.com/kfkonrad)

## Contributing

PRs accepted.

Small note: If editing the README, please conform to the
[standard-readme](https://github.com/RichardLitt/standard-readme) specification.

## License

MIT © 2026 Kevin F. Konrad
