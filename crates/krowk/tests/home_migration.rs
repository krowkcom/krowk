//! The one move from an older krowk's XDG layout into `~/.krowk`, the built
//! binary against a home laid out the way the last release (and a dev
//! build) left it: config.json, the registry key, the provider file with a
//! SuperGrok login and a stored key, a named Claude account (the fake
//! `claude`, no real login), krowk.db, a session and the models.dev cache.
//! The config and the keys move and work, the old secrets are deleted,
//! and what is not moved is named once with what to do; a move a crash
//! cut short after its rename is finished; a home that exists takes
//! nothing; a KROWK_HOME never takes the person's own files; and a
//! malformed old key file stops the move with its line and column only.

#![cfg(all(feature = "harness", unix))]

use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const REGISTRY_KEY: &str = "krowk_sk_MIGRATE-SENTINEL-51";
const STORED_KEY: &str = "sk-ant-MIGRATE-SENTINEL-8c2e";
const ACCESS: &str = "xai-at-MIGRATE-SENTINEL-d04";
const REFRESH: &str = "xai-rt-MIGRATE-SENTINEL-77f";

struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Sandbox {
        let root = std::env::temp_dir().join(format!("krowk-migrate-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["home", "repo", "bin"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        let fake = Path::new(env!("CARGO_MANIFEST_DIR")).join("../krowk-harness/tests/fixtures/claude/fake-claude");
        std::fs::copy(fake, root.join("bin/claude")).unwrap();
        std::fs::set_permissions(root.join("bin/claude"), std::fs::Permissions::from_mode(0o755)).unwrap();
        Sandbox { root: root.canonicalize().unwrap() }
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    fn krowk_home(&self) -> PathBuf {
        self.home().join(".krowk")
    }

    fn old_config(&self) -> PathBuf {
        self.home().join(".config/krowk")
    }

    fn old_data(&self) -> PathBuf {
        self.home().join(".local/share/krowk")
    }

    fn old_cache(&self) -> PathBuf {
        self.home().join(".cache/krowk")
    }

    fn krowk(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut c = Command::new(env!("CARGO_BIN_EXE_krowk"));
        c.args(args)
            .env_clear()
            .env("PATH", format!("{}:/usr/bin:/bin", self.root.join("bin").display()))
            .env("HOME", self.home())
            .env("KROWK_NO_UPDATE_CHECK", "1")
            .env("KROWK_API_URL", "http://127.0.0.1:9/v1")
            .env("FAKE_CLAUDE_LOG", self.root.join("fake.log"))
            .current_dir(self.root.join("repo"))
            .stdin(Stdio::null());
        for (k, v) in env {
            c.env(k, v);
        }
        c.output().unwrap()
    }

    fn write(&self, path: &Path, body: &str, mode: u32) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    /// The layout the last release left: config and keys under
    /// ~/.config/krowk, accounts and krowk.db under ~/.local/share/krowk,
    /// prices under ~/.cache/krowk.
    fn old_layout(&self) {
        let (cfg, data, cache) = (self.old_config(), self.old_data(), self.old_cache());
        let account = data.join("claude/claude-work");
        let config = json!({
            "workspace": "ws_mig",
            "instances": {
                "claude:work": {"kind": "claude-code", "configDir": account.display().to_string()},
                "supergrok": {"kind": "xai-oauth"},
            },
        });
        self.write(&cfg.join("config.json"), &config.to_string(), 0o644);
        let registry = json!({"default": "ws_mig", "workspaces": {"ws_mig": {"token": REGISTRY_KEY, "key_id": "key_mig", "workspace": "ws_mig"}}});
        self.write(&cfg.join("credentials.json"), &registry.to_string(), 0o600);
        let login = json!({"issuer": "https://auth.x.ai", "clientId": "krowk-test", "tokenEndpoint": "https://auth.x.ai/oauth2/token", "accessToken": ACCESS, "refreshToken": REFRESH, "expiresAtMs": 4_102_444_800_000_i64, "obtainedAtMs": 0});
        let providers = json!({"version": 1, "instances": {"supergrok": login}, "keys": {"anthropic": {"literal": STORED_KEY}}});
        self.write(&cfg.join("providers/credentials.json"), &providers.to_string(), 0o600);
        self.write(&cfg.join("providers/credentials.lock"), "", 0o600);
        self.write(&cfg.join("trusted.json"), &json!({"directories": [self.root.join("repo")]}).to_string(), 0o600);
        // What the harness reads from krowk's own directory besides.
        self.write(&cfg.join("permissions.json"), &json!({"grants": {}}).to_string(), 0o600);
        self.write(&cfg.join("skills/greet/SKILL.md"), "---\nname: greet\ndescription: say hi\n---\nhi\n", 0o644);
        self.write(&cfg.join("AGENTS.md"), "be brief\n", 0o644);
        self.write(&data.join("import.lock"), "", 0o600);
        self.write(&account.join("fake-login"), "", 0o600);
        std::fs::set_permissions(&account, std::fs::Permissions::from_mode(0o700)).unwrap();
        self.write(&data.join("sessions/sess-old/events.jsonl"), "", 0o600);
        self.write(&cache.join("models.json"), r#"{"anthropic":{"claude-sonnet-4-6":{"input":3}}}"#, 0o644);
        // A real krowk.db, made in a scratch home and put where the last
        // release kept it.
        let seed = self.root.join("seed");
        assert!(self.krowk(&["sessions", "--json"], &[("KROWK_HOME", seed.to_str().unwrap())]).status.success());
        std::fs::rename(seed.join("sessions/krowk.db"), data.join("krowk.db")).unwrap();
    }

    fn credentials(&self) -> Value {
        serde_json::from_str(&std::fs::read_to_string(self.krowk_home().join("credentials.json")).unwrap()).unwrap()
    }

    fn config(&self) -> Value {
        serde_json::from_str(&std::fs::read_to_string(self.krowk_home().join("config.json")).unwrap()).unwrap()
    }

    fn row(&self, instance: &str) -> Value {
        let out = self.krowk(&["status", "--json"], &[]);
        let v: Value = serde_json::from_slice(&out.stdout).or_else(|_| serde_json::from_slice(&out.stderr)).unwrap();
        let rows = v.pointer("/data/instances").or_else(|| v.pointer("/error/details/instances")).unwrap_or_else(|| panic!("{v}"));
        rows.as_array().unwrap().iter().find(|r| r["instance"] == instance).cloned().unwrap_or_else(|| panic!("{instance} not listed in {v}"))
    }

    /// Every path under the home with its size and mode: what a second run
    /// must leave as it was.
    fn tree(&self, dir: &Path) -> Vec<String> {
        let mut out = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).unwrap().flatten() {
                let m = e.metadata().unwrap();
                out.push(format!("{} {} {:o}", e.path().strip_prefix(dir).unwrap().display(), if m.is_dir() { 0 } else { m.len() }, m.permissions().mode() & 0o777));
                if m.is_dir() {
                    stack.push(e.path());
                }
            }
        }
        out.sort();
        out
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn printed(o: &Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
}

fn mode(p: &Path) -> u32 {
    std::fs::symlink_metadata(p).unwrap().permissions().mode() & 0o777
}

#[test]
fn an_older_layout_moves_its_config_and_keys_deletes_the_old_secrets_and_names_the_rest() {
    let b = Sandbox::new("full");
    b.old_layout();

    let first = b.krowk(&["doctor", "--json"], &[]);
    let said = String::from_utf8_lossy(&first.stderr).into_owned();
    let h = b.krowk_home();
    assert_eq!(said.matches("moved krowk's config and keys to").count(), 1, "one line says so: {said}");
    // What is left is named once, with what to do about it.
    let account = b.old_data().join("claude/claude-work");
    for want in [
        "run `krowk sessions rebuild`".to_string(),
        format!("mkdir -p {h}/accounts && mv {} {h}/accounts/claude-work", account.display(), h = h.display()),
        "set its configDir".into(),
        "krowk connect anthropic --method subscription --name work".into(),
        format!("mv {} {}/trusted.json", b.old_config().join("trusted.json").display(), h.display()),
    ] {
        assert!(said.contains(&want), "{want} in: {said}");
    }
    // The keys are gone from the old directory; what krowk never moves is
    // still there.
    for gone in [b.old_config().join("credentials.json"), b.old_config().join("providers"), b.old_config().join("config.json"), b.old_cache(), b.home().join(".krowk.migrating")] {
        assert!(!gone.exists(), "{} is left behind", gone.display());
    }
    for kept in [account.clone(), b.old_data().join("krowk.db"), b.old_data().join("sessions"), b.old_config().join("trusted.json")] {
        assert!(kept.exists(), "{} was moved", kept.display());
    }
    assert_eq!(mode(&h), 0o700);
    assert_eq!(mode(&h.join("credentials.json")), 0o600);
    assert_eq!(mode(&h.join("config.json")), 0o644, "config.json keeps its mode");

    // One credentials file holds the registry's key, the login and the
    // stored key.
    let c = b.credentials();
    assert_eq!(c["workspaces"]["ws_mig"]["token"], REGISTRY_KEY);
    assert_eq!(c["default"], "ws_mig");
    assert_eq!(c["instances"]["supergrok"]["refreshToken"], REFRESH);
    assert_eq!(c["keys"]["anthropic"], json!({"literal": STORED_KEY}));
    assert_eq!(b.config()["workspace"], "ws_mig");

    // And they work from there: the registry key is found, the login and
    // the stored key make their instances ready, the store opens (empty).
    let doctor: Value = serde_json::from_slice(&first.stdout).unwrap();
    assert_eq!(doctor["token_source"], "credentials file", "{doctor}");
    assert_eq!(doctor["workspace"], "ws_mig (global config)", "{doctor}");
    assert_eq!(b.row("supergrok")["state"], "ready");
    assert_eq!(b.row("anthropic")["state"], "ready");
    let sessions = b.krowk(&["sessions", "--json"], &[]);
    assert!(sessions.status.success(), "{}", printed(&sessions));

    // A second run moves nothing, says nothing, and changes nothing.
    let before = b.tree(&h);
    let again = b.krowk(&["doctor", "--json"], &[]);
    assert!(!String::from_utf8_lossy(&again.stderr).contains("krowk:"), "{}", printed(&again));
    assert_eq!(b.tree(&h), before);

    // The account, moved as the line says, is signed in from there.
    std::fs::create_dir_all(h.join("accounts")).unwrap();
    std::fs::rename(&account, h.join("accounts/claude-work")).unwrap();
    let mut cfg = b.config();
    cfg["instances"]["claude:work"]["configDir"] = json!(h.join("accounts/claude-work"));
    std::fs::write(h.join("config.json"), cfg.to_string()).unwrap();
    assert_eq!(b.row("claude:work")["state"], "ready");

    // One file, two writers, and neither drops the other's.
    let stored = b.krowk(&["connect", "openai", "--method", "api-key", "--key-ref", "$OPENAI_KEY_ELSEWHERE"], &[]);
    assert!(stored.status.success(), "{}", printed(&stored));
    assert_eq!(b.credentials()["workspaces"]["ws_mig"]["token"], REGISTRY_KEY);
    let logout = b.krowk(&["logout"], &[]);
    assert!(logout.status.success(), "{}", printed(&logout));
    let c = b.credentials();
    assert!(c["workspaces"].get("ws_mig").is_none(), "{c}");
    assert_eq!((c["instances"]["supergrok"]["accessToken"].as_str(), c["keys"]["anthropic"]["literal"].as_str()), (Some(ACCESS), Some(STORED_KEY)));
    for out in [&first, &again, &stored, &logout] {
        for secret in [REGISTRY_KEY, STORED_KEY, ACCESS, REFRESH] {
            assert!(!printed(out).contains(secret), "{secret} printed: {}", printed(out));
        }
    }
}

#[test]
fn a_move_a_crash_cut_short_after_its_rename_is_finished_by_the_next_start() {
    let b = Sandbox::new("finish");
    b.old_layout();
    // As a crash after the rename leaves it: the home in place, the old
    // key files still there.
    let merged = json!({
        "default": "ws_mig",
        "workspaces": {"ws_mig": {"token": REGISTRY_KEY}},
        "version": 1,
        "instances": {"supergrok": {"issuer": "x"}},
        "keys": {"anthropic": {"literal": STORED_KEY}},
    });
    b.write(&b.krowk_home().join("credentials.json"), &merged.to_string(), 0o600);
    std::fs::set_permissions(b.krowk_home(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let out = b.krowk(&["doctor", "--json"], &[]);
    assert!(out.status.success(), "{}", printed(&out));
    assert!(!b.old_config().join("credentials.json").exists() && !b.old_config().join("providers").exists(), "the old keys are deleted: {}", printed(&out));
    assert!(!printed(&out).contains(REGISTRY_KEY));
}

#[test]
fn a_home_that_exists_takes_nothing_from_an_old_layout_and_names_it_once() {
    let b = Sandbox::new("exists");
    b.old_layout();
    std::fs::create_dir_all(b.krowk_home()).unwrap();
    let first = b.krowk(&["doctor", "--json"], &[]);
    let said = String::from_utf8_lossy(&first.stderr).into_owned();
    assert!(said.contains("old krowk files krowk no longer reads") && said.contains(&b.old_config().join("config.json").display().to_string()), "{said}");
    assert!(b.old_config().join("credentials.json").exists() && !b.krowk_home().join("credentials.json").exists(), "nothing merged in");
    let again = b.krowk(&["doctor", "--json"], &[]);
    assert!(!String::from_utf8_lossy(&again.stderr).contains("no longer reads"), "said once: {}", printed(&again));
    for out in [&first, &again] {
        for secret in [REGISTRY_KEY, STORED_KEY, ACCESS, REFRESH] {
            assert!(!printed(out).contains(secret));
        }
    }
}

#[test]
fn a_krowk_home_is_a_sandbox_and_never_takes_the_persons_own_files() {
    let b = Sandbox::new("sandboxed");
    b.old_layout();
    let own = b.root.join("sandbox-home");
    let out = b.krowk(&["doctor", "--json"], &[("KROWK_HOME", own.to_str().unwrap())]);
    assert!(out.status.success(), "{}", printed(&out));
    assert!(!printed(&out).contains("moved"), "{}", printed(&out));
    assert!(b.old_config().join("credentials.json").exists() && b.old_data().join("krowk.db").exists(), "nothing moved");
    assert!(!b.krowk_home().exists());
    let doctor: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doctor["token_source"], "none", "the old key is not read either: {doctor}");
    assert_eq!(mode(&own), 0o700);
}

#[test]
fn a_malformed_old_key_file_stops_the_move_naming_only_where_and_a_fixed_one_lets_it_finish() {
    let b = Sandbox::new("malformed");
    b.old_layout();
    let providers = b.old_config().join("providers/credentials.json");
    b.write(&providers, &format!("{{\"version\": 1, \"keys\": {{\"anthropic\": \"{STORED_KEY}\"}}, oops}}"), 0o600);
    for args in [&["doctor", "--json"][..], &["status", "--json"], &["config", "show"], &["whoami"]] {
        let out = b.krowk(args, &[]);
        assert!(!out.status.success(), "{args:?}: {}", printed(&out));
        assert!(printed(&out).contains(&format!("{} is not valid (line 1, column", providers.display())), "{args:?}: {}", printed(&out));
        for secret in [REGISTRY_KEY, STORED_KEY, ACCESS, REFRESH] {
            assert!(!printed(&out).contains(secret), "{secret} printed: {}", printed(&out));
        }
    }
    assert!(!b.krowk_home().exists(), "nothing is used half-moved");
    assert!(b.old_data().join("krowk.db").exists());
    b.write(&providers, &json!({"version": 1, "keys": {"anthropic": {"literal": STORED_KEY}}}).to_string(), 0o600);
    let out = b.krowk(&["doctor", "--json"], &[]);
    assert!(out.status.success(), "{}", printed(&out));
    assert_eq!(b.credentials()["keys"]["anthropic"], json!({"literal": STORED_KEY}));
    assert_eq!(b.credentials()["workspaces"]["ws_mig"]["token"], REGISTRY_KEY);
}
