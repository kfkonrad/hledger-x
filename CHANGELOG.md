# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/)

## [Unreleased]

### Fixed
- `add`: an account pre-filled from a transaction template is shown as grey ghost text instead of being written into the
  buffer, so typing replaces it rather than appending to it
- `add`: `Enter`, `Tab` and `→` all accept a ghost suggestion at every prompt. Previously `Enter` accepted the date and
  description ghosts but not the amount ghost
- `add`: a calculated balancing amount no longer carries trailing zeros left over from the arithmetic that produced it —
  `10.00 EUR @ 1.105 USD` balances with `-11.05 USD`, not `-11.05000 USD`

### Changed
- `add`: an amount you enter is filled out to its commodity's declared decimal places — with `commodity 1_000.00 EUR`,
  `4 EUR` is written as `4.00 EUR`. The declared places are a floor, never a ceiling, so `4.001 EUR` is kept as typed.
  Applies to the face amount, an `@`/`@@` price and a `=` assertion alike, each against its own commodity. `fmt` still
  never touches the precision of text already in the journal

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
