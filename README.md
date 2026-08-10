# rledger

[![standard-readme compliant](https://img.shields.io/badge/standard--readme-OK-green.svg?style=flat-square)](https://github.com/RichardLitt/standard-readme)

Format hledger journals and enter transactions ergonomically

`rledger` is a CLI for plain text accounting journals with two subcommands:

- `rledger fmt` — a format-preserving journal formatter, a drop-in equivalent to
  [`hledger-fmt`](https://github.com/mikluko/hledger-fmt)
- `rledger add` — ergonomic interactive data entry, a better `hledger add`
  (**not implemented yet**)

The formatter is line-oriented and never builds a semantic model. Directives,
comments, `include` lines and `P` price lines pass through byte-for-byte, so
price-only and include-only files are safe by construction. Only posting lines
are reflowed.

## Table of Contents

- [Status](#status)
- [Install](#install)
- [Usage](#usage)
- [Formatting rules](#formatting-rules)
- [Testing](#testing)
- [Maintainers](#maintainers)
- [Contributing](#contributing)
- [License](#license)

## Status

`rledger fmt` is complete and verified against the reference implementation's
golden fixtures. `rledger add` is designed but not yet built; see `DESIGN.md`
for what it will do and `IMPLEMENTATION.md` for how.

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
