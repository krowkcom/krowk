//! Routing a bare `--model`, and no model at all, to a Claude Code or
//! Codex account ready here: the built binary against fake `claude` and
//! `codex` binaries on PATH (scripts; no real key or login anywhere). gruntas
//! runs no model itself (D4): `-p` runs on each kind of account — a
//! subscription or an API key, in Claude Code or Codex — and only there.
//! Only a Claude subscription signed in, `sonnet` and no model at all run
//! on `claude`; with a keyed account too, gruntas refuses to guess between
//! them (`ambiguous_model`) until the session's instance or
//! `defaultModel`'s decides; with nothing connected, or a provider gruntas no
//! longer runs, the refusal is the one line; and an explicit
//! `<instance>/<model>` is never rerouted. Vendors are asked in one pass
//! that the turn's own check reads from the cache, and never in a
//! repository nobody trusted.

#![cfg(all(feature = "harness", unix))]


use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    /// A home, a repository, and the fake `claude` first on PATH —
    /// signed in to the default account (`~/.claude`) when `signed_in`.
    fn new(name: &str, signed_in: bool) -> Sandbox {
        let root = std::env::temp_dir().join(format!("gruntas-routing-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["home/.claude", "repo/.git", "bin"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        std::fs::write(root.join("repo/README.md"), "# gruntas\n").unwrap();
        let bin = root.join("bin/claude");
        // Linked, not copied: a copy is a file open for writing that a test
        // forking beside it can inherit, and running it then fails with
        // "Text file busy" (ETXTBSY) — read as a vendor that could not be checked.
        std::os::unix::fs::symlink(Path::new(env!("CARGO_MANIFEST_DIR")).join("../gruntas-harness/tests/fixtures/claude/fake-claude"), &bin).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink(Path::new(env!("CARGO_MANIFEST_DIR")).join("../gruntas-harness/tests/fixtures/codex/fake-codex"), root.join("bin/codex")).unwrap();
        if signed_in {
            std::fs::write(root.join("home/.claude/fake-login"), "").unwrap();
        }
        let b = Sandbox { root: root.canonicalize().unwrap() };
        b.config(&json!({}));
        b
    }

    fn gruntas(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut c = Command::new(env!("CARGO_BIN_EXE_gruntas"));
        c.args(args)
            .env_clear()
            .env("PATH", format!("{}:{}", self.root.join("bin").display(), std::env::var("PATH").unwrap_or_default()))
            .env("HOME", self.root.join("home"))
            .env("GRUNTAS_NO_UPDATE_CHECK", "1")
            .env("FAKE_CLAUDE_LOG", self.root.join("fake.log"))
            .env("FAKE_CODEX_LOG", self.root.join("fake.log"))
            .current_dir(self.root.join("repo"))
            .stdin(Stdio::null());
        for (k, v) in env {
            c.env(k, v);
        }
        c.output().unwrap()
    }

    /// `gruntas -p hi <args> --output-format json`, which must succeed: its
    /// result.
    fn run(&self, args: &[&str], env: &[(&str, &str)]) -> Value {
        let mut all = vec!["-p", "hi", "--output-format", "json"];
        all.extend_from_slice(args);
        let out = self.gruntas(&all, env);
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(out.status.success(), "gruntas {all:?}: {stdout}{}", String::from_utf8_lossy(&out.stderr));
        serde_json::from_str(stdout.trim()).unwrap_or_else(|e| panic!("gruntas {all:?}: {e}: {stdout}"))
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

    /// gruntas's config: `v`. The implicit `codex` is the fake on PATH,
    /// signed in to nothing, so a Codex installed on the machine running
    /// the tests is never asked.
    fn config(&self, v: &Value) {
        std::fs::create_dir_all(self.root.join("home/.gruntas")).unwrap();
        std::fs::write(self.root.join("home/.gruntas/config.json"), v.to_string()).unwrap();
    }

    /// gruntas's config: `v`, with the implicit `codex` the fake one, signed
    /// in to ChatGPT in its default home (`~/.codex`).
    fn config_with_codex(&self, v: &Value) {
        let mut v = v.clone();
        v["instances"]["codex"] = json!({"kind": "codex-app-server", "binary": self.root.join("bin/codex").display().to_string()});
        std::fs::create_dir_all(self.root.join("home/.codex")).unwrap();
        std::fs::write(self.root.join("home/.codex/fake-login"), "chatgpt person@example.com\n").unwrap();
        std::fs::create_dir_all(self.root.join("home/.gruntas")).unwrap();
        std::fs::write(self.root.join("home/.gruntas/config.json"), v.to_string()).unwrap();
    }

    /// An account's own directory in gruntas's home, as `gruntas connect` makes it.
    fn account(&self, name: &str) -> String {
        let dir = self.root.join("home/.gruntas/accounts").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        dir.display().to_string()
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

    // Untrusted and headless: routing asks the vendor in gruntas's own
    // directory, never the repository, and the turn is refused before
    // anything runs there.
    b.clear_log();
    let out = b.gruntas(&["-p", "hi", "--model", "sonnet"], &[]);
    assert_eq!(out.status.code(), Some(4), "untrusted_directory: {}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stderr).contains("trust"), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(b.status_checks(), vec![b.root.join("home/.gruntas/readiness")], "{}", b.fake_log());
    assert!(b.turns().is_empty(), "Claude Code never ran a turn in an untrusted repository");
}

/// The line D4 refuses with.
const NO_DIRECT: &str = "gruntas no longer runs models directly; connect a Claude Code or Codex account.";

/// A Claude Code account on an API key, as `gruntas connect anthropic
/// --method api-key` makes it: its own directory, the key handed over.
fn keyed_claude(b: &Sandbox) -> Value {
    json!({"kind": "claude-code", "configDir": b.account("anthropic"), "apiKeyEnv": "ANTHROPIC_API_KEY"})
}

#[test]
fn with_a_keyed_account_and_a_subscription_a_bare_id_is_refused_as_ambiguous_until_a_default_or_the_session_decides() {
    let b = Sandbox::new("both", true);
    let key = [("ANTHROPIC_API_KEY", "sk-ant-test")];
    b.config(&json!({"instances": {"anthropic": keyed_claude(&b)}}));

    // Both are Claude Code accounts that serve Claude models: gruntas does
    // not choose between a key and a subscription, with a model or without.
    for args in [&["--model", "sonnet", "--trust"][..], &["--model", "claude-sonnet-4-6", "--trust"], &["--trust"]] {
        let mut all = vec!["-p", "hi"];
        all.extend_from_slice(args);
        let out = b.gruntas(&all, &key);
        let err = fail(&out, "ambiguous_model");
        assert_eq!(out.status.code(), Some(1), "{err}");
        assert!(err.contains("\n  anthropic, ") && err.contains(": --model anthropic/") && err.contains("\n  claude, Claude subscription: --model claude/"), "{err}");
    }
    assert!(b.turns().is_empty(), "nothing ran");

    // A resumed session's own instance decides a bare id.
    let first = b.run(&["--model", "claude/sonnet", "--trust"], &key);
    let id = first["sessionId"].as_str().unwrap();
    assert_eq!(b.ran_on(&["--resume", id, "--model", "haiku", "--trust"], &key), "claude/haiku");

    // So does defaultModel's instance, and the default itself, named with
    // its instance, is never rerouted.
    b.config(&json!({"defaultModel": "claude/opus", "instances": {"anthropic": keyed_claude(&b)}}));
    assert_eq!(b.ran_on(&["--model", "claude-sonnet-4-6", "--trust"], &key), "claude/claude-sonnet-4-6");
    assert_eq!(b.ran_on(&["--trust"], &key), "claude/opus");
    b.config(&json!({"defaultModel": "anthropic/claude-opus-5-5", "instances": {"anthropic": keyed_claude(&b)}}));
    b.clear_log();
    assert_eq!(b.ran_on(&["--model", "sonnet", "--trust"], &key), "anthropic/sonnet", "Claude Code on a key takes its own alias too");
    assert_eq!(b.ran_on(&["--trust"], &key), "anthropic/claude-opus-5-5");
    assert!(b.status_checks().is_empty(), "the default's account is ready by its key: nothing asked of Claude Code: {}", b.fake_log());
    assert!(b.fake_log().contains("env ANTHROPIC_API_KEY=set"), "the key, handed to Claude Code: {}", b.fake_log());
    // defaultModel's instance counts only when it is ready: without the
    // key, the one account that is runs it.
    assert_eq!(b.ran_on(&["--model", "sonnet", "--trust"], &[]), "claude/sonnet");
}

/// R-NAT-3: `-p` runs on a Codex account, on a ChatGPT subscription or an
/// API key, as it does on Claude Code.
#[test]
fn r_nat_3_gruntas_p_runs_on_a_codex_subscription_and_a_codex_account_on_a_key() {
    let b = Sandbox::new("codex", false);
    let home = b.account("openai");
    // How the fake tells a key's account from a ChatGPT login.
    std::fs::write(Path::new(&home).join("fake-login"), "apiKey\n").unwrap();
    let keyed = json!({"kind": "codex-app-server", "binary": b.root.join("bin/codex").display().to_string(), "codexHome": home, "apiKeyEnv": "OPENAI_API_KEY"});
    b.config_with_codex(&json!({"instances": {"openai": keyed}}));
    let key = [("OPENAI_API_KEY", "sk-openai-test")];
    assert_eq!(b.ran_on(&["--model", "codex/gpt-5.5", "--trust"], &key), "codex/gpt-5.5");
    b.clear_log();
    assert_eq!(b.ran_on(&["--model", "openai/gpt-5.4", "--trust"], &key), "openai/gpt-5.4");
    assert!(b.fake_log().contains("env OPENAI_API_KEY=sk-openai-test"), "the key, handed to Codex: {}", b.fake_log());
    // A bare GPT on the subscription alone, once the key is gone.
    assert_eq!(b.ran_on(&["--model", "gpt-5.5", "--trust"], &[]), "codex/gpt-5.5");
}

/// The failure's fix, as `gruntas -p` writes it to a stderr that is not a
/// terminal, which must carry `code`.
fn fail(out: &Output, code: &str) -> String {
    let err = String::from_utf8_lossy(&out.stderr);
    let v: Value = serde_json::from_str(err.trim()).unwrap_or_else(|e| panic!("{e}: {err}"));
    assert_eq!(v["error"]["error"], code, "{v}");
    v["error"]["fix"].as_str().unwrap().to_string()
}

/// R-NAT-3, D4: with no Claude Code or Codex account connected, `-p` is
/// refused with the one line — an API key in the environment alone is not
/// an account gruntas runs.
#[test]
fn with_nothing_connected_the_refusal_is_the_one_line() {
    let b = Sandbox::new("none", false);
    for env in [&[][..], &[("ANTHROPIC_API_KEY", "sk-ant-test"), ("OPENAI_API_KEY", "sk-test"), ("XAI_API_KEY", "xai-test")]] {
        let out = b.gruntas(&["-p", "hi"], env);
        let err = fail(&out, "none_ready");
        assert_eq!(out.status.code(), Some(3), "none_ready is the missing-login class: {err}");
        assert_eq!(err, NO_DIRECT, "{env:?}");
    }

    // An account the person connected, not ready, says what it needs
    // rather than to connect one.
    b.config(&json!({"instances": {"claude:work": {"kind": "claude-code", "configDir": b.account("claude-work")}}}));
    let err = fail(&b.gruntas(&["-p", "hi"], &[]), "none_ready");
    assert!(err.starts_with("no connected account is ready to run a model (no --model, and config names no defaultModel). What each one needs:") && err.contains("\n  claude:work, Claude subscription (not signed in): "), "{err}");
    // So does one connected under a built-in name: an API key's account.
    b.config(&json!({"instances": {"anthropic": keyed_claude(&b)}}));
    let err = fail(&b.gruntas(&["-p", "hi"], &[]), "none_ready");
    assert!(err.contains("\n  anthropic, Claude Code, API key (key not set): "), "{err}");
    b.config(&json!({}));

    // A bare id names only the accounts of its own line, and how to
    // connect one.
    let out = b.gruntas(&["-p", "hi", "--model", "sonnet"], &[]);
    let err = fail(&out, "none_ready");
    assert_eq!(out.status.code(), Some(3), "{err}");
    assert!(err.starts_with("no connected account serves Claude models (asked for \"sonnet\") — run `gruntas connect anthropic --method api-key` (Anthropic API key) or `gruntas connect anthropic --method subscription` (Claude subscription)"), "{err}");
    assert!(!err.contains("openai") && !err.contains("anthropic,") && err.contains("\n  claude, Claude subscription (not signed in): sign in with"), "{err}");
    let out = b.gruntas(&["-p", "hi", "--model", "grok-4.7"], &[]);
    let err = fail(&out, "none_ready");
    assert!(!err.to_lowercase().contains("supergrok") && err.contains("`gruntas connect xai --method api-key`"), "{err}");
    // Neither run spawned anything but the status checks, and those in
    // gruntas's own directory.
    assert!(b.turns().is_empty());
    assert!(b.status_checks().iter().all(|d| d.ends_with(".gruntas/readiness")), "{}", b.fake_log());
}

/// R-NAT-3, D4: a provider gruntas no longer runs — named by `--model`, by
/// `defaultModel` or by a SuperGrok instance in config.json — is refused
/// with the same line, and nothing else is asked or run in its place.
#[test]
fn a_removed_provider_is_refused_with_the_one_line_and_never_rerouted() {
    let b = Sandbox::new("removed", true);
    let refused = |args: &[&str], env: &[(&str, &str)]| {
        let mut all = vec!["-p", "hi", "--trust"];
        all.extend_from_slice(args);
        let out = b.gruntas(&all, env);
        let err = fail(&out, "removed_provider");
        assert_eq!(out.status.code(), Some(3), "{args:?}: {err}");
        assert_eq!(err, NO_DIRECT, "{args:?}");
    };
    refused(&["--model", "anthropic/claude-opus-5-5"], &[("ANTHROPIC_API_KEY", "sk-ant-test")]);
    refused(&["--model", "supergrok/grok-4.7"], &[]);
    b.config(&json!({"defaultModel": "xai/grok-4.7"}));
    refused(&[], &[("XAI_API_KEY", "xai-test")]);
    b.config(&json!({"defaultModel": "grok:team/grok-4.7", "instances": {"grok:team": {"kind": "xai-oauth"}}}));
    refused(&[], &[]);
    refused(&["--model", "grok:team/grok-4.7"], &[]);
    assert!(b.turns().is_empty(), "claude, signed in, never ran in its place: {}", b.fake_log());
}

#[test]
fn an_explicit_instance_is_never_rerouted_even_when_only_another_one_is_ready() {
    let b = Sandbox::new("explicit", true);
    // `codex` is not signed in here; `claude` is.
    let out = b.gruntas(&["-p", "hi", "--model", "codex/gpt-5.5", "--trust"], &[]);
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(out.status.code(), Some(3), "{err}");
    assert!(err.contains("Codex is not signed in for the codex instance"), "the codex instance's own refusal: {err}");
    assert!(b.turns().is_empty(), "claude never ran in its place: {}", b.fake_log());
}
