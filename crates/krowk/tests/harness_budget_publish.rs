//! `krowk -p`, the built binary, held to a budget and publishing evidence:
//! `--max-usd` and `--max-tokens` stop a session before the model call that
//! would take it past them, a subagent's spend counts toward its parent's,
//! and `publish` pushes a file through krowk_push's own path to the
//! stand-in registry, tagged with the session and grouped under its run.

#![cfg(feature = "harness")]

#[path = "../../krowk-harness/tests/common/mock.rs"]
mod mock;

use serde_json::{json, Value};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

struct Sandbox {
    root: PathBuf,
    url: String,
    /// The stand-in registry's API, when the test runs one.
    api: String,
    /// The krowk key sent to it; empty for a keyless run.
    token: String,
}

impl Sandbox {
    fn new(name: &str, url: &str) -> Sandbox {
        let root = std::env::temp_dir().join(format!("krowk-budget-publish-{name}-{}", std::process::id()));
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
            std::fs::copy(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../krowk-harness/tests/fixtures").join(dir).join(bin), &at).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&at, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        let root = root.canonicalize().unwrap();
        Sandbox { root, url: url.into(), api: "http://127.0.0.1:9/v1".into(), token: String::new() }
    }

    fn krowk(&self, args: &[&str]) -> Output {
        let mut c = Command::new(env!("CARGO_BIN_EXE_krowk"));
        c.args(args)
            .env_clear()
            .env("PATH", format!("{}:{}", self.root.join("bin").display(), std::env::var("PATH").unwrap_or_default()))
            .env("HOME", self.root.join("home"))
            .env("KROWK_NO_UPDATE_CHECK", "1")
            .env("ANTHROPIC_API_KEY", "sk-test")
            .env("ANTHROPIC_BASE_URL", &self.url)
            .env("KROWK_API_URL", &self.api)
            .current_dir(self.root.join("repo"))
            .stdin(Stdio::null());
        if !self.token.is_empty() {
            c.env("KROWK_TOKEN", &self.token);
        }
        c.output().unwrap()
    }

    fn sessions(&self) -> PathBuf {
        self.root.join("home/.krowk/sessions")
    }

    fn log(&self, session: &str) -> Vec<Value> {
        std::fs::read_to_string(self.sessions().join(session).join("events.jsonl")).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// A model that reads README.md over and over, every call reading 20,000
/// tokens from cache: about $0.0067 a call at Sonnet's prices, so a cent
/// allows one call and not two.
fn rereading(_: &Value, n: usize) -> mock::Reply {
    let call = mock::tool_use(&format!("toolu_{n:02}"), "read", &json!({"path": "README.md"}));
    mock::Reply::sse(&call.replace("\"cache_read_input_tokens\":0", "\"cache_read_input_tokens\":20000"))
}

#[test]
fn r_budget_1_max_usd_stops_the_session_before_the_call_that_would_cross_it_and_exits_4() {
    let m = mock::serve(rereading);
    let b = Sandbox::new("usd", &m.url);
    let out = b.krowk(&["-p", "read the README until you are told to stop", "--model", "claude-sonnet-4-6", "--max-usd", "0.01", "--output-format", "json"]);
    let (stdout, stderr) = (text(&out.stdout), text(&out.stderr));
    assert_eq!(out.status.code(), Some(4), "exits 4, like `krowk sessions budget`: {stderr}");
    assert_eq!(m.seen.lock().unwrap().len(), 1, "the second call, which would have crossed the cent, was never sent");
    let result: Value = serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("{e}: {stdout}"));
    assert_eq!((result["status"].as_str(), result["error"]["code"].as_str()), (Some("failed"), Some("budget_exceeded")));
    let session = result["sessionId"].as_str().unwrap();
    assert!(stderr.contains("budget_exceeded") && stderr.contains("over --max-usd $0.0100"), "{stderr}");
    assert!(stderr.contains(&format!("krowk -p --resume {session} --max-usd")), "the message carries the fix: {stderr}");
    // What it made before the stop is kept: the call, and the read it asked for.
    let log = b.log(session);
    assert_eq!(log.iter().filter(|e| e["type"] == "response.completed").count(), 1);
    assert!(log.iter().any(|e| e["item"]["kind"] == "toolResult"));

    // The same limit on a resume refuses at once: the session is already
    // at it, so not even the first call of the turn is made.
    let again = b.krowk(&["-p", "go on", "--resume", session, "--max-usd", "0.01"]);
    assert_eq!(again.status.code(), Some(4));
    assert_eq!(m.seen.lock().unwrap().len(), 1);
    // A limit with room goes on while each next call still fits: two more
    // ($0.0200 would be crossed by a fourth).
    let more = b.krowk(&["-p", "go on", "--resume", session, "--max-usd", "0.02", "--output-format", "json"]);
    assert_eq!(more.status.code(), Some(4), "{}", text(&more.stderr));
    assert_eq!(m.seen.lock().unwrap().len(), 3);
}

#[test]
fn r_budget_1_max_tokens_stops_the_session_the_same_way() {
    let m = mock::serve(rereading);
    let b = Sandbox::new("tokens", &m.url);
    // Each call generates 40 tokens: two calls reach 80, which is inside a
    // limit of 80, and a third would generate at least one more.
    let out = b.krowk(&["-p", "read the README", "--model", "claude-sonnet-4-6", "--max-tokens", "80"]);
    let stderr = text(&out.stderr);
    assert_eq!(out.status.code(), Some(4), "{stderr}");
    assert_eq!(m.seen.lock().unwrap().len(), 2, "the third call was never sent");
    assert!(stderr.contains("budget_exceeded") && stderr.contains("80 tokens generated of --max-tokens 80"), "{stderr}");
    assert!(stderr.contains("--max-tokens <more>"), "{stderr}");
    // A limit given blank is a mistake, not an unlimited run.
    let blank = b.krowk(&["-p", "hi", "--max-tokens", ""]);
    assert_eq!(blank.status.code(), Some(1));
    assert!(text(&blank.stderr).contains("--max-tokens was given with no value"));
    let elsewhere = b.krowk(&["push", "README.md", "--max-usd", "1"]);
    assert!(text(&elsewhere.stderr).contains("only a flag of `krowk -p`, the TUI and `krowk sessions budget`"));
}

#[test]
fn r_budget_1_a_child_sessions_spend_counts_toward_its_parents_budget() {
    let m = mock::serve(|_, _| mock::Reply::sse(&mock::fixture("turn2_answer.sse")));
    let b = Sandbox::new("child", &m.url);
    let first: Value = serde_json::from_slice(&b.krowk(&["-p", "hello", "--model", "claude-sonnet-4-6", "--output-format", "json"]).stdout).unwrap();
    let parent = first["sessionId"].as_str().unwrap().to_string();
    // A synthetic subagent under it that has generated 5,000 tokens.
    {
        use krowk_harness::protocol::{LogBody, ModelRef, Usage, WireApi};
        let (mut child, _) = krowk_harness::log::SessionLog::create_child(&b.sessions(), &b.root.join("repo"), "test", Some(&parent), None).unwrap();
        let model = ModelRef { instance: "anthropic".into(), model: "claude-sonnet-4-6".into() };
        child.append(LogBody::TurnStarted { turn_id: "t".into(), model, provider: "anthropic".into(), wire_api: WireApi::AnthropicMessages, permission_mode: Default::default(), effort: None }).unwrap();
        let usage = Usage { input_tokens: 10, output_tokens: 5000, ..Usage::default() };
        child.append(LogBody::ResponseCompleted { turn_id: "t".into(), response_id: None, model: "claude-sonnet-4-6".into(), usage, stop_reason: None, item_ids: vec![] }).unwrap();
    }
    let calls = m.seen.lock().unwrap().len();
    // The parent alone has generated 7: 1,000 would allow it. With its
    // subagent it is at 5,007, past the limit before any call.
    let out = b.krowk(&["-p", "go on", "--resume", &parent, "--max-tokens", "1000"]);
    let stderr = text(&out.stderr);
    assert_eq!(out.status.code(), Some(4), "{stderr}");
    assert!(stderr.contains("5007 tokens generated"), "{stderr}");
    assert_eq!(m.seen.lock().unwrap().len(), calls, "refused before the call");
    // And in dollars: 5,000 Sonnet output tokens are $0.075.
    let out = b.krowk(&["-p", "go on", "--resume", &parent, "--max-usd", "0.05"]);
    assert_eq!(out.status.code(), Some(4), "{}", text(&out.stderr));
    let out = b.krowk(&["-p", "go on", "--resume", &parent, "--max-usd", "0.10", "--output-format", "stream-json"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    // R-BUDGET-2: the cost frame after each call counts the subagent too.
    let cost = text(&out.stdout).lines().map(|l| serde_json::from_str::<Value>(l).unwrap()).find(|l| l["type"] == "cost").expect("a cost frame");
    assert!(cost["costUsd"].as_f64().unwrap() > 0.075 && cost["generatedTokens"].as_i64().unwrap() > 5007, "{cost}");
}

/// A model that publishes what it is asked to, then says it is done.
fn publishing(files: Value) -> impl Fn(&Value, usize) -> mock::Reply + Send + 'static {
    move |body, n| {
        let last = body["messages"].as_array().and_then(|m| m.last().cloned()).unwrap_or_default();
        if last["content"].as_array().is_some_and(|c| c.iter().any(|b| b["type"] == "tool_result")) {
            return mock::Reply::sse(&mock::fixture("turn2_answer.sse"));
        }
        mock::Reply::sse(&mock::tool_use(&format!("toolu_pub{n}"), "publish", &json!({"files": files, "caption": "the fixed page"})))
    }
}

/// The result the model was sent for its publish call.
fn tool_result(m: &mock::Mock) -> (String, bool) {
    let seen = m.seen.lock().unwrap();
    let last = seen.last().unwrap();
    let msgs = last.body["messages"].as_array().unwrap();
    let block = msgs.last().unwrap()["content"].as_array().unwrap().iter().find(|b| b["type"] == "tool_result").unwrap().clone();
    let text = match &block["content"] {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    (text, block["is_error"].as_bool().unwrap_or(false))
}

#[test]
fn r_evid_1_publish_pushes_a_screenshot_tagged_with_the_session_under_its_run() {
    let registry = krowk_devregistry::start(TcpListener::bind("127.0.0.1:0").unwrap(), krowk_devregistry::Config::default()).unwrap();
    let m = mock::serve(publishing(json!(["shot.png"])));
    let mut b = Sandbox::new("publish", &m.url);
    b.api = format!("{}/v1", registry.url());
    b.token = "krowk_sk_test_publish".into();
    // A real PNG header, so it is served as the image it is.
    std::fs::write(b.root.join("repo/shot.png"), b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01\x08\x06\0\0\0\x1f\x15\xc4\x89").unwrap();
    let out = b.krowk(&["-p", "publish the screenshot", "--model", "claude-sonnet-4-6", "--output-format", "json", "--permission-mode", "acceptEdits"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let session = serde_json::from_slice::<Value>(&out.stdout).unwrap()["sessionId"].as_str().unwrap().to_string();
    let (said, is_error) = tool_result(&m);
    assert!(!is_error, "{said}");
    // The card URL, in the paste form a chat surface wants.
    let card = said.lines().find(|l| l.starts_with("http") && l.contains("/a/art_")).unwrap_or_else(|| panic!("no card URL in {said}"));
    let slug = card.rsplit('/').next().unwrap().to_string();
    let run = b.log(&session).into_iter().find(|e| e["type"] == "run.opened").expect("the run is logged")["run"].as_str().unwrap().to_string();
    assert!(said.contains(&format!("Grouped under run {run}")), "{said}");

    // The artifact carries the session, and belongs to the session's run.
    let shown: Value = serde_json::from_slice(&b.krowk(&["uploads", "show", &slug, "--json"]).stdout).unwrap();
    let artifact = &shown["data"]["artifacts"][0];
    assert_eq!(artifact["metadata"]["krowk.session"], session.as_str(), "{shown}");
    assert_eq!(artifact["metadata"]["krowk.caption"], "the fixed page");
    assert_eq!(artifact["metadata"]["krowk.client"], "krowk/dev", "published by krowk itself, not its MCP server");
    assert_eq!(artifact["run"]["slug"], run.as_str(), "{shown}");
    assert_eq!(artifact["run"]["metadata"]["krowk.session"], session.as_str(), "the run records the session too");
    assert_eq!(artifact["run"]["metadata"]["krowk.harness"], "krowk");

    // A resumed session publishes under the same run, and logs no second one.
    let out = b.krowk(&["-p", "publish it again", "--resume", &session, "--permission-mode", "acceptEdits"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let (said, _) = tool_result(&m);
    assert!(said.contains(&format!("Grouped under run {run}")), "{said}");
    assert_eq!(b.log(&session).iter().filter(|e| e["type"] == "run.opened").count(), 1);
    let listed: Value = serde_json::from_slice(&b.krowk(&["uploads", "list", "--run", &run, "--json"]).stdout).unwrap();
    assert_eq!(listed["data"]["artifacts"].as_array().map(Vec::len), Some(2), "{listed}");
}

#[test]
fn r_evid_1_without_a_key_publish_is_anonymous_opens_no_run_and_its_claim_token_reaches_only_the_person() {
    let registry = krowk_devregistry::start(TcpListener::bind("127.0.0.1:0").unwrap(), krowk_devregistry::Config::default()).unwrap();
    let m = mock::serve(publishing(json!(["notes.txt"])));
    let mut b = Sandbox::new("keyless", &m.url);
    b.api = format!("{}/v1", registry.url());
    std::fs::write(b.root.join("repo/notes.txt"), "what the agent saw\n").unwrap();
    let out = b.krowk(&["-p", "publish the notes", "--model", "claude-sonnet-4-6", "--output-format", "stream-json", "--permission-mode", "acceptEdits"]);
    let (stdout, stderr) = (text(&out.stdout), text(&out.stderr));
    assert!(out.status.success(), "{stderr}");
    let session = stdout.lines().next().and_then(|l| serde_json::from_str::<Value>(l).ok()).unwrap()["sessionId"].as_str().unwrap().to_string();
    let (said, is_error) = tool_result(&m);
    assert!(!is_error && said.contains("/a/art_"), "{said}");
    assert!(said.contains("belongs to no run") && said.contains("krowk doctor"), "{said}");
    assert!(said.contains("shown to the person rather than to you"), "{said}");
    assert!(!b.log(&session).iter().any(|e| e["type"] == "run.opened"));
    // The claim token is a secret: the person gets it on stderr, and it is
    // nowhere a model, a log or a program reading stdout would see it.
    let token = stderr.split_whitespace().find(|w| w.starts_with("krowk_claim_")).unwrap_or_else(|| panic!("no claim command on stderr: {stderr}")).trim_end_matches(['`', ')']).to_string();
    assert!(stderr.contains("krowk claim art_"), "{stderr}");
    assert!(!said.contains("krowk_claim_"), "the model was sent the claim token: {said}");
    let dir = b.sessions().join(&session);
    for f in ["events.jsonl", "context.jsonl"] {
        assert!(!std::fs::read_to_string(dir.join(f)).unwrap().contains(&token), "{f} holds the claim token");
    }
    assert!(!stdout.contains(&token), "stream-json printed the claim token");
    assert!(!m.seen.lock().unwrap().iter().any(|r| r.raw.contains(&token)), "the provider was sent the claim token");
}

/// publish uploads to a public link, so the permission evaluator holds it
/// to what an edit is: asked about in the default mode (refused, with
/// nobody to ask) and refused in plan.
#[test]
fn r_evid_1_publish_needs_accept_edits_as_an_edit_does() {
    let registry = krowk_devregistry::start(TcpListener::bind("127.0.0.1:0").unwrap(), krowk_devregistry::Config::default()).unwrap();
    let m = mock::serve(publishing(json!(["notes.txt"])));
    let mut b = Sandbox::new("mode", &m.url);
    b.api = format!("{}/v1", registry.url());
    b.token = "krowk_sk_test_mode".into();
    std::fs::write(b.root.join("repo/notes.txt"), "x\n").unwrap();
    for mode in ["default", "plan"] {
        let out = b.krowk(&["-p", "publish the notes", "--model", "claude-sonnet-4-6", "--output-format", "json", "--permission-mode", mode]);
        assert!(out.status.success(), "{}", text(&out.stderr));
        let (said, is_error) = tool_result(&m);
        let why = if mode == "plan" { "plan mode" } else { "--permission-mode acceptEdits" };
        assert!(is_error && said.contains(why), "{mode}: {said}");
        let session = serde_json::from_slice::<Value>(&out.stdout).unwrap()["sessionId"].as_str().unwrap().to_string();
        assert!(!b.log(&session).iter().any(|e| e["type"] == "run.opened"), "{mode}: nothing was published");
    }
}

/// A repository can name a command in its git config (`core.fsmonitor`);
/// the run metadata publish detects runs none of it, in any mode.
#[test]
fn r_evid_1_publish_detects_the_run_context_without_running_the_repositorys_git_config() {
    let registry = krowk_devregistry::start(TcpListener::bind("127.0.0.1:0").unwrap(), krowk_devregistry::Config::default()).unwrap();
    let m = mock::serve(publishing(json!(["notes.txt"])));
    let mut b = Sandbox::new("fsmonitor", &m.url);
    b.api = format!("{}/v1", registry.url());
    b.token = "krowk_sk_test_fsmonitor".into();
    let repo = b.root.join("repo");
    std::fs::remove_dir_all(repo.join(".git")).unwrap();
    if !Command::new("git").args(["init", "-q"]).current_dir(&repo).status().is_ok_and(|s| s.success()) {
        eprintln!("git is not installed: skipping");
        return;
    }
    let marker = b.root.join("fsmonitor-ran");
    let config = std::fs::read_to_string(repo.join(".git/config")).unwrap();
    std::fs::write(repo.join(".git/config"), format!("{config}[core]\n\tfsmonitor = \"touch '{}'; false\"\n", marker.display())).unwrap();
    std::fs::write(repo.join("notes.txt"), "x\n").unwrap();
    let out = b.krowk(&["-p", "publish the notes", "--model", "claude-sonnet-4-6", "--permission-mode", "acceptEdits"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(!tool_result(&m).1, "published");
    assert!(!marker.exists(), "the repository's fsmonitor ran");
}

/// The golden case `mcp-credential-refusals` holds krowk_push to these
/// refusals; `publish` is the same code, and is held to the same files.
#[test]
fn r_evid_1_publish_refuses_credentials_paths_outside_the_root_and_hard_links_as_krowk_push_does() {
    let registry = krowk_devregistry::start(TcpListener::bind("127.0.0.1:0").unwrap(), krowk_devregistry::Config::default()).unwrap();
    let mut b = Sandbox::new("refusals", "http://127.0.0.1:9");
    b.api = format!("{}/v1", registry.url());
    b.token = "krowk_sk_test_refusals".into();
    let repo = b.root.join("repo");
    std::fs::write(repo.join(".env"), "SECRET=1").unwrap();
    std::fs::write(repo.join("id_rsa"), "key").unwrap();
    std::fs::create_dir_all(repo.join(".aws")).unwrap();
    std::fs::write(repo.join(".aws/credentials"), "x").unwrap();
    std::fs::write(b.root.join("outside.txt"), "not yours").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("/etc/hosts", repo.join("link.txt")).unwrap();
    std::fs::write(repo.join("real.txt"), "x").unwrap();
    std::fs::hard_link(repo.join("real.txt"), repo.join("hard.txt")).unwrap();
    let cases = [
        (".env", "secret_path"),
        ("id_rsa", "secret_path"),
        (".aws/credentials", "secret_path"),
        ("../outside.txt", "outside_root"),
        ("link.txt", "outside_root"),
        ("hard.txt", "hard_linked"),
    ];
    for (file, code) in cases {
        let m = mock::serve(publishing(json!([file])));
        b.url = m.url.clone();
        let out = b.krowk(&["-p", "publish it", "--model", "claude-sonnet-4-6", "--output-format", "json", "--permission-mode", "acceptEdits"]);
        assert!(out.status.success(), "a refusal is a tool result, not a failed turn: {}", text(&out.stderr));
        let (said, is_error) = tool_result(&m);
        // A path that leads out is refused before krowk_push sees it — the
        // permission evaluator's own reach — and by krowk_push behind it.
        let outside = code == "outside_root" && said.contains("outside the working directory");
        assert!(is_error && (said.contains(&format!("krowk failed: {code}")) || outside), "{file}: {said}");
        let session = serde_json::from_slice::<Value>(&out.stdout).unwrap()["sessionId"].as_str().unwrap().to_string();
        assert!(!b.log(&session).iter().any(|e| e["type"] == "run.opened"), "{file}: a refused publish opens no run");
    }
}
