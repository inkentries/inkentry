//! Reading a portable dump: the format `docs/dump-format.md` specifies.
//!
//! That document, not this module, is the contract. It is written for readers
//! and writers that are not this product, so where the two disagree the
//! document wins.

pub mod import;
pub mod reader;
pub mod record;

#[cfg(test)]
mod reader_tests;

pub use import::{ImportOutcome, ImportSummary, ImportTargets, apply};
pub use reader::{Dump, read};
