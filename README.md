# rledger

[![standard-readme compliant](https://img.shields.io/badge/standard--readme-OK-green.svg?style=flat-square)](https://github.com/RichardLitt/standard-readme)

Format hledger journals and enter transactions ergonomically

`rledger` is a CLI for plain text accounting journals with two subcommands:

- `rledger fmt` — a format-preserving journal formatter, a drop-in equivalent to
  [`hledger-fmt`](https://github.com/mikluko/hledger-fmt)
- `rledger add` — ergonomic interactive data entry, a better `hledger add`

The formatter is line-oriented and never builds a semantic model. Directives,
comments, `include` lines and `P` price lines pass through byte-for-byte, so
price-only and include-only files are safe by construction. Only posting lines
are reflowed.

## Table of Contents

- [Status](#status)
- [Install](#install)
- [Usage](#usage)
- [Formatting rules](#formatting-rules)
- [Interactive entry](#interactive-entry)
- [Configuration](#configuration)
- [Testing](#testing)
- [Maintainers](#maintainers)
- [Contributing](#contributing)
- [License](#license)

## Status

`rledger fmt` is complete and verified against the reference implementation's
golden fixtures. `rledger add` is implemented; its key-navigation scheme is
young and may still change with use. See `DESIGN.md` for the decisions behind
both.

## Install

Build from source with cargo:

```sh
cargo install --path .
```

Binaries are published in the [releases section of this
repo](https://github.com/kfkonrad/rledger/releases).

## Usage

```sh
rledger fmt [--check] [--sort] [FILE|-]...
```

Format files in place:

```sh
rledger fmt main.journal 2025.journal
```

Format standard input to standard output — `-` and no arguments both mean
stdin:

```sh
rledger fmt < main.journal
```

Check without writing anything. Exits non-zero and lists the offending files on
stderr if any file is not already formatted, which makes it suitable for CI and
pre-commit hooks:

```sh
rledger fmt --check *.journal
```

Also sort transactions by date. Sorting is stable (equal dates keep their source
order) and directive-bounded: directives and standalone comment blocks act as
barriers, so transactions reorder only within the runs between them and
positional directives keep their scope. A comment line directly above a
transaction travels with it.

```sh
rledger fmt --sort main.journal
```

Files are processed one at a time, like a linter: `include` directives are not
followed. A file that cannot be read is reported on stderr and skipped; the
remaining files are still processed and the run exits non-zero.

| Exit code | Meaning                                              |
|-----------|------------------------------------------------------|
| 0         | success, or `--check` found nothing to fix           |
| 1         | `--check` found unformatted files, or a file errored |
| 2         | usage error                                          |

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
7. Numbers are never restyled — digit grouping, decimal places and sign spacing
   are copied through unchanged.
8. Blank lines collapse to empty. Transaction headers lose trailing whitespace
   and nothing more.
9. Output always ends in a newline, and formatting is idempotent.

Alignment is **file-wide**, not per transaction: the account and number columns
are computed over every posting in the file. Adding one transaction with a
longer account name or number therefore reflows every posting line in the file.

## Interactive entry

```sh
rledger add [-f FILE] [--to FILE]
```

The journal is `-f FILE` or `$LEDGER_FILE`. `--to` writes new transactions
into a different file, which must be reachable through the journal's `include`
graph.

`rledger add` walks the whole include tree (nested includes, globs), honours
`account`, `commodity`, `decimal-mark` and `D` directives, and builds a
frecency index over every transaction it finds. Entry is field by field —
date, description, then account/amount pairs — with:

- a **live preview** above the prompt, formatted exactly as it will be
  written, at the file's own alignment widths
- **pre-filled postings** from the most recent transaction with the same
  description; the final amount is pre-filled with the balancing amount.
  Typing over a pre-fill replaces it; Enter accepts it as-is. The first
  posting must carry an amount; accepting an **empty amount** on any later
  posting writes the balancing amount explicitly and finishes the
  transaction — the quickest way to end one
- **completion** everywhere: ghost-text suggestion (`→` accepts) plus a
  `Tab` menu, ranked by frecency and conditioned on the description already
  entered. Account queries match by substring, or per-segment once the query
  contains a colon (`ex:gro` → `expenses:groceries`). In amounts, commodities
  complete both for the face amount and for the second commodity of an
  `@`/`@@` cost or `=`/`==`/`=*`/`==*` assertion tail
- an optional **strict mode** (`strict = true`): using an account or
  commodity that is not declared at the insertion point asks first — "…is
  not a declared account — use it anyway?" — surfacing near-misses ("did
  you mean …?") instead of silently forking your account tree. rledger
  never declares anything itself; the question is only whether to use the
  name. Off by default: undeclared names are then accepted, with a passing
  note when they are new to the journal
- **smart dates**: `30`, `8/30`, `yesterday`, resolved against today and
  shown resolved in the preview before you commit
- a running per-commodity imbalance in the separator line; a transaction that
  provably does not balance cannot be finished

Keys: `Enter` accepts a field (on an empty amount or an empty account line it
finishes the transaction), `↑`/`↓` move between fields, `Tab`/`Shift-Tab`
cycle the completion menu, `→` accepts the ghost suggestion, `Ctrl-W` deletes
one word — on account fields one `:`-segment at a time — `Ctrl-E` opens the
draft in `$EDITOR`, `Ctrl-C` aborts the current transaction. To leave, type
`q` at the date prompt or press `Ctrl-D` anywhere; both write everything
completed. `u` at the date prompt undoes the last completed transaction. The
UI shows these hints inline as the fields come up, so none of this needs
remembering.

Everything is buffered and written **once, on exit** — both `Ctrl-C` and
`Ctrl-D` keep completed transactions. A recovery journal under
`$XDG_STATE_HOME/rledger/` is maintained during the session and replayed on
the next launch if the process dies without writing; it is invisible when
nothing goes wrong.

When stdin is not a terminal, `rledger add` falls back to plain line-based
prompts, so it can be scripted and tested through pipes.

Amounts are handled exactly (never floating point), historical amounts are
never reinterpreted — pre-fills reproduce them verbatim — and a commodity is
never inferred invisibly: generated amounts follow the journal's `commodity` /
`decimal-mark` display styles. A default commodity (from `D` or the config)
is written into bare amounts as you accept them — visible in the preview and
editable like any other text (`↑` to go back), never attached behind your
back at write time. Without it, a bare `12.50` and the `-12.50 EUR`
balancing pre-fill would read as two different commodities and the
transaction could never balance.

## Configuration

`~/.config/rledger/config.toml`, overridden key-by-key by the nearest
`.rledger.toml` found walking up from the current directory. All keys are
optional; unknown keys are rejected so typos cannot silently disable anything.

```toml
format_file = true            # rewrite the whole file, formatted, on write
sort = false                  # also sort transactions by date on write
insertion = "append"          # or "chronological"
strict = false                # ask before using undeclared accounts/commodities
half_life_days = 90           # frecency decay half-life
account_matching = "substring"     # prefix | substring | segment | fuzzy
description_matching = "substring"
default_commodity = "EUR"     # written into bare amounts as visible, editable text
```

With `strict = true`, accounts and commodities are checked against the
declarations (`account` / `commodity` directives) visible at the insertion
point — the same set `hledger check accounts` / `check commodities` would
accept there — and anything undeclared asks for confirmation before being
used. The commodity check covers both the face amount and any second
commodity in a cost or assertion tail. Unitless amounts are always valid. `format_file = false` writes only
the new lines (rendered at the would-be file-wide widths) and warns when the
file's existing lines become stale; combining it with `sort = true` is
rejected, since sorting rewrites the whole file anyway.

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
