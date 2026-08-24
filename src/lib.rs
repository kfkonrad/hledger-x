//! hledger-x — plain text accounting tooling.
//!
//! `fmt` is line-oriented and builds no semantic model; `add` is built on top
//! of it. The dependency direction is one-way: `fmt` must never learn what a
//! directive means.

pub mod add;
pub mod amount;
pub mod config;
pub mod errors;
pub mod fmt;
pub mod lex;
pub mod status;
