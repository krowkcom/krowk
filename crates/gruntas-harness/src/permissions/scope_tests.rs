//! What the scope lets the file tools reach, fences and keeps secret,
//! asked of it directly: the working directory however a path is spelled
//! or leads (R-TOOL-1), the fenced directories (`.git`, `.claude`, `.codex`,
//! `.krowk`) and the protected ones (R-BACK-3, R-SUB-5), and gruntas's home.

use super::*;

/// A fresh scratch directory for one test, as it leads.
fn dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("gruntas-harness-scope-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d.canonicalize().unwrap()
}

/// The scope bypassPermissions leaves: nothing outside, nothing fenced.
fn bypassed(cwd: &Path) -> Scope {
    Scope { outside: true, open: true, ..Scope::within(cwd) }
}

#[cfg(unix)]
#[test]
fn r_tool_1_file_tools_stay_inside_the_working_directory_unless_bypassed() {
    use std::os::unix::fs::symlink;
    let base = dir("inside");
    let (cwd, outside) = (base.join("cwd"), base.join("outside"));
    std::fs::create_dir_all(cwd.join("sub")).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("secret.txt"), "secret\n").unwrap();
    std::fs::write(cwd.join("a.txt"), "a\n").unwrap();
    symlink(outside.join("secret.txt"), cwd.join("file-link")).unwrap();
    symlink(&outside, cwd.join("dir-link")).unwrap();
    symlink(outside.join("not-yet.txt"), cwd.join("dangling")).unwrap();
    symlink(cwd.join("a.txt"), cwd.join("inside-link")).unwrap();
    let scope = Scope::within(&cwd);
    let refused = |r: Result<PathBuf, String>| r.is_err_and(|e| e.contains("outside the working directory") && e.contains("bypassPermissions"));
    // Written however the path is spelled or leads: `..`, absolute, a
    // symlinked directory, a dangling symlink a write would follow, a
    // symlinked file — and a patch's Add, Move or Delete target.
    let abs = outside.join("new.txt").display().to_string();
    for path in ["../outside/new.txt", abs.as_str(), "dir-link/new.txt", "dangling", "file-link", "sub/../../outside/x", "new/../../x", "../outside/p.txt", "dir-link/moved.txt", "file-link/../../outside/secret.txt"] {
        assert!(refused(scope.edit_path(path)), "edit {path}: {:?}", scope.edit_path(path));
    }
    // Read and searched the same: a symlinked file is judged by its target,
    // a search root by where it leads.
    for path in ["file-link", "../outside/secret.txt", "..", "dir-link", "/"] {
        assert!(refused(scope.path(path)), "read {path}: {:?}", scope.path(path));
    }
    assert!(matches!(scope.reach(&cwd.join("dangling"), true), Reach::Outside(why) if why.contains("it leads to")), "a dangling link is judged by where a write would create it");
    // Inside is fine however it is spelled, and a symlink inside that leads
    // inside is returned as spelled, so the write goes through the link.
    assert_eq!(scope.edit_path("sub/../b.txt"), Ok(cwd.join("sub/../b.txt")));
    assert_eq!(scope.edit_path(&cwd.join("c.txt").display().to_string()), Ok(cwd.join("c.txt")));
    assert_eq!(scope.edit_path("inside-link"), Ok(cwd.join("inside-link")));
    assert_eq!(scope.reach(&cwd.join("inside-link"), true), Reach::Inside);
    // Bypassed, the working directory is no fence.
    assert_eq!(bypassed(&cwd).edit_path("../outside/new.txt"), Ok(cwd.join("../outside/new.txt")));
    assert_eq!(bypassed(&cwd).path("file-link"), Ok(cwd.join("file-link")));
    // The directories the settings add are reached as the working
    // directory is; the read-only ones only for reading.
    let added = Scope { roots: vec![outside.clone()], ..Scope::within(&cwd) };
    assert!(added.edit_path("file-link").is_ok() && added.path("dir-link").is_ok());
    let readable = Scope { read_roots: vec![outside.clone()], ..Scope::within(&cwd) };
    assert!(readable.path("file-link").is_ok() && refused(readable.edit_path("file-link")));
    let _ = std::fs::remove_dir_all(base);
}

