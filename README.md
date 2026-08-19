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

An opinionated formatter heavily inspired by [`hledger-fmt`](https://github.com/mikluko/hledger-fmt). The output format
should be similar to hledger print, except `hledger-x fmt` preserves directives and only changes transaction formatting.
`hledger-x fmt` fixes indentation as well as amount formatting.

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
reported and skipped.

<details><summary>Details about the formatting rules</summary>
Sorting is directive-bounded: directives and standalone comment blocks act as barriers, so transactions only reorder
within the runs between them. A comment directly above a transaction travels with it. `--sort` and `--no-sort` override
the configured `sort`.

Alignment is computed file-wide, so one long account name or number reflows every posting in the file — expect
occasional diffs much larger than the edit.

The postings of `~` (periodic) and `=` (auto posting) rules are indented and aligned with everything else, but nothing
more is done to them: a rule is not a transaction, so no amount in one is padded, restyled or filled in, and a `*2`
multiplier is only lined up. The rule's own header line passes through untouched.

A `comment` … `end comment` block is opaque. Its contents are prose, not journal syntax, so nothing in it is
reformatted, restyled, re-spaced or reordered — the lines pass through byte-for-byte — and nothing in it counts towards
the alignment columns. Declarations inside a block declare nothing, and an `include` inside one is not followed, which
is what hledger does too.

Blank lines are normalized. A run of them collapses to exactly one, leading and trailing ones go, and one is inserted
wherever a transaction directly abuts the block above or below it. Consecutive directives, `P` price lines and
`include`s stay dense — only boundaries involving a transaction gain a blank line. Comments attach downward: a comment
block directly above a transaction heads it, so the blank line goes above the comment, and a comment written directly
*below* a transaction's last posting is pushed away from it by one blank line. This is the same attachment `--sort`
uses, so a comment that heads a transaction travels with it.

`fmt` never changes an amount's precision — that belongs to whoever wrote it, and altering it would change what
`hledger print` emits. (`add` does fill an amount you type out to the commodity's declared decimal places; see below,
and `--explicit` opts `fmt` into the same thing.)

A `commodity`, `D` or `decimal-mark` directive governs the amounts written **below** it, and only those — including
across `include` lines, which take effect where they stand. hledger reads a journal top to bottom, so a style declared
further down is not yet known when it reaches an amount, and `1,234 GBP` written above `commodity 1,000.00 GBP` means
1.234 rather than 1234. Amounts above their commodity's declaration are therefore passed through exactly as written,
by `fmt` and `--explicit` alike. Keeping your declarations at the top of the journal, or in a file you `include`
first, is what most journals do already and is the layout everything is formatted against.
</details>

#### `--explicit`

`--explicit` writes out what the journal leaves implied. It does two things:

- **Fills in the amount you left off.** A transaction may omit one posting's amount and let hledger work it out;
  `--explicit` writes that amount into the file, exactly as `hledger print -x` would show it.

  ```txt
  2026-01-01 groceries        2026-01-01 groceries
      expenses:food  10 EUR  →     expenses:food   10 EUR
      assets:cash                  assets:cash    -10 EUR
  ```

- **Pads amounts to their commodity's declared decimal places.** With `commodity 1_000.00 EUR`, `1 EUR` becomes
  `1.00 EUR` — the same rule `add` applies to an amount you type. The declared places are a minimum, never a maximum,
  so `4.001 EUR` is left alone, and a commodity with no `commodity` directive is left alone entirely.

<details><summary>Details about explicit formatting rules</summary>
A `D` directive gives amounts written without a commodity that commodity, so `--explicit` writes it out: under
`D 1,000.00 GBP`, a bare `10` becomes `10.00 GBP`. `add` does not do this — its default commodity comes from
`add.default_commodity` in the configuration alone, so an amount you type never quietly picks one up from the
journal.

When a commodity has no `commodity` directive, padding simply does not apply — `1 USD` stays `1 USD`. An amount that
has to be *generated*, though, still has to be written somehow, and its style is taken from the postings it balances
against: `$10` balances with `$-10`, `10€` with `-10€`, and a typed `1,234.50 USD` keeps its digit grouping. A declared
style always wins over what the neighbours happen to look like.

`--check` honours `--explicit`: `hledger-x fmt --check --explicit` fails on a journal that still leaves an amount
implied, which is what you want in a pre-commit hook or CI.

Unlike `sort`, `--explicit` is a flag only and cannot be set in the configuration file. It rewrites amounts rather than
just their layout, so every run that does it says so on the command line.

What it deliberately does *not* do:

- It never infers a conversion cost. `hledger print -x` turns `10 EUR` / `-11 USD` into `10 EUR @@ 11 USD`; that is a
  claim about the transaction rather than a value already implied by it, so `fmt` leaves it out.
- Lot notation (`10 AAPL {$5}`) is not read, so a transaction using one is passed through rather than balanced from
  the lot cost.
- It never guesses. A transaction with two amount-less postings, an amount that does not parse, or a periodic (`~`) or
  auto (`=`) rule is passed through untouched — `fmt` is not a validator, and a wrong amount would be far worse than
  no amount.
- A posting whose account and amount are separated by a *single tab* is passed through with its transaction. hledger
  itself has changed its mind here: releases read the tab as part of the account name, so the posting has no amount at
  all, while an unreleased development build reads it as a separator. Whichever amount was filled in would be wrong
  under the other reading, so none is. Two spaces, or a space and a tab, are unambiguous everywhere.

When a commodity has no `commodity` directive, padding simply does not apply, and an amount that has to be generated
copies the notation of the posting it balances — including a unitless one, so `1 000` balances with `-1 000`.

`tests/golden/explicit.in.ledger` is a reference journal with one annotated transaction per case, if you want to see
all of this at once — including what is deliberately left alone. It is valid hledger, so
`hledger print -x -f tests/golden/explicit.in.ledger` shows the comparison directly.

Real, `[balanced virtual]` and `(unbalanced virtual)` postings balance separately, the way hledger balances them: an
unbalanced virtual posting contributes to nothing and never receives an inferred amount. When the remainder spans
several commodities the inferred posting is split into one posting per commodity, again matching `hledger print -x`.
</details>

#### Exit codes

| Code | Meaning                                         |
|------|-------------------------------------------------|
| 0    | success                                         |
| 1    | `fmt --check` found files that need formatting  |
| 2    | the invocation or the configuration was invalid |
| 3    | a file could not be read, written or walked     |

A `fmt` run that both finds unformatted files and hits an error reports the error. Unreadable files are reported and
skipped rather than aborting the run, so a single bad file still yields exit 3 with every other file formatted.

### add

An `hledger add` alternative that shows the transaction as you enter it and completes from your journal's history.

```sh
hledger-x add                                              # add to the configured ledger_file (or $LEDGER_FILE)
hledger-x add -f 2025/main.journal                         # use -f to pass a journal file explicitly
hledger-x add --to 2025/inbox.journal                      # add transactions into another file of the include tree
hledger-x add -f 2025/main.journal --to 2025/inbox.journal # -f and --to can be combined
```

`add` reads the journal from `-f`/`--file`, the `ledger_file` configuration or `$LEDGER_FILE` (in that order of
precedence) and all included files to provide completions for descriptions, commodities and accounts as well as format
amounts correctly when `commodity`/`decimal-mark`/`D` directives specify formatting.

Note that for `hledger-x add` the `-f` flag may only be used once while `hledger-x fmt` supports it multiple times.

#### Completion

`hledger-x add` offers completions and pre-fills transactions from the most recent transaction of the same descriptions.
This works like so:

- Date: The current date is suggested as a greyed out text. Press enter to accept or enter a partial date where
  `hledger-x` will fill in the current year or month if left out.
- Description: Use `Tab` to get completions for descriptions. This determines which accounts and amounts will be
  suggested later.
- Account: Pre-filled with the account of the previous transaction with that description. See completions subsection for
  how to configure completion behavior. Entering an empty account (no account) closes the transaction.
- Amount: pre-filled but greyed out, press `→` or `Tab` to accept or type an amount to override. If the amount is left
  empty `hledger-x` will calculate the balance and close the transaction.

An amount you enter is filled out to the decimal places its commodity declares: with `commodity 1_000.00 EUR`, both
`4 EUR` and `4.0 EUR` are written as `4.00 EUR` — the same form the calculated balancing amount takes. The declared
places are a minimum, never a maximum, so `4.000 EUR` and `4.001 EUR` are written exactly as typed; hledger accepts
more precision than a commodity declares, and rounding would lose value. Commodities with no `commodity` directive are
left alone entirely.

A price or balance assertion is filled out the same way, against its own commodity's declared style, so
`10 EUR @ 1.1 USD` is written as `10.00 EUR @ 1.10 USD`.

#### Navigating `hledger-x add`

| Key                                                                | Action                                                           |
|--------------------------------------------------------------------|------------------------------------------------------------------|
| `Enter`                                                            | accept the field as typed                                        |
| `↑` / `↓`                                                          | move between fields                                              |
| `Tab` / `Shift-Tab`                                                | complete, then open and cycle the menu                           |
| `Tab` / `→`                                                        | pick up the grey suggestion                                      |
| `Ctrl-U`                                                           | clear the buffer                                                 |
| `Ctrl-W`                                                           | delete one word, or one `:`-segment on accounts                  |
| `Ctrl-E`                                                           | open the current transaction draft in `$VISUAL`/`$EDITOR`        |
| `Ctrl-C`                                                           | abort the current transaction                                    |
| `Ctrl-D`, or `q`+`Enter` at the date prompt                        | quit                                                             |
| `u`+`Enter` at the date prompt                                     | undo the last completed transaction                              |
| `y`/`n` at the strict mode's account/commodity confirmation prompt | accept/don't accept new account/commodity when using strict mode |

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
strict = false                       # ask before using new accounts/commodities
account_completion = "substring"     # prefix | substring | fuzzy
description_completion = "substring" # prefix | substring | fuzzy
# default_commodity = ""             # fill in this commodity (e.g. USD, EUR, $, ¥) on bare amounts
equity_conversion = false            # add equity postings for multi-commodity transactions
equity_conversion_account = "equity:conversion"
```

`ledger_file` and `add.default_commodity` have no default. If `ledger_file` is unset, `-f main.journal` has to be passed
or `$LEDGER_FILE` needs to be set.

`format_file = false` writes only the new lines and warns when the file's existing lines become stale; combining it with
`sort = true` is rejected.

Note that `substring` and `fuzzy` account completions don't complete cross `:`-boundaries. That means, `ac` doesn't
complete to `assets:cash` but `a:c` does.

With `equity_conversion = true`, each `@`/`@@` cost gets a pair of postings to `equity_conversion_account` cancelling
it, written after the postings that conversion covers:

```txt
2001-01-01 Example
    expenses:foo            10 USD @@ 9.06 EUR
    assets:cash          -9.06 EUR
    equity:conversion      -10 USD
    equity:conversion     9.06 EUR
```

A transaction with several conversions is written as one group per conversion, separated by a `;` line. A posting joins
the conversion whose commodities include its own, so the groups read in order however you typed the postings:

```txt
2001-01-01 Example
    assets:dollars       $-135
    assets:euros          €100 @ $1.35
    equity:conversion    €-100
    equity:conversion     $135
    ;
    assets:yen           ¥-100
    assets:euros            €1 @@ ¥100
    equity:conversion      €-1
    equity:conversion     ¥100
```

The pairing matters: hledger only recognises equity postings as cancelling a cost when the two sit next to each other,
so a single summed posting per commodity would make the transaction unbalanced. A posting that funds more than one
conversion belongs to none of them and is written last, after every group.

## Maintainers

[@kfkonrad](https://github.com/kfkonrad)

## Contributing

PRs accepted.

Small note: If editing the README, please conform to the
[standard-readme](https://github.com/RichardLitt/standard-readme) specification.

## License

MIT © 2026 Kevin F. Konrad
