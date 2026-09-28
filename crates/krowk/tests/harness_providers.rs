//! `krowk providers` and `krowk -p` on the new providers, the built binary
//! against stand-ins: a named OpenAI profile with a base URL running a GPT
//! that patches a file, its effort mapped through the models.dev cache and
//! its second turn reading the cache; a SuperGrok device login, the
//! credentials file it leaves (0600), and a Grok task run on it.

#![cfg(feature = "harness")]

#[path = "../../krowk-harness/tests/common/mock.rs"]
mod mock;
#[path = "../../krowk-harness/tests/common/providers.rs"]
mod providers;

use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Sandbox {
        let root = std::env::temp_dir().join(format!("krowk-harness-providers-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("home")).unwrap();
        std::fs::create_dir_all(root.join("repo/.git")).unwrap();
        std::fs::write(root.join("repo/README.md"), "# krowk\n\nPermalinks for agent output.\n").unwrap();
        // The fake `claude` and `codex`, signed in to nothing, first on
        // PATH: routing asks every vendor there is, and never the real ones
        // the machine may have.
        std::fs::create_dir_all(root.join("bin")).unwrap();
        for (dir, bin) in [("claude", "fake-claude"), ("codex", "fake-codex")] {
            let at = root.join("bin").join(dir);
            // Linked, not copied: a copy is a file open for writing that a test
            // forking beside it can inherit, and running it then fails with
            // "Text file busy" (ETXTBSY) — read as a vendor that could not be checked.
            std::os::unix::fs::symlink(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../krowk-harness/tests/fixtures").join(dir).join(bin), &at).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&at, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        Sandbox { root: root.canonicalize().unwrap() }
    }

    fn krowk(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut c = Command::new(env!("CARGO_BIN_EXE_krowk"));
        c.args(args)
            .env_clear()
            .env("PATH", format!("{}:{}", self.root.join("bin").display(), std::env::var("PATH").unwrap_or_default()))
            .env("HOME", self.root.join("home"))
            .env("KROWK_NO_UPDATE_CHECK", "1")
            .current_dir(self.root.join("repo"))
            .stdin(Stdio::null());
        for (k, v) in env {
            c.env(k, v);
        }
        c.output().unwrap()
    }

    fn ok(&self, args: &[&str], env: &[(&str, &str)]) -> Value {
        let out = self.krowk(args, env);
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(out.status.success(), "krowk {args:?}: {stdout}{}", String::from_utf8_lossy(&out.stderr));
        serde_json::from_str(stdout.trim()).unwrap_or_else(|e| panic!("krowk {args:?}: {e}: {stdout}"))
    }

    fn config(&self) -> Value {
        serde_json::from_str(&std::fs::read_to_string(self.root.join("home/.krowk/config.json")).unwrap()).unwrap()
    }

    fn readme(&self) -> String {
        std::fs::read_to_string(self.root.join("repo/README.md")).unwrap()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn r_prov_4_a_named_openai_profile_runs_a_gpt_that_patches_a_file_and_reads_the_cache() {
    let m = mock::serve(providers::responses_script);
    let b = Sandbox::new("openai");
    // The catalog: gpt-5.4 reasons, up to xhigh.
    let cache = b.root.join("home/.krowk/cache");
    std::fs::create_dir_all(&cache).unwrap();
    let catalog = json!({"openai": {"npm": "@ai-sdk/openai", "models": {"gpt-5.4": {"family": "gpt", "reasoning": true, "tool_call": true,
        "reasoning_options": [{"type": "effort", "values": ["none", "low", "medium", "high", "xhigh"]}], "limit": {"context": 1050000, "output": 128000},
        "cost": {"input": 2.5, "output": 15, "cache_read": 0.25}}}}});
    std::fs::write(cache.join("models.json"), catalog.to_string()).unwrap();

    let base = format!("{}/v1", m.url);
    let added = b.ok(&["providers", "add", "openai", "--name", "work", "--base-url", &base, "--json"], &[]);
    assert_eq!(added["data"]["instance"], "openai:work", "{added}");
    assert_eq!(added["data"]["api_key_env"], "OPENAI_WORK_API_KEY", "a named profile reads its own variable");
    assert_eq!(b.config()["instances"]["openai:work"], json!({"kind": "openai-api", "apiKeyEnv": "OPENAI_WORK_API_KEY", "baseUrl": base}), "a name, never a key");
    let listed = b.ok(&["providers", "list", "--json"], &[]);
    let work = listed["data"]["instances"].as_array().unwrap().iter().find(|i| i["instance"] == "openai:work").unwrap().clone();
    assert_eq!((work["ready"].as_bool(), work["wire_api"].as_str()), (Some(false), Some("openai-responses")), "no key in this environment yet");

    let key = [("OPENAI_WORK_API_KEY", "sk-work-test")];
    let r = b.ok(&["-p", "reword the README tagline", "--model", "openai:work/gpt-5.4", "--permission-mode", "acceptEdits", "--effort", "max", "--output-format", "json"], &key);
    assert_eq!((r["status"].as_str(), r["result"].as_str()), (Some("completed"), Some("The README now says: Permalinks for everything agents make.")), "{r}");
    assert_eq!(b.readme(), "# krowk\n\nPermalinks for everything agents make.\n");
    assert!(r["costUsd"].as_f64().unwrap() > 0.0, "priced from the same cache: {r}");
    let session = r["sessionId"].as_str().unwrap().to_string();
    let r2 = b.ok(&["-p", "what language is it written in?", "--resume", &session, "--output-format", "json"], &key);
    assert_eq!(r2["model"], json!({"instance": "openai:work", "model": "gpt-5.4"}), "a resume keeps the instance");
    assert!(r2["usage"]["cacheReadTokens"].as_i64().unwrap() > 0, "the second turn reports cached tokens: {r2}");
    let seen = m.seen.lock().unwrap();
    assert_eq!(seen[0].body["reasoning"]["effort"], "xhigh", "max, mapped onto gpt-5.4's top rung by the catalog");
    assert!(seen.iter().all(|s| s.header("authorization") == Some("Bearer sk-work-test") && s.body["prompt_cache_key"] == session.as_str()));
    drop(seen);
    // The session lists beside the others, on its provider.
    let listed = b.ok(&["sessions", "--json"], &[]);
    let s = listed["data"]["sessions"].as_array().unwrap().iter().find(|s| s["foreign_session_id"] == session.as_str()).cloned().unwrap();
    assert_eq!(s["harness"], "krowk");
    // Flags that belong to another command are refused, not ignored.
    let out = b.krowk(&["sessions", "--effort", "high"], &[]);
    assert!(!out.status.success() && String::from_utf8_lossy(&out.stderr).contains("only a flag of `krowk -p`"));
    let out = b.krowk(&["-p", "x", "--effort", "extreme"], &key);
    assert!(!out.status.success() && String::from_utf8_lossy(&out.stderr).contains("none, minimal, low, medium, high, xhigh, max"));
    // Removed, the instance is gone from the config.
    b.ok(&["providers", "remove", "openai:work", "--json"], &[]);
    assert!(b.config()["instances"].get("openai:work").is_none());
}

#[test]
fn a_renamed_instance_keeps_its_key_its_default_and_its_sessions() {
    let m = mock::serve(providers::responses_script);
    let b = Sandbox::new("rename");
    let base = format!("{}/v1", m.url);
    b.ok(&["connect", "openai", "--method", "api-key", "--name", "work", "--base-url", &base, "--key-ref", "$WORK_KEY", "--json"], &[]);
    // Everywhere config.json can name it, and a name no instance has.
    let path = b.root.join("home/.krowk/config.json");
    let mut cfg = b.config();
    cfg["defaultModel"] = json!("openai:work/gpt-5.4");
    cfg["rolloverOrder"] = json!(["openai:work", "openai/gpt-5.4"]);
    cfg["subagents"] = json!({"model": "openai:work/gpt-5.4-mini"});
    std::fs::write(&path, cfg.to_string()).unwrap();
    let key = [("WORK_KEY", "sk-work-test")];
    let r = b.ok(&["-p", "reword the README tagline", "--permission-mode", "acceptEdits", "--output-format", "json"], &key);
    assert_eq!(r["model"]["instance"], "openai:work", "{r}");
    let session = r["sessionId"].as_str().unwrap().to_string();

    let done = b.ok(&["providers", "rename", "openai:work", "openai:job", "--json"], &[]);
    assert_eq!((done["data"]["from"].as_str(), done["data"]["to"].as_str()), (Some("openai:work"), Some("openai:job")), "{done}");
    assert_eq!(done["data"]["moved_login"], true, "the stored key went with it");
    assert_eq!(done["data"]["api_key_env"], "OPENAI_WORK_API_KEY", "a variable made from the old name is said, not renamed");
    let cfg = b.config();
    assert!(cfg["instances"].get("openai:work").is_none());
    assert_eq!(cfg["instances"]["openai:job"]["baseUrl"], base.as_str());
    assert_eq!(cfg["defaultModel"], "openai:job/gpt-5.4");
    assert_eq!(cfg["rolloverOrder"], json!(["openai:job", "openai/gpt-5.4"]));
    assert_eq!(cfg["subagents"]["model"], "openai:job/gpt-5.4-mini");
    assert_eq!(cfg["renamed"], json!({"openai:work": "openai:job"}));
    let creds: Value = serde_json::from_str(&std::fs::read_to_string(b.root.join("home/.krowk/credentials.json")).unwrap()).unwrap();
    assert_eq!(creds["keys"].as_object().unwrap().keys().collect::<Vec<_>>(), ["openai:job"], "{creds}");

    // The session that ran on the old name resumes on the new one.
    let r2 = b.ok(&["-p", "what language is it written in?", "--resume", &session, "--output-format", "json"], &key);
    assert_eq!(r2["model"], json!({"instance": "openai:job", "model": "gpt-5.4"}), "{r2}");
    assert!(m.seen.lock().unwrap().iter().all(|s| s.header("authorization") == Some("Bearer sk-work-test")));
    // What cannot be: a built-in, a name taken, a name that is gone.
    for (args, code) in [(["claude", "mine"], "built_in"), (["openai:job", "claude:x"], "bad_flag"), (["openai:job", "job"], "e.g. openai:job"), (["openai:work", "x"], "no_instance")] {
        let out = b.krowk(&["providers", "rename", args[0], args[1], "--json"], &[]);
        let err = String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success() && err.contains(code), "rename {args:?}: {err}");
    }
    let out = b.krowk(&["providers", "add", "openai", "--name", "other", "--json"], &[]);
    assert!(out.status.success());
    let out = b.krowk(&["providers", "rename", "openai:job", "openai:other", "--json"], &[]);
    assert!(!out.status.success() && String::from_utf8_lossy(&out.stderr).contains("instance_exists"), "{}", String::from_utf8_lossy(&out.stderr));
    // A built-in is refused before its new name would be asked.
    let out = b.krowk(&["providers", "rename", "claude", "--json"], &[]);
    assert!(!out.status.success() && String::from_utf8_lossy(&out.stderr).contains("built_in"), "{}", String::from_utf8_lossy(&out.stderr));
    // An old name still leads to its instance: nothing else may take it,
    // but the instance may take it back.
    for args in [vec!["connect", "openai", "--method", "api-key", "--name", "work", "--api-key-env", "X", "--json"], vec!["providers", "rename", "openai:other", "openai:work", "--json"]] {
        let out = b.krowk(&args, &[]);
        assert!(!out.status.success() && String::from_utf8_lossy(&out.stderr).contains("sessions that ran on it resume there"), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }
    b.ok(&["providers", "rename", "openai:job", "openai:work", "--json"], &[]);
    assert_eq!(b.config()["renamed"], json!({"openai:job": "openai:work"}));
    // Removed, its old names lead nowhere, and are anyone's to take.
    b.ok(&["providers", "remove", "openai:work", "--json"], &[]);
    assert_eq!(b.config()["renamed"], json!({}));
    b.ok(&["providers", "rename", "openai:other", "openai:job", "--json"], &[]);
}

#[test]
fn r_prov_4_a_supergrok_device_login_writes_0600_credentials_and_runs_a_grok_task() {
    let auth = providers::auth_server(3600);
    let chat = providers::chat_behind(auth.state.clone());
    let b = Sandbox::new("supergrok");
    // Where this xAI stand-in lives: the definition keeps it across logins.
    let dir = b.root.join("home/.krowk");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("config.json"), json!({"instances": {"supergrok": {"kind": "xai-oauth", "issuer": auth.mock.url, "baseUrl": chat.url}}}).to_string()).unwrap();

    let not_yet = b.krowk(&["-p", "hi", "--model", "supergrok/grok-4.7"], &[]);
    assert!(!not_yet.status.success());
    assert!(String::from_utf8_lossy(&not_yet.stderr).contains("krowk connect xai --method subscription"), "{}", String::from_utf8_lossy(&not_yet.stderr));

    let out = b.krowk(&["providers", "add", "supergrok", "--device", "--json"], &[]);
    let (stdout, stderr) = (String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned());
    assert!(out.status.success(), "{stdout}{stderr}");
    assert!(stderr.contains("KRWK-2026") && stderr.contains("/device?user_code=KRWK-2026"), "the code and where to enter it: {stderr}");
    assert!(!stdout.contains("xai-at-") && !stderr.contains("xai-at-") && !stdout.contains("xai-rt-") && !stderr.contains("xai-rt-"), "no token is printed");
    let added: Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(added["data"]["signed_in"], true);
    assert_eq!(b.config()["instances"]["supergrok"]["issuer"], auth.mock.url.as_str(), "the definition kept its issuer");
    let creds = dir.join("credentials.json");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&creds).unwrap().permissions().mode() & 0o777, 0o600, "the credentials file is created 0600");
    }
    let stored: Value = serde_json::from_str(&std::fs::read_to_string(&creds).unwrap()).unwrap();
    assert_eq!(stored["instances"]["supergrok"]["accessToken"], "xai-at-1");
    assert!(!std::fs::read_to_string(dir.join("config.json")).unwrap().contains("xai-at-"), "config.json holds no token");

    let listed = b.ok(&["providers", "list", "--json"], &[]);
    let sg = listed["data"]["instances"].as_array().unwrap().iter().find(|i| i["instance"] == "supergrok").unwrap().clone();
    assert_eq!((sg["ready"].as_bool(), sg["kind"].as_str()), (Some(true), Some("xai-oauth")));

    let r = b.ok(&["-p", "reword the README tagline", "--model", "supergrok/grok-4.7", "--permission-mode", "acceptEdits", "--output-format", "json"], &[]);
    assert_eq!(r["status"], "completed", "{r}");
    assert_eq!(b.readme(), "# krowk\n\nPermalinks for everything agents make.\n");
    assert!(chat.seen.lock().unwrap().iter().all(|s| s.header("authorization") == Some("Bearer xai-at-1")));

    let removed = b.ok(&["providers", "remove", "supergrok", "--json"], &[]);
    assert_eq!((removed["data"]["removed_definition"].as_bool(), removed["data"]["removed_login"].as_bool()), (Some(true), Some(true)));
    let stored: Value = serde_json::from_str(&std::fs::read_to_string(&creds).unwrap()).unwrap();
    assert!(stored["instances"].get("supergrok").is_none(), "the login is forgotten");
}
