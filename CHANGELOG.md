# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/)

## [Unreleased]

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
