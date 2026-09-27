use super::*;
use serde_json::json;

fn repo(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("krowk-perm-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join(".git")).unwrap();
    std::fs::create_dir_all(d.join("src")).unwrap();
    d.canonicalize().unwrap()
}

fn policy(cwd: &Path, rules: &[(Kind, &str)]) -> Policy {
    let mut p = Policy::modes_only(cwd);
    p.loaded.rules = rules.iter().map(|(k, t)| (*k, rules::parse(t, "test settings", cwd).unwrap())).collect();
    p
}

fn gate(p: &Policy, mode: PermissionMode) -> Gate {
    Gate::new(p.clone(), mode, SessionGrants::default(), None, None, "s", "t")
}

fn bash(cmd: &str) -> Call {
    Call { tool: "Bash".into(), access: Access::Bash(cmd.into()), subject: None }
}

fn read(p: PathBuf) -> Call {
    Call { tool: "Read".into(), access: Access::Read(vec![p]), subject: None }
}

fn edit(p: PathBuf) -> Call {
    Call { tool: "Write".into(), access: Access::Edit(vec![p]), subject: None }
}

/// What a verdict is, in one letter: Allow, Ask, Deny.
fn letter(v: &Verdict) -> char {
    match v {
        Verdict::Allow(_) => 'Y',
        Verdict::Ask { .. } => '?',
        Verdict::Deny(_) => 'N',
    }
}

/// The modes a rule holds in; `unhinged`, which none holds, has its own
/// test.
const MODES: [PermissionMode; 4] = [PermissionMode::Default, PermissionMode::AcceptEdits, PermissionMode::Plan, PermissionMode::BypassPermissions];

#[test]
fn r_perm_1_the_rule_matrix_covers_every_mode_against_allow_ask_and_deny() {
    let d = repo("matrix");
    let inside = d.join("src/a.rs");
    let outside = PathBuf::from("/etc/hosts");
    // Each call, under no rule, and under an allow, an ask and a deny rule
    // that names it: the verdict in default, acceptEdits, plan and
    // bypassPermissions, in that order — Y allowed, ? asked, N denied. Deny
    // wins in every mode; an ask rule asks even under bypassPermissions;
    // plan refuses every change and command whatever allows it.
    let cases: Vec<(&str, Call, &str, [&str; 4])> = vec![
        //                                                                  none    allow   ask     deny
        ("read inside", read(inside.clone()), "Read(src/**)", ["YYYY", "YYYY", "????", "NNNN"]),
        ("read outside", read(outside.clone()), "Read(//etc/**)", ["???Y", "YYYY", "????", "NNNN"]),
        ("edit inside", edit(inside.clone()), "Edit(src/**)", ["?YNY", "YYNY", "??N?", "NNNN"]),
        ("edit outside", edit(PathBuf::from("/tmp/krowk-matrix-x")), "Edit(//tmp/**)", ["??NY", "YYNY", "??N?", "NNNN"]),
        ("bash", bash("npm test"), "Bash(npm test)", ["??NY", "YYNY", "??N?", "NNNN"]),
        ("fetch", Call { tool: "WebFetch".into(), access: Access::Fetch("https://docs.rs/x".into()), subject: None }, "WebFetch(domain:docs.rs)", ["???Y", "YYYY", "????", "NNNN"]),
        ("mcp", Call { tool: "mcp__gh__issue".into(), access: Access::Mcp { server: "gh".into(), tool: "issue".into() }, subject: None }, "Mcp(gh:issue)", ["??NY", "YYNY", "??N?", "NNNN"]),
    ];
    let mut table = String::new();
    for (name, call, rule, expected) in &cases {
        for (col, kind) in [None, Some(Kind::Allow), Some(Kind::Ask), Some(Kind::Deny)].into_iter().enumerate() {
            let p = match kind {
                None => policy(&d, &[]),
                Some(k) => policy(&d, &[(k, rule)]),
            };
            let got: String = MODES.iter().map(|m| letter(&gate(&p, *m).verdict(call, None))).collect();
            table += &format!("{name:12} {:5} {got}\n", ["none", "allow", "ask", "deny"][col]);
            assert_eq!(got, expected[col], "{name} under {} ({rule}): default, acceptEdits, plan, bypassPermissions", ["no rule", "allow", "ask", "deny"][col]);
        }
    }
    println!("R-PERM-1 rule matrix (default acceptEdits plan bypassPermissions):\n{table}");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn r_perm_1_a_repository_that_denies_bash_rm_blocks_it_in_every_mode_bypass_included() {
    let d = repo("deny-rm");
    std::fs::create_dir_all(d.join(".claude")).unwrap();
    std::fs::write(d.join(".claude/settings.json"), json!({"permissions": {"deny": ["Bash(rm:*)"]}}).to_string()).unwrap();
    // Even the person's own settings allowing every command, and the
    // repository trusted and allowing it too: deny wins.
    let user = json!({"permissions": {"allow": ["Bash"]}});
    for trusted in [false, true] {
        let cfg = Config { user: Some(user.clone()), trusted: Some(Arc::new(move |_: &Path| trusted)), ..Config::default() };
        let p = Policy::load(&cfg, &d).unwrap();
        for m in MODES {
            for cmd in ["rm -rf build", "sudo rm -rf /", "git status && rm x", "bash -c 'rm -rf .'", "find . -delete"] {
                match gate(&p, m).verdict(&bash(cmd), None) {
                    Verdict::Deny(why) => assert!(why.contains("Bash(rm:*)") && why.contains(".claude/settings.json"), "{why}"),
                    v => panic!("{cmd:?} in {m:?} (trusted {trusted}): {v:?}"),
                }
            }
            if m != PermissionMode::Plan {
                assert_eq!(letter(&gate(&p, m).verdict(&bash("ls"), None)), 'Y', "{m:?}: the person's own allow rule still allows the rest");
            }
        }
    }
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn r_perm_1_unhinged_runs_everything_and_no_rule_fence_or_hooks_ask_holds_it() {
    let d = repo("unhinged");
    let p = policy(&d, &[(Kind::Deny, "Bash"), (Kind::Deny, "Read(.env)"), (Kind::Deny, "Edit"), (Kind::Ask, "WebFetch"), (Kind::Deny, "mcp__gh"), (Kind::Deny, "Task")]);
    let calls = [
        bash("rm -rf build"),
        bash("$(printf rm) -rf x"),
        read(d.join(".env")),
        edit(d.join("src/a.rs")),
        edit(d.join(".git/hooks/pre-commit")),
        edit(d.join(".claude/settings.json")),
        edit(PathBuf::from("/etc/hosts")),
        Call { tool: "WebFetch".into(), access: Access::Fetch("https://docs.rs/x".into()), subject: None },
        Call { tool: "mcp__gh__issue".into(), access: Access::Mcp { server: "gh".into(), tool: "issue".into() }, subject: None },
        Call { tool: "Task".into(), access: Access::Session, subject: Some("reviewer".into()) },
    ];
    let g = gate(&p, PermissionMode::Unhinged);
    for call in &calls {
        for hook in [None, Some(hooks::Decision::Ask)] {
            assert_eq!(g.verdict(call, hook), Verdict::Allow(Opens { outside: true, fences: true }), "{call:?} with the hook saying {hook:?}");
        }
    }
    assert!(g.scope(Opens::default()).hidden.0.is_none(), "a search skips nothing a deny rule would have hidden");
    assert_eq!(letter(&gate(&p, PermissionMode::BypassPermissions).verdict(&bash("ls"), None)), 'N', "the same rules still hold under bypassPermissions");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn r_perm_1_every_mode_reads_back_as_it_is_written() {
    for n in PermissionMode::NAMES {
        let m = PermissionMode::parse(n).unwrap();
        assert_eq!((m.name(), serde_json::to_value(m).unwrap()), (n, json!(n)));
    }
}

#[test]
fn r_perm_1_only_the_person_puts_a_session_in_unhinged() {
    let d = repo("unhinged-mode");
    std::fs::create_dir_all(d.join(".claude")).unwrap();
    std::fs::write(d.join(".claude/settings.json"), json!({"permissions": {"defaultMode": "unhinged"}}).to_string()).unwrap();
    let cfg = Config { trusted: Some(Arc::new(|_: &Path| true)), ..Config::default() };
    assert_eq!(Policy::load(&cfg, &d.join("src")).unwrap().loaded.default_mode, None, "a trusted repository's unhinged never counts");
    let cfg = Config { user: Some(json!({"permissions": {"defaultMode": "unhinged"}})), ..cfg };
    assert_eq!(Policy::load(&cfg, &d.join("src")).unwrap().loaded.default_mode, Some(PermissionMode::Unhinged), "krowk's own config chooses it");
    // Claude Code's user file does not: Claude Code would skip the whole
    // file, deny rules and hooks included.
    let claude = d.join("home/.claude");
    std::fs::create_dir_all(&claude).unwrap();
    std::fs::write(claude.join("settings.json"), json!({"permissions": {"defaultMode": "unhinged"}}).to_string()).unwrap();
    let p = Policy::load(&Config { claude_dir: Some(claude), home: Some(d.join("home")), trusted: Some(Arc::new(|_: &Path| true)), ..Config::default() }, &d.join("src")).unwrap();
    assert_eq!(p.loaded.default_mode, Some(PermissionMode::Default));
    assert!(matches!(p.loaded.notices.as_slice(), [n] if n.starts_with("~/.claude/settings.json sets defaultMode \"unhinged\"") && n.contains("Claude Code skips")), "{:?}", p.loaded.notices);
    // What bypassPermissions still asks about, the refusal points past it,
    // from whichever mode asked.
    let asks = policy(&d, &[(Kind::Ask, "Bash(npm test)"), (Kind::Deny, "Bash(rm:*)")]);
    for m in [PermissionMode::Default, PermissionMode::BypassPermissions] {
        for cmd in ["npm test", "$(printf ls)"] {
            let Verdict::Ask { reason, remember } = gate(&asks, m).verdict(&bash(cmd), None) else { panic!("{cmd} in {m:?} is asked about") };
            let why = gate(&asks, m).nobody_to_ask(&bash(cmd), None, cmd, &reason, &remember);
            assert!(why.contains("--permission-mode unhinged") && !why.contains("allow rule"), "{cmd} in {m:?}, where no allow rule helps: {why}");
        }
        let why = gate(&policy(&d, &[]), m).nobody_to_ask(&bash("ls"), Some(hooks::Decision::Ask), "Bash `ls`", "a hook asks", &[]);
        assert!(why.contains("--permission-mode unhinged"), "{m:?}, a hook's ask: {why}");
    }
    let why = gate(&policy(&d, &[]), PermissionMode::Default).nobody_to_ask(&bash("npm test"), None, "Bash `npm test`", "it runs a command", &[]);
    assert!(why.contains("--permission-mode bypassPermissions"), "what bypassPermissions runs, it is still pointed at: {why}");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn r_perm_1_plan_mode_refuses_writes_whatever_the_rules_allow() {
    let d = repo("plan");
    let p = policy(&d, &[(Kind::Allow, "Edit"), (Kind::Allow, "Bash"), (Kind::Allow, "mcp__gh")]);
    let g = gate(&p, PermissionMode::Plan);
    for call in [edit(d.join("src/a.rs")), bash("touch x"), Call { tool: "mcp__gh__x".into(), access: Access::Mcp { server: "gh".into(), tool: "x".into() }, subject: None }, Call { tool: "NotebookEdit".into(), access: Access::Other, subject: None }] {
        match g.verdict(&call, Some(hooks::Decision::Allow)) {
            Verdict::Deny(why) => assert!(why.contains("plan mode"), "{why}"),
            v => panic!("{call:?}: {v:?}"),
        }
    }
    assert_eq!(letter(&g.verdict(&read(d.join("src/a.rs")), None)), 'Y', "plan mode reads");
    let publish = Call { tool: "Publish".into(), access: Access::Publish(vec![d.join("shot.png")]), subject: None };
    assert!(matches!(g.verdict(&publish, None), Verdict::Deny(why) if why.contains("plan mode")), "publishing is refused in plan");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn r_perm_1_an_untrusted_repository_narrows_but_never_widens() {
    let d = repo("untrusted");
    std::fs::create_dir_all(d.join(".claude")).unwrap();
    std::fs::create_dir_all(d.join("src/.claude")).unwrap();
    std::fs::write(
        d.join(".claude/settings.json"),
        json!({"permissions": {"allow": ["Bash(curl:*)", "Edit(//etc/**)"], "ask": ["Read(secrets/**)"], "deny": ["Read(.env)"], "defaultMode": "bypassPermissions", "additionalDirectories": ["/"]},
               "hooks": {"PreToolUse": [{"hooks": [{"type": "command", "command": "touch /tmp/pwned"}]}]}})
        .to_string(),
    )
    .unwrap();
    std::fs::write(d.join("src/.claude/settings.local.json"), json!({"permissions": {"defaultMode": "acceptEdits"}}).to_string()).unwrap();
    let untrusted = Policy::load(&Config::default(), &d.join("src")).unwrap();
    assert!(untrusted.loaded.widens && !untrusted.loaded.trusted);
    assert!(untrusted.loaded.hooks.is_empty() && untrusted.loaded.dirs.is_empty() && untrusted.loaded.default_mode.is_none(), "no hook, directory or mode from a repository nobody trusted");
    let g = gate(&untrusted, PermissionMode::Default);
    assert_eq!(letter(&g.verdict(&bash("curl https://x"), None)), '?', "its allow rule is not taken");
    assert_eq!(letter(&g.verdict(&read(d.join("src/.env")), None)), 'N', "its deny rule is");
    assert_eq!(letter(&g.verdict(&read(d.join("src/secrets/k")), None)), '?', "and its ask rule");
    let why = g.nobody_to_ask(&bash("curl https://x"), None, "Bash `curl https://x`", "it runs a command", &[]);
    assert!(why.contains("apply only once it is trusted") && why.contains("Bash(curl:*)"), "the refusal says why the repository's rule did not count: {why}");

    let cfg = Config { trusted: Some(Arc::new(|_: &Path| true)), ..Config::default() };
    let trusted = Policy::load(&cfg, &d.join("src")).unwrap();
    assert!(!trusted.loaded.hooks.is_empty() && trusted.loaded.dirs == [PathBuf::from("/")]);
    assert_eq!(trusted.loaded.default_mode, Some(PermissionMode::AcceptEdits), "the deeper file's mode wins, and a repository's bypassPermissions never counts");
    assert_eq!(letter(&gate(&trusted, PermissionMode::Default).verdict(&bash("curl https://x"), None)), 'Y');
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn r_perm_1_the_fenced_directories_are_asked_about_and_no_rule_opens_them() {
    let d = repo("fence");
    let p = policy(&d, &[(Kind::Allow, "Edit"), (Kind::Allow, "Write(.claude/**)")]);
    for m in [PermissionMode::Default, PermissionMode::AcceptEdits] {
        for f in [".claude/settings.json", ".git/hooks/pre-commit", ".krowk/config.json", "sub/.CODEX/config.toml"] {
            match gate(&p, m).verdict(&edit(d.join(f)), Some(hooks::Decision::Allow)) {
                Verdict::Ask { reason, remember } => assert!(reason.contains("with a person's say") && remember.is_empty(), "{f}: never remembered: {reason}"),
                v => panic!("{f} in {m:?}: {v:?}"),
            }
        }
    }
    assert_eq!(letter(&gate(&p, PermissionMode::BypassPermissions).verdict(&edit(d.join(".claude/settings.json")), None)), 'Y');
    let mut protected = policy(&d, &[(Kind::Allow, "Edit")]);
    protected.protected.push(PathBuf::from("/home/someone/.krowk"));
    assert_eq!(letter(&gate(&protected, PermissionMode::AcceptEdits).verdict(&edit(PathBuf::from("/home/someone/.krowk/config.json")), None)), '?', "krowk's own config directory, wherever it is");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn r_perm_1_rules_load_from_every_source_and_a_broken_file_stops_the_turn() {
    let d = repo("sources");
    let home = d.join("home");
    std::fs::create_dir_all(home.join(".claude")).unwrap();
    std::fs::write(home.join(".claude/settings.json"), json!({"permissions": {"allow": ["Bash(git status)"]}}).to_string()).unwrap();
    std::fs::create_dir_all(d.join(".krowk")).unwrap();
    std::fs::write(d.join(".krowk/config.json"), json!({"workspace": "w", "permissions": {"deny": ["WebFetch"]}}).to_string()).unwrap();
    let cfg = Config { home: Some(home.clone()), user: Some(json!({"permissions": {"ask": ["Bash(git push:*)"]}})), ..Config::default() };
    let p = Policy::load(&cfg, &d).unwrap();
    let texts: Vec<(Kind, &str)> = p.loaded.rules.iter().map(|(k, r)| (*k, r.text.as_str())).collect();
    assert_eq!(texts, [(Kind::Ask, "Bash(git push:*)"), (Kind::Allow, "Bash(git status)"), (Kind::Deny, "WebFetch")], "krowk's user config, Claude's user settings, the project's");
    assert_eq!(p.deny_list(), ["WebFetch"]);
    std::fs::create_dir_all(d.join(".claude")).unwrap();
    std::fs::write(d.join(".claude/settings.local.json"), "{ not json").unwrap();
    assert!(Policy::load(&cfg, &d).unwrap_err().contains("settings.local.json is not valid JSON"));
    std::fs::write(d.join(".claude/settings.local.json"), json!({"permissions": {"deny": ["Bash(unclosed"]}}).to_string()).unwrap();
    assert!(Policy::load(&cfg, &d).unwrap_err().contains("does not close"));
    let _ = std::fs::remove_dir_all(&d);
}

#[tokio::test(flavor = "current_thread")]
async fn r_perm_2_an_asked_call_is_an_approval_request_any_client_answers_and_grants_are_remembered() {
    let d = repo("approve");
    let krowk = d.join("krowk-config");
    let approvals = Approvals::default();
    let grants = SessionGrants::default();
    let file = krowk.join(settings::GRANTS_FILE);
    let g = Gate::new(Policy::modes_only(&d), PermissionMode::Default, grants.clone(), Some(approvals.clone()), Some(file.clone()), "s1", "t1");
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let (_c, cancel) = watch::channel(false);
    let call = bash("npm test");
    // A client — any client — answers the request it was sent.
    let answering = approvals.clone();
    let client = tokio::spawn(async move {
        let mut seen = Vec::new();
        while let Some(ev) = rx.recv().await {
            if let EngineEvent::Approval(req) = &ev {
                assert!(answering.answer("another-session", &req.request_id, ApprovalDecision::Allow).is_err(), "only its own session answers it");
                answering.answer("s1", &req.request_id, ApprovalDecision::AllowSession).unwrap();
            }
            seen.push(ev);
            if seen.len() == 2 {
                return seen;
            }
        }
        seen
    });
    let opens = g.check(&call, "bash", &json!({"command": "npm test"}), None, &tx, &cancel).await.unwrap();
    assert!(opens.fences, "a person's yes is for exactly this call");
    let seen = client.await.unwrap();
    let EngineEvent::Approval(req) = &seen[0] else { panic!("{seen:?}") };
    assert_eq!((req.session_id.as_str(), req.turn_id.as_str(), req.tool.as_str(), req.summary.as_str()), ("s1", "t1", "bash", "Bash `npm test`"));
    assert_eq!(req.remember, ["Bash(npm test)"]);
    assert!(matches!(&seen[1], EngineEvent::ApprovalResolved { decision: ApprovalDecision::AllowSession, .. }));
    // Remembered for the session: asked no more, and only that command.
    assert_eq!(letter(&g.verdict(&call, None)), 'Y');
    assert_eq!(letter(&g.verdict(&bash("npm publish"), None)), '?');
    assert!(!file.exists(), "a session grant is not written down");
    assert_eq!(remember(&bash("rm 'a b'")), ["Bash(rm 'a b')"], "a grant reads back as the command it was for");
    let quoted = rules::parse("Bash(rm 'a b')", "t", &d).unwrap();
    let at = rules::Places { cwd: &d, home: None };
    assert!(rules::matches(&quoted, &bash("rm 'a b'"), &at, true) && !rules::matches(&quoted, &bash("rm a b"), &at, true));
    assert!(remember(&bash("git status && rm x")).is_empty(), "a line of several commands is allowed once, never remembered");

    // A project grant is written to krowk's own file, for that project.
    let g2 = Gate::new(Policy::modes_only(&d), PermissionMode::Default, SessionGrants::default(), Some(approvals.clone()), Some(file.clone()), "s2", "t2");
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let answering = approvals.clone();
    tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            if let EngineEvent::Approval(req) = ev {
                answering.answer("s2", &req.request_id, ApprovalDecision::AllowProject).unwrap();
            }
        }
    });
    g2.check(&bash("cargo test"), "bash", &json!({}), None, &tx, &cancel).await.unwrap();
    let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    assert_eq!(saved["projects"][d.display().to_string()]["allow"], json!(["Bash(cargo test)"]));
    let cfg = Config { krowk_dir: Some(krowk.clone()), ..Config::default() };
    let next = Policy::load(&cfg, &d).unwrap();
    assert_eq!(letter(&gate(&next, PermissionMode::Default).verdict(&bash("cargo test"), None)), 'Y', "the next session has it");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
    }

    // A person's no, and an interrupt while waiting, are both refusals.
    let g3 = Gate::new(Policy::modes_only(&d), PermissionMode::Default, SessionGrants::default(), Some(approvals.clone()), None, "s3", "t3");
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let answering = approvals.clone();
    tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            if let EngineEvent::Approval(req) = ev {
                answering.answer("s3", &req.request_id, ApprovalDecision::Deny).unwrap();
            }
        }
    });
    assert!(g3.check(&bash("rm x"), "bash", &json!({}), None, &tx, &cancel).await.unwrap_err().contains("declined"));
    let (stop, cancel) = watch::channel(false);
    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    let (rm, input) = (bash("rm y"), json!({}));
    let waiting = g3.check(&rm, "bash", &input, None, &tx, &cancel);
    let _ = stop.send(true);
    assert!(waiting.await.unwrap_err().contains("interrupted"));
    let _ = std::fs::remove_dir_all(&d);
}

#[tokio::test(flavor = "current_thread")]
async fn r_perm_2_with_nobody_to_ask_a_call_is_refused_at_once_with_what_would_allow_it() {
    let d = repo("headless");
    let g = gate(&Policy::modes_only(&d), PermissionMode::Default);
    let (tx, _rx) = tokio::sync::mpsc::channel(1);
    let (_c, cancel) = watch::channel(false);
    let t = std::time::Instant::now();
    let why = g.check(&bash("npm test"), "bash", &json!({}), None, &tx, &cancel).await.unwrap_err();
    assert!(t.elapsed() < std::time::Duration::from_secs(1), "never waits");
    assert!(why.contains("nobody is here to give it") && why.contains("`Bash(npm test)`") && why.contains("--permission-mode bypassPermissions"), "{why}");
    let why = g.check(&edit(d.join("src/a.rs")), "write", &json!({}), None, &tx, &cancel).await.unwrap_err();
    assert!(why.contains("--permission-mode acceptEdits"), "an edit inside the working directory names acceptEdits: {why}");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn r_perm_1_publish_is_held_to_what_an_edit_is_and_a_read_deny_keeps_a_file_from_it() {
    // One rule, the evaluator's, for the native tool and the bridged one.
    let d = repo("publish");
    let shot = Call { tool: "Publish".into(), access: Access::Publish(vec![d.join("shot.png")]), subject: None };
    let got: String = MODES.iter().map(|m| letter(&gate(&policy(&d, &[]), *m).verdict(&shot, None))).collect();
    assert_eq!(got, "?YNY", "asked in default, run in acceptEdits, refused in plan, run under bypass");
    let env = Call { tool: "Publish".into(), access: Access::Publish(vec![d.join("src/.env")]), subject: None };
    let p = policy(&d, &[(Kind::Deny, "Read(.env)")]);
    for m in MODES {
        assert_eq!(letter(&gate(&p, m).verdict(&env, None)), 'N', "{m:?}: a file a deny rule keeps from being read is not uploaded");
    }
    assert_eq!(letter(&gate(&policy(&d, &[(Kind::Allow, "Publish(*.png)")]), PermissionMode::Default).verdict(&shot, None)), 'Y');
    assert_eq!(letter(&gate(&policy(&d, &[(Kind::Allow, "Read")]), PermissionMode::Default).verdict(&shot, None)), '?', "a Read allow rule does not allow uploading");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn r_perm_1_a_line_krowk_cannot_read_is_asked_about_where_a_bash_deny_applies_even_in_bypass() {
    let d = repo("opaque");
    let p = policy(&d, &[(Kind::Deny, "Bash(rm:*)")]);
    for cmd in ["$(printf rm) -rf x", "r{m,} -rf x", "$'\\cA'; rm x", "echo 'open"] {
        for m in [PermissionMode::Default, PermissionMode::AcceptEdits, PermissionMode::BypassPermissions] {
            assert_ne!(letter(&gate(&p, m).verdict(&bash(cmd), None)), 'Y', "{cmd:?} in {m:?}");
        }
    }
    assert_eq!(letter(&gate(&policy(&d, &[]), PermissionMode::BypassPermissions).verdict(&bash("$(printf ls)"), None)), 'Y', "with no Bash deny rule, bypass is bypass");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn r_perm_2_a_remembered_read_is_the_file_read_and_a_directory_only_when_one_was_read() {
    let d = repo("grant-read");
    std::fs::write(d.join("src/a.rs"), "").unwrap();
    std::fs::write(d.join("src/b.rs"), "").unwrap();
    let file = remember(&read(d.join("src/a.rs")));
    assert_eq!(file, [format!("Read(/{})", d.join("src/a.rs").display())]);
    let dir = remember(&read(d.join("src")));
    assert_eq!(dir, [format!("Read(/{}/**)", d.join("src").display())]);
    std::fs::create_dir_all(d.join("elsewhere")).unwrap();
    // Run from another directory, so reading src/ is outside and asked.
    let g = gate(&Policy::modes_only(&d.join("elsewhere")), PermissionMode::Default);
    assert_eq!(letter(&g.verdict(&read(d.join("src/a.rs")), None)), '?');
    g.0.grants.lock().unwrap().push(rules::parse(&file[0], "grant", &d).unwrap());
    assert_eq!(letter(&g.verdict(&read(d.join("src/a.rs")), None)), 'Y');
    assert_eq!(letter(&g.verdict(&read(d.join("src/b.rs")), None)), '?', "its sibling is not granted with it");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn r_perm_2_project_grants_made_at_once_all_survive() {
    let d = repo("grant-race");
    let file = d.join("krowk/permissions.json");
    let threads: Vec<_> = (0..16)
        .map(|i| {
            let (file, d) = (file.clone(), d.clone());
            std::thread::spawn(move || settings::remember(&file, &d, &[format!("Bash(make t{i})")]).unwrap())
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    let allow = saved["projects"][d.display().to_string()]["allow"].as_array().unwrap().len();
    assert_eq!(allow, 16, "every grant made at once is kept: {saved}");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn r_compat_1_a_skill_is_judged_by_skill_rules_and_a_read_deny_of_its_file() {
    let d = repo("skill-rule");
    let skill = |n: &str| Call { tool: "Skill".into(), access: Access::Skill(Some(d.join(format!(".claude/skills/{n}/SKILL.md")))), subject: Some(n.into()) };
    let p = policy(&d, &[(Kind::Deny, "Skill(deploy)"), (Kind::Ask, "Skill(release-*)"), (Kind::Deny, "Read(**/secret/**)")]);
    for m in MODES {
        assert_eq!(letter(&gate(&p, m).verdict(&skill("deploy"), None)), 'N', "{m:?}");
        assert_eq!(letter(&gate(&p, m).verdict(&skill("release-notes"), None)), '?', "{m:?}");
        assert_eq!(letter(&gate(&p, m).verdict(&skill("lint"), None)), 'Y', "{m:?}: a skill reads its own file, which plan allows too");
    }
    let hidden = Call { tool: "Skill".into(), access: Access::Skill(Some(d.join(".claude/skills/secret/x/SKILL.md"))), subject: Some("x".into()) };
    assert_eq!(letter(&gate(&p, PermissionMode::Default).verdict(&hidden, None)), 'N', "a Read deny of its file");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn r_perm_2_a_path_with_glob_characters_is_allowed_once_never_remembered() {
    let d = repo("glob-grant");
    for name in ["a[1].rs", "*.rs", "b{c.rs", "q?.rs"] {
        std::fs::write(d.join("src").join(name), "").unwrap();
        for call in [read(d.join("src").join(name)), edit(d.join("src").join(name))] {
            assert!(remember(&call).is_empty(), "{name}: not offered for the session or project");
        }
    }
    // Were one written by hand, it would not cover its sibling …
    std::fs::write(d.join("src/a1.rs"), "").unwrap();
    // … and a rule that would not load is never written at all.
    let file = d.join("krowk/permissions.json");
    settings::remember(&file, &d, &["Bash(ok)".into()]).unwrap();
    let bad = format!("Read(/{}/src/b{{c.rs)", d.display());
    assert!(settings::remember(&file, &d, &[bad]).unwrap_err().contains("was not remembered"));
    let cfg = Config { krowk_dir: Some(d.join("krowk")), ..Config::default() };
    let p = Policy::load(&cfg, &d).expect("later prompts still load their settings");
    assert_eq!(letter(&gate(&p, PermissionMode::Default).verdict(&bash("ok"), None)), 'Y');
    let exact = rules::parse(&format!("Read(/{}/src/a[1].rs)", d.display()), "hand", &d).unwrap();
    let at = rules::Places { cwd: &d, home: None };
    assert!(rules::matches(&exact, &read(d.join("src/a1.rs")), &at, true), "why it is never offered: as glob text it covers a1.rs too");
    let _ = std::fs::remove_dir_all(&d);
}

#[tokio::test(flavor = "current_thread")]
async fn r_perm_2_allowing_a_glob_named_file_for_the_session_does_not_cover_its_sibling() {
    let d = repo("glob-session");
    std::fs::create_dir_all(d.join("elsewhere")).unwrap();
    std::fs::write(d.join("src/a[1].rs"), "").unwrap();
    std::fs::write(d.join("src/a1.rs"), "").unwrap();
    let approvals = Approvals::default();
    let g = Gate::new(Policy::modes_only(&d.join("elsewhere")), PermissionMode::Default, SessionGrants::default(), Some(approvals.clone()), None, "s", "t");
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let answering = approvals.clone();
    tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            if let EngineEvent::Approval(req) = ev {
                assert!(req.remember.is_empty(), "nothing to remember is offered");
                answering.answer("s", &req.request_id, ApprovalDecision::AllowSession).unwrap();
            }
        }
    });
    let (_c, cancel) = watch::channel(false);
    g.check(&read(d.join("src/a[1].rs")), "read", &json!({}), None, &tx, &cancel).await.unwrap();
    assert_eq!(letter(&g.verdict(&read(d.join("src/a1.rs")), None)), '?', "the sibling is still asked about");
    assert_eq!(letter(&g.verdict(&read(d.join("src/a[1].rs")), None)), '?', "and so is the file itself, next time");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn r_sub_1_krowks_session_tools_need_no_mode_but_deny_and_ask_rules_and_a_hooks_ask_hold_them() {
    let cwd = repo("session-tools");
    let task = |agent: &str| Call { tool: "Task".into(), access: Access::Session, subject: Some(agent.into()) };
    let todo = Call { tool: "TodoWrite".into(), access: Access::Session, subject: None };
    let none = policy(&cwd, &[]);
    for mode in [PermissionMode::Default, PermissionMode::AcceptEdits, PermissionMode::Plan, PermissionMode::BypassPermissions] {
        assert_eq!(letter(&gate(&none, mode).verdict(&task("reviewer"), None)), 'Y', "{mode:?}: no mode holds it, plan's included");
        assert_eq!(letter(&gate(&none, mode).verdict(&todo, None)), 'Y', "{mode:?}");
        assert_eq!(letter(&gate(&none, mode).verdict(&task("reviewer"), Some(crate::hooks::Decision::Ask))), '?', "{mode:?}: a hook's ask asks");
    }
    let ask = policy(&cwd, &[(Kind::Ask, "Task(reviewer)"), (Kind::Deny, "TodoWrite")]);
    let bypass = gate(&ask, PermissionMode::BypassPermissions);
    assert_eq!(letter(&bypass.verdict(&task("reviewer"), None)), '?', "an ask rule asks, bypass or not");
    assert_eq!(letter(&bypass.verdict(&task("writer"), None)), 'Y', "another agent is not that rule's");
    assert_eq!(letter(&bypass.verdict(&task("Reviewer"), None)), '?', "an agent's name is matched regardless of case");
    let deny = gate(&policy(&cwd, &[(Kind::Deny, "Task(REV*)")]), PermissionMode::BypassPermissions);
    assert_eq!(letter(&deny.verdict(&task("reviewer"), None)), 'N', "the rule's case too, wildcards included");
    assert_eq!(letter(&bypass.verdict(&todo, None)), 'N');
    match bypass.verdict(&task("reviewer"), None) {
        Verdict::Ask { remember, .. } => assert_eq!(remember, ["Task(reviewer)"]),
        v => panic!("{v:?}"),
    }
}

/// Claude Code's own user settings, the way Claude Code writes them, with
/// the `auto` mode krowk does not run.
fn claude_settings(dir: &Path, deny: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("settings.json"),
        serde_json::to_string_pretty(&json!({
            "$schema": "https://json.schemastore.org/claude-code-settings.json",
            "model": "opus",
            "permissions": {"allow": ["Bash(npm test)"], "deny": [deny], "defaultMode": "auto"},
            "enabledPlugins": {"caveman@caveman": true},
            "alwaysThinkingEnabled": true
        }))
        .unwrap(),
    )
    .unwrap();
}

#[test]
fn r_perm_1_a_default_mode_krowk_does_not_run_is_read_as_default_with_a_notice() {
    let d = repo("auto-user");
    let claude = d.join("home/.claude");
    claude_settings(&claude, "Read(.env)");
    // Claude's file names `auto`, which krowk does not run, and nothing
    // else names a mode: default, and a notice that says how to choose.
    let cfg = Config { claude_dir: Some(claude.clone()), home: Some(d.join("home")), ..Config::default() };
    let p = Policy::load(&cfg, &d.join("src")).expect("an unknown defaultMode does not refuse the settings");
    assert_eq!(p.loaded.default_mode, None, "runs in default");
    assert!(matches!(p.loaded.notices.as_slice(), [n] if n.starts_with("~/.claude/settings.json sets defaultMode \"auto\"") && n.contains("asks before edits and commands")), "{:?}", p.loaded.notices);
    // krowk's own config naming a mode is the mode, and nothing is said.
    let cfg = Config { user: Some(json!({"permissions": {"defaultMode": "acceptEdits"}})), claude_dir: Some(claude.clone()), ..Config::default() };
    let p = Policy::load(&cfg, &d.join("src")).unwrap();
    assert_eq!(p.loaded.default_mode, Some(PermissionMode::AcceptEdits), "a mode krowk does not run sets nothing");
    assert!(p.loaded.notices.is_empty(), "{:?}", p.loaded.notices);
    let g = gate(&p, PermissionMode::Default);
    assert_eq!(letter(&g.verdict(&read(d.join("src/.env")), None)), 'N', "the file's deny rule still holds");
    assert_eq!(letter(&g.verdict(&bash("npm test"), None)), 'Y', "and its allow rule");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn r_perm_1_an_unknown_default_mode_never_excuses_a_rule_that_does_not_parse() {
    let d = repo("auto-bad-deny");
    let claude = d.join("home/.claude");
    claude_settings(&claude, "Read(.env");
    let cfg = Config { claude_dir: Some(claude.clone()), ..Config::default() };
    let e = Policy::load(&cfg, &d.join("src")).unwrap_err();
    assert!(e.contains(&claude.join("settings.json").display().to_string()) && e.contains("permissions.deny"), "{e}");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn r_perm_1_an_unknown_default_mode_in_a_repository_counts_only_once_trusted() {
    let d = repo("auto-repo");
    std::fs::create_dir_all(d.join(".claude")).unwrap();
    std::fs::write(d.join(".claude/settings.json"), json!({"permissions": {"defaultMode": "auto"}}).to_string()).unwrap();
    let user = Some(json!({"permissions": {"defaultMode": "acceptEdits"}}));
    let untrusted = Policy::load(&Config { user: user.clone(), ..Config::default() }, &d.join("src")).unwrap();
    assert_eq!(untrusted.loaded.default_mode, Some(PermissionMode::AcceptEdits), "an untrusted repository's mode is ignored");
    assert!(untrusted.loaded.notices.is_empty() && !untrusted.loaded.widens, "and neither noticed nor a reason to ask for trust");
    let cfg = Config { user, trusted: Some(Arc::new(|_: &Path| true)), ..Config::default() };
    let trusted = Policy::load(&cfg, &d.join("src")).unwrap();
    assert_eq!(trusted.loaded.default_mode, Some(PermissionMode::AcceptEdits), "trusted, it still sets nothing");
    assert!(trusted.loaded.notices.is_empty(), "the person's own mode stands, so nothing is said: {:?}", trusted.loaded.notices);
    let alone = Policy::load(&Config { trusted: Some(Arc::new(|_: &Path| true)), ..Config::default() }, &d.join("src")).unwrap();
    assert_eq!((alone.loaded.default_mode, alone.loaded.notices.len()), (None, 1), "with no other mode it is default, said once");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn r_perm_1_a_mode_krowk_does_not_run_other_than_auto_narrows_to_default() {
    // `dontAsk` is Claude Code's deny-by-default: stricter, so it must not
    // leave an earlier, looser mode standing.
    let d = repo("dont-ask");
    std::fs::create_dir_all(d.join(".claude")).unwrap();
    std::fs::write(d.join(".claude/settings.local.json"), json!({"permissions": {"defaultMode": "dontAsk"}}).to_string()).unwrap();
    let cfg = Config { user: Some(json!({"permissions": {"defaultMode": "acceptEdits"}})), trusted: Some(Arc::new(|_: &Path| true)), ..Config::default() };
    let p = Policy::load(&cfg, &d.join("src")).unwrap();
    assert_eq!(p.loaded.default_mode, Some(PermissionMode::Default));
    assert!(matches!(p.loaded.notices.as_slice(), [n] if n.contains("\"dontAsk\"")), "{:?}", p.loaded.notices);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn r_perm_1_a_narrowing_notice_goes_when_a_later_file_sets_the_mode() {
    // Claude's own file says dontAsk (default, said); a trusted repository
    // then sets acceptEdits, which is what runs: nothing is said.
    let d = repo("dont-ask-then");
    let claude = d.join("home/.claude");
    std::fs::create_dir_all(&claude).unwrap();
    std::fs::write(claude.join("settings.json"), json!({"permissions": {"defaultMode": "dontAsk"}}).to_string()).unwrap();
    std::fs::create_dir_all(d.join(".claude")).unwrap();
    std::fs::write(d.join(".claude/settings.json"), json!({"permissions": {"defaultMode": "acceptEdits"}}).to_string()).unwrap();
    let cfg = Config { claude_dir: Some(claude.clone()), trusted: Some(Arc::new(|_: &Path| true)), ..Config::default() };
    let p = Policy::load(&cfg, &d.join("src")).unwrap();
    assert_eq!(p.loaded.default_mode, Some(PermissionMode::AcceptEdits));
    assert!(p.loaded.notices.is_empty(), "{:?}", p.loaded.notices);
    // A null mode is no mode at all.
    std::fs::write(d.join(".claude/settings.json"), json!({"permissions": {"defaultMode": null}}).to_string()).unwrap();
    let cfg = Config { user: Some(json!({"permissions": {"defaultMode": "acceptEdits"}})), trusted: Some(Arc::new(|_: &Path| true)), ..Config::default() };
    let p = Policy::load(&cfg, &d.join("src")).unwrap();
    assert_eq!((p.loaded.default_mode, p.loaded.notices.len()), (Some(PermissionMode::AcceptEdits), 0));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn r_perm_1_a_default_mode_that_is_not_a_string_is_read_as_default_with_a_notice() {
    let d = repo("mode-not-string");
    let claude = d.join("home/.claude");
    std::fs::create_dir_all(&claude).unwrap();
    for bad in [json!(2), json!(["acceptEdits"]), json!({"mode": "auto"}), json!(true)] {
        std::fs::write(claude.join("settings.json"), json!({"permissions": {"defaultMode": bad}}).to_string()).unwrap();
        let cfg = Config { user: Some(json!({"permissions": {"defaultMode": "acceptEdits"}})), claude_dir: Some(claude.clone()), ..Config::default() };
        let p = Policy::load(&cfg, &d.join("src")).unwrap();
        assert_eq!(p.loaded.default_mode, Some(PermissionMode::Default), "{bad} does not leave the earlier acceptEdits standing");
        let file = claude.join("settings.json").display().to_string();
        assert!(matches!(p.loaded.notices.as_slice(), [n] if n.contains(&file) && n.contains(&bad.to_string())), "{bad}: {:?}", p.loaded.notices);
    }
    let _ = std::fs::remove_dir_all(&d);
}
