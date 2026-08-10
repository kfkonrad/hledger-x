//! rledger — plain text accounting tooling.
//!
//! `fmt` is line-oriented and builds no semantic model; `add` (epic 2) will be
//! built on top of it. The dependency direction is one-way: `fmt` must never
//! learn what a directive means.

pub mod fmt;
pub mod lex;
