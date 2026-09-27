//! `krowk status`, `krowk providers list` and `krowk doctor` — the one
//! readiness check — the built binary against fake `claude` and `codex`
//! binaries on PATH (no real login anywhere) and a stand-in environment:
//! the seven implicit instances with no config, each in the state its
//! credential puts it in; vendor checks run in parallel; the `--json`
//! shape; and no secret in any output, checked with sentinel values.

#![cfg(all(feature = "harness", unix))]

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

fn fixture(vendor: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../krowk-harness/tests/fixtures").join(vendor).join(format!("fake-{vendor}"))
}

/// Values that must never appear in anything krowk prints.
const KEY_SENTINEL: &str = "sk-ant-SENTINEL-4f1d9c";
const ROUTER_SENTINEL: &str = "sk-or-SENTINEL-77ab02";
const TOKEN_SENTINEL: &str = "xai-at-SENTINEL-e3c8";
const REFRESH_SENTINEL: &str = "xai-rt-SENTINEL-19d0";

struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Sandbox {
        let root = std::env::temp_dir().join(format!("krowk-status-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["home", "repo", "bin"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        Sandbox { root: root.canonicalize().unwrap() }
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    /// A binary on the sandbox's PATH: a fixture's fake, or a script.
    fn install(&self, name: &str, script: &str) {
        let bin = self.root.join("bin").join(name);
        std::fs::write(&bin, script).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn install_fakes(&self) {
        for v in ["claude", "codex"] {
            self.install(v, &std::fs::read_to_string(fixture(v)).unwrap());
        }
    }

    /// krowk with only the sandbox's binaries and the system's on PATH — never
    /// a real `claude` or `codex` of the machine running the tests.
    fn krowk(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut c = Command::new(env!("CARGO_BIN_EXE_krowk"));
        c.args(args)
            .env_clear()
            .env("PATH", format!("{}:/usr/bin:/bin", self.root.join("bin").display()))
            .env("HOME", self.home())
            .env("KROWK_NO_UPDATE_CHECK", "1")
            .env("KROWK_API_URL", "http://127.0.0.1:9/v1")
            .env("FAKE_CLAUDE_LOG", self.root.join("fake.log"))
            .env("FAKE_CODEX_LOG", self.root.join("fake.log"))
            .current_dir(self.root.join("repo"))
            .stdin(Stdio::null());
        for (k, v) in env {
            c.env(k, v);
        }
        c.output().unwrap()
    }

    fn write_config(&self, v: Value) {
        let dir = self.home().join(".config/krowk");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.json"), v.to_string()).unwrap();
    }

    /// krowk's provider credentials file, with logins as `providers add
    /// supergrok` would have left them.
    fn write_logins(&self, logins: Value) {
        let dir = self.home().join(".config/krowk/providers");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("credentials.json"), json!({"version": 1, "instances": logins}).to_string()).unwrap();
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn login(access: &str, refresh: Option<&str>, expires_at_ms: i64) -> Value {
    let mut v = json!({"issuer": "https://auth.x.ai", "clientId": "krowk-test", "tokenEndpoint": "https://auth.x.ai/oauth2/token", "accessToken": access, "expiresAtMs": expires_at_ms, "obtainedAtMs": 0});
    if let Some(r) = refresh {
        v["refreshToken"] = json!(r);
    }
    v
}

fn stdout_json(out: &Output) -> Value {
    let s = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str(s.trim()).unwrap_or_else(|e| panic!("{e}: {s}{}", String::from_utf8_lossy(&out.stderr)))
}

fn instances(data: &Value) -> Vec<Value> {
    data["instances"].as_array().unwrap_or_else(|| panic!("no instances in {data}")).clone()
}

fn row(rows: &[Value], name: &str) -> Value {
    rows.iter().find(|r| r["instance"] == name).unwrap_or_else(|| panic!("{name} not in {rows:?}")).clone()
}

#[test]
fn status_with_no_config_lists_the_seven_implicit_instances_in_their_states() {
    let b = Sandbox::new("implicit");
    b.install_fakes();
    // Claude Code signed in (the fake's marker in its default ~/.claude),
    // Codex not; an Anthropic key; a SuperGrok login whose access token is
    // spent with no refresh token to get another.
    std::fs::create_dir_all(b.home().join(".claude")).unwrap();
    std::fs::write(b.home().join(".claude/fake-login"), "").unwrap();
    b.write_logins(json!({"supergrok": login(TOKEN_SENTINEL, None, 1_000)}));

    let out = b.krowk(&["status", "--json"], &[("ANTHROPIC_API_KEY", KEY_SENTINEL)]);
    assert_eq!(out.status.code(), Some(0), "one instance is ready: {}", String::from_utf8_lossy(&out.stderr));
    let v = stdout_json(&out);
    let rows = instances(&v["data"]);
    let names: Vec<&str> = rows.iter().map(|r| r["instance"].as_str().unwrap()).collect();
    assert_eq!(names, ["anthropic", "claude", "codex", "openai", "openrouter", "supergrok", "xai"]);
    let state = |n: &str| row(&rows, n)["state"].as_str().unwrap().to_string();
    assert_eq!(
        names.iter().map(|n| (n.to_string(), state(n))).collect::<Vec<_>>(),
        [("anthropic", "ready"), ("claude", "ready"), ("codex", "not_signed_in"), ("openai", "key_not_set"), ("openrouter", "key_not_set"), ("supergrok", "expired"), ("xai", "key_not_set")]
            .map(|(n, s)| (n.to_string(), s.to_string()))
    );
    assert_eq!(v["data"]["ready"], 2);
    // The documented shape: every key on every row, null when it has no value.
    for r in &rows {
        let keys: Vec<&String> = r.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["instance", "kind", "state", "ready", "source", "fix", "var", "reason"], "{r}");
        assert_eq!(r["ready"].as_bool(), Some(r["state"] == "ready"));
        assert_eq!(r["fix"].is_null(), r["state"] == "ready", "a fix exactly when it is not ready: {r}");
    }
    assert_eq!(row(&rows, "anthropic")["source"], "$ANTHROPIC_API_KEY", "a key is named by its variable");
    assert_eq!(row(&rows, "openai")["var"], "OPENAI_API_KEY");
    assert_eq!(row(&rows, "claude")["source"], format!("Claude Code's own login in {} (signed in with a Claude max subscription)", b.home().join(".claude").display()));
    assert_eq!(row(&rows, "codex")["fix"], "sign in with `krowk providers add codex`, which runs Codex's own login");
    assert_eq!(row(&rows, "supergrok")["fix"], "sign in with `krowk providers add supergrok`");
    assert_eq!(row(&rows, "supergrok")["kind"], "xai-oauth");
    let printed = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(!printed.contains(KEY_SENTINEL) && !printed.contains(TOKEN_SENTINEL), "no secret is printed: {printed}");

    // The table says the same, and why each one is not ready.
    let out = b.krowk(&["status", "--format", "human"], &[("ANTHROPIC_API_KEY", KEY_SENTINEL)]);
    assert_eq!(out.status.code(), Some(0));
    let table = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(table.lines().any(|l| l.starts_with("supergrok") && l.contains("expired") && l.contains("krowk providers add supergrok")), "{table}");
    assert!(table.ends_with("2 of 7 instances ready\n"), "{table}");
    assert!(!table.contains(KEY_SENTINEL) && !table.contains(TOKEN_SENTINEL));

    // Nothing ready: every row still printed, and exit 3 with the rows as
    // the failure's details.
    std::fs::remove_file(b.home().join(".claude/fake-login")).unwrap();
    std::fs::remove_file(b.root.join("bin/codex")).unwrap();
    let out = b.krowk(&["status", "--json"], &[]);
    assert_eq!(out.status.code(), Some(3));
    let err: Value = serde_json::from_str(String::from_utf8_lossy(&out.stderr).trim()).unwrap();
    assert_eq!(err["error"]["error"], "none_ready", "{err}");
    let details = instances(&err["error"]["details"]);
    assert_eq!((details.len(), row(&details, "codex")["state"].as_str(), row(&details, "claude")["state"].as_str()), (7, Some("not_installed"), Some("not_signed_in")));
    let out = b.krowk(&["status", "--format", "human"], &[]);
    assert_eq!(out.status.code(), Some(3));
    assert_eq!(String::from_utf8_lossy(&out.stdout).lines().count(), 7, "the table, before the failure");
}

#[test]
fn vendor_checks_run_in_parallel_so_two_slow_vendors_take_as_long_as_one() {
    let b = Sandbox::new("parallel");
    // Each vendor takes five seconds to answer, then says signed in.
    b.install("claude", "#!/bin/bash\nsleep 5\nprintf '{\"loggedIn\":true,\"authMethod\":\"claude.ai\",\"subscriptionType\":\"pro\"}\\n'\n");
    b.install(
        "codex",
        "#!/bin/bash\n[ \"$1\" = app-server ] || exit 2\nsleep 5\nwhile read -r l; do case \"$l\" in *'\"method\":\"account/read\"'*) printf '{\"id\":2,\"result\":{\"account\":{\"type\":\"chatgpt\"},\"requiresOpenaiAuth\":true}}\\n' ;; esac; done\n",
    );
    let started = Instant::now();
    let out = b.krowk(&["status", "--json"], &[]);
    let took = started.elapsed();
    let rows = instances(&stdout_json(&out)["data"]);
    assert_eq!((row(&rows, "claude")["state"].as_str(), row(&rows, "codex")["state"].as_str()), (Some("ready"), Some("ready")), "{rows:?}");
    assert!(took >= Duration::from_secs(5) && took < Duration::from_secs(8), "two 5 s checks took {took:?}: in parallel, about 5 s, not 10");
}

#[test]
fn no_secret_reaches_status_list_or_doctor_and_a_refreshable_login_is_ready() {
    let b = Sandbox::new("secrets");
    b.install_fakes();
    b.write_config(json!({"instances": {
        "claude:router": {"kind": "claude-code", "env": {"ANTHROPIC_BASE_URL": "https://router.example/api"}, "apiKeyEnv": "OR_KEY"},
        "codex:router": {"kind": "codex-app-server", "args": ["-c", "model_provider=openrouter"], "apiKeyEnv": "OR_KEY"},
        "anthropic:work": {"kind": "anthropic-api", "apiKeyEnv": "WORK_KEY"},
        "supergrok:team": {"kind": "xai-oauth"},
    }}));
    // An expired access token with a refresh token is still good: the next
    // call refreshes it.
    b.write_logins(json!({"supergrok:team": login(TOKEN_SENTINEL, Some(REFRESH_SENTINEL), 1_000)}));
    let env = [("ANTHROPIC_API_KEY", KEY_SENTINEL), ("WORK_KEY", KEY_SENTINEL), ("OR_KEY", ROUTER_SENTINEL)];

    let status = b.krowk(&["status", "--json"], &env);
    let rows = instances(&stdout_json(&status)["data"]);
    assert_eq!(rows.len(), 11, "the seven implicit and the four configured");
    assert_eq!(row(&rows, "supergrok:team")["state"], "ready");
    assert_eq!(row(&rows, "claude:router")["source"], "$OR_KEY, handed to Claude Code");
    assert_eq!(row(&rows, "codex:router")["source"], "$OR_KEY, handed to Codex");
    assert_eq!(row(&rows, "anthropic:work")["source"], "$WORK_KEY");

    let list = b.krowk(&["providers", "list", "--json"], &env);
    let listed = instances(&stdout_json(&list)["data"]);
    for r in &rows {
        let l = row(&listed, r["instance"].as_str().unwrap());
        assert_eq!((&l["state"], &l["source"], &l["fix"]), (&r["state"], &r["source"], &r["fix"]), "providers list and status agree");
    }
    // The harness build's doctor names each instance's state.
    let doctor = b.krowk(&["doctor", "--json"], &env);
    let d = stdout_json(&doctor);
    assert_eq!(d["providers"]["status"], "pass", "{d}");
    assert_eq!(d["providers"]["instances"]["supergrok:team"], "ready");
    assert_eq!(d["providers"]["instances"]["codex"], "not_signed_in");

    for out in [&status, &list, &doctor, &b.krowk(&["status", "--format", "human"], &env), &b.krowk(&["providers", "list", "--format", "human"], &env), &b.krowk(&["doctor", "--format", "human"], &env)] {
        let printed = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        for secret in [KEY_SENTINEL, ROUTER_SENTINEL, TOKEN_SENTINEL, REFRESH_SENTINEL] {
            assert!(!printed.contains(secret), "{secret} printed: {printed}");
        }
    }
}

#[test]
fn codex_status_falls_back_to_login_status_when_app_server_cannot_be_asked() {
    let b = Sandbox::new("codex-fallback");
    // An older Codex: no app-server to ask, only `codex login status`'s words.
    b.install("codex", "#!/bin/bash\ncase \"$1 $2\" in\n'login status') echo 'Logged in using ChatGPT' >&2; exit 0 ;;\n*) echo \"unknown command $1\" >&2; exit 2 ;;\nesac\n");
    let out = b.krowk(&["status", "--json"], &[]);
    let rows = instances(&stdout_json(&out)["data"]);
    assert_eq!((row(&rows, "codex")["state"].as_str(), row(&rows, "codex")["source"].as_str()), (Some("ready"), Some(format!("Codex's own login in {} (signed in with ChatGPT)", b.home().join(".codex").display()).as_str())));
}

#[test]
fn status_asks_vendors_in_krowks_own_directory_so_planted_settings_change_nothing() {
    let b = Sandbox::new("planted");
    // Signed in only where its working directory's project settings say so.
    b.install("claude", "#!/bin/bash\nif [ -f \"$PWD/.claude/settings.json\" ]; then echo '{\"loggedIn\":true,\"authMethod\":\"bedrock\"}'; exit 0; fi\necho '{\"loggedIn\":false}'; exit 1\n");
    // Settings planted in a shared temporary directory, and in the
    // repository krowk runs in, which nobody trusted.
    let tmp = b.root.join("tmp");
    for d in [&tmp, &b.root.join("repo")] {
        std::fs::create_dir_all(d.join(".claude")).unwrap();
        std::fs::write(d.join(".claude/settings.json"), r#"{"apiKeyHelper":"echo planted"}"#).unwrap();
    }
    let out = b.krowk(&["status", "--json"], &[("TMPDIR", tmp.to_str().unwrap()), ("ANTHROPIC_API_KEY", KEY_SENTINEL)]);
    let rows = instances(&stdout_json(&out)["data"]);
    assert_eq!(row(&rows, "claude")["state"], "not_signed_in", "{rows:?}");
    let own = b.home().join(".local/share/krowk/readiness");
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(&own).unwrap().permissions().mode() & 0o777, 0o700, "krowk's own directory, closed to others");
}
