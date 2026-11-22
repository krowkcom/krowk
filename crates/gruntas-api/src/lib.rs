//! The gruntas registry's wire: its records, its failure shape, and the local
//! credentials a call is made with. The contract is owned by canon and
//! served by the registry; this is one of its three implementations.

pub mod client;
pub mod creds;
pub mod error;
pub mod git;
pub mod home;
pub mod login;
pub mod migrate;
pub mod slug;
pub mod spec;
pub mod sync;
pub mod tempfile;
pub mod types;

pub use client::Client;
pub use error::{fail, private_needs_key, Error};
pub use types::*;

/// The production registries, each with the hosted relay on its domain:
/// gruntas.com first, then api./relay.krowk.com, which CLIs released before
/// the rename default to and which serve in parallel until at least
/// 2027-01-20. Either counts as
/// production wherever that is decided.
const PRODUCTION: [(&str, &str); 2] = [("https://api.gruntas.com/v1", "wss://relay.gruntas.com"), ("https://api.krowk.com/v1", "wss://relay.krowk.com")];

/// Overridden by GRUNTAS_API_URL.
pub const DEFAULT_BASE_URL: &str = PRODUCTION[0].0;

/// The hosted relay beside a production registry, matched exactly (a
/// trailing slash aside); None for any other registry.
pub fn hosted_relay(base_url: &str) -> Option<&'static str> {
    let url = base_url.trim_end_matches('/');
    PRODUCTION.iter().find(|(api, _)| *api == url).map(|(_, relay)| *relay)
}
/// Where the local stand-in registry listens, and what --dev points at.
pub const DEV_BASE_URL: &str = "http://localhost:8787/v1";

// Historical: the registry mints its tokens with these prefixes, so they keep
// the old name (the gruntas crate's `historical_names` test pins them).
/// What every API key starts with.
pub const API_KEY_PREFIX: &str = "krowk_sk_";
/// What every claim token, handed back by an anonymous upload, starts with.
pub const CLAIM_TOKEN_PREFIX: &str = "krowk_claim_";

/// Which registry to talk to: --dev, then GRUNTAS_API_URL, then GRUNTAS_DEV.
pub fn base_url_for(dev: bool, env: creds::Env) -> String {
    let url = env("GRUNTAS_API_URL");
    if dev {
        DEV_BASE_URL.into()
    } else if !url.is_empty() {
        url
    } else if truthy(&env("GRUNTAS_DEV")) {
        DEV_BASE_URL.into()
    } else {
        DEFAULT_BASE_URL.into()
    }
}

/// Which relay environment a device's joins say they come from (relay.md →
/// Joining, `env`): "development" for a debug build, for `GRUNTAS_ENV=development`,
/// or for any registry but a production one, else "production". The hosted
/// relay keeps the two apart — a channel is named by the
/// env and the session — so a developer's session never meets, buffers
/// beside or is counted with a user's.
pub fn relay_env(base_url: &str, env: creds::Env) -> &'static str {
    relay_env_for(cfg!(debug_assertions), &env("GRUNTAS_ENV"), base_url)
}

fn relay_env_for(debug: bool, env_name: &str, base_url: &str) -> &'static str {
    let production_registry = hosted_relay(base_url).is_some();
    if debug || env_name.trim().eq_ignore_ascii_case("development") || !production_registry {
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
    /// production; a debug build, GRUNTAS_ENV=development or any other
    /// registry says development.
    #[test]
    fn r_relay_1_only_a_release_build_on_the_production_registry_joins_as_production() {
        assert_eq!(relay_env_for(false, "", DEFAULT_BASE_URL), "production");
        assert_eq!(relay_env_for(false, "production", "https://api.gruntas.com/v1/"), "production");
        assert_eq!(relay_env_for(false, "", "https://api.krowk.com/v1"), "production");
        assert_eq!(relay_env_for(true, "", DEFAULT_BASE_URL), "development");
        assert_eq!(relay_env_for(false, "development", DEFAULT_BASE_URL), "development");
        assert_eq!(relay_env_for(false, " Development ", DEFAULT_BASE_URL), "development");
        assert_eq!(relay_env_for(false, "", DEV_BASE_URL), "development");
        assert_eq!(relay_env_for(false, "", "https://api.gruntas.com.evil.example/v1"), "development");
        assert_eq!(relay_env_for(false, "", "https://api.krowk.com.evil.example/v1"), "development");
        let none = |_: &str| String::new();
        assert_eq!(relay_env(DEFAULT_BASE_URL, &none), if cfg!(debug_assertions) { "development" } else { "production" });
    }

    /// D7: the gruntas registry is the default, and each production
    /// registry gets the hosted relay on its own domain.
    #[test]
    fn each_production_registry_has_the_hosted_relay_on_its_domain() {
        assert_eq!(DEFAULT_BASE_URL, "https://api.gruntas.com/v1");
        assert_eq!(hosted_relay("https://api.gruntas.com/v1/"), Some("wss://relay.gruntas.com"));
        assert_eq!(hosted_relay("https://api.krowk.com/v1"), Some("wss://relay.krowk.com"));
        assert_eq!(hosted_relay(DEV_BASE_URL), None);
        assert_eq!(hosted_relay("https://api.gruntas.com.evil.example/v1"), None);
    }
}
