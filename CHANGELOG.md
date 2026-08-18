# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/)

## [Unreleased]

### Added
- `fmt`: a new `--explicit` (`-x`) flag writes out what your journal leaves implied. Where a transaction omits one
  posting's amount and lets hledger work it out, that amount is written into the file; and amounts are padded to the
  decimals their commodity declares, so with `commodity 1_000.00 EUR` a written `1 EUR` becomes `1.00 EUR`.
  `--check --explicit` fails on a journal that still leaves an amount implied. `--explicit` is a command-line flag only
  and cannot be set in the config file, since it rewrites amounts rather than just their layout. Transactions it cannot
  work out on its own — two postings missing an amount, an amount it cannot read, a `~` or `=` rule — are left exactly
  as they are

### Fixed
- A space used as the digit group mark (`1 234.00 USD`) is now understood, and a `commodity 1 000.00 USD` directive
  declaring one is honoured. Such amounts used to be misaligned — the number column treated `1 234.00 USD` as the
  number `1` — and were never restyled or padded
- A calculated amount with no commodity now keeps the grouping it was written with: `1 000` balances with `-1 000` and
  `1,000` with `-1,000`, rather than `-1000` and `-1.000`
- `fmt`/`add`: a cost written with its amount attached to the operator (`@1.1EUR`) is now understood. Before, a posting
  using one was treated as unreadable
- Balance assertions written `=*` or `==*`, and a cost or assertion with its amount written right up against the
  operator (`@1.1EUR`, `=10USD`), are now understood. Before, a posting using any of these forms was treated as
  unreadable: `add` reported the imbalance as unknown, and `fmt --explicit` quietly declined to fill in the other
  posting's amount
- A negative amount written the way hledger writes it, with the sign between the symbol and the digits (`$-10`), is no
  longer read as a separate commodity from `$10`. This made `add` report an imbalance on a transaction that balanced
  fine
- `add`: a calculated balancing amount is now written the way the postings it balances are written. Entering `$10`
  gives you `$-10` rather than `-10 $`, `10€` gives `-10€`, and a typed `1,234.50 USD` keeps its digit grouping. A
  `commodity` directive, where you have one, still decides

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

[Unreleased]: https://github.com/kfkonrad/hledger-x/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/kfkonrad/hledger-x/releases/tag/v0.1.0
