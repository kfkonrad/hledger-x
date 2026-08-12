# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/)

## [Unreleased]

### Added
- `payee` directive support, and descriptions are now split into `payee | note` as hledger splits them. Declared payees
  join the description completion pool; a payee that is neither declared nor used anywhere in the journal is accepted
  with a note and a "did you mean …?" — so entering `bahn` when `Deutsche Bahn` exists no longer quietly forks the
  payee. Under `strict`, an undeclared payee asks first, mirroring `hledger check payees`, which likewise tests only
  the payee half. Transaction templates and account ranking key on the payee too, so putting a distinct note on every
  entry no longer stops them matching
- Comments in `hledger-x add`, entered inline: a `;` in the description or an amount makes the rest of that field the
  comment, exactly as the journal line reads, so entering no comment costs no extra keystrokes. Past the `;`, `Tab`
  completes tag names from `tag` directives and from tags already used in the journal
- `Y` / `year` directive support: partial dates like `01-15` now resolve instead of skipping their transaction with a
  warning. Dates are always *written* in full `YYYY-MM-DD` form
- `apply account` and `alias` support, both directions: account names are resolved on read, so completion and the live
  preview show the name hledger sees rather than the remainder the file spells it with, and written back in the form the
  region at the insertion point requires. Where an account cannot be expressed there at all, `add` refuses and says
  which directive is responsible instead of silently entering a different account

### Fixed
- `insertion = "chronological"` could place a transaction inside an `apply account` region, and appending could land
  inside an unclosed one; both wrote fully-qualified names that hledger then read with the prefix applied twice

## [0.3.0]

### Added
- `fmt`: a new `--explicit` (`-x`) flag writes out what your journal leaves implied.
  - The amount a transaction omits is written into the file
  - Amounts are padded to the decimals their commodity declares (with `commodity 1_000.00 EUR`, `1 EUR` becomes
    `1.00 EUR`)
  - Bare amount are spelled out if a `D` directive is supplied (e.g. with `D 1,000.00 GBP`, `10` becomes `10.00 GBP`)
  - `--check --explicit` fails on a journal that still leaves an amount implied
  - `--check` succeeds on a journal formatted with `--explicit` (that is: `fmt --explicit` is more strict and regular
    `fmt` but still compatible)
  - Transactions it cannot work out on its own — two postings missing an amount, an amount it cannot read, a `~` or `=`
    rule, a posting whose account and amount are separated by a single tab — are left exactly as they are
  - It is a command-line flag only and cannot be set in the config file, since it rewrites amounts rather than just
    their layout

### Changed
- `fmt`: `--diff` now implied `--check` and will no longer change the files, only denote the changes `fmt` _would_ make
- `fmt`: the postings of `~` (periodic) and `=` (auto posting) rules are now indented and aligned like every other
  posting, instead of being left exactly as typed. Nothing else about them changes, so no amount in one is padded,
  restyled or filled in, and a `*2` multiplier is only lined up
- `add`: the default commodity now comes from `add.default_commodity` in the configuration only. It used to be taken
  from the journal's `D` directive when there was one, which meant a bare amount you typed could silently pick up a
  commodity from the file. Spelling `D` out is `fmt --explicit`'s job now

### Fixed
- `add`: a transaction with two or more `@`/`@@` conversions is no longer written in a form hledger rejects as
  unbalanced when using automatic equity conversion (`equity_conversion = true`)
  - Each conversion now gets its own pair of `equity:conversion` postings, written as a group next to each other after
    the postings that conversion covers
  - A `;` line separates groups
  - Transactions with a single conversion are unaffected.
  - A posting that funds more than one conversion is written last, after every group
- `commodity` directives are now properly scoped and only applied after they are declared
- `fmt`: a `D` directive now also settles which character is the decimal mark for commodities that declare no format of
  their own, matching hledger
- Several postings that used to be treated as unreadable are now understood: a cost or assertion written up against
  its operator (`@1.1EUR`, `=10USD`), a `=*` or `==*` assertion, and a space as the digit group mark (`1 234.00 USD`,
  including a `commodity 1 000.00 USD` declaring one). Such postings previously went unaligned and unrestyled, `add`
  reported the imbalance as unknown, and `fmt --explicit` declined to fill in the other amount
- A negative amount written the way hledger writes it, with the sign between the symbol and the digits (`$-10`), is no
  longer read as a separate commodity from `$10`. This made `add` report an imbalance on a transaction that balanced
  fine
- A posting's own status flag is no longer mistaken for part of its account name. `* (budget:food)` is again an
  unbalanced virtual posting rather than a real one — `fmt --explicit` was counting its amount into the wrong total and
  writing a balancing amount that left the transaction unbalanced — and `* assets:bank` is now recognised as
  `assets:bank`, so `add` completes it and stops calling it a new account
- A calculated balancing amount is now written the way the postings it balances are written. Entering `$10` gives you
  `$-10` rather than `-10 $`, `10€` gives `-10€`, `1 000` balances with `-1 000` and a typed `1,234.50 USD` keeps its
  digit grouping. A `commodity` directive, where you have one, still decides

## [0.2.0] - 2026-08-17

### Fixed
- Text inside a `comment` … `end comment` block is now left alone by both `add` and `fmt`, instead of being mistaken for
  real journal entries
- `add`: suggestions filled in from a past transaction now appear as grey ghost text. Typing replaces the suggestion
  instead of appending to it
- `add`: `Enter`, `Tab` and `→` all accept a suggestion at every prompt. `Enter` previously did not work on amounts
- `add`: a balancing amount calculated from a price no longer picks up extra trailing zeros — `10.00 EUR @ 1.105 USD`
  now balances with `-11.05 USD` instead of `-11.05000 USD`

### Changed
- `fmt`: blank lines are tidied up. Transactions are always separated by exactly one blank line, runs of blank lines
  collapse to one, and blank lines at the start and end of the file are removed. Groups of directives, prices and
  `include` lines stay packed together, and `comment` blocks are left untouched. A comment sitting directly above a
  transaction is treated as belonging to it, the same way `--sort` already keeps them together
- `add`: an amount you type is padded out to the number of decimals its commodity declares — with
  `commodity 1_000.00 EUR`, typing `4 EUR` writes `4.00 EUR`. Extra decimals you type yourself are never trimmed, and
  prices and balance assertions get the same treatment. Amounts already in your journal are still never reformatted
  by `fmt`

## [0.1.0] - 2026-08-12

Initial release.

### Added
- `hledger-x fmt` — a line-oriented journal formatter that passes directives through byte-for-byte, with `--diff`,
  in-place rewriting, and project-aware file discovery through `include` directives
- Amount restyling from declared commodity display styles (`commodity` and its `format` subdirective, `D`,
  `decimal-mark`), always value-preserving and always in exact decimals
- `hledger-x add` — interactive data entry with live preview, transaction templates from journal history, completion for
  accounts, payees and commodities, and smart-date resolution
- Dispatch as an hledger subcommand: with `hledger-x` on `PATH`, `hledger x fmt` and `hledger x add` work
- TOML configuration (`.hledger-x.toml`), including an `[add]` section for entry-specific settings

[Unreleased]: https://github.com/kfkonrad/hledger-x/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/kfkonrad/hledger-x/compare/v0.3.0...HEAD
[0.2.0]: https://github.com/kfkonrad/hledger-x/releases/tag/v0.2.0
[0.1.0]: https://github.com/kfkonrad/hledger-x/releases/tag/v0.1.0
