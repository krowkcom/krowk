//! `krowk connect` and `krowk disconnect`, the built binary against fake
//! `claude` and `codex` binaries on a PATH that holds nothing else of a
//! vendor's (no real login anywhere, and the real binaries unreachable):
//! a Claude subscription connected by name into a directory of its own,
//! a failed sign-in that writes nothing, renewing against adding, the
//! default model the first connection sets, `--method` required without a
//! terminal, each kind signed out, and the vendor → method → account walk
//! at a real terminal.

#![cfg(all(feature = "harness", unix))]

#[path = "common/pty.rs"]
mod pty;

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

fn fixture(vendor: &str, name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../krowk-harness/tests/fixtures").join(vendor).join(name)
}

struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Sandbox {
        let root = std::env::temp_dir().join(format!("krowk-connect-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("home")).unwrap();
        std::fs::create_dir_all(root.join("repo/.git")).unwrap();
        std::fs::create_dir_all(root.join("bin")).unwrap();
        use std::os::unix::fs::PermissionsExt;
        for (vendor, bin) in [("claude", "claude"), ("codex", "codex")] {
            let to = root.join("bin").join(bin);
            std::fs::copy(fixture(vendor, &format!("fake-{bin}")), &to).unwrap();
            std::fs::set_permissions(&to, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        Sandbox { root: root.canonicalize().unwrap() }
    }

    fn command(&self, args: &[&str], env: &[(&str, &str)]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_krowk"));
        c.args(args)
            .env_clear()
            // The fakes and the system's own tools, and no other `claude` or
            // `codex`: the person's real ones are never reached.
            .env("PATH", format!("{}:/usr/bin:/bin", self.root.join("bin").display()))
            .env("HOME", self.root.join("home"))
            .env("KROWK_NO_UPDATE_CHECK", "1")
            .env("KROWK_API_URL", "http://127.0.0.1:9/v1")
            .env("FAKE_CLAUDE_LOG", self.root.join("fake.log"))
            .env("FAKE_CODEX_LOG", self.root.join("fake.log"))
            .current_dir(self.root.join("repo"))
            .stdin(Stdio::null());
        for (k, v) in env {
            c.env(k, v);
        }
        c
    }

    fn krowk(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        self.command(args, env).output().unwrap()
    }

    fn json(&self, args: &[&str], env: &[(&str, &str)]) -> Value {
        let out = self.krowk(args, env);
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(out.status.success(), "krowk {args:?}: {stdout}{}", String::from_utf8_lossy(&out.stderr));
        serde_json::from_str(stdout.trim()).unwrap_or_else(|e| panic!("krowk {args:?}: {e}: {stdout}"))
    }

    fn config_path(&self) -> PathBuf {
        self.root.join("home/.krowk/config.json")
    }

    fn config(&self) -> Value {
        std::fs::read_to_string(self.config_path()).map(|s| serde_json::from_str(&s).unwrap()).unwrap_or(json!({}))
    }

    fn data(&self) -> PathBuf {
        self.root.join("home/.krowk")
    }

    fn logins(&self, what: &str) -> usize {
        std::fs::read_to_string(self.root.join("fake.log")).unwrap_or_default().lines().filter(|l| *l == what).count()
    }

    fn status(&self) -> Vec<Value> {
        let out = self.krowk(&["status", "--json"], &[]);
        let v: Value = serde_json::from_slice(&out.stdout).or_else(|_| serde_json::from_slice(&out.stderr)).unwrap();
        let rows = v.pointer("/data/instances").or_else(|| v.pointer("/error/details/instances")).unwrap_or_else(|| panic!("{v}"));
        rows.as_array().unwrap().clone()
    }

    fn state(&self, instance: &str) -> String {
        self.status().iter().find(|r| r["instance"] == instance).map(|r| r["state"].as_str().unwrap().to_string()).unwrap_or_else(|| panic!("{instance} is not listed"))
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn r_inst_2_connect_a_claude_subscription_by_name_into_its_own_directory_and_a_failed_sign_in_writes_nothing() {
    let b = Sandbox::new("claude");
    let c = b.json(&["connect", "anthropic", "--method", "subscription", "--name", "work", "--json"], &[]);
    let dir = b.data().join("accounts/claude-work");
    assert_eq!((c["data"]["instance"].as_str(), c["data"]["kind"].as_str()), (Some("claude:work"), Some("claude-code")));
    assert_eq!(c["data"]["definition"]["configDir"], dir.display().to_string());
    assert_eq!((c["data"]["signed_in"].as_bool(), c["data"]["renewed"].as_bool()), (Some(true), Some(false)));
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
    assert!(dir.join("fake-login").exists(), "`claude auth login` ran in the account's own directory");
    assert_eq!(b.logins("argv auth login"), 1);
    // Claude's login starts in krowk's own empty directory, never in the
    // repository krowk runs in, whose settings nobody trusted.
    assert_eq!(b.logins(&format!("login-cwd {}", b.data().join("readiness").display())), 1);
    // The first connection is the default model.
    assert_eq!(c["data"]["default_model"], "claude:work/claude-opus-5-5");
    assert_eq!(b.config()["defaultModel"], "claude:work/claude-opus-5-5");
    assert_eq!(b.state("claude:work"), "ready");

    // A sign-in given up on writes nothing: config.json is as it was, and
    // the directory made for it is gone.
    let before = std::fs::read(b.config_path()).unwrap();
    let out = b.krowk(&["connect", "anthropic", "--method", "subscription", "--name", "broken"], &[("FAKE_CLAUDE_LOGIN", "fail")]);
    assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));
    assert!(stderr(&out).contains("claude auth login") && stderr(&out).contains("krowk connect anthropic --method subscription --name broken"), "{}", stderr(&out));
    assert_eq!(std::fs::read(b.config_path()).unwrap(), before, "config.json is untouched");
    assert!(!b.data().join("accounts/claude-broken").exists());
}

#[test]
fn r_inst_2_connecting_again_renews_the_account_and_only_a_name_adds_another() {
    let b = Sandbox::new("renew");
    b.json(&["connect", "anthropic", "--method", "subscription", "--name", "work", "--json"], &[]);
    // Again: the same account, signed in already, so Claude's login does
    // not run a second time, and the default model is left alone.
    let again = b.json(&["connect", "anthropic", "--method", "subscription", "--name", "work", "--json"], &[]);
    assert_eq!((again["data"]["renewed"].as_bool(), again["data"]["default_model"].is_null()), (Some(true), true));
    assert_eq!(b.logins("argv auth login"), 1);
    // Signed out behind krowk's back, connecting renews the login.
    std::fs::remove_file(b.data().join("accounts/claude-work/fake-login")).unwrap();
    let renewed = b.json(&["connect", "claude:work", "--json"], &[]);
    assert_eq!((renewed["data"]["instance"].as_str(), renewed["data"]["renewed"].as_bool()), (Some("claude:work"), Some(true)));
    assert_eq!(b.logins("argv auth login"), 2);

    // Without --name: the default-named account, made the first time and
    // renewed after — never a second one.
    let default = b.json(&["connect", "anthropic", "--method", "subscription", "--json"], &[]);
    assert_eq!((default["data"]["instance"].as_str(), default["data"]["renewed"].as_bool()), (Some("claude"), Some(false)));
    assert!(default["data"]["definition"].get("configDir").is_none(), "the default account is Claude Code's own directory");
    b.json(&["connect", "anthropic", "--method", "subscription", "--json"], &[]);
    let mut names: Vec<String> = b.config()["instances"].as_object().unwrap().keys().cloned().collect();
    names.sort();
    assert_eq!(names, ["claude", "claude:work"]);
    // `--name work` of the API-key method is another instance, not a clash.
    let key = b.json(&["connect", "anthropic", "--method", "api-key", "--name", "work", "--json"], &[]);
    assert_eq!((key["data"]["instance"].as_str(), key["data"]["api_key_env"].as_str()), (Some("anthropic:work"), Some("ANTHROPIC_WORK_API_KEY")));
    assert_eq!(b.config()["defaultModel"], "claude:work/claude-opus-5-5", "later connections leave the default alone");
    // … unless asked.
    let made = b.json(&["connect", "anthropic", "--method", "api-key", "--name", "work", "--default", "--json"], &[]);
    assert_eq!(made["data"]["default_model"], "anthropic:work/claude-opus-5-5");
    assert_eq!(b.config()["defaultModel"], "anthropic:work/claude-opus-5-5");
}

#[test]
fn connect_without_a_terminal_needs_the_method_when_there_is_a_choice_and_the_vendor_always() {
    let b = Sandbox::new("noterm");
    let out = b.krowk(&["connect", "anthropic"], &[]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(stderr(&out).contains("--method subscription|api-key"), "{}", stderr(&out));
    let out = b.krowk(&["connect", "openai", "--json"], &[]);
    assert!(stderr(&out).contains("--method subscription|device|api-key"), "{}", stderr(&out));
    let out = b.krowk(&["connect"], &[]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("krowk connect anthropic"), "the error names a vendor to pass: {}", stderr(&out));
    assert!(!b.config_path().exists(), "nothing was written");

    // One way in needs no flag: the key's variable is named.
    let or = b.json(&["connect", "openrouter", "--json"], &[]);
    assert_eq!((or["data"]["instance"].as_str(), or["data"]["api_key_env"].as_str()), (Some("openrouter"), Some("OPENROUTER_API_KEY")));
    let out = b.krowk(&["connect", "openrouter", "--name", "team", "--format", "human"], &[]);
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(said.contains("$OPENROUTER_TEAM_API_KEY is not set here"), "{said}");

    // An account word is not a vendor to `krowk login`, and says where to
    // go in the vendor, method and name form every fix line takes.
    for (word, want) in [
        ("anthropic", "krowk connect anthropic --method subscription"),
        ("chatgpt", "krowk connect openai --method subscription"),
        ("grok", "krowk connect xai --method subscription"),
        ("claude:work", "krowk connect anthropic --method subscription --name work"),
        // The prefix is the method: an `anthropic:` instance is a key.
        ("anthropic:work", "krowk connect anthropic --method api-key --name work"),
        ("openai:team", "krowk connect openai --method api-key --name team"),
        ("codex:team", "krowk connect openai --method subscription --name team"),
        ("xai:x", "krowk connect xai --method api-key --name x"),
        ("supergrok:x", "krowk connect xai --method subscription --name x"),
        ("grok:team", "krowk connect grok:team"),
    ] {
        let out = b.krowk(&["login", word], &[]);
        assert!(stderr(&out).contains(&format!("`{want}`")), "{word}: {}", stderr(&out));
    }
}

#[test]
fn disconnecting_your_own_claude_login_says_so_and_is_refused_without_a_terminal_unless_asked_for() {
    let b = Sandbox::new("own");
    // The person's own Claude Code, signed in, in the sandbox's ~/.claude.
    std::fs::create_dir_all(b.root.join("home/.claude")).unwrap();
    std::fs::write(b.root.join("home/.claude/fake-login"), "").unwrap();
    let out = b.krowk(&["connect", "anthropic", "--method", "subscription", "--format", "human"], &[]);
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(said.contains("signed in already") && said.contains("another account:  krowk connect anthropic --method subscription --name <new>") && !said.contains("disconnect"), "{said}");

    let out = b.krowk(&["disconnect", "claude"], &[]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(stderr(&out).contains("signs you out of Claude Code itself") && stderr(&out).contains("--sign-out-vendor"), "{}", stderr(&out));
    assert!(b.root.join("home/.claude/fake-login").exists(), "nothing was signed out");
    assert_eq!(b.logins("argv auth logout"), 0);

    let d = b.json(&["disconnect", "claude", "--sign-out-vendor", "--json"], &[]);
    assert_eq!(d["data"]["command"], "claude auth logout");
    assert!(!b.root.join("home/.claude/fake-login").exists());
}

#[test]
fn connect_refuses_a_name_that_would_share_a_login_or_a_key_or_pass_for_another_kind() {
    let b = Sandbox::new("names");
    b.json(&["connect", "anthropic", "--method", "subscription", "--name", "work", "--json"], &[]);
    let refused = |args: &[&str], why: &str| {
        let out = b.krowk(args, &[]);
        assert!(!out.status.success(), "{args:?} was taken");
        assert!(stderr(&out).contains(why), "{args:?}: {}", stderr(&out));
    };
    // `a:b` and `a-b` would be one directory.
    refused(&["connect", "anthropic", "--method", "subscription", "--name", "claude:a:b"], "holds no `:`");
    // A whole name keeps its method's prefix.
    refused(&["connect", "anthropic", "--method", "api-key", "--name", "codex:x"], "is not a anthropic name");
    // A built-in of another kind is not taken over.
    refused(&["connect", "openai-compatible", "--name", "claude", "--base-url", "http://127.0.0.1:1/v1"], "claude is already claude-code");
    // Nor a name that differs only in case.
    refused(&["connect", "anthropic", "--method", "subscription", "--name", "Work"], "only in case");
    // Nor another account's directory.
    let taken = b.data().join("accounts/claude-work").display().to_string();
    refused(&["connect", "anthropic", "--method", "subscription", "--name", "other", "--config-dir", &taken], "is claude:work's directory already");
    // Nor another instance's key variable.
    b.json(&["connect", "anthropic", "--method", "api-key", "--name", "my-work", "--json"], &[]);
    refused(&["connect", "anthropic", "--method", "api-key", "--name", "my_work"], "--api-key-env");
    let cfg = b.config();
    let names: Vec<&String> = cfg["instances"].as_object().unwrap().keys().collect();
    assert_eq!(names.len(), 2, "nothing refused was written: {names:?}");

    // A relative --config-dir is the directory it means where krowk runs,
    // kept absolute, and the login lands there.
    let c = b.json(&["connect", "anthropic", "--method", "subscription", "--name", "rel", "--config-dir", "accounts/rel", "--json"], &[]);
    let dir = b.root.join("repo/accounts/rel");
    assert_eq!(c["data"]["definition"]["configDir"], dir.display().to_string());
    assert!(dir.join("fake-login").exists());

    // A `~` the shell left alone is the home directory, never a `~` here.
    let c = b.json(&["connect", "anthropic", "--method", "subscription", "--name", "tilde", "--config-dir", "~/accounts/tilde", "--json"], &[]);
    assert_eq!(c["data"]["definition"]["configDir"], b.root.join("home/accounts/tilde").display().to_string());
    assert!(!b.root.join("repo/~").exists());

    // Your own ~/.claude is never a new account's, even while
    // CLAUDE_CONFIG_DIR names another directory.
    let own = b.root.join("home/.claude").display().to_string();
    let elsewhere = b.root.join("elsewhere").display().to_string();
    let out = b.krowk(&["connect", "anthropic", "--method", "subscription", "--name", "mine", "--config-dir", &own], &[("CLAUDE_CONFIG_DIR", &elsewhere)]);
    assert!(!out.status.success() && stderr(&out).contains("your own login's directory"), "{}", stderr(&out));
    // Nor is a defined account renewed onto it.
    let out = b.krowk(&["connect", "claude:tilde", "--config-dir", &own], &[("CLAUDE_CONFIG_DIR", &elsewhere)]);
    assert!(!out.status.success() && stderr(&out).contains("your own login's directory"), "a renew: {}", stderr(&out));
    // `~user` is the shell's to expand.
    let out = b.krowk(&["connect", "anthropic", "--method", "subscription", "--name", "other-user", "--config-dir", "~root/acct"], &[]);
    assert!(!out.status.success() && stderr(&out).contains("give the whole path"), "{}", stderr(&out));
}

#[test]
fn a_failed_codex_sign_in_leaves_a_home_that_was_there_as_it_was() {
    let b = Sandbox::new("codex-existing");
    // The person's own Codex configuration, which a new account's home links.
    std::fs::create_dir_all(b.root.join("home/.codex")).unwrap();
    std::fs::write(b.root.join("home/.codex/config.toml"), "model = \"gpt-5.5\"\n").unwrap();
    std::fs::write(b.root.join("home/.codex/AGENTS.md"), "be brief\n").unwrap();
    let there = b.root.join("accounts/team");
    std::fs::create_dir_all(&there).unwrap();
    let out = b.krowk(&["connect", "openai", "--method", "subscription", "--name", "team", "--config-dir", &there.display().to_string()], &[("FAKE_CODEX_LOGIN", "fail")]);
    assert!(!out.status.success(), "the login gives up: {}", stderr(&out));
    let left: Vec<_> = std::fs::read_dir(&there).unwrap().flatten().map(|e| e.file_name()).collect();
    assert!(left.is_empty(), "nothing was linked into it: {left:?}");
}

#[test]
fn disconnect_signs_each_kind_out_its_own_way_and_keeps_the_definition_unless_removed() {
    let b = Sandbox::new("disconnect");
    b.json(&["connect", "anthropic", "--method", "subscription", "--name", "work", "--json"], &[]);
    b.json(&["connect", "openai", "--method", "subscription", "--name", "team", "--json"], &[]);
    b.json(&["connect", "anthropic", "--method", "api-key", "--name", "work", "--json"], &[]);
    let creds = b.root.join("home/.krowk");
    std::fs::create_dir_all(&creds).unwrap();
    let token = json!({"issuer": "https://auth.x.ai", "clientId": "krowk-test", "tokenEndpoint": "https://auth.x.ai/oauth2/token", "accessToken": "xai-at-sentinel", "obtainedAtMs": 0});
    std::fs::write(creds.join("credentials.json"), json!({"version": 1, "instances": {"supergrok": token}}).to_string()).unwrap();
    assert_eq!((b.state("claude:work"), b.state("codex:team"), b.state("supergrok")), ("ready".into(), "ready".into(), "ready".into()));

    // Never a guess: with no terminal and no instance, the instances are listed.
    let out = b.krowk(&["disconnect"], &[]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("claude:work") && stderr(&out).contains("codex:team"), "{}", stderr(&out));

    // A subscription runs the vendor's own logout in its own directory; the
    // definition stays and is not signed in.
    let d = b.json(&["disconnect", "claude:work", "--json"], &[]);
    assert_eq!((d["data"]["signed_out"].as_str(), d["data"]["command"].as_str(), d["data"]["removed_definition"].as_bool()), (Some("vendor"), Some("claude auth logout"), Some(false)));
    assert!(!b.data().join("accounts/claude-work/fake-login").exists());
    assert_eq!(b.state("claude:work"), "not_signed_in");
    let d = b.json(&["disconnect", "codex:team", "--json"], &[]);
    assert_eq!(d["data"]["command"], "codex logout");
    assert_eq!(b.state("codex:team"), "not_signed_in");
    // SuperGrok's tokens are krowk's own, and deleted.
    let d = b.json(&["disconnect", "supergrok", "--json"], &[]);
    assert_eq!((d["data"]["signed_out"].as_str(), d["data"]["had_login"].as_bool()), (Some("tokens"), Some(true)));
    assert!(!std::fs::read_to_string(creds.join("credentials.json")).unwrap().contains("xai-at-sentinel"));
    assert_eq!(b.state("supergrok"), "not_signed_in");
    // An API key is the environment's: the variable is named, nothing run.
    let out = b.krowk(&["disconnect", "anthropic:work", "--format", "human"], &[]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("$ANTHROPIC_WORK_API_KEY"), "{}", String::from_utf8_lossy(&out.stdout));
    assert!(b.config()["instances"].get("anthropic:work").is_some());

    // --remove takes the definition too, and the default model it ran on.
    assert_eq!(b.config()["defaultModel"], "claude:work/claude-opus-5-5");
    let d = b.json(&["disconnect", "claude:work", "--remove", "--json"], &[]);
    assert_eq!((d["data"]["removed_definition"].as_bool(), d["data"]["cleared_default"].as_str()), (Some(true), Some("claude:work/claude-opus-5-5")));
    assert!(b.config()["instances"].get("claude:work").is_none() && b.config().get("defaultModel").is_none());
    assert!(!b.status().iter().any(|r| r["instance"] == "claude:work"));
}

#[test]
fn connect_at_a_terminal_walks_vendor_method_and_account() {
    let b = Sandbox::new("pty");
    b.json(&["connect", "anthropic", "--method", "subscription", "--name", "work", "--json"], &[]);
    let mut cmd = b.command(&["connect"], &[]);
    cmd.env("TERM", "xterm-256color");
    let mut t = pty::Pty::spawn(cmd, 120, 30);
    let wait = Duration::from_secs(20);
    assert!(t.wait_for("Anthropic — Claude", wait).is_some(), "the vendors: {}", t.text());
    t.write(b"\r");
    assert!(t.wait_for("Claude subscription (Pro, Max, Team)", wait).is_some(), "the methods: {}", t.text());
    t.write(b"\r");
    // The accounts there are, each with its readiness, and a new one.
    assert!(t.wait_for("+ new account", wait).is_some(), "the accounts: {}", t.text());
    let text = t.text();
    assert!(text.contains("claude:work (Claude subscription) — ready, reconnect"), "{text}");
    assert!(text.contains("claude (Claude subscription, your own Claude Code login) — not signed in, reconnect"), "{text}");
    t.write(b"\x1b[B\x1b[B\r");
    assert!(t.wait_for("Name the new account", wait).is_some(), "{}", t.text());
    t.write(b"team\r");
    let exit = t.wait(wait).expect("krowk connect finished");
    assert!(exit.success(), "{}", t.text());
    assert!(t.text().contains("Connected claude:team") && t.text().contains("try it:"), "{}", t.text());
    assert!(b.data().join("accounts/claude-team/fake-login").exists());
}

#[test]
fn a_failed_sign_in_removes_only_the_directory_it_made_and_never_one_a_dotdot_climbs_into() {
    let b = Sandbox::new("dotdot");
    let acct = b.root.join("acct");
    std::fs::create_dir_all(acct.join("Other")).unwrap();
    std::fs::write(acct.join("Other/settings.json"), "{}").unwrap();
    for leaf in ["Other", "Fresh"] {
        let dir = format!("{}/new/../{leaf}", acct.display());
        let out = b.krowk(&["connect", "anthropic", "--method", "subscription", "--name", &leaf.to_lowercase(), "--config-dir", &dir], &[("FAKE_CLAUDE_LOGIN", "fail")]);
        assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));
    }
    assert!(acct.join("Other/settings.json").exists(), "the directory that was there survives");
    assert!(!acct.join("new").exists() && !acct.join("Fresh").exists(), "nothing made is left behind");
}

#[test]
fn accounts_sharing_a_directory_by_hand_still_renew_and_a_new_one_cannot_take_any_instances_directory() {
    let b = Sandbox::new("shared");
    let shared = b.root.join("shared");
    std::fs::create_dir_all(&shared).unwrap();
    std::fs::write(shared.join("fake-login"), "").unwrap();
    let dir = shared.display().to_string();
    std::fs::create_dir_all(b.config_path().parent().unwrap()).unwrap();
    std::fs::write(b.config_path(), json!({"instances": {"claude:a": {"kind": "claude-code", "configDir": dir}, "claude:b": {"kind": "claude-code", "configDir": dir}}}).to_string()).unwrap();
    let r = b.json(&["connect", "claude:a", "--json"], &[]);
    assert_eq!(r["data"]["renewed"], true);
    // The built-in's own ~/.claude, and a symlink to an account's directory,
    // are both taken.
    let own = b.root.join("home/.claude");
    std::fs::create_dir_all(&own).unwrap();
    std::os::unix::fs::symlink(&shared, b.root.join("link")).unwrap();
    for taken in [own.display().to_string(), b.root.join("link").display().to_string()] {
        let out = b.krowk(&["connect", "anthropic", "--method", "subscription", "--name", "new", "--config-dir", &taken], &[]);
        assert!(!out.status.success() && stderr(&out).contains("directory already"), "{taken}: {}", stderr(&out));
    }
}

#[test]
fn a_directory_in_home_is_the_persons_own_login_even_with_claude_config_dir_set_elsewhere() {
    let b = Sandbox::new("ownvar");
    let own = b.root.join("home/.claude");
    std::fs::create_dir_all(&own).unwrap();
    std::fs::write(own.join("fake-login"), "").unwrap();
    std::fs::create_dir_all(b.config_path().parent().unwrap()).unwrap();
    std::fs::write(b.config_path(), json!({"instances": {"claude:mine": {"kind": "claude-code", "configDir": own.display().to_string()}}}).to_string()).unwrap();
    let elsewhere = b.root.join("elsewhere").display().to_string();
    let out = b.krowk(&["disconnect", "claude:mine"], &[("CLAUDE_CONFIG_DIR", &elsewhere)]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(stderr(&out).contains("krowk disconnect claude:mine --sign-out-vendor") && !stderr(&out).contains("--remove"), "{}", stderr(&out));
    assert!(own.join("fake-login").exists());
}

#[test]
fn with_no_home_to_name_it_the_built_in_claude_is_still_the_persons_own_login() {
    let b = Sandbox::new("nohome");
    let mut c = b.command(&["disconnect", "claude"], &[]);
    // No HOME (krowk has KROWK_HOME) and no CLAUDE_CONFIG_DIR: Claude Code finds its directory
    // through the password database anyway, so it is still asked about.
    c.env_remove("HOME").env("KROWK_HOME", b.root.join("home/.krowk"));
    let out = c.output().unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(stderr(&out).contains("signs you out of Claude Code itself, in ~/.claude"), "{}", stderr(&out));
    assert_eq!(b.logins("argv auth logout"), 0);
}

#[test]
fn a_failed_sign_in_removes_every_parent_it_made_and_stops_at_the_first_it_did_not() {
    let b = Sandbox::new("walkup");
    // An empty directory that was there before: removable, so only the
    // walk's stop keeps it. (krowk's own data directory cannot play this
    // part: the vendor check makes `readiness` in it first.)
    let kept = b.root.join("empty");
    std::fs::create_dir_all(&kept).unwrap();
    let dir = kept.join("fresh/a/b").display().to_string();
    let out = b.krowk(&["connect", "anthropic", "--method", "subscription", "--name", "walk", "--config-dir", &dir], &[("FAKE_CLAUDE_LOGIN", "fail")]);
    assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));
    assert!(!kept.join("fresh").exists(), "every parent the sign-in made is gone");
    assert!(kept.is_dir(), "the walk stopped at the outermost one it made");
}

#[test]
fn a_hand_written_relative_or_dotdot_config_dir_is_never_made_by_krowk() {
    let b = Sandbox::new("handwritten");
    let dots = format!("{}/x/../y", b.root.display());
    std::fs::create_dir_all(b.config_path().parent().unwrap()).unwrap();
    std::fs::write(b.config_path(), json!({"instances": {"claude:rel": {"kind": "claude-code", "configDir": "rel/acct"}, "claude:dots": {"kind": "claude-code", "configDir": dots}}}).to_string()).unwrap();
    // A relative one is refused before the vendor runs: its login runs in
    // krowk's own directory, where the path would lead somewhere else.
    let out = b.krowk(&["connect", "claude:rel"], &[]);
    assert!(!out.status.success() && stderr(&out).contains("is relative"), "claude:rel: {}", stderr(&out));
    assert!(!b.root.join("fake.log").exists() || !std::fs::read_to_string(b.root.join("fake.log")).unwrap().contains("auth"), "the vendor never ran for it");
    let out = b.krowk(&["connect", "claude:dots"], &[("FAKE_CLAUDE_LOGIN", "fail")]);
    assert_eq!(out.status.code(), Some(3), "claude:dots: {}", stderr(&out));
    // Where krowk runs, where the vendor runs, and both readings of `..`.
    for nothing in [b.root.join("repo/rel"), b.data().join("readiness/rel"), b.root.join("x"), b.root.join("y")] {
        assert!(!nothing.exists(), "{} was made", nothing.display());
    }
}