/// The scope half of the `.git` fence: nothing under `.git` (or `.claude`)
/// may be changed below bypass, however it is spelled or reached — and it
/// may be read.
#[test]
fn nothing_inside_git_is_changed_and_git_runs_nothing_from_its_config() {
    let d = dir("git-guard");
    std::fs::create_dir_all(d.join(".git/hooks")).unwrap();
    std::fs::write(d.join(".git/config"), "[core]\n").unwrap();
    std::fs::write(d.join("a.txt"), "needle\n").unwrap();
    let scope = Scope::within(&d);
    let refused = |p: &str| scope.edit_path(p).is_err_and(|e| e.contains("inside a .git directory"));
    for p in [".git/config", ".git/hooks/pre-commit", "sub/../.git/config", ".git/moved"] {
        assert!(refused(p), "{p}");
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(d.join(".git"), d.join("gitlink")).unwrap();
        assert!(refused("gitlink/config"), "judged by where it leads");
    }
    // Reading it is fine, and bypassed the fence is gone.
    assert_eq!(scope.path(".git/config"), Ok(d.join(".git/config")));
    assert_eq!(scope.reach(&d.join(".git/config"), false), Reach::Inside);
    assert!(bypassed(&d).edit_path(".git/info/note").is_ok());
    // .claude is fenced the same way: Claude Code runs its hooks.
    let claude = |p: &str| scope.edit_path(p).is_err_and(|e| e.contains("inside a .claude directory"));
    assert!(claude(".claude/settings.json"));
    assert!(claude("sub/.claude/agents/x.md"), "any component");
    // However a case-insensitive or a Windows file system would open it.
    for p in [".Claude/settings.json", ".CLAUDE/hooks/h.sh", ".claude./settings.json", ".claude /agents/a.md", "sub/.ClAuDe/settings.local.json"] {
        assert!(claude(p), "{p}");
        assert_eq!(fenced_dir(Path::new(p)), Some(".claude"), "{p}");
    }
    for p in [".GIT/config", ".Git/hooks/pre-commit", ".git./config", ".git /hooks/x"] {
        assert!(refused(p), "{p}");
        assert_eq!(fenced_dir(Path::new(p)), Some(".git"), "{p}");
    }
    assert!(scope.edit_path(".claudette/notes.md").is_ok(), "a name that only starts like it is fine");
    assert_eq!(fenced_dir(Path::new(".claudette/notes.md")), None);
    assert!(bypassed(&d).edit_path(".claude/settings.json").is_ok());
    let _ = std::fs::remove_dir_all(d);
}

/// Codex runs what `.codex` holds (project config, rules, hooks), and
/// what its home holds for every session: both are kept like `.git`,
/// in any case a case-insensitive file system would open.
#[test]
fn r_back_3_codex_config_and_the_instances_home_are_kept_like_git() {
    let d = dir("codex-guard");
    let scope = Scope::within(&d);
    let fenced = |p: &str, dir: &str| scope.edit_path(p).is_err_and(|e| e.contains(&format!("inside a {dir} directory")));
    assert!(fenced(".codex/config.toml", ".codex"));
    assert!(fenced("sub/.Codex/rules/x.rules", ".codex"), "any component, any case");
    assert!(fenced(".codex/hooks.json", ".codex"), "a patch's Add target too");
    assert!(fenced(".GIT/config", ".git"));
    assert!(fenced(".claude/settings.json", ".claude"));
    // The instance's own home, protected by name.
    let home = d.join("codex-home");
    std::fs::create_dir_all(&home).unwrap();
    let scope = Scope { protected: vec![home.clone()], ..Scope::within(&d) };
    assert!(scope.edit_path("codex-home/config.toml").unwrap_err().contains("settings decide what runs"));
    assert!(scope.edit_path(&home.join("sub/x").display().to_string()).is_err());
    assert!(scope.edit_path("codex-homework.txt").is_ok(), "a sibling that shares the prefix is not inside it");
    assert!(scope.path("codex-home/config.toml").is_ok(), "read, it is not fenced");
    // Named through a link, and spelled as the link: both are the home.
    #[cfg(unix)]
    {
        let alias = d.join("home-alias");
        std::os::unix::fs::symlink(&home, &alias).unwrap();
        let via_alias = Scope { protected: vec![alias.clone()], ..Scope::within(&d) };
        assert!(via_alias.edit_path("codex-home/config.toml").is_err(), "the home as it leads");
        assert!(via_alias.edit_path("home-alias/config.toml").is_err(), "the home as spelled");
    }
    assert!(Scope { open: true, outside: true, ..scope }.edit_path("codex-home/config.toml").is_ok());
    assert!(bypassed(&d).edit_path(".codex/config.toml").is_ok());
    let _ = std::fs::remove_dir_all(d);
}

