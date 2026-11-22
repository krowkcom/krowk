//! The one move from an older gruntas's XDG layout into `~/.gruntas`, the built
//! binary against a home laid out the way the last release (and a dev
//! build) left it: config.json, the registry key, the provider file with a
//! SuperGrok login and a stored key, a named Claude account (the fake
//! `claude`, no real login), krowk.db, a session and the models.dev cache.
//! The config and the keys move and work, the old secrets are deleted,
//! and what is not moved is named once with what to do; a move a crash
//! cut short after its rename is finished; a home that exists takes
//! nothing; a GRUNTAS_HOME never takes the person's own files; and a
//! malformed old key file stops the move with its line and column only.
//! And the one copy of krowk's `~/.krowk` into `~/.gruntas`: once, with the
//! login, sync identity, sessions and accounts working from the copy, and
//! `~/.krowk` left as it was.

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
        let fake = Path::new(env!("CARGO_MANIFEST_DIR")).join("../gruntas-harness/tests/fixtures/claude/fake-claude");
        // Linked, not copied: a copy is a file open for writing that a test
        // forking beside it can inherit, and running it then fails with
        // "Text file busy" (ETXTBSY) — read as a vendor that could not be checked.
        std::os::unix::fs::symlink(fake, root.join("bin/claude")).unwrap();
        std::fs::set_permissions(root.join("bin/claude"), std::fs::Permissions::from_mode(0o755)).unwrap();
        Sandbox { root: root.canonicalize().unwrap() }
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    fn gruntas_home(&self) -> PathBuf {
        self.home().join(".gruntas")
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

    fn gruntas(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut c = Command::new(env!("CARGO_BIN_EXE_gruntas"));
        c.args(args)
            .env_clear()
            .env("PATH", format!("{}:/usr/bin:/bin", self.root.join("bin").display()))
            .env("HOME", self.home())
            .env("GRUNTAS_NO_UPDATE_CHECK", "1")
            .env("GRUNTAS_API_URL", "http://127.0.0.1:9/v1")
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
                // The stored key's account: Claude Code on that key (NL03).
                "anthropic": {"kind": "claude-code", "configDir": account.display().to_string(), "apiKeyEnv": "ANTHROPIC_API_KEY"},
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
        // What the harness reads from gruntas's own directory besides.
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
        assert!(self.gruntas(&["sessions", "--json"], &[("GRUNTAS_HOME", seed.to_str().unwrap())]).status.success());
        std::fs::rename(seed.join("sessions/gruntas.db"), data.join("krowk.db")).unwrap();
    }

    fn credentials(&self) -> Value {
        serde_json::from_str(&std::fs::read_to_string(self.gruntas_home().join("credentials.json")).unwrap()).unwrap()
    }

    fn config(&self) -> Value {
        serde_json::from_str(&std::fs::read_to_string(self.gruntas_home().join("config.json")).unwrap()).unwrap()
    }

    fn row(&self, instance: &str) -> Value {
        let out = self.gruntas(&["status", "--json"], &[]);
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

    let first = b.gruntas(&["doctor", "--json"], &[]);
    let said = String::from_utf8_lossy(&first.stderr).into_owned();
    let h = b.gruntas_home();
    assert_eq!(said.matches("moved krowk's config and keys to").count(), 1, "one line says so: {said}");
    let account = b.old_data().join("claude/claude-work");
    assert_names_what_is_left(&b, &said, &h, &account);
    assert_moved_and_old_keys_gone(&b, &h, &account);

    // One credentials file holds the registry's key, the login and the
    // stored key.
    let c = b.credentials();
    assert_eq!(c["workspaces"]["ws_mig"]["token"], REGISTRY_KEY);
    assert_eq!(c["default"], "ws_mig");
    assert_eq!(c["instances"]["supergrok"]["refreshToken"], REFRESH);
    assert_eq!(c["keys"]["anthropic"], json!({"literal": STORED_KEY}));
    assert_eq!(b.config()["workspace"], "ws_mig");

    // And they work from there: the registry key is found, the stored key
    // makes its account ready (SuperGrok's login is moved but runs nothing:
    // D4), the store opens (empty).
    let doctor: Value = serde_json::from_slice(&first.stdout).unwrap();
    assert_eq!(doctor["token_source"], "credentials file", "{doctor}");
    assert_eq!(doctor["workspace"], "ws_mig (global config)", "{doctor}");
    assert_eq!(b.row("anthropic")["state"], "ready");
    let sessions = b.gruntas(&["sessions", "--json"], &[]);
    assert!(sessions.status.success(), "{}", printed(&sessions));

    // A second run moves nothing, says nothing, and changes nothing.
    let before = b.tree(&h);
    let again = b.gruntas(&["doctor", "--json"], &[]);
    assert!(!String::from_utf8_lossy(&again.stderr).contains("gruntas:"), "{}", printed(&again));
    assert_eq!(b.tree(&h), before);

    // The account, moved as the line says, is signed in from there.
    std::fs::create_dir_all(h.join("accounts")).unwrap();
    std::fs::rename(&account, h.join("accounts/claude-work")).unwrap();
    let mut cfg = b.config();
    cfg["instances"]["claude:work"]["configDir"] = json!(h.join("accounts/claude-work"));
    std::fs::write(h.join("config.json"), cfg.to_string()).unwrap();
    assert_eq!(b.row("claude:work")["state"], "ready");

    // One file, two writers, and neither drops the other's.
    let stored = b.gruntas(&["connect", "anthropic", "--method", "api-key", "--name", "other", "--key-ref", "$ANTHROPIC_KEY_ELSEWHERE"], &[]);
    assert!(stored.status.success(), "{}", printed(&stored));
    assert_eq!(b.credentials()["workspaces"]["ws_mig"]["token"], REGISTRY_KEY);
    let logout = b.gruntas(&["logout"], &[]);
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

/// What is left behind is named once, with what to do about it.
fn assert_names_what_is_left(b: &Sandbox, said: &str, h: &Path, account: &Path) {
    for want in [
        "run `gruntas sessions rebuild`".to_string(),
        format!("mkdir -p {h}/accounts && mv {} {h}/accounts/claude-work", account.display(), h = h.display()),
        "set its configDir".into(),
        "gruntas connect anthropic --method subscription --name work".into(),
        format!("mv -n {} {}/trusted.json", b.old_config().join("trusted.json").display(), h.display()),
    ] {
        assert!(said.contains(&want), "{want} in: {said}");
    }
}

/// The keys are gone from the old directory, what gruntas never moves is still
/// there, and the new home is private.
fn assert_moved_and_old_keys_gone(b: &Sandbox, h: &Path, account: &Path) {
    for gone in [b.old_config().join("credentials.json"), b.old_config().join("providers"), b.old_config().join("config.json"), b.old_cache(), b.home().join(".gruntas.migrating")] {
        assert!(!gone.exists(), "{} is left behind", gone.display());
    }
    for kept in [account.to_path_buf(), b.old_data().join("krowk.db"), b.old_data().join("sessions"), b.old_config().join("trusted.json")] {
        assert!(kept.exists(), "{} was moved", kept.display());
    }
    assert_eq!(mode(h), 0o700);
    assert_eq!(mode(&h.join("credentials.json")), 0o600);
    assert_eq!(mode(&h.join("config.json")), 0o644, "config.json keeps its mode");
}

#[test]
fn a_move_a_crash_cut_short_after_its_rename_is_finished_by_the_next_start() {
    let b = Sandbox::new("finish");
    b.old_layout();
    // As a crash after the rename leaves it: the home in place, holding
    // what the move wrote — each key as it was — and the old key files
    // still there.
    let merged = json!({
        "default": "ws_mig",
        "workspaces": {"ws_mig": {"token": REGISTRY_KEY, "key_id": "key_mig", "workspace": "ws_mig"}},
        "version": 1,
        "instances": {"supergrok": {"issuer": "x"}},
        "keys": {"anthropic": {"literal": STORED_KEY}},
    });
    b.write(&b.gruntas_home().join("credentials.json"), &merged.to_string(), 0o600);
    std::fs::set_permissions(b.gruntas_home(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let out = b.gruntas(&["doctor", "--json"], &[]);
    assert!(reported(&out), "{}", printed(&out));
    assert!(!b.old_config().join("credentials.json").exists() && !b.old_config().join("providers").exists(), "the old keys are deleted: {}", printed(&out));
    assert!(!printed(&out).contains(REGISTRY_KEY));
}

#[test]
fn a_home_that_exists_takes_nothing_from_an_old_layout_and_names_it_once() {
    let b = Sandbox::new("exists");
    b.old_layout();
    std::fs::create_dir_all(b.gruntas_home()).unwrap();
    let first = b.gruntas(&["doctor", "--json"], &[]);
    let said = String::from_utf8_lossy(&first.stderr).into_owned();
    assert!(said.contains("old krowk files gruntas no longer reads") && said.contains(&b.old_config().join("config.json").display().to_string()), "{said}");
    assert!(b.old_config().join("credentials.json").exists() && !b.gruntas_home().join("credentials.json").exists(), "nothing merged in");
    let again = b.gruntas(&["doctor", "--json"], &[]);
    assert!(!String::from_utf8_lossy(&again.stderr).contains("no longer reads"), "said once: {}", printed(&again));
    for out in [&first, &again] {
        for secret in [REGISTRY_KEY, STORED_KEY, ACCESS, REFRESH] {
            assert!(!printed(out).contains(secret));
        }
    }
}

/// Doctor ran and reported: with no account or machine of its own in the
/// sandbox it exits non-zero for what is unset, which these cases are not about.
fn reported(out: &std::process::Output) -> bool {
    serde_json::from_slice::<Value>(&out.stdout).is_ok_and(|d| d["checks"].is_array())
}

#[test]
fn a_gruntas_home_is_a_sandbox_and_never_takes_the_persons_own_files() {
    let b = Sandbox::new("sandboxed");
    b.old_layout();
    let own = b.root.join("sandbox-home");
    let out = b.gruntas(&["doctor", "--json"], &[("GRUNTAS_HOME", own.to_str().unwrap())]);
    assert!(reported(&out), "{}", printed(&out));
    assert!(!printed(&out).contains("moved"), "{}", printed(&out));
    assert!(b.old_config().join("credentials.json").exists() && b.old_data().join("krowk.db").exists(), "nothing moved");
    assert!(!b.gruntas_home().exists());
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
        let out = b.gruntas(args, &[]);
        assert!(!out.status.success(), "{args:?}: {}", printed(&out));
        assert!(printed(&out).contains(&format!("{} is not valid (line 1, column", providers.display())), "{args:?}: {}", printed(&out));
        for secret in [REGISTRY_KEY, STORED_KEY, ACCESS, REFRESH] {
            assert!(!printed(&out).contains(secret), "{secret} printed: {}", printed(&out));
        }
    }
    assert!(!b.gruntas_home().exists(), "nothing is used half-moved");
    assert!(b.old_data().join("krowk.db").exists());
    b.write(&providers, &json!({"version": 1, "keys": {"anthropic": {"literal": STORED_KEY}}}).to_string(), 0o600);
    let out = b.gruntas(&["doctor", "--json"], &[]);
    assert!(reported(&out), "{}", printed(&out));
    assert_eq!(b.credentials()["keys"]["anthropic"], json!({"literal": STORED_KEY}));
    assert_eq!(b.credentials()["workspaces"]["ws_mig"]["token"], REGISTRY_KEY);
}

/// A krowk home as krowk left it: a registry key, an account signed in
/// in its own directory, a session it ran, and sync set up against the
/// stand-in registry (`api`). Made by this binary with `GRUNTAS_HOME`
/// pointed at `~/.krowk`, which is the layout krowk keeps.
fn krowk_home(b: &Sandbox, api: &str) -> PathBuf {
    let old = b.home().join(".krowk");
    let at = [("GRUNTAS_HOME", old.to_str().unwrap()), ("GRUNTAS_API_URL", api), ("GRUNTAS_DEVICE_NAME", "this machine"), ("GRUNTAS_TEST_UNATTENDED_DEVICE_APPROVAL", "1")];
    b.write(&old.join("credentials.json"), &json!({"default": "ws_mig", "workspaces": {"ws_mig": {"token": REGISTRY_KEY, "key_id": "key_mig", "workspace": "ws_mig"}}}).to_string(), 0o600);
    b.write(&old.join("config.json"), &json!({"workspace": "ws_mig"}).to_string(), 0o644);
    for args in [&["connect", "anthropic", "--method", "subscription", "--name", "work"][..], &["-p", "hello", "--model", "claude:work/sonnet", "--trust", "--output-format", "json"], &["sync", "init", "--save", b.root.join("kit.txt").to_str().unwrap(), "--json"]] {
        let out = b.gruntas(args, &at);
        assert!(out.status.success(), "{args:?}: {}", printed(&out));
    }
    // Under the name gruntas gives it.
    for end in ["", "-wal", "-shm"] {
        let _ = std::fs::rename(old.join(format!("sessions/gruntas.db{end}")), old.join(format!("sessions/krowk.db{end}")));
    }
    assert!(old.join("sessions/krowk.db").exists());
    old
}

#[test]
fn krowks_home_is_copied_into_gruntas_once_and_everything_works_from_the_copy() {
    let b = Sandbox::new("copy");
    let registry = gruntas_devregistry::start(std::net::TcpListener::bind("127.0.0.1:0").unwrap(), Default::default()).unwrap();
    let api = format!("{}/v1", registry.url());
    let old = krowk_home(&b, &api);
    let before = b.tree(&old);
    let old_config = std::fs::read(old.join("config.json")).unwrap();
    let krowk_keys = gruntas_client::keystore::Keystore::new(&old);
    let device = krowk_keys.device().unwrap().expect("gruntas's device").id();
    assert!(krowk_keys.user_keys().unwrap().is_some(), "sync set up in gruntas's home");

    // The first run copies it, and says so once.
    let first = b.gruntas(&["doctor", "--json"], &[("GRUNTAS_API_URL", &api)]);
    let said = String::from_utf8_lossy(&first.stderr).into_owned();
    assert_eq!(said.matches("gruntas: copying").count(), 1, "one line says so: {said}");
    let h = b.gruntas_home();
    assert!(h.join("sessions/gruntas.db").exists() && !h.join("sessions/krowk.db").exists(), "the index is renamed");
    assert_eq!(mode(&h), 0o700);
    assert_eq!(mode(&h.join("credentials.json")), 0o600);

    // The login: the registry key is read from the copy.
    let doctor: Value = serde_json::from_slice(&first.stdout).unwrap();
    assert_eq!((doctor["token_source"].as_str(), doctor["workspace"].as_str()), (Some("credentials file"), Some("ws_mig (global config)")), "{doctor}");
    // The account: signed in, from the copy's own directory.
    let account = h.join("accounts/claude-work");
    assert_eq!(b.config()["instances"]["claude:work"]["configDir"], account.display().to_string());
    let row = b.row("claude:work");
    assert_eq!(row["state"], "ready", "{row}");
    assert!(row["source"].as_str().unwrap().contains(&account.display().to_string()), "{row}");
    // The session gruntas ran is listed, and runs again.
    let sessions: Value = serde_json::from_slice(&b.gruntas(&["sessions", "--json"], &[]).stdout).unwrap();
    let listed = sessions["data"]["sessions"].as_array().unwrap();
    assert_eq!(listed.len(), 1, "{sessions}");
    let id = listed[0]["id"].as_str().or(listed[0]["sessionId"].as_str()).unwrap_or_else(|| panic!("{sessions}")).to_string();
    let resumed = b.gruntas(&["-p", "go on", "--resume", &id, "--trust", "--output-format", "json"], &[]);
    assert!(resumed.status.success(), "{}", printed(&resumed));
    // The sync identity: the same device, with its user key.
    let ks = gruntas_client::keystore::Keystore::new(&h);
    assert_eq!(ks.device().unwrap().expect("the device, copied").id(), device);
    assert!(ks.user_keys().unwrap().is_some());

    // ~/.krowk is as krowk left it.
    assert_eq!(b.tree(&old), before);
    assert_eq!(std::fs::read(old.join("config.json")).unwrap(), old_config);

    // A second run copies nothing and says nothing, even with ~/.krowk changed.
    std::fs::write(old.join("later.json"), "{}").unwrap();
    let again = b.gruntas(&["doctor", "--json"], &[("GRUNTAS_API_URL", &api)]);
    assert!(!String::from_utf8_lossy(&again.stderr).contains("copying"), "{}", printed(&again));
    assert!(!h.join("later.json").exists());
    for out in [&first, &again, &resumed] {
        assert!(!printed(out).contains(REGISTRY_KEY), "{}", printed(out));
    }
}

#[test]
fn a_gruntas_home_set_takes_nothing_from_krowks() {
    let b = Sandbox::new("copy-skipped");
    let old = b.home().join(".krowk");
    b.write(&old.join("credentials.json"), &json!({"default": "ws_mig", "workspaces": {"ws_mig": {"token": REGISTRY_KEY}}}).to_string(), 0o600);
    let own = b.root.join("sandbox-home");
    let out = b.gruntas(&["doctor", "--json"], &[("GRUNTAS_HOME", own.to_str().unwrap())]);
    assert!(reported(&out), "{}", printed(&out));
    assert!(!printed(&out).contains("copying"), "{}", printed(&out));
    assert!(!b.gruntas_home().exists() && !own.join("credentials.json").exists());
    let doctor: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doctor["token_source"], "none", "{doctor}");
}
