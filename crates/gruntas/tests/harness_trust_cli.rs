//! `gruntas trust`, the built binary against a stand-in home: grant, list and
//! revoke the repositories the engine reads its trust from, with no
//! terminal, in JSON and in words.

#![cfg(all(feature = "harness", unix))]

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Sandbox {
        let root = std::env::temp_dir().join(format!("krowk-trust-cli-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["home", "data", "repo", "other"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        let b = Sandbox { root: root.canonicalize().unwrap() };
        for d in ["repo", "other"] {
            git(&b.root.join(d), &["init", "-q", "-b", "main"]);
        }
        std::fs::write(b.repo().join("a.txt"), "a\n").unwrap();
        git(&b.repo(), &["add", "a.txt"]);
        git(&b.repo(), &["commit", "-q", "-m", "one"]);
        b
    }

    fn repo(&self) -> PathBuf {
        self.root.join("repo")
    }

    fn krowk_in(&self, dir: &Path, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_gruntas"))
            .args(args)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.root.join("home"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("GRUNTAS_NO_UPDATE_CHECK", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .current_dir(dir)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn gruntas(&self, args: &[&str]) -> Output {
        self.krowk_in(&self.repo(), args)
    }

    fn json(&self, args: &[&str]) -> Value {
        let o = self.gruntas(args);
        assert!(o.status.success(), "gruntas {args:?}: {}", String::from_utf8_lossy(&o.stderr));
        serde_json::from_slice(&o.stdout).unwrap()
    }

    fn listed(&self) -> Value {
        self.json(&["trust", "--list", "--json"])["data"]["directories"].clone()
    }
}

fn git(dir: &Path, args: &[&str]) {
    let o = Command::new("git").args(["-c", "user.name=t", "-c", "user.email=t@t"]).args(args).current_dir(dir).env("GIT_CONFIG_GLOBAL", "/dev/null").output().unwrap();
    assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
}

fn text(o: &Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
}

/// R-CUT-3: grant, list and revoke, with the JSON shapes the spec names.
#[test]
fn trust_grant_then_list_shows_the_repository_and_revoke_removes_it() {
    let b = Sandbox::new("round");
    assert_eq!(b.listed(), json!([]), "nothing is trusted yet");
    let repo = b.repo().display().to_string();
    // From a subdirectory, a path inside the repository trusts the repository.
    std::fs::create_dir_all(b.repo().join("src")).unwrap();
    let granted = b.json(&["trust", "src", "--json"]);
    assert_eq!(granted["data"], json!({"directory": repo, "trusted": true}), "{granted}");
    assert_eq!(b.listed(), json!([repo]));
    // Idempotent, and a second repository is listed beside it.
    b.json(&["trust", "--json"]);
    let other = b.root.join("other").display().to_string();
    b.json(&["trust", &other, "--json"]);
    assert_eq!(b.listed(), json!([other, repo]));
    // Human output works without a terminal too.
    let human = b.gruntas(&["trust", "--list", "--format", "human"]);
    assert_eq!(String::from_utf8_lossy(&human.stdout), format!("{other}\n{repo}\n"));

    let revoked = b.json(&["trust", "--revoke", "--json"]);
    assert_eq!(revoked["data"], json!({"directory": repo, "trusted": false}), "{revoked}");
    assert_eq!(b.listed(), json!([other]));
    // Revoking what was not trusted is not an error.
    let again = b.json(&["trust", "--revoke", "--json"]);
    assert_eq!(again["data"]["trusted"], false);

    // A repository deleted since it was trusted is revoked by the path --list shows.
    std::fs::remove_dir_all(b.root.join("other")).unwrap();
    assert_eq!(b.listed(), json!([other]));
    let gone = b.json(&["trust", "--revoke", &other, "--json"]);
    assert_eq!(gone["data"], json!({"directory": other, "trusted": false}), "{gone}");
    assert_eq!(b.listed(), json!([]));
}

/// Revoking removes the MCP digest recorded with the entry.
#[test]
fn trust_revoke_drops_the_recorded_mcp_digest() {
    let b = Sandbox::new("digest");
    std::fs::write(b.repo().join(".mcp.json"), r#"{"mcpServers": {"x": {"command": "x"}}}"#).unwrap();
    b.json(&["trust", "--json"]);
    let file = b.root.join("home/.gruntas/trusted.json");
    let held: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    assert!(held["mcp"].as_object().is_some_and(|m| m.len() == 1), "{held}");
    b.json(&["trust", "--revoke", "--json"]);
    let held: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    assert_eq!(held.get("mcp"), None, "{held}");
    assert_eq!(held["directories"], json!([]));
}

/// D5: `/` and the home directory are refused with the store's own message.
#[test]
fn trust_refuses_root_and_the_home_directory_with_the_stores_message() {
    let b = Sandbox::new("refused");
    let out = b.gruntas(&["trust", "/", "--json"]);
    assert!(!out.status.success());
    let said = text(&out);
    assert!(said.contains("/ is not remembered as trusted: / is every directory on the machine"), "{said}");
    let home = b.root.join("home").display().to_string();
    let out = b.gruntas(&["trust", &home, "--json"]);
    assert!(!out.status.success() && text(&out).contains("is your home directory"), "{}", text(&out));
    assert_eq!(b.listed(), json!([]));
}

/// A gruntas worktree is trusted as its main repository.
#[test]
fn trust_of_a_krowk_worktree_records_the_main_repository() {
    let b = Sandbox::new("worktree");
    let made = b.json(&["worktrees", "add", "--json"]);
    let wt = PathBuf::from(made["data"]["path"].as_str().unwrap());
    assert!(wt.is_dir());
    let granted = b.krowk_in(&wt, &["trust", "--json"]);
    assert!(granted.status.success(), "{}", text(&granted));
    let v: Value = serde_json::from_slice(&granted.stdout).unwrap();
    assert_eq!(v["data"], json!({"directory": b.repo().display().to_string(), "trusted": true}), "{v}");
    assert_eq!(b.listed(), json!([b.repo().display().to_string()]));
    // By path from elsewhere, and revoked the same way.
    let revoked = b.krowk_in(&b.root.join("other"), &["trust", "--revoke", wt.to_str().unwrap(), "--json"]);
    assert!(revoked.status.success(), "{}", text(&revoked));
    assert_eq!(b.listed(), json!([]));
}

#[test]
fn trust_flags_are_checked() {
    let b = Sandbox::new("flags");
    assert!(text(&b.gruntas(&["trust", "--list", "--revoke"])).contains("pass one"));
    assert!(text(&b.gruntas(&["trust", "--list", "x"])).contains("takes no path"));
    assert!(text(&b.gruntas(&["trust", "a", "b"])).contains("takes one directory"));
    assert!(text(&b.gruntas(&["trust", "/no/such/dir"])).contains("is not a directory"));
    assert!(text(&b.gruntas(&["status", "--revoke"])).contains("only a flag of `gruntas trust`"));
}
