# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/)

## [Unreleased]

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