/// Ticket 10: a repository's agent definitions pick a subagent's model,
/// prompt and tools, so the scope fences them below bypass.
#[test]
fn r_sub_5_gruntas_agent_definitions_are_kept_like_git() {
    let d = dir("gruntas-guard");
    let scope = Scope::within(&d);
    for path in [".gruntas/agents/evil.md", ".GRUNTAS/agents/evil.md", "sub/.gruntas./agents/x.md", ".claude/agents/evil.md"] {
        let r = scope.edit_path(path);
        assert!(r.as_ref().is_err_and(|e| e.contains("inside a .")), "{path}: {r:?}");
        assert!(matches!(scope.reach(&d.join(path), true), Reach::Fenced(_)), "{path}");
    }
    assert!(bypassed(&d).edit_path(".gruntas/agents/ok.md").is_ok());
    assert!(scope.edit_path(".krowk/agents/x.md").is_ok(), "an old .krowk is no fenced directory");
    let _ = std::fs::remove_dir_all(d);
}

/// gruntas's home is secret wherever it sits and however it is reached:
/// spelled with `..`, in another case, or through a symlink — fenced for
/// reading as well as writing, and opened by no mode.
#[cfg(unix)]
#[test]
fn gruntas_home_is_secret_however_it_is_spelled_or_reached() {
    let d = dir("secret");
    let home = d.join(".gruntas-home");
    std::fs::create_dir_all(home.join("keys")).unwrap();
    std::os::unix::fs::symlink(&home, d.join("alias")).unwrap();
    let scope = Scope { secrets: vec![home.clone()], ..bypassed(&d) };
    for p in [home.join("keys/k"), d.join(".gruntas-home/../.gruntas-home/keys"), d.join(".GRUNTAS-HOME/keys"), d.join("alias/keys/k")] {
        assert!(scope.secret(&p), "{}", p.display());
        for edit in [false, true] {
            assert!(matches!(scope.reach(&p, edit), Reach::Fenced(why) if why.contains("gruntas's home")), "{} (edit: {edit})", p.display());
        }
    }
    assert!(!scope.secret(&d.join(".gruntas-homework")) && !scope.secret(&d.join("a.txt")));
    assert!(!Scope::within(&d).secret(&home.join("keys")), "no secrets named, none kept");
    let _ = std::fs::remove_dir_all(d);
}

/// `real_path` follows every symlink, a dangling one to where a write would
/// create its target, and refuses a loop rather than following it forever.
#[cfg(unix)]
#[test]
fn real_path_follows_links_to_where_a_write_would_land_and_refuses_a_loop() {
    use std::os::unix::fs::symlink;
    let d = dir("real-path");
    symlink(d.join("target.txt"), d.join("dangling")).unwrap();
    symlink("relative-target", d.join("rel")).unwrap();
    symlink(d.join("loop-b"), d.join("loop-a")).unwrap();
    symlink(d.join("loop-a"), d.join("loop-b")).unwrap();
    assert_eq!(real_path(&d.join("dangling"), 0), Ok(d.join("target.txt")));
    assert_eq!(real_path(&d.join("rel"), 0), Ok(d.join("relative-target")));
    assert_eq!(real_path(&d.join("missing/deeper/x"), 0), Ok(d.join("missing/deeper/x")));
    assert_eq!(real_path(&d.join("missing/.."), 0), Ok(d.clone()));
    assert!(real_path(&d.join("loop-a"), 0).is_err_and(|e| e.contains("too many levels")));
    assert_eq!(resolve(&d, "x/y"), d.join("x/y"));
    assert_eq!(resolve(&d, "/abs"), PathBuf::from("/abs"));
    assert!(!Hidden::default().hides(&d), "no rule, nothing hidden");
    let _ = std::fs::remove_dir_all(d);
}
