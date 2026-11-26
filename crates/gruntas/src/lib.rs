//! gruntas: orchestration for autonomous agents, whose lean build only
//! publishes evidence of agents' work to permalinks. `cli::run` is the whole entry point,
//! taking its streams and environment as arguments so tests never touch the
//! process.

pub mod cli;
pub mod config;
pub mod mcp;
#[cfg(feature = "harness")]
pub mod osc7501;
pub mod output;
#[cfg(feature = "sessions")]
pub mod pricing;
pub mod runctx;
#[cfg(feature = "harness")]
pub mod settings;
pub mod termclean;

#[cfg(feature = "harness")]
/// `~/…` for a directory under the home directory.
pub fn home_relative(dir: &std::path::Path) -> String {
    match std::env::var_os("HOME").map(std::path::PathBuf::from) {
        Some(home) if !home.as_os_str().is_empty() && dir.starts_with(&home) => match dir.strip_prefix(&home) {
            Ok(rest) if rest.as_os_str().is_empty() => "~".into(),
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => dir.display().to_string(),
        },
        _ => dir.display().to_string(),
    }
}
