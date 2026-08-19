# hledger-x

[![standard-readme compliant](https://img.shields.io/badge/standard--readme-OK-green.svg?style=flat-square)](https://github.com/RichardLitt/standard-readme)

Format hledger journals and enter transactions ergonomically

`hledger-x` has two subcommands:

- `hledger-x fmt` — a journal formatter that can also sort transactions by date
- `hledger-x add` — interactive data entry, a better `hledger add`

## Table of Contents

- [Install](#install)
- [Usage](#usage)
  - [fmt](#fmt)
  - [add](#add)
- [Configuration](#configuration)
- [Maintainers](#maintainers)
- [Contributing](#contributing)
- [License](#license)

## Install

Download a binary from the [releases section of this repo](https://github.com/kfkonrad/hledger-x/releases), or build
from source:

```sh
cargo install --path .
```

With `hledger-x` on your `PATH`, hledger dispatches to it too, so e.g. `hledger x fmt` and `hledger-x fmt` are
interchangeable.

## Usage

### fmt

An opinionated formatter heavily inspired by [`hledger-fmt`](https://github.com/mikluko/hledger-fmt). It fixes
indentation and amount formatting, leaving directives untouched, so the result reads much like `hledger print`.

```sh
hledger-x fmt                            # the configured ledger_file (or $LEDGER_FILE) and its includes
hledger-x fmt -f 2025/main.journal       # another journal, with its includes
hledger-x fmt main.journal 2025.journal  # exactly these files, includes not followed
hledger-x fmt - < main.journal           # stdin to stdout
hledger-x fmt --check                    # write nothing, exit non-zero if anything needs formatting
hledger-x fmt --diff                     # show a unified diff of the changes; writes nothing
hledger-x fmt --sort                     # sort transactions by date in addition to formatting them
hledger-x fmt --explicit                 # write out inferred amounts and pad decimals
hledger-x fmt --quiet                    # do not print a list of changed files
```

`-f`/`--follow` is repeatable and can be combined with plain, non-following file arguments. Unreadable files are
reported and skipped, so one bad file does not abort the run.

Sorting is directive-bounded: directives and standalone comment blocks act as barriers, so transactions only reorder
within the runs between them, and a comment directly above a transaction travels with it. An amount's precision is
never changed — that belongs to whoever wrote it. A `commodity`, `D` or `decimal-mark` directive governs the amounts
*below* it and only those, so keep your declarations at the top of the journal or in a file you `include` first.

#### `--explicit`

`--explicit` writes out what the journal leaves implied — the amount you left off, exactly as `hledger print -x`
would show it:

```txt
2026-01-01 groceries        2026-01-01 groceries
    expenses:food  10 EUR  →     expenses:food   10 EUR
    assets:cash                  assets:cash    -10 EUR
```

It also pads amounts to their commodity's declared decimal places, the same rule `add` applies to an amount you type:
under `commodity 1_000.00 EUR`, `1 EUR` becomes `1.00 EUR`. The declared places are a minimum, so `4.001 EUR` is left
alone, as is a commodity with no `commodity` directive. It never guesses — no conversion cost is inferred, and a
transaction it cannot work out with certainty is passed through untouched. `--check` honours `--explicit`.

### add

An `hledger add` alternative that shows the transaction as you enter it and completes from your journal's history.

```sh
hledger-x add                                              # add to the configured ledger_file (or $LEDGER_FILE)
hledger-x add -f 2025/main.journal                         # use -f to pass a journal file explicitly
hledger-x add --to 2025/inbox.journal                      # add transactions into another file of the include tree
hledger-x add -f 2025/main.journal --to 2025/inbox.journal # -f and --to can be combined
```

`add` reads the journal from `-f`/`--file`, the `ledger_file` configuration or `$LEDGER_FILE` (in that order of
precedence) and all included files, and honours the directives that change what a transaction means — `account`,
`commodity`/`D`/`decimal-mark`, `payee`, `tag`, `Y`, `apply account`, `alias` and `include`. Unlike `fmt`, `add`
accepts `-f` only once.

#### Completion

`hledger-x add` walks you through date, description, account and amount, showing the transaction as it grows. Each
field offers a greyed-out suggestion you can accept or type over: today's date (or a partial date, with the year and
month filled in), completions for descriptions and declared `payee`s on `Tab`, and the account and amount of the last
transaction with that payee. An empty account closes the transaction, as does accepting the calculated balancing
amount on the last posting. A payee that is new to your journal is accepted with a note and a "did you mean …?", so
typing `bahn` when you meant `Deutsche Bahn` does not quietly fork it.

An amount you enter is filled out to the decimal places its commodity declares — with `commodity 1_000.00 EUR`, typing
`4 EUR` writes `4.00 EUR` — as are prices and balance assertions.

hledger splits a description at the first `|` into payee and note; `hledger-x` uses the payee half wherever a
description acts as an identity (the new-payee note, `strict`, the template, account ranking) and writes the whole
description to the file.

#### Comments and tags

Comments are not a separate prompt. Type a `;` anywhere in the description or an amount and the rest of the field is
the comment, exactly as the journal line reads — so entering no comment, which is the usual case, costs nothing:

```txt
description> Rewe ; trip: berlin
amount 1>     18.20 EUR ; receipt: yes
```

writes

```txt
2026-08-12 Rewe  ; trip: berlin
    expenses:groceries    18.20 EUR  ; receipt: yes
    assets:bank:checking -18.20 EUR
```

Past the `;`, `Tab` completes tag names — from `tag` directives and from tags already used in your journal. Comments
are never pre-filled from the previous transaction.

#### Navigating `hledger-x add`

| Key                                              | Action                                                           |
|--------------------------------------------------|------------------------------------------------------------------|
| `Enter`                                          | accept the field as typed, or the grey suggestion if it is empty |
| `↑` / `↓`                                        | move between fields                                              |
| `Tab` / `Shift-Tab`                              | complete, then open and cycle the menu                           |
| `Tab` / `→`                                      | pick up the grey suggestion to edit it                           |
| `Ctrl-U`                                         | clear the buffer, or dismiss an account suggestion               |
| `Ctrl-W`                                         | delete one word, or one `:`-segment on accounts                  |
| `Ctrl-E`                                         | open the current transaction draft in `$VISUAL`/`$EDITOR`        |
| `Ctrl-C`                                         | abort the current transaction                                    |
| `Ctrl-D`, or `q`+`Enter` at the date prompt      | quit                                                             |
| `u`+`Enter` at the date prompt                   | undo the last completed transaction                              |
| `y`/`n` at a strict-mode confirmation prompt     | accept/don't accept an new payee/account/commodity               |

#### Recovery

Every transaction you finish is appended to a recovery journal under `$XDG_STATE_HOME/hledger-x/` (one file per write
target) before the session ends, and the whole batch is written to the journal in one go when you quit. If the session
dies first — a crash, a closed terminal, a failed write — nothing is lost: the next `hledger-x add` against the same
target replays the pending transactions into the new session and tells you it did. `u` (undo) rewrites the recovery
journal too, so an undone transaction does not come back. The file is removed once the batch reaches the journal.

## Configuration

`hledger-x` reads its config from `~/.config/hledger-x/config.toml`, overridden key-by-key by the nearest
`.hledger-x.toml` found walking up from the current directory.

```toml
# ledger_file = ""                   # default journal path
sort = false                         # sort transactions by date

[add]
format_file = true                   # rewrite the whole file, formatted, on write
insertion = "append"                 # or "chronological"
strict = false                       # or true, or a list: ["accounts", "commodities", "payees"]
account_completion = "substring"     # prefix | substring | fuzzy
description_completion = "substring" # prefix | substring | fuzzy
# default_commodity = ""             # fill in this commodity (e.g. USD, EUR, $, ¥) on bare amounts
equity_conversion = false            # add equity postings for multi-commodity transactions
equity_conversion_account = "equity:conversion"
```

- `ledger_file` and `add.default_commodity` have no default. If `ledger_file` is unset, `-f main.journal` has to be
  passed or `$LEDGER_FILE` needs to be set
- `strict` is a list of checks to opt into. Support `accounts`, `commodities` and `payees`. You can also set this to
  `true` to opt into all checks or `false` to opt into none
- `format_file = false` writes only the new lines and warns when the file's existing lines become stale; combining it
  with `sort = true` is rejected
- `substring` and `fuzzy` account completions don't complete cross `:`-boundaries. That means, `ac` doesn't complete to
  `assets:cash` but `a:c` does
- With `equity_conversion = true`, each `@`/`@@` cost gets a pair of postings to `equity_conversion_account` cancelling
  it, written after the postings that conversion covers:

  ```txt
  2001-01-01 Example
      expenses:foo            10 USD @@ 9.06 EUR
      assets:cash          -9.06 EUR
      equity:conversion      -10 USD
      equity:conversion     9.06 EUR
  ```

  A transaction with several conversions is written as one group per conversion, separated by a `;` line

## Maintainers

[@kfkonrad](https://github.com/kfkonrad)

## Contributing

PRs accepted.

Small note: If editing the README, please conform to the
[standard-readme](https://github.com/RichardLitt/standard-readme) specification.

## License

MIT © 2026 Kevin F. Konrad
