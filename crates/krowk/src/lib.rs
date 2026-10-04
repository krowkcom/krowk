//! krowk: a coding agent harness, whose lean build only publishes agent
//! output to permalinks. `cli::run` is the whole entry point,
//! taking its streams and environment as arguments so tests never touch the
//! process.

pub mod cli;
pub mod config;
pub mod mcp;
pub mod output;
#[cfg(feature = "sessions")]
pub mod pricing;
pub mod runctx;
pub mod termclean;
