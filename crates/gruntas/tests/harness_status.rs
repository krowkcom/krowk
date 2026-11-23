//! `gruntas status`, `gruntas providers list` and `gruntas doctor` — the one
//! readiness check — the built binary against fake `claude` and `codex`
//! binaries on PATH (no real login anywhere) and a stand-in environment:
//! the two implicit Claude Code and Codex accounts with no config, each in the state its
//! credential puts it in; vendor checks run in parallel; the `--json`
//! shape; and no secret in any output, checked with sentinel values.

#![cfg(all(feature = "harness", unix))]

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

fn fixture(vendor: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../gruntas-harness/tests/fixtures").join(vendor).join(format!("fake-{vendor}"))
}

/// Values that must never appear in anything gruntas prints.
const KEY_SENTINEL: &str = "sk-ant-SENTINEL-4f1d9c";
const ROUTER_SENTINEL: &str = "sk-or-SENTINEL-77ab02";
const TOKEN_SENTINEL: &str = "xai-at-SENTINEL-e3c8";
const REFRESH_SENTINEL: &str = "xai-rt-SENTINEL-19d0";

struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Sandbox {
        let root = std::env::temp_dir().join(format!("gruntas-status-{name}-{}", std::process::id()));
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

    /// gruntas with only the sandbox's binaries and the system's on PATH — never
    /// a real `claude` or `codex` of the machine running the tests.
    fn gruntas(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut c = Command::new(env!("CARGO_BIN_EXE_gruntas"));
        c.args(args)
            .env_clear()
            .env("PATH", format!("{}:/usr/bin:/bin", self.root.join("bin").display()))
            .env("HOME", self.home())
            .env("GRUNTAS_NO_UPDATE_CHECK", "1")
            .env("GRUNTAS_API_URL", "http://127.0.0.1:9/v1")
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
        let dir = self.home().join(".gruntas");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.json"), v.to_string()).unwrap();
    }

    /// gruntas's provider credentials file, with logins as `providers add
    /// supergrok` would have left them.
    fn write_logins(&self, logins: Value) {
        let dir = self.home().join(".gruntas");
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
    let mut v = json!({"issuer": "https://auth.x.ai", "clientId": "gruntas-test", "tokenEndpoint": "https://auth.x.ai/oauth2/token", "accessToken": access, "expiresAtMs": expires_at_ms, "obtainedAtMs": 0});
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
fn status_with_no_config_lists_the_two_implicit_accounts_in_their_states() {
    let b = Sandbox::new("implicit");
    b.install_fakes();
    // Claude Code signed in (the fake's marker in its default ~/.claude),
    // Codex not; an Anthropic key and a SuperGrok login, neither of which
    // is an account gruntas runs (D4, D6): neither is listed.
    std::fs::create_dir_all(b.home().join(".claude")).unwrap();
    std::fs::write(b.home().join(".claude/fake-login"), "").unwrap();
    b.write_logins(json!({"supergrok": login(TOKEN_SENTINEL, None, 1_000)}));

    let out = b.gruntas(&["status", "--json"], &[("ANTHROPIC_API_KEY", KEY_SENTINEL)]);
    assert_eq!(out.status.code(), Some(0), "one instance is ready: {}", String::from_utf8_lossy(&out.stderr));
    let v = stdout_json(&out);
    let rows = instances(&v["data"]);
    let names: Vec<&str> = rows.iter().map(|r| r["instance"].as_str().unwrap()).collect();
    assert_eq!(names, ["claude", "codex"]);
    let state = |n: &str| row(&rows, n)["state"].as_str().unwrap().to_string();
    assert_eq!((state("claude"), state("codex")), ("ready".to_string(), "not_signed_in".to_string()));
    assert_eq!(v["data"]["ready"], 1);
    // The documented shape: every key on every row, null when it has no value.
    for r in &rows {
        let keys: Vec<&String> = r.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["instance", "kind", "label", "state", "ready", "source", "fix", "var", "reason"], "{r}");
        assert_eq!(r["ready"].as_bool(), Some(r["state"] == "ready"));
        assert_eq!(r["fix"].is_null(), r["state"] == "ready", "a fix exactly when it is not ready: {r}");
    }
    assert_eq!(row(&rows, "claude")["source"], format!("Claude Code's own login in {} (signed in with a Claude max subscription)", b.home().join(".claude").display()));
    assert_eq!(row(&rows, "codex")["fix"], "sign in with `gruntas connect openai --method subscription`, which runs Codex's own login");
    let printed = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(!printed.contains(KEY_SENTINEL) && !printed.contains(TOKEN_SENTINEL), "no secret is printed: {printed}");

    // The table says the same, and why each one is not ready.
    let out = b.gruntas(&["status", "--format", "human"], &[("ANTHROPIC_API_KEY", KEY_SENTINEL)]);
    assert_eq!(out.status.code(), Some(0));
    let table = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(!table.contains("supergrok") && !table.contains("anthropic "), "{table}");
    assert!(table.ends_with("1 of 2 instances ready\n"), "{table}");
    assert!(!table.contains(KEY_SENTINEL) && !table.contains(TOKEN_SENTINEL));

    // Nothing ready: every row still printed, and exit 3 with the rows as
    // the failure's details.
    std::fs::remove_file(b.home().join(".claude/fake-login")).unwrap();
    std::fs::remove_file(b.root.join("bin/codex")).unwrap();
    let out = b.gruntas(&["status", "--json"], &[]);
    assert_eq!(out.status.code(), Some(3));
    let err: Value = serde_json::from_str(String::from_utf8_lossy(&out.stderr).trim()).unwrap();
    assert_eq!(err["error"]["error"], "none_ready", "{err}");
    let details = instances(&err["error"]["details"]);
    assert_eq!((details.len(), row(&details, "codex")["state"].as_str(), row(&details, "claude")["state"].as_str()), (2, Some("not_installed"), Some("not_signed_in")));
    let out = b.gruntas(&["status", "--format", "human"], &[]);
    assert_eq!(out.status.code(), Some(3));
    assert_eq!(String::from_utf8_lossy(&out.stdout).lines().count(), 2, "the table, before the failure");
}

/// SL06 (the owner's decision on D10): `providers list --check` is the
/// check `status` is today, the same JSON and exit codes; `status` works as
/// before and says on stderr that it becomes the agent tree next release,
/// except beside a failure's JSON there.
#[test]
fn providers_list_check_is_todays_status_and_status_says_it_is_moving() {
    let b = Sandbox::new("check");
    b.install_fakes();
    std::fs::create_dir_all(b.home().join(".claude")).unwrap();
    std::fs::write(b.home().join(".claude/fake-login"), "").unwrap();
    let notice = "In the next release gruntas status becomes the agent tree (gruntas agents); gruntas providers list --check is this readiness check";
    let status = b.gruntas(&["status", "--json"], &[]);
    let check = b.gruntas(&["providers", "list", "--check", "--json"], &[]);
    assert_eq!((status.status.code(), check.status.code()), (Some(0), Some(0)));
    assert_eq!(stdout_json(&status), stdout_json(&check));
    assert!(String::from_utf8_lossy(&status.stderr).contains(notice), "{}", String::from_utf8_lossy(&status.stderr));
    assert!(!String::from_utf8_lossy(&check.stderr).contains(notice));
    let human = b.gruntas(&["status", "--format", "human"], &[]);
    assert!(String::from_utf8_lossy(&human.stderr).contains(notice));
    assert_eq!(human.stdout, b.gruntas(&["providers", "list", "--check", "--format", "human"], &[]).stdout);
    assert_eq!(stdout_json(&b.gruntas(&["providers", "--check", "--json"], &[])), stdout_json(&check), "bare providers lists as providers list");
    let elsewhere = b.gruntas(&["mcp", "list", "--check", "--json"], &[]);
    assert!(elsewhere.status.code() != Some(0) && String::from_utf8_lossy(&elsewhere.stderr).contains("only a flag of `gruntas providers list`"), "{}", String::from_utf8_lossy(&elsewhere.stderr));

    std::fs::remove_file(b.home().join(".claude/fake-login")).unwrap();
    std::fs::remove_file(b.root.join("bin/codex")).unwrap();
    for args in [&["status", "--json"][..], &["providers", "list", "--check", "--json"]] {
        let out = b.gruntas(args, &[]);
        assert_eq!(out.status.code(), Some(3), "{args:?}");
        let err: Value = serde_json::from_str(String::from_utf8_lossy(&out.stderr).trim()).unwrap();
        assert_eq!(err["error"]["error"], "none_ready", "{err}");
    }
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
    let out = b.gruntas(&["status", "--json"], &[]);
    let took = started.elapsed();
    let rows = instances(&stdout_json(&out)["data"]);
    assert_eq!((row(&rows, "claude")["state"].as_str(), row(&rows, "codex")["state"].as_str()), (Some("ready"), Some("ready")), "{rows:?}");
    assert!(took >= Duration::from_secs(5) && took < Duration::from_secs(8), "two 5 s checks took {took:?}: in parallel, about 5 s, not 10");
}

#[test]
fn r_roll_14_doctor_names_the_rollover_order_entries_rollover_ignores() {
    let b = Sandbox::new("rollover-doctor");
    b.install_fakes();
    // `auto` with an empty order is a valid config: doctor passes it.
    b.write_config(json!({"rollover": "auto"}));
    let d = stdout_json(&b.gruntas(&["doctor", "--json"], &[]));
    assert_eq!(d["rollover"]["status"], "pass", "{d}");
    b.write_config(json!({"rollover": "auto", "rolloverOrder": ["claude", "claude/sonnet", "anthropic"]}));
    let d = stdout_json(&b.gruntas(&["doctor", "--json"], &[]));
    let said = d["rollover"]["message"].as_str().unwrap_or_default();
    assert_eq!(d["rollover"]["status"], "warn", "{d}");
    assert!(said.contains("\"claude/sonnet\": the model") && said.contains("\"anthropic\": a "), "{said}");
}

/// R-EASE-8: doctor lists every check with its state and the visible command
/// that fixes it, and exits non-zero exactly when one failed.
#[test]
fn doctor_checks_everything_names_the_fix_and_exits_non_zero_exactly_when_a_check_failed() {
    let b = Sandbox::new("doctor-everything");
    b.install_fakes();
    let names = |d: &Value| d["checks"].as_array().unwrap().iter().map(|c| c["name"].as_str().unwrap().to_string()).collect::<Vec<_>>();
    let check = |d: &Value, name: &str| d["checks"].as_array().unwrap().iter().find(|c| c["name"] == name).cloned().unwrap_or_else(|| panic!("no {name} in {d}"));

    // No account, and not chosen to go without: it fails, and says how to sign in.
    let out = b.gruntas(&["doctor", "--json"], &[]);
    let d = stdout_json(&out);
    assert_eq!(names(&d), ["account", "machine", "always_on", "providers", "trust", "recovery_kit", "plan", "store", "pricing", "rollover", "sandbox"], "{d}");
    assert_eq!((check(&d, "account")["status"].as_str(), check(&d, "account")["fix"].as_str()), (Some("fail"), Some("gruntas login")), "{d}");
    assert!(!out.status.success(), "a failed check is a non-zero exit");
    let human = b.gruntas(&["doctor", "--format", "human"], &[]);
    assert!(String::from_utf8_lossy(&human.stdout).contains("✗ Not signed in to gruntas — gruntas login"), "{}", String::from_utf8_lossy(&human.stdout));
    assert!(!human.status.success());

    // Using gruntas locally: the account step is skipped, nothing needs fixing.
    std::fs::create_dir_all(b.home().join(".gruntas")).unwrap();
    std::fs::write(b.home().join(".gruntas/first-run.json"), "{\"local\": true}\n").unwrap();
    let out = b.gruntas(&["doctor", "--json"], &[]);
    let d = stdout_json(&out);
    assert!(d["checks"].as_array().unwrap().iter().all(|c| c["status"] != "fail"), "{d}");
    assert_eq!(check(&d, "account")["status"], "skip", "{d}");
    assert!(out.status.success(), "no failed check, exit 0: {d}");

    // A key in GRUNTAS_TOKEN (CI, an agent): signed in, and no machine of the
    // person's, so nothing about registration or the service is asked.
    let out = b.gruntas(&["doctor", "--json"], &[("GRUNTAS_TOKEN", "krowk_sk_ci#a")]);
    let d = stdout_json(&out);
    assert_eq!((check(&d, "account")["status"].as_str(), check(&d, "machine")["status"].as_str(), check(&d, "always_on")["status"].as_str()), (Some("pass"), Some("skip"), Some("skip")), "{d}");
    assert!(out.status.success(), "{d}");

    // Signed in with a key, on a machine never registered: that fails with
    // its fix, and the service has no machine to keep on.
    std::fs::write(b.home().join(".gruntas/credentials.json"), json!({"version": 1, "default": "ws_t", "workspaces": {"ws_t": {"token": "krowk_sk_doctor#a", "key_id": "key_t", "workspace": "ws_t"}}}).to_string()).unwrap();
    let out = b.gruntas(&["doctor", "--json"], &[("GRUNTAS_API_URL", "http://127.0.0.1:9")]);
    let d = stdout_json(&out);
    assert_eq!(check(&d, "account")["status"], "pass", "{d}");
    assert_eq!((check(&d, "machine")["status"].as_str(), check(&d, "machine")["fix"].as_str()), (Some("fail"), Some("gruntas login")), "{d}");
    assert_eq!(check(&d, "always_on")["status"], "skip", "{d}");
    assert!(!out.status.success());
}

#[test]
fn no_secret_reaches_status_list_or_doctor() {
    let b = Sandbox::new("secrets");
    b.install_fakes();
    b.write_config(json!({"instances": {
        "claude:router": {"kind": "claude-code", "env": {"ANTHROPIC_BASE_URL": "https://router.example/api"}, "apiKeyEnv": "OR_KEY"},
        "codex:router": {"kind": "codex-app-server", "args": ["-c", "model_provider=openrouter"], "apiKeyEnv": "OR_KEY"},
        "anthropic:work": {"kind": "anthropic-api", "apiKeyEnv": "WORK_KEY"},
        "supergrok:team": {"kind": "xai-oauth"},
    }}));
    // A SuperGrok login left from before: never listed, never printed.
    b.write_logins(json!({"supergrok:team": login(TOKEN_SENTINEL, Some(REFRESH_SENTINEL), 1_000)}));
    let env = [("ANTHROPIC_API_KEY", KEY_SENTINEL), ("WORK_KEY", KEY_SENTINEL), ("OR_KEY", ROUTER_SENTINEL)];

    let status = b.gruntas(&["status", "--json"], &env);
    let rows = instances(&stdout_json(&status)["data"]);
    assert_eq!(rows.len(), 5, "the two implicit accounts and the three configured; SuperGrok runs nothing (D4)");
    assert!(!rows.iter().any(|r| r["instance"] == "supergrok:team"));
    assert_eq!(row(&rows, "claude:router")["source"], "env OR_KEY, handed to Claude Code");
    assert_eq!(row(&rows, "codex:router")["source"], "env OR_KEY, handed to Codex");
    assert_eq!(row(&rows, "anthropic:work")["source"], "env WORK_KEY, handed to Claude Code", "converted on first use");
    // R-NAT-4: an account on a key reads as one, not as a subscription.
    let labels: Vec<&str> = ["claude", "claude:router", "codex:router", "anthropic:work"].iter().map(|n| rows.iter().find(|r| r["instance"] == *n).unwrap()["label"].as_str().unwrap()).collect();
    assert_eq!(labels, ["Claude subscription", "Claude Code, API key", "Codex, API key", "Claude Code, API key"]);

    let list = b.gruntas(&["providers", "list", "--json"], &env);
    let listed = instances(&stdout_json(&list)["data"]);
    for r in &rows {
        let l = row(&listed, r["instance"].as_str().unwrap());
        assert_eq!((&l["state"], &l["source"], &l["fix"]), (&r["state"], &r["source"], &r["fix"]), "providers list and status agree");
    }
    // The harness build's doctor names each instance's state.
    let doctor = b.gruntas(&["doctor", "--json"], &env);
    let d = stdout_json(&doctor);
    assert_eq!(d["providers"]["status"], "pass", "{d}");
    assert_eq!(d["providers"]["instances"]["codex"], "not_signed_in");

    for out in [&status, &list, &doctor, &b.gruntas(&["status", "--format", "human"], &env), &b.gruntas(&["providers", "list", "--format", "human"], &env), &b.gruntas(&["doctor", "--format", "human"], &env)] {
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
    let out = b.gruntas(&["status", "--json"], &[]);
    let rows = instances(&stdout_json(&out)["data"]);
    assert_eq!((row(&rows, "codex")["state"].as_str(), row(&rows, "codex")["source"].as_str()), (Some("ready"), Some(format!("Codex's own login in {} (signed in with ChatGPT)", b.home().join(".codex").display()).as_str())));
}

#[test]
fn status_asks_vendors_in_gruntas_own_directory_so_planted_settings_change_nothing() {
    let b = Sandbox::new("planted");
    // Signed in only where its working directory's project settings say so.
    b.install("claude", "#!/bin/bash\nif [ -f \"$PWD/.claude/settings.json\" ]; then echo '{\"loggedIn\":true,\"authMethod\":\"bedrock\"}'; exit 0; fi\necho '{\"loggedIn\":false}'; exit 1\n");
    // Settings planted in a shared temporary directory, and in the
    // repository gruntas runs in, which nobody trusted.
    let tmp = b.root.join("tmp");
    for d in [&tmp, &b.root.join("repo")] {
        std::fs::create_dir_all(d.join(".claude")).unwrap();
        std::fs::write(d.join(".claude/settings.json"), r#"{"apiKeyHelper":"echo planted"}"#).unwrap();
    }
    let out = b.gruntas(&["status", "--json"], &[("TMPDIR", tmp.to_str().unwrap())]);
    assert_eq!(out.status.code(), Some(3), "nothing is ready: {}", String::from_utf8_lossy(&out.stderr));
    let err: Value = serde_json::from_str(String::from_utf8_lossy(&out.stderr).trim()).unwrap();
    let rows = instances(&err["error"]["details"]);
    assert_eq!(row(&rows, "claude")["state"], "not_signed_in", "{rows:?}");
    let own = b.home().join(".gruntas/readiness");
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(&own).unwrap().permissions().mode() & 0o777, 0o700, "gruntas's own directory, closed to others");
}
