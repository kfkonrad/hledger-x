//! `rledger add` — interactive data entry (epic 2).
//!
//! Built on top of `fmt` and the shared lexical layer; the dependency
//! direction is one-way. Everything except `ui` is terminal-free and
//! unit-testable headlessly.

pub mod amount;
pub mod index;
pub mod parser;
pub mod ui;
pub mod write;
