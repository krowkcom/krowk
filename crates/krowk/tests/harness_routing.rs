//! Routing a bare `--model`, and no model at all, to an instance ready
//! here: the built binary against the stand-in Anthropic API and a fake
//! `claude` on PATH (a script; no real key or login anywhere). Only a
//! Claude subscription signed in, `sonnet` and no model at all run on
//! `claude`; with an API key too, krowk refuses to guess between them
//! (`ambiguous_model`) until the session's instance or `defaultModel`'s
//! decides; with nothing connected, the refusal names every instance that
//! could and `krowk connect`; and an explicit `<instance>/<model>` is never
//! rerouted. Vendors are asked in one pass that the turn's own check reads
//! from the cache, and never in a repository nobody trusted.

#![cfg(all(feature = "harness", unix))]

#[path = "../../krowk-harness/tests/common/mock.rs"]
mod mock;

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};

struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    /// A home, a repository, and the fake `claude` first on PATH —
    /// signed in to the default account (`~/.claude`) when `signed_in`.
    fn new(name: &str, signed_in: bool) -> Sandbox {
        let root = std::env::temp_dir().join(format!("krowk-routing-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["home/.claude", "repo/.git", "bin"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        std::fs::write(root.join("repo/README.md"), "# krowk\n").unwrap();
        let bin = root.join("bin/claude");
        std::fs::copy(Path::new(env!("CARGO_MANIFEST_DIR")).join("../krowk-harness/tests/fixtures/claude/fake-claude"), &bin).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        if signed_in {
            std::fs::write(root.join("home/.claude/fake-login"), "").unwrap();
        }
        let b = Sandbox { root: root.canonicalize().unwrap() };
        b.config(&json!({}));
        b
    }

    fn krowk(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut c = Command::new(env!("CARGO_BIN_EXE_krowk"));
        c.args(args)
            .env_clear()
            .env("PATH", format!("{}:{}", self.root.join("bin").display(), std::env::var("PATH").unwrap_or_default()))
            .env("HOME", self.root.join("home"))
            .env("KROWK_NO_UPDATE_CHECK", "1")
            .env("FAKE_CLAUDE_LOG", self.root.join("fake.log"))
            .current_dir(self.root.join("repo"))
            .stdin(Stdio::null());
        for (k, v) in env {
            c.env(k, v);
        }
        c.output().unwrap()
    }

    /// `krowk -p hi <args> --output-format json`, which must succeed: its
    /// result.
    fn run(&self, args: &[&str], env: &[(&str, &str)]) -> Value {
        let mut all = vec!["-p", "hi", "--output-format", "json"];
        all.extend_from_slice(args);
        let out = self.krowk(&all, env);
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(out.status.success(), "krowk {all:?}: {stdout}{}", String::from_utf8_lossy(&out.stderr));
        serde_json::from_str(stdout.trim()).unwrap_or_else(|e| panic!("krowk {all:?}: {e}: {stdout}"))
    }

    /// Where the result says the turn ran: `<instance>/<model>`.
    fn ran_on(&self, args: &[&str], env: &[(&str, &str)]) -> String {
        let r = self.run(args, env);
        assert_eq!(r["status"], "completed", "{r}");
        format!("{}/{}", r["model"]["instance"].as_str().unwrap(), r["model"]["model"].as_str().unwrap())
    }

    fn fake_log(&self) -> String {
        std::fs::read_to_string(self.root.join("fake.log")).unwrap_or_default()
    }

    fn clear_log(&self) {
        let _ = std::fs::remove_file(self.root.join("fake.log"));
    }

    /// The working directory of every `claude auth status` the fake ran.
    fn status_checks(&self) -> Vec<PathBuf> {
        let log = self.fake_log();
        let lines: Vec<&str> = log.lines().collect();
        lines
            .iter()
            .enumerate()
            .filter(|(_, l)| **l == "argv auth status --json")
            .map(|(i, _)| lines[i..].iter().find_map(|l| l.strip_prefix("cwd ")).map(PathBuf::from).expect("the fake logs its cwd"))
            .collect()
    }

    fn repo(&self) -> PathBuf {
        self.root.join("repo")
    }

    /// krowk's config: `v`, with the implicit `codex` pointed at a binary
    /// that is not there, so a Codex installed on the machine running the
    /// tests is never asked.
    fn config(&self, v: &Value) {
        let mut v = v.clone();
        v["instances"]["codex"] = json!({"kind": "codex-app-server", "binary": self.root.join("bin/no-codex").display().to_string()});
        std::fs::create_dir_all(self.root.join("home/.config/krowk")).unwrap();
        std::fs::write(self.root.join("home/.config/krowk/config.json"), v.to_string()).unwrap();
    }

    /// The argv of every turn the fake ran.
    fn turns(&self) -> Vec<String> {
        self.fake_log().lines().filter_map(|l| l.strip_prefix("argv -p ")).map(String::from).collect()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// The stand-in Messages API, answering every call and keeping the model
/// each asked for.
fn api() -> (mock::Mock, Arc<Mutex<Vec<String>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s2 = seen.clone();
    let m = mock::serve(move |body, _| {
        s2.lock().unwrap().push(body["model"].as_str().unwrap_or_default().to_string());
        mock::Reply::sse(&mock::text_stream("the API answered"))
    });
    (m, seen)
}

#[test]
fn only_claude_signed_in_a_bare_sonnet_and_no_model_at_all_run_on_claude_asking_it_once() {
    let b = Sandbox::new("claude-only", true);
    assert_eq!(b.ran_on(&["--model", "sonnet", "--trust"], &[]), "claude/sonnet", "Claude Code takes its own alias");
    assert!(b.turns().iter().all(|t| t.contains("--model sonnet")) && b.turns().len() == 1, "{}", b.fake_log());
    // One readiness pass, in the trusted repository where the turn runs;
    // the turn's own check before it starts is the cache's.
    assert_eq!(b.status_checks(), vec![b.repo()], "{}", b.fake_log());

    b.clear_log();
    assert_eq!(b.ran_on(&["--trust"], &[]), "claude/claude-opus-5-5", "no --model and no defaultModel: the first ready instance's default");
    assert!(b.turns()[0].contains("--model claude-opus-5-5"), "{}", b.fake_log());

    // Untrusted and headless: routing asks the vendor in krowk's own
    // directory, never the repository, and the turn is refused before
    // anything runs there.
    b.clear_log();
    let out = b.krowk(&["-p", "hi", "--model", "sonnet"], &[]);
    assert_eq!(out.status.code(), Some(4), "untrusted_directory: {}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stderr).contains("trust"), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(b.status_checks(), vec![b.root.join("home/.local/share/krowk/readiness")], "{}", b.fake_log());
    assert!(b.turns().is_empty(), "Claude Code never ran a turn in an untrusted repository");
}

#[test]
fn with_a_key_and_a_subscription_a_bare_id_is_refused_as_ambiguous_until_a_default_or_the_session_decides() {
    let (m, seen) = api();
    let b = Sandbox::new("both", true);
    let key = [("ANTHROPIC_API_KEY", "sk-ant-test"), ("ANTHROPIC_BASE_URL", m.url.as_str())];

    // Both serve Claude models: krowk does not choose between a key and a
    // subscription, with a model or without one.
    for args in [&["--model", "sonnet", "--trust"][..], &["--model", "claude-sonnet-4-6", "--trust"], &["--trust"]] {
        let mut all = vec!["-p", "hi"];
        all.extend_from_slice(args);
        let out = b.krowk(&all, &key);
        let err = fail(&out, "ambiguous_model");
        assert_eq!(out.status.code(), Some(1), "{err}");
        assert!(err.contains("\n  anthropic, Anthropic API key: --model anthropic/") && err.contains("\n  claude, Claude subscription: --model claude/"), "{err}");
        assert!(err.contains("`krowk connect <vendor> --default`"), "{err}");
    }
    let out = b.krowk(&["-p", "hi", "--model", "sonnet", "--trust"], &key);
    assert!(fail(&out, "ambiguous_model").contains("claude, Claude subscription: --model claude/sonnet"));
    assert!(b.turns().is_empty() && seen.lock().unwrap().is_empty(), "nothing ran");

    // A resumed session's own instance decides a bare id.
    let first = b.run(&["--model", "claude/sonnet", "--trust"], &key);
    let id = first["sessionId"].as_str().unwrap();
    assert_eq!(b.ran_on(&["--resume", id, "--model", "haiku", "--trust"], &key), "claude/haiku");

    // So does defaultModel's instance, and the default itself, named with
    // its instance, is never rerouted.
    b.config(&json!({"defaultModel": "claude/opus"}));
    assert_eq!(b.ran_on(&["--model", "claude-sonnet-4-6", "--trust"], &key), "claude/claude-sonnet-4-6");
    assert_eq!(b.ran_on(&["--trust"], &key), "claude/opus");
    b.config(&json!({"defaultModel": "anthropic/claude-opus-5-5"}));
    b.clear_log();
    assert_eq!(b.ran_on(&["--model", "claude-sonnet-4-6"], &key), "anthropic/claude-sonnet-4-6");
    assert_eq!(b.ran_on(&[], &key), "anthropic/claude-opus-5-5");
    assert!(b.status_checks().is_empty(), "the default's instance is ready by its key: nothing asked of Claude Code: {}", b.fake_log());
    // The API takes an alias as the newest model of its family the catalog
    // lists, and says so when it lists none.
    let out = b.krowk(&["-p", "hi", "--model", "sonnet"], &key);
    assert!(fail(&out, "bad_model").contains("krowk pricing refresh"));
    let cache = b.root.join("home/.cache/krowk");
    std::fs::create_dir_all(&cache).unwrap();
    let catalog = json!({"anthropic": {"models": {
        "claude-sonnet-4-6": {"family": "claude-sonnet", "release_date": "2026-02-17", "tool_call": true},
        "claude-sonnet-5": {"family": "claude-sonnet", "release_date": "2026-08-01", "tool_call": true},
    }}});
    std::fs::write(cache.join("models.json"), catalog.to_string()).unwrap();
    assert_eq!(b.ran_on(&["--model", "Sonnet"], &key), "anthropic/claude-sonnet-5");
    assert_eq!(seen.lock().unwrap().as_slice(), ["claude-sonnet-4-6", "claude-opus-5-5", "claude-sonnet-5"]);
    // defaultModel's instance counts only when it is ready: without the
    // key, the one instance that is runs it.
    assert_eq!(b.ran_on(&["--model", "sonnet", "--trust"], &[]), "claude/sonnet");
}

/// The failure's fix, as `krowk -p` writes it to a stderr that is not a
/// terminal, which must carry `code`.
fn fail(out: &Output, code: &str) -> String {
    let err = String::from_utf8_lossy(&out.stderr);
    let v: Value = serde_json::from_str(err.trim()).unwrap_or_else(|e| panic!("{e}: {err}"));
    assert_eq!(v["error"]["error"], code, "{v}");
    v["error"]["fix"].as_str().unwrap().to_string()
}

#[test]
fn with_nothing_connected_the_refusal_names_each_instance_and_krowk_connect() {
    let b = Sandbox::new("none", false);
    let out = b.krowk(&["-p", "hi"], &[]);
    let err = fail(&out, "none_ready");
    assert_eq!(out.status.code(), Some(3), "none_ready is the missing-login class: {err}");
    assert!(err.starts_with("no connected instance can run a model here (no --model, and config names no defaultModel) — run `krowk connect anthropic` (Anthropic API key), `krowk connect claude` (Claude subscription)"), "{err}");
    assert!(err.contains("`krowk connect codex` (ChatGPT subscription)") && err.contains("`krowk connect supergrok` (SuperGrok)"), "{err}");
    assert!(err.contains("\n  anthropic, Anthropic API key (key not set): set ANTHROPIC_API_KEY"), "{err}");
    assert!(err.contains("\n  claude, Claude subscription (not signed in): "), "{err}");
    for other in ["openai, OpenAI API key (key not set)", "codex, ChatGPT subscription (not installed)", "xai, xAI API key (key not set)", "supergrok, SuperGrok (not signed in)"] {
        assert!(err.contains(other), "{other} in {err}");
    }

    // A bare id names only the instances of its own line.
    let out = b.krowk(&["-p", "hi", "--model", "sonnet"], &[]);
    let err = fail(&out, "none_ready");
    assert_eq!(out.status.code(), Some(3), "{err}");
    assert!(err.starts_with("no connected instance serves Claude models (asked for \"sonnet\") — run `krowk connect anthropic` (Anthropic API key) or `krowk connect claude` (Claude subscription)"), "{err}");
    assert!(!err.contains("openai") && err.contains("\n  claude, Claude subscription (not signed in): sign in with"), "{err}");
    // Neither run spawned anything but the status checks, and those in
    // krowk's own directory.
    assert!(b.turns().is_empty());
    assert!(b.status_checks().iter().all(|d| d.ends_with("krowk/readiness")), "{}", b.fake_log());
}

#[test]
fn an_explicit_instance_is_never_rerouted_even_when_only_another_one_is_ready() {
    let b = Sandbox::new("explicit", true);
    let out = b.krowk(&["-p", "hi", "--model", "anthropic/claude-opus-5-5", "--trust"], &[]);
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(out.status.code(), Some(3), "{err}");
    assert!(err.contains("ANTHROPIC_API_KEY"), "the anthropic instance's own refusal: {err}");
    assert!(b.fake_log().is_empty(), "claude was neither asked nor run: {}", b.fake_log());

    let (m, seen) = api();
    assert_eq!(b.ran_on(&["--model", "anthropic/claude-opus-5-5"], &[("ANTHROPIC_API_KEY", "sk-ant-test"), ("ANTHROPIC_BASE_URL", m.url.as_str())]), "anthropic/claude-opus-5-5");
    assert_eq!(seen.lock().unwrap().as_slice(), ["claude-opus-5-5"]);
}
