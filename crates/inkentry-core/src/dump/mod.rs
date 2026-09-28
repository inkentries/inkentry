//! Reading a portable dump in the documented interchange format, written for
//! readers and writers that are not this product.

pub mod import;
pub mod reader;
pub mod record;

#[cfg(test)]
mod reader_tests;

pub use import::{ImportOutcome, ImportSummary, ImportTargets, apply};
pub use reader::{Dump, read};
