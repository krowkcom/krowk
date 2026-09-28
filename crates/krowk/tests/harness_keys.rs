//! Stored API keys (R-CRED-1, R-INST-5 as amended): `krowk connect <vendor>
//! --method api-key` storing a pasted key, a `$VAR` or a `!command` in
//! krowk's provider credentials file, the built binary against the
//! stand-in Anthropic API, fake vendor binaries and a fake password
//! manager. A sentinel key is followed through every path — connect, status,
//! providers list, doctor, a turn's stream-json and log, a failing request,
//! disconnect — and found only in the credentials file.

#![cfg(all(feature = "harness", unix))]

#[path = "../../krowk-harness/tests/common/mock.rs"]
mod mock;
#[path = "common/pty.rs"]
mod pty;

use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

const SENTINEL: &str = "sk-ant-STORED-SENTINEL-5e21b7";
/// What the environment holds beside it: never sent while a key is stored.
const DECOY: &str = "sk-ant-ENV-DECOY-09c3aa";

struct Sandbox {
    root: PathBuf,
    url: String,
}

impl Sandbox {
    fn new(name: &str, url: &str) -> Sandbox {
        let root = std::env::temp_dir().join(format!("krowk-keys-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["home", "repo/.git", "bin"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        let b = Sandbox { root: root.canonicalize().unwrap(), url: url.into() };
        for v in ["claude", "codex"] {
            let fake = Path::new(env!("CARGO_MANIFEST_DIR")).join("../krowk-harness/tests/fixtures").join(v).join(format!("fake-{v}"));
            b.install(v, &std::fs::read_to_string(fake).unwrap());
        }
        // A password manager: prints the key and counts its runs; told to
        // fail, it prints the key to both streams and exits 1.
        b.install(
            "fake-pass",
            &format!("#!/bin/sh\necho run >> \"{}\"\nif [ -e \"{}\" ]; then sleep 1; echo \"{SENTINEL}\"; echo \"gpg: {SENTINEL}\" >&2; exit 1; fi\necho \"{SENTINEL}\"\n", b.root.join("pass.log").display(), b.root.join("pass.fail").display()),
        );
        b
    }

    fn install(&self, name: &str, script: &str) {
        let bin = self.root.join("bin").join(name);
        std::fs::write(&bin, script).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn command(&self, args: &[&str], env: &[(&str, &str)]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_krowk"));
        c.args(args)
            .env_clear()
            .env("PATH", format!("{}:/usr/bin:/bin", self.root.join("bin").display()))
            .env("HOME", self.root.join("home"))
            .env("KROWK_NO_UPDATE_CHECK", "1")
            // The TUI runs its sessions in this process here: the daemon has
            // its own tests (tui.rs, host_daemon), and none is left behind.
            .env("KROWK_TUI_HOST", "local")
            .env("KROWK_API_URL", "http://127.0.0.1:9/v1")
            .env("ANTHROPIC_BASE_URL", &self.url)
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

    /// krowk with `input` piped to its stdin.
    fn piped(&self, args: &[&str], input: &str) -> Output {
        let mut child = self.command(args, &[]).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
        child.wait_with_output().unwrap()
    }

    fn credentials(&self) -> PathBuf {
        self.root.join("home/.krowk/credentials.json")
    }

    fn stored(&self) -> Value {
        std::fs::read_to_string(self.credentials()).map(|s| serde_json::from_str(&s).unwrap()).unwrap_or(json!({}))
    }

    fn config(&self) -> Value {
        std::fs::read_to_string(self.root.join("home/.krowk/config.json")).map(|s| serde_json::from_str(&s).unwrap()).unwrap_or(json!({}))
    }

    fn row(&self, instance: &str, env: &[(&str, &str)]) -> Value {
        let out = self.krowk(&["status", "--json"], env);
        let v: Value = serde_json::from_slice(&out.stdout).or_else(|_| serde_json::from_slice(&out.stderr)).unwrap();
        let rows = v.pointer("/data/instances").or_else(|| v.pointer("/error/details/instances")).unwrap_or_else(|| panic!("{v}"));
        rows.as_array().unwrap().iter().find(|r| r["instance"] == instance).cloned().unwrap_or_else(|| panic!("{instance} not listed"))
    }

    fn pass_runs(&self) -> usize {
        std::fs::read_to_string(self.root.join("pass.log")).unwrap_or_default().lines().count()
    }

    /// Every file under the sandbox holding `needle`, but the credentials
    /// file and the fake's own log.
    fn holding(&self, needle: &str) -> Vec<PathBuf> {
        fn walk(d: &Path, out: &mut Vec<PathBuf>) {
            for e in std::fs::read_dir(d).into_iter().flatten().flatten() {
                let p = e.path();
                if p.is_dir() && !p.is_symlink() {
                    walk(&p, out);
                } else if p.is_file() {
                    out.push(p);
                }
            }
        }
        let mut files = Vec::new();
        walk(&self.root, &mut files);
        let bin = self.root.join("bin");
        files.into_iter().filter(|p| *p != self.credentials() && !p.starts_with(&bin)).filter(|p| std::fs::read(p).is_ok_and(|b| b.windows(needle.len()).any(|w| w == needle.as_bytes()))).collect()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn printed(out: &Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))
}

fn answer() -> mock::Mock {
    mock::serve(|_, _| mock::Reply::sse(&mock::text_stream("stored keys work")))
}

#[test]
fn r_cred_1_a_pasted_key_is_stored_0600_used_first_and_never_shown_anywhere() {
    let m = answer();
    let b = Sandbox::new("literal", &m.url);
    let out = b.piped(&["connect", "anthropic", "--method", "api-key", "--key-stdin", "--json"], &format!("{SENTINEL}\n"));
    assert!(out.status.success(), "{}", printed(&out));
    let c: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(c["data"]["key_source"], "stored", "{c}");
    // In the credentials file, 0600, and nowhere in the definition.
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(b.credentials()).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(b.stored()["keys"]["anthropic"], json!({"literal": SENTINEL}));
    assert!(!b.config().to_string().contains(SENTINEL));

    // Used before the environment's key, which is set too.
    let env = [("ANTHROPIC_API_KEY", DECOY)];
    let row = b.row("anthropic", &env);
    assert_eq!((row["state"].as_str(), row["source"].as_str()), (Some("ready"), Some("stored")), "{row}");
    let turn = b.krowk(&["-p", "hello", "--model", "anthropic/claude-sonnet-4-6", "--output-format", "stream-json"], &env);
    assert!(turn.status.success(), "{}", printed(&turn));
    assert_eq!(m.seen.lock().unwrap()[0].header("x-api-key"), Some(SENTINEL), "the stored key, not the environment's");

    // A failing request: refused by the API, and the key is not in the error.
    let refused = mock::serve(|_, _| mock::Reply::json(401, &json!({"type": "error", "error": {"type": "authentication_error", "message": "invalid x-api-key"}})));
    let failed = b.krowk(&["-p", "hello", "--model", "anthropic/claude-sonnet-4-6", "--output-format", "stream-json"], &[("ANTHROPIC_BASE_URL", &refused.url)]);
    assert!(!failed.status.success());
    assert_eq!(refused.seen.lock().unwrap()[0].header("x-api-key"), Some(SENTINEL));

    let mut outs = vec![out, turn, failed];
    for args in [&["status", "--json"][..], &["status"], &["providers", "list", "--json"], &["providers", "list"], &["doctor", "--json"], &["doctor"]] {
        outs.push(b.krowk(args, &env));
    }
    for o in &outs {
        assert!(!printed(o).contains(SENTINEL), "the key was printed: {}", printed(o));
    }
    // Not in the session log, krowk.db, or anything else krowk wrote.
    assert_eq!(b.holding(SENTINEL), Vec::<PathBuf>::new());

    // Disconnected, the key is deleted, and the variable is read again.
    let gone = b.krowk(&["disconnect", "anthropic", "--format", "human"], &env);
    assert!(gone.status.success(), "{}", printed(&gone));
    assert!(printed(&gone).contains("its stored key is deleted") && printed(&gone).contains("$ANTHROPIC_API_KEY is set too"), "{}", printed(&gone));
    assert!(!std::fs::read_to_string(b.credentials()).unwrap().contains(SENTINEL));
    assert_eq!(b.row("anthropic", &env)["source"], "env ANTHROPIC_API_KEY");
}

#[test]
fn r_cred_1_a_var_and_a_command_reference_resolve_and_one_that_fails_has_no_fallback() {
    let m = answer();
    let b = Sandbox::new("refs", &m.url);
    let env = [("ANTHROPIC_API_KEY", DECOY)];

    // `$VAR`: read from that variable, and with it unset nothing is sent.
    let out = b.krowk(&["connect", "anthropic", "--method", "api-key", "--name", "work", "--key-ref", "$WORK_SECRET", "--base-url", &m.url], &[]);
    assert!(out.status.success(), "{}", printed(&out));
    assert_eq!(b.stored()["keys"]["anthropic:work"], json!({"env": "WORK_SECRET"}));
    let row = b.row("anthropic:work", &[("WORK_SECRET", SENTINEL)]);
    assert_eq!((row["state"].as_str(), row["source"].as_str()), (Some("ready"), Some("stored ($WORK_SECRET)")), "{row}");
    let unset = b.row("anthropic:work", &[("ANTHROPIC_WORK_API_KEY", DECOY)]);
    assert_eq!((unset["state"].as_str(), unset["var"].as_str()), (Some("key_not_set"), Some("WORK_SECRET")), "{unset}");
    let refused = b.krowk(&["-p", "hi", "--model", "anthropic:work/claude-sonnet-4-6"], &[("ANTHROPIC_WORK_API_KEY", DECOY)]);
    assert_eq!(refused.status.code(), Some(3), "{}", printed(&refused));
    assert!(printed(&refused).contains("$WORK_SECRET, which is not set") && printed(&refused).contains("does not fall back"), "{}", printed(&refused));
    let sent = b.krowk(&["-p", "hi", "--model", "anthropic:work/claude-sonnet-4-6"], &[("WORK_SECRET", SENTINEL)]);
    assert!(sent.status.success(), "{}", printed(&sent));
    assert_eq!(m.seen.lock().unwrap().last().unwrap().header("x-api-key"), Some(SENTINEL));

    // `!command`: run once to connect, then once per krowk process — status
    // runs it, and a turn in another process runs it again.
    let out = b.krowk(&["connect", "anthropic", "--method", "api-key", "--key-ref", "!fake-pass show anthropic", "--format", "human"], &[]);
    assert!(out.status.success(), "{}", printed(&out));
    assert_eq!(b.pass_runs(), 1, "connecting checks the command");
    assert!(printed(&out).contains("key stored (!fake-pass …) in krowk's credentials file (0600), used before any variable"), "{}", printed(&out));
    let again = b.krowk(&["connect", "anthropic", "--method", "api-key", "--format", "human"], &[]);
    assert!(printed(&again).contains("key was already stored (!fake-pass …)"), "{}", printed(&again));
    // Naming a variable while a key is stored would change nothing: refused.
    let named = b.krowk(&["connect", "anthropic", "--method", "api-key", "--api-key-env", "OTHER_KEY"], &[]);
    assert!(!named.status.success() && printed(&named).contains("`krowk disconnect anthropic` first"), "{}", printed(&named));
    // A stored $VAR that is unset says so, and offers nothing to try yet.
    let unset = b.krowk(&["connect", "openai", "--method", "api-key", "--key-ref", "$NOT_SET_HERE", "--format", "human"], &[]);
    assert!(printed(&unset).contains("$NOT_SET_HERE is not set here") && !printed(&unset).contains("try it"), "{}", printed(&unset));
    let row = b.row("anthropic", &env);
    assert_eq!((row["state"].as_str(), row["source"].as_str()), (Some("ready"), Some("stored (!fake-pass …)")), "{row}");
    assert_eq!(b.pass_runs(), 2);
    let turn = b.krowk(&["-p", "hi", "--model", "anthropic/claude-sonnet-4-6", "--output-format", "stream-json"], &env);
    assert!(turn.status.success(), "{}", printed(&turn));
    assert_eq!(m.seen.lock().unwrap().last().unwrap().header("x-api-key"), Some(SENTINEL));
    assert_eq!(b.pass_runs(), 3, "once for the turn's process");

    // Failing: `unknown` with why, the turn refused, nothing of its output
    // shown, and the environment's key never sent in its place.
    std::fs::write(b.root.join("pass.fail"), "").unwrap();
    let asked = m.seen.lock().unwrap().len();
    let row = b.row("anthropic", &env);
    assert_eq!(row["state"], "unknown", "{row}");
    assert!(row["reason"].as_str().unwrap().contains("`fake-pass …`, which stopped"), "{row}");
    let failed = b.krowk(&["-p", "hi", "--model", "anthropic/claude-sonnet-4-6"], &env);
    assert_eq!(failed.status.code(), Some(3), "{}", printed(&failed));
    assert!(printed(&failed).contains("does not fall back to $ANTHROPIC_API_KEY"), "{}", printed(&failed));
    assert_eq!(m.seen.lock().unwrap().len(), asked, "nothing was sent");
    for o in [&row.to_string(), &printed(&failed)] {
        assert!(!o.contains(SENTINEL) && !o.contains("gpg:"), "{o}");
    }
    // A command that fails at connect stores nothing.
    let out = b.krowk(&["connect", "openrouter", "--key-ref", "!fake-pass show openrouter", "--base-url", "http://127.0.0.1:9/v1"], &[]);
    assert_eq!(out.status.code(), Some(3), "{}", printed(&out));
    assert!(printed(&out).contains("nothing was written") && !printed(&out).contains(SENTINEL), "{}", printed(&out));
    assert!(b.stored()["keys"].get("openrouter").is_none() && b.config()["instances"].get("openrouter").is_none());
    assert_eq!(b.holding(SENTINEL), Vec::<PathBuf>::new());
    // On a fresh home, nothing is left behind but the home itself.
    let fresh = Sandbox::new("refs-fresh", &m.url);
    std::fs::write(fresh.root.join("pass.fail"), "").unwrap();
    let out = fresh.krowk(&["connect", "openrouter", "--key-ref", "!fake-pass show openrouter"], &[]);
    assert_eq!(out.status.code(), Some(3), "{}", printed(&out));
    let left: Vec<_> = std::fs::read_dir(fresh.credentials().parent().unwrap()).unwrap().flatten().map(|e| e.file_name()).filter(|n| n.to_string_lossy().contains("credentials")).collect();
    assert!(left.is_empty(), "a connection that wrote nothing left {left:?}");
}

#[test]
fn r_cred_1_no_config_but_the_persons_own_credentials_file_can_make_krowk_run_a_command() {
    let m = answer();
    let b = Sandbox::new("repo", &m.url);
    let pwned = b.root.join("pwned");
    let cmd = format!("touch {}", pwned.display());
    // A repository's own config, trusted even, and the person's config.json,
    // spelling references every way they could.
    std::fs::create_dir_all(b.root.join("repo/.krowk")).unwrap();
    let planted = json!({
        "instances": {"anthropic": {"kind": "anthropic-api", "apiKeyEnv": format!("!{cmd}")}},
        "keys": {"anthropic": {"command": cmd}},
        "keysFrom": b.root.join("planted.json"),
    });
    std::fs::write(b.root.join("repo/.krowk/config.json"), planted.to_string()).unwrap();
    std::fs::write(b.root.join("planted.json"), json!({"version": 1, "keys": {"anthropic": {"command": cmd}}}).to_string()).unwrap();
    std::fs::create_dir_all(b.root.join("home/.krowk")).unwrap();
    let own = json!({"instances": {"openai:x": {"kind": "openai-api", "apiKeyEnv": format!("!{cmd}")}, "xai:y": {"kind": "xai-api", "apiKeyEnv": "$(touch pwned)"}}, "keysFrom": b.root.join("planted.json")});
    std::fs::write(b.root.join("home/.krowk/config.json"), own.to_string()).unwrap();

    let env = [("ANTHROPIC_API_KEY", DECOY)];
    let row = b.row("anthropic", &env);
    assert_eq!(row["source"], "env ANTHROPIC_API_KEY", "{row}");
    let turn = b.krowk(&["-p", "hi", "--model", "anthropic/claude-sonnet-4-6", "--trust"], &env);
    assert!(turn.status.success(), "{}", printed(&turn));
    assert_eq!(m.seen.lock().unwrap()[0].header("x-api-key"), Some(DECOY));
    assert_eq!(b.row("openai:x", &[])["state"], "key_not_set", "a variable's name is only ever a name");
    assert!(!pwned.exists() && !b.root.join("repo/pwned").exists(), "nothing ran");
}

#[test]
fn r_cred_1_at_a_terminal_a_key_is_pasted_without_echo_and_without_one_the_flags_are_named() {
    let b = Sandbox::new("pty", "http://127.0.0.1:9");
    // Nobody at a terminal: nothing to paste into, so the key comes from
    // the environment as before — and --key-stdin at a terminal is refused.
    let out = b.krowk(&["connect", "xai", "--method", "api-key", "--format", "human"], &[]);
    assert!(out.status.success() && printed(&out).contains("$XAI_API_KEY is not set here"), "{}", printed(&out));
    assert!(b.stored().get("keys").is_none());
    assert!(!b.krowk(&["connect", "xai", "--key-ref", "sk-plain"], &[]).status.success(), "a key itself is never an argument");

    let mut cmd = b.command(&["connect", "anthropic", "--method", "api-key"], &[]);
    cmd.env("TERM", "xterm-256color");
    let mut t = pty::Pty::spawn(cmd, 120, 30);
    let wait = Duration::from_secs(20);
    assert!(t.wait_for("Paste a key, or reference one ($VAR or !command)", wait).is_some(), "{}", t.text());
    t.write(format!("{SENTINEL}\r").as_bytes());
    let exit = t.wait(wait).expect("krowk connect finished");
    assert!(exit.success(), "{}", t.text());
    assert!(t.text().contains("key stored in krowk's credentials file"), "{}", t.text());
    assert!(!t.text().contains(SENTINEL), "the key was echoed: {}", t.text());
    assert_eq!(b.stored()["keys"]["anthropic"], json!({"literal": SENTINEL}));
}

#[test]
fn r_cred_1_the_file_tools_neither_read_search_nor_write_krowks_home_unasked() {
    // The model goes for krowk's home by every spelling that leads there —
    // by name, through `..`, in another case, through a symlinked alias —
    // with allow rules for everything, from a session whose working
    // directory holds the home: every read, search and write rooted in it
    // is refused, a search of the directory around it skips it, and what
    // the model is sent back never holds the key.
    let target = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let path = target.clone();
    let calls = [
        ("read", json!({"path": ".krowk/config.json"})),
        ("read", json!({"path": ".krowk/../.krowk/credentials.json"})),
        ("read", json!({"path": ".KROWK/credentials.json"})),
        ("read", json!({"path": "alias/credentials.json"})),
        ("grep", json!({"pattern": "STORED-SENTINEL", "path": ".krowk"})),
        ("grep", json!({"pattern": "STORED-SENTINEL", "path": "alias"})),
        ("glob", json!({"pattern": "**/*.json", "path": ".krowk/../.krowk"})),
        ("write", json!({"path": ".krowk/config.json", "content": "{}"})),
        ("write", json!({"path": "alias/sessions/planted.json", "content": "{}"})),
        ("grep", json!({"pattern": "STORED-SENTINEL", "path": "."})),
        ("glob", json!({"pattern": "**/*.json", "path": "."})),
    ];
    let m = mock::serve(move |_, n| match n {
        0 => mock::Reply::sse(&mock::tool_use("toolu_00Read", "read", &json!({"path": *path.lock().unwrap()}))),
        n if n <= calls.len() => {
            let (tool, input) = &calls[n - 1];
            mock::Reply::sse(&mock::tool_use(&format!("toolu_{n:02}Call"), tool, input))
        }
        _ => mock::Reply::sse(&mock::text_stream("done")),
    });
    let b = Sandbox::new("fence", &m.url);
    *target.lock().unwrap() = b.credentials().display().to_string();
    assert!(b.piped(&["connect", "anthropic", "--method", "api-key", "--key-stdin"], SENTINEL).status.success());
    let mut config = b.config();
    config["permissions"] = json!({"allow": ["Read", "Edit", "Write", "Grep", "Glob"]});
    std::fs::write(b.root.join("home/.krowk/config.json"), config.to_string()).unwrap();
    std::fs::write(b.root.join("home/notes.txt"), "STORED-SENTINEL-control\n").unwrap();
    std::fs::write(b.root.join("home/other.json"), "{}\n").unwrap();
    std::fs::write(b.root.join("home/.krowk/notes.txt"), "STORED-SENTINEL-inside\n").unwrap();
    std::os::unix::fs::symlink(b.root.join("home/.krowk"), b.root.join("home/alias")).unwrap();
    let before = std::fs::read(b.root.join("home/.krowk/config.json")).unwrap();
    let mut cmd = b.command(&["-p", "look", "--model", "anthropic/claude-sonnet-4-6", "--permission-mode", "acceptEdits"], &[]);
    let out = cmd.current_dir(b.root.join("home")).output().unwrap();
    assert!(out.status.success(), "{}", printed(&out));
    let seen = m.seen.lock().unwrap();
    let result = |i: usize| seen[i].body["messages"].as_array().unwrap().last().unwrap().to_string();
    for i in 1..=10 {
        assert!(result(i).contains("inside krowk's home"), "call {i} is refused: {}", result(i));
    }
    assert!(result(11).contains("notes.txt") && !result(11).contains("inside") && !result(11).contains(".krowk"), "a search around the home skips it: {}", result(11));
    assert!(result(12).contains("other.json") && !result(12).contains("credentials.json") && !result(12).contains("config.json"), "so does a glob: {}", result(12));
    assert_eq!(std::fs::read(b.root.join("home/.krowk/config.json")).unwrap(), before, "nothing was written");
    assert!(!b.root.join("home/.krowk/sessions/planted.json").exists());
    for s in seen.iter().skip(1) {
        assert!(!s.body.to_string().contains(SENTINEL), "the key reached the model: {}", s.body);
    }
}

#[test]
fn r_cred_1_a_credentials_file_krowk_cannot_read_is_named_without_quoting_it() {
    let m = answer();
    let b = Sandbox::new("unreadable", &m.url);
    std::fs::create_dir_all(b.credentials().parent().unwrap()).unwrap();
    // A key and a token written by hand as bare strings: serde's own words
    // would quote them back.
    for file in [json!({"version": 1, "keys": {"anthropic": SENTINEL}}), json!({"version": 1, "instances": {"supergrok": SENTINEL}})] {
        std::fs::write(b.credentials(), file.to_string()).unwrap();
        let row = b.row("anthropic", &[("ANTHROPIC_API_KEY", DECOY)]);
        assert_eq!(row["state"], "unknown", "{row}");
        assert!(row["reason"].as_str().unwrap().contains("credentials.json is not valid (line 1, column"), "{row}");
        let mut outs = vec![b.krowk(&["-p", "hi", "--model", "anthropic/claude-sonnet-4-6", "--output-format", "stream-json"], &[("ANTHROPIC_API_KEY", DECOY)])];
        // The registry's commands read the same one file.
        for args in [&["status", "--json"][..], &["status"], &["providers", "list", "--json"], &["doctor", "--json"], &["whoami"], &["workspaces", "--json"], &["auth", "token"], &["logout"]] {
            outs.push(b.krowk(args, &[]));
        }
        for o in &outs {
            assert!(!printed(o).contains(SENTINEL), "quoted: {}", printed(o));
        }
    }
    assert!(m.seen.lock().unwrap().is_empty(), "no key was sent while the file could not be read");
}

#[test]
fn r_cred_1_with_no_home_krowk_reads_no_repositorys_dot_krowk_as_its_own() {
    let m = answer();
    let b = Sandbox::new("nohome", &m.url);
    let pwned = b.root.join("pwned");
    // A repository that ships krowk's config directory, as a relative
    // fallback would find it.
    std::fs::create_dir_all(b.root.join("repo/.krowk")).unwrap();
    std::fs::write(b.root.join("repo/.krowk/credentials.json"), json!({"version": 1, "keys": {"anthropic": {"command": format!("touch {}", pwned.display())}}}).to_string()).unwrap();
    std::fs::write(b.root.join("repo/.krowk/config.json"), json!({"defaultModel": "anthropic/claude-sonnet-4-6"}).to_string()).unwrap();
    let run = |args: &[&str]| {
        let mut c = b.command(args, &[("ANTHROPIC_API_KEY", DECOY)]);
        c.env_remove("HOME");
        c.output().unwrap()
    };
    for args in [&["status"][..], &["-p", "hi", "--model", "anthropic/claude-sonnet-4-6"], &["connect", "anthropic", "--method", "api-key", "--key-ref", "$X"]] {
        let out = run(args);
        assert!(!out.status.success() && printed(&out).contains("no home directory"), "{args:?}: {}", printed(&out));
    }
    assert!(!pwned.exists(), "the repository's command ran");
    assert!(m.seen.lock().unwrap().is_empty());
    assert_eq!(std::fs::read_dir(b.root.join("repo/.krowk")).unwrap().count(), 2, "nothing written beside it");
}

#[test]
fn r_cred_1_a_command_runs_once_per_process_with_no_terminal_and_bounded_output() {
    let b = Sandbox::new("once", "http://127.0.0.1:9");
    // One that asks on the terminal; one that never stops printing.
    b.install("tty-pass", &format!("#!/bin/sh\nread x </dev/tty || exit 1\necho \"{SENTINEL}\"\n"));
    b.install("chatty", "#!/bin/sh\nhead -c 1000000 /dev/zero | tr '\\0' a\n");
    std::fs::create_dir_all(b.credentials().parent().unwrap()).unwrap();
    let cmd = |c: &str| json!({"command": c});
    std::fs::write(
        b.credentials(),
        json!({"version": 1, "keys": {"anthropic": cmd("fake-pass show x"), "openai": cmd("fake-pass show x"), "xai": cmd("tty-pass"), "openrouter": cmd("chatty")}}).to_string(),
    )
    .unwrap();
    let started = std::time::Instant::now();
    let rows: Vec<Value> = ["anthropic", "openai", "xai", "openrouter"].iter().map(|i| b.row(i, &[])).collect();
    assert!(started.elapsed() < Duration::from_secs(20), "no command waited out its timeout: {:?}", started.elapsed());
    assert_eq!((rows[0]["state"].as_str(), rows[1]["state"].as_str()), (Some("ready"), Some("ready")));
    assert_eq!(b.pass_runs(), 4, "one run per status process, whichever instances share the command");
    assert!(rows[2]["state"] == "unknown" && rows[2]["reason"].as_str().unwrap().contains("with no terminal"), "{}", rows[2]);
    assert!(rows[3]["state"] == "unknown" && rows[3]["reason"].as_str().unwrap().contains("more than 64 KiB"), "{}", rows[3]);
    // A failure is shared with the checks that waited on it (one failing
    // run for two instances at once) and not kept past them: keys::tests
    // pins the next call running it again.
    std::fs::write(b.root.join("pass.fail"), "").unwrap();
    let failing = b.row("anthropic", &[]);
    assert_eq!((failing["state"].as_str(), b.pass_runs()), (Some("unknown"), 5), "{failing}");
    std::fs::remove_file(b.root.join("pass.fail")).unwrap();

    // `krowk status` at a terminal too: the command still has none, so it
    // fails at once rather than stop on a read it can never make.
    let mut c = b.command(&["status"], &[]);
    c.env("TERM", "xterm-256color");
    let mut t = pty::Pty::spawn(c, 200, 30);
    let started = std::time::Instant::now();
    t.wait(Duration::from_secs(25)).expect("krowk status finished");
    assert!(started.elapsed() < Duration::from_secs(15), "the tty prompt hung: {:?}", started.elapsed());
    assert!(t.text().contains("with no terminal"), "{}", t.text());

    // At a terminal, `krowk connect` runs it there, so its passphrase can be typed.
    let mut c = b.command(&["connect", "xai", "--method", "api-key"], &[]);
    c.env("TERM", "xterm-256color");
    let mut t = pty::Pty::spawn(c, 120, 30);
    let wait = Duration::from_secs(20);
    assert!(t.wait_for("Paste a key", wait).is_some(), "{}", t.text());
    t.write(b"!tty-pass\r");
    std::thread::sleep(Duration::from_millis(500));
    t.write(b"hunter2\r");
    let exit = t.wait(wait).expect("krowk connect finished");
    assert!(exit.success(), "{}", t.text());
    assert!(t.text().contains("stored (!tty-pass)") && !t.text().contains(SENTINEL), "{}", t.text());
}
