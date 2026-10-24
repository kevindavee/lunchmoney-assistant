//! The expense assistant as a library. `main.rs` only wires these modules
//! together; keeping them here lets tests and future binaries reuse them.

pub mod config;
pub mod db;
pub mod gmail;
pub mod ledger;
pub mod parsers;
pub mod telegram;
