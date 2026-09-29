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

/// Which relay environment a device's joins say they come from (relay.md →
/// Joining, `env`): "development" for a debug build, for `KROWK_ENV=development`,
/// or for any registry but the production one, else "production". There is
/// one hosted relay, and it keeps the two apart — a channel is named by the
/// env and the session — so a developer's session never meets, buffers
/// beside or is counted with a user's.
pub fn relay_env(base_url: &str, env: creds::Env) -> &'static str {
    relay_env_for(cfg!(debug_assertions), &env("KROWK_ENV"), base_url)
}

fn relay_env_for(debug: bool, krowk_env: &str, base_url: &str) -> &'static str {
    let production_registry = base_url.trim_end_matches('/') == DEFAULT_BASE_URL;
    if debug || krowk_env.trim().eq_ignore_ascii_case("development") || !production_registry {
        "development"
    } else {
        "production"
    }
}

/// The spellings people type into an environment variable.
pub fn truthy(v: &str) -> bool {
    matches!(v.trim().to_lowercase().as_str(), "1" | "true" | "yes" | "on")
}

#[cfg(test)]
mod relay_env_tests {
    use super::*;

    /// R-RELAY-1: a release build on the production registry says
    /// production; a debug build, KROWK_ENV=development or any other
    /// registry says development.
    #[test]
    fn r_relay_1_only_a_release_build_on_the_production_registry_joins_as_production() {
        assert_eq!(relay_env_for(false, "", DEFAULT_BASE_URL), "production");
        assert_eq!(relay_env_for(false, "production", "https://api.krowk.com/v1/"), "production");
        assert_eq!(relay_env_for(true, "", DEFAULT_BASE_URL), "development");
        assert_eq!(relay_env_for(false, "development", DEFAULT_BASE_URL), "development");
        assert_eq!(relay_env_for(false, " Development ", DEFAULT_BASE_URL), "development");
        assert_eq!(relay_env_for(false, "", DEV_BASE_URL), "development");
        assert_eq!(relay_env_for(false, "", "https://api.krowk.com.evil.example/v1"), "development");
        let none = |_: &str| String::new();
        assert_eq!(relay_env(DEFAULT_BASE_URL, &none), if cfg!(debug_assertions) { "development" } else { "production" });
    }
}
