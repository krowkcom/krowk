//! The krowk registry's wire: its records, its failure shape, and the local
//! credentials a call is made with. The contract is owned by krowk-canon and
//! served by krowk-registry; this is one of its three implementations.

pub mod client;
pub mod creds;
pub mod error;
pub mod home;
pub mod migrate;
pub mod slug;
pub mod spec;
pub mod sync;
pub mod tempfile;
pub mod types;

pub use client::Client;
pub use error::{fail, private_needs_key, Error};
pub use types::*;

/// Overridden by KROWK_API_URL.
pub const DEFAULT_BASE_URL: &str = "https://api.krowk.com/v1";
/// Where the local stand-in registry listens, and what --dev points at.
pub const DEV_BASE_URL: &str = "http://localhost:8787/v1";

/// Which registry to talk to: --dev, then KROWK_API_URL, then KROWK_DEV.
pub fn base_url_for(dev: bool, env: creds::Env) -> String {
    let url = env("KROWK_API_URL");
    if dev {
        DEV_BASE_URL.into()
    } else if !url.is_empty() {
        url
    } else if truthy(&env("KROWK_DEV")) {
        DEV_BASE_URL.into()
    } else {
        DEFAULT_BASE_URL.into()
    }
}

/// The spellings people type into an environment variable.
pub fn truthy(v: &str) -> bool {
    matches!(v.trim().to_lowercase().as_str(), "1" | "true" | "yes" | "on")
}
