//! `krowk_harness::connect` driven the way a front end drives it: through
//! an `AuthInteraction` that answers from a script, as the TUI's overlay
//! will, against a fake `claude` (no real login anywhere). The account
//! picker lists each account of the chosen way in with its readiness and a
//! new one; a vendor login runs inside `terminal`, the hook a front end
//! that owns the screen suspends itself in; with nobody to ask, a choice
//! is an error that names the flag, never a guess.

#![cfg(unix)]

use krowk_harness::connect::{Answer, AuthInteraction, Method, Notice, Options, Prompt, ProviderAuth};
use krowk_harness::engine::EngineError;
use serde_json::json;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;

struct Script {
    interactive: bool,
    answers: VecDeque<Answer>,
    /// Each question, and the options of a choice.
    asked: Vec<(String, Vec<String>)>,
    told: Vec<String>,
    terminal_runs: usize,
}

impl Script {
    fn new(interactive: bool, answers: Vec<Answer>) -> Script {
        Script { interactive, answers: answers.into(), asked: Vec::new(), told: Vec::new(), terminal_runs: 0 }
    }
}

impl AuthInteraction for Script {
    fn interactive(&self) -> bool {
        self.interactive
    }

    fn prompt(&mut self, prompt: Prompt<'_>) -> Result<Answer, EngineError> {
        let (message, options, flag) = match prompt {
            Prompt::Text { message, flag } | Prompt::Secret { message, flag } => (message, Vec::new(), flag),
            Prompt::Select { message, options, flag } => (message, options.iter().map(|o| o.to_string()).collect(), flag),
        };
        self.asked.push((message.to_string(), options));
        if !self.interactive {
            return Err(EngineError::new("bad_argument", format!("{message} — pass {flag}")));
        }
        Ok(self.answers.pop_front().expect("an answer for every question"))
    }

    fn notify(&mut self, notice: Notice<'_>) {
        if let Notice::Info(s) | Notice::Progress(s) = notice {
            self.told.push(s.to_string());
        }
    }

    fn terminal(&mut self, run: &mut dyn FnMut() -> Result<ExitStatus, String>) -> Result<ExitStatus, String> {
        self.terminal_runs += 1;
        run()
    }
}

struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Sandbox {
        let root = std::env::temp_dir().join(format!("krowk-harness-connect-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("home/.config/krowk")).unwrap();
        std::fs::create_dir_all(root.join("bin")).unwrap();
        let bin = root.join("bin/claude");
        std::fs::copy(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claude/fake-claude"), &bin).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let root = root.canonicalize().unwrap();
        // claude:work, signed in; `claude` is not. `claude` names the
        // sandbox's ~/.claude, Claude Code's default there, so it is the
        // person's own login — and the fake, which reads CLAUDE_CONFIG_DIR,
        // never looks at the real home of whoever runs the tests.
        let work = root.join("accounts/work");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(work.join("fake-login"), "").unwrap();
        std::fs::create_dir_all(root.join("home/.claude")).unwrap();
        let own = root.join("home/.claude").display().to_string();
        let config = json!({"instances": {"claude:work": {"kind": "claude-code", "configDir": work.display().to_string()}, "claude": {"kind": "claude-code", "configDir": own}}});
        std::fs::write(root.join("home/.config/krowk/config.json"), config.to_string()).unwrap();
        Sandbox { root }
    }

    fn env(&self) -> impl Fn(&str) -> String + '_ {
        move |k: &str| match k {
            "HOME" => self.root.join("home").display().to_string(),
            "PATH" => format!("{}:/usr/bin:/bin", self.root.join("bin").display()),
            _ => String::new(),
        }
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn auth<'a>(b: &Sandbox, env: &'a dyn Fn(&str) -> String) -> ProviderAuth<'a> {
    ProviderAuth { config: b.root.join("home/.config/krowk/config.json"), credentials: b.root.join("home/.config/krowk/providers/credentials.json"), env }
}

#[test]
fn r_inst_2_the_account_picker_lists_each_account_with_its_readiness_and_a_new_one_signs_in_in_the_terminal() {
    let b = Sandbox::new("picker");
    let env = b.env();
    let pa = auth(&b, &env);
    // The method (Claude subscription, the first), then "+ new account…",
    // then its name.
    let mut ui = Script::new(true, vec![Answer::Choice(0), Answer::Choice(2), Answer::Text("team".into())]);
    let req = pa.request(Some("anthropic"), None, Options::default(), &mut ui).unwrap();
    assert_eq!(ui.asked[0].1, ["Claude subscription (Pro, Max, Team) — Claude Code's own login", "Anthropic API key"]);
    assert_eq!(ui.asked[1].0, "Which account?");
    assert_eq!(ui.asked[1].1, ["claude (Claude subscription, your own Claude Code login) — not signed in, reconnect", "claude:work (Claude subscription) — ready, reconnect", "+ new account…"]);
    assert_eq!(req.instance.as_deref(), Some("claude:team"));

    let done = pa.connect(&req, &mut ui).unwrap();
    assert_eq!((done.instance.as_str(), done.renewed), ("claude:team", false));
    assert!(done.vendor.as_ref().is_some_and(|v| v.logged_in && v.ran), "Claude's own login ran");
    assert_eq!(ui.terminal_runs, 1, "the login ran inside the terminal hook, where a TUI suspends itself");
    assert!(ui.told.iter().any(|t| t.starts_with("Signing claude:team in to Claude Code")), "{:?}", ui.told);
    let dir = b.root.join("home/.local/share/krowk/claude/claude-team");
    assert!(dir.join("fake-login").exists(), "signed in in its own directory");

    // Picking an account there is reconnects it; a new name that is one
    // already is refused rather than taken for it.
    let mut ui = Script::new(true, vec![Answer::Choice(0), Answer::Choice(3), Answer::Text("work".into())]);
    let e = pa.request(Some("anthropic"), None, Options::default(), &mut ui).err().expect("a name taken already");
    assert!(e.message.contains("claude:work is connected already"), "{}", e.message);
    let mut ui = Script::new(true, vec![Answer::Choice(0), Answer::Choice(2)]);
    let req = pa.request(Some("anthropic"), None, Options::default(), &mut ui).unwrap();
    assert_eq!(req.instance.as_deref(), Some("claude:work"));
    let done = pa.connect(&req, &mut ui).unwrap();
    assert!(done.renewed && done.vendor.as_ref().is_some_and(|v| !v.ran), "signed in already: nothing ran");
    assert_eq!(ui.terminal_runs, 0);
}

#[test]
fn with_nobody_to_ask_a_choice_is_an_error_that_names_the_flag_and_a_given_method_takes_the_default_account() {
    let b = Sandbox::new("nobody");
    let env = b.env();
    let pa = auth(&b, &env);
    let mut ui = Script::new(false, Vec::new());
    let e = pa.request(Some("anthropic"), None, Options::default(), &mut ui).err().expect("no method, nobody to ask");
    assert!(e.message.contains("--method subscription|api-key"), "{}", e.message);
    // Given the method, nothing is asked: the default-named account.
    let mut ui = Script::new(false, Vec::new());
    let req = pa.request(Some("anthropic"), Some(Method::ApiKey), Options::default(), &mut ui).unwrap();
    assert!(ui.asked.is_empty() && req.instance.is_none());
    // Disconnecting is never a guess either.
    let e = pa.disconnect_target(None, &mut ui).expect_err("no instance, nobody to ask");
    assert!(e.message.contains("claude:work") && e.message.contains("supergrok"), "{}", e.message);
}

#[test]
fn signing_out_the_persons_own_claude_login_is_asked_first_and_no_keeps_it() {
    let b = Sandbox::new("own");
    let env = b.env();
    let pa = auth(&b, &env);
    let own = b.root.join("home/.claude/fake-login");
    std::fs::write(&own, "").unwrap();
    let mut ui = Script::new(true, vec![Answer::Choice(0)]);
    let e = pa.disconnect("claude", false, false, &mut ui).err().expect("no keeps it");
    assert_eq!(e.code, "selection_cancelled");
    assert!(ui.asked[0].0.contains("signs you out of Claude Code itself") && ui.asked[0].0.contains(".claude"), "{:?}", ui.asked);
    assert!(own.exists());
    let mut ui = Script::new(true, vec![Answer::Choice(1)]);
    pa.disconnect("claude", false, false, &mut ui).unwrap();
    assert!(!own.exists(), "yes signs it out");
    // A named account is its own, and is not asked about.
    let mut ui = Script::new(true, Vec::new());
    pa.disconnect("claude:work", false, false, &mut ui).unwrap();
    assert!(ui.asked.is_empty());
}

/// A front end during whose vendor login another connection finishes an
/// account beside this one.
struct Meanwhile(Box<dyn FnMut()>);

impl AuthInteraction for Meanwhile {
    fn interactive(&self) -> bool {
        false
    }
    fn prompt(&mut self, _: Prompt<'_>) -> Result<Answer, EngineError> {
        unreachable!("nothing is asked")
    }
    fn notify(&mut self, _: Notice<'_>) {}
    fn terminal(&mut self, run: &mut dyn FnMut() -> Result<ExitStatus, String>) -> Result<ExitStatus, String> {
        (self.0)();
        run()
    }
}

#[test]
fn a_failed_sign_in_leaves_the_account_another_connection_made_beside_it_under_a_new_parent() {
    let b = Sandbox::new("beside");
    // A `claude` whose login gives up, as a person closing the browser would.
    let fake = b.root.join("bin/claude");
    std::fs::rename(&fake, b.root.join("bin/fake-claude")).unwrap();
    std::fs::write(&fake, format!("#!/bin/sh\nFAKE_CLAUDE_LOGIN=fail exec {} \"$@\"\n", b.root.join("bin/fake-claude").display())).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let env = b.env();
    let pa = auth(&b, &env);
    let parent = b.root.join("home/.local/share/krowk/claude");
    assert!(!parent.exists(), "the accounts' parent is new");
    let other = parent.join("claude-b");
    let finished = other.clone();
    let mut ui = Meanwhile(Box::new(move || {
        std::fs::create_dir_all(&finished).unwrap();
        std::fs::write(finished.join("fake-login"), "").unwrap();
    }));
    let req = pa.request(Some("anthropic"), Some(Method::Subscription), Options { name: Some("fail".into()), ..Options::default() }, &mut ui).unwrap();
    let e = pa.connect(&req, &mut ui).err().expect("the login gives up");
    assert_eq!(e.code, "not_authenticated");
    assert!(!parent.join("claude-fail").exists(), "its own directory is gone");
    assert!(other.join("fake-login").exists(), "the other connection's account is not");
}
