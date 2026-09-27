//! Whole headless tasks with tool calls on the new wire APIs, against
//! stand-ins: a GPT editing README.md with a freeform `apply_patch` over
//! the Responses API, then a second turn that reads the cache; a Grok
//! editing it with `search_replace` over Chat Completions on an xAI key;
//! and the same Grok signed in with SuperGrok, its token refreshed as it
//! expires and when the server refuses it.

#[path = "common/mock.rs"]
mod mock;
#[path = "common/providers.rs"]
mod providers;

use krowk_harness::headless::{self, OutputFormat};
use krowk_harness::host::HostConfig;
use krowk_harness::instances::{InstanceKind, InstancesConfig, Registry};
use krowk_harness::log;
use krowk_harness::oauth::{self, Store};
use krowk_harness::protocol::{ContextRecord, Effort, LogBody, PermissionMode, RunResult, StreamLine, TurnStatus, WireApi};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

struct Home {
    root: PathBuf,
    env: BTreeMap<String, String>,
    cfg: InstancesConfig,
}

impl Home {
    fn new(name: &str) -> Home {
        let root = std::env::temp_dir().join(format!("krowk-providers-e2e-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("home")).unwrap();
        std::fs::create_dir_all(root.join("repo/.git")).unwrap();
        std::fs::write(root.join("repo/README.md"), "# krowk\n\nPermalinks for agent output.\n").unwrap();
        let env = BTreeMap::from([("HOME".to_string(), root.join("home").display().to_string())]);
        Home { root, env, cfg: InstancesConfig::default() }
    }

    fn env(&self) -> impl Fn(&str) -> String + '_ {
        move |k| self.env.get(k).cloned().unwrap_or_default()
    }

    fn credentials(&self) -> PathBuf {
        self.root.join("home/.config/krowk").join(oauth::CREDENTIALS_FILE)
    }

    fn readme(&self) -> String {
        std::fs::read_to_string(self.root.join("repo/README.md")).unwrap()
    }

    fn run(&self, prompt: &str, resume: Option<&str>, model: &str, effort: Option<Effort>) -> (Vec<StreamLine>, RunResult) {
        let reg = Registry::resolve(&self.cfg, &self.env());
        let cfg = HostConfig {
            sessions_dir: log::sessions_dir(&self.env()).unwrap(),
            cwd: self.root.join("repo"),
            registry: reg.clone(),
            krowk_version: "test".into(),
            pricer: Arc::new(|_, _, _| None),
            catalog: Arc::new(|_, _| None),
            credentials: self.credentials(),
            trust: krowk_harness::trust::allow_all(),
            publisher: None,
            permissions: Default::default(),
            agents: krowk_harness::subagent::AgentsConfig::none(),
        };
        let opts = headless::Options {
            prompt: prompt.into(),
            resume: resume.map(String::from),
            model: Some(reg.parse_model(model).unwrap()),
            permission_mode: PermissionMode::AcceptEdits,
            toolset: None,
            effort,
            budget: None,
            format: OutputFormat::StreamJson,
        };
        let mut out = Vec::new();
        let outcome = headless::run(cfg, opts, &mut out);
        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        let lines = String::from_utf8(out).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        (lines, outcome.result.unwrap())
    }

    fn context(&self, session: &str) -> ContextRecord {
        let dir = log::sessions_dir(&self.env()).unwrap().join(session);
        serde_json::from_str(std::fs::read_to_string(dir.join(log::CONTEXT_FILE)).unwrap().lines().next().unwrap()).unwrap()
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn wire_of(lines: &[StreamLine]) -> WireApi {
    lines
        .iter()
        .find_map(|l| match l {
            StreamLine::Log(ev) => match &ev.body {
                LogBody::TurnStarted { wire_api, .. } => Some(*wire_api),
                _ => None,
            },
            _ => None,
        })
        .unwrap()
}

#[test]
fn r_prov_1_a_gpt_task_patches_a_file_and_its_second_turn_reads_the_cache() {
    let m = mock::serve(providers::responses_script);
    let mut home = Home::new("gpt");
    home.env.insert("OPENAI_API_KEY".into(), "sk-openai-test".into());
    home.env.insert("OPENAI_BASE_URL".into(), format!("{}/v1", m.url));

    let (lines, r) = home.run("reword the README tagline", None, "openai/gpt-5.4", Some(Effort::Max));
    assert_eq!(r.status, TurnStatus::Completed, "{:?}", r.error);
    assert_eq!(r.result, "The README now says: Permalinks for everything agents make.");
    assert_eq!(home.readme(), "# krowk\n\nPermalinks for everything agents make.\n", "the freeform apply_patch landed");
    assert_eq!(r.num_model_calls, 2);
    assert_eq!(wire_of(&lines), WireApi::OpenaiResponses);
    // The tools the turn recorded are the ones sent: apply_patch freeform.
    let ctx = home.context(&r.session_id);
    assert_eq!((ctx.toolset.as_str(), ctx.wire_api, ctx.provider.as_str()), ("gpt", WireApi::OpenaiResponses, "openai"));
    assert!(ctx.tools.iter().any(|t| t.name == "apply_patch" && t.grammar.is_some()));

    // The second turn: resumed, it reads the prefix the first one cached.
    let (_, r2) = home.run("what language is it written in?", Some(&r.session_id), "openai/gpt-5.4", None);
    assert_eq!((r2.status, r2.result.as_str()), (TurnStatus::Completed, "It is written in Rust."));
    assert!(r2.usage.cache_read_tokens > 0, "the second turn reports cached tokens: {:?}", r2.usage);

    let seen = m.seen.lock().unwrap();
    assert_eq!(seen.len(), 3);
    for s in seen.iter() {
        assert_eq!(s.path, "/v1/responses");
        assert_eq!(s.header("authorization"), Some("Bearer sk-openai-test"));
        assert_eq!(s.body["store"], false);
        assert_eq!(s.body["prompt_cache_key"], r.session_id.as_str(), "one cache key for the whole session");
        assert_eq!(s.body["instructions"], seen[0].body["instructions"]);
        assert_eq!(s.body["tools"], seen[0].body["tools"]);
    }
    // With no catalog entry, the gpt family's safe rungs: max is high.
    assert_eq!(seen[0].body["reasoning"]["effort"], "high", "max is as hard as the family surely goes");
    assert!(seen[2].body.get("reasoning").is_some_and(|r| r.get("effort").is_none()), "no effort asked, none sent");
    // Every call's input begins with the one before it, and the encrypted
    // reasoning is in it as it came.
    let input = |n: usize| seen[n].body["input"].as_array().unwrap().clone();
    assert_eq!(input(0)[..], input(1)[..input(0).len()]);
    assert_eq!(input(1)[..], input(2)[..input(1).len()]);
    assert_eq!(input(1)[1]["encrypted_content"], providers::ENCRYPTED);
    assert_eq!(input(1)[4]["type"], "custom_tool_call_output");
}

#[test]
fn r_prov_4_a_grok_task_edits_a_file_on_an_xai_key() {
    let m = mock::serve(providers::chat_script);
    let mut home = Home::new("xai");
    home.env.insert("XAI_API_KEY".into(), "xai-key-test".into());
    home.cfg.instances.insert("xai".into(), InstanceKind::XaiApi { api_key_env: None, base_url: Some(m.url.clone()), effort: None });

    let (lines, r) = home.run("reword the README tagline", None, "grok-4.7", None);
    assert_eq!(r.status, TurnStatus::Completed, "{:?}", r.error);
    assert_eq!(r.model.instance, "xai", "a bare grok id runs on the xai instance");
    assert_eq!(home.readme(), "# krowk\n\nPermalinks for everything agents make.\n", "search_replace landed");
    assert_eq!(wire_of(&lines), WireApi::ChatCompletions);
    assert_eq!(home.context(&r.session_id).toolset, "grok");
    assert_eq!(r.usage.reasoning_tokens, 54);
    let seen = m.seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    for s in seen.iter() {
        assert_eq!(s.path, "/chat/completions");
        assert_eq!(s.header("authorization"), Some("Bearer xai-key-test"));
        assert_eq!(s.header("x-grok-conv-id"), Some(r.session_id.as_str()), "xAI's cache routing key is the session");
    }
    // The second call carries xAI's own reasoning back on the call's message.
    let msgs = seen[1].body["messages"].as_array().unwrap();
    assert_eq!(msgs[2]["reasoning_content"], "The tagline is in README.md. One search_replace changes it.");
    assert_eq!(msgs[3]["role"], "tool");
}

#[test]
fn r_prov_4_a_grok_task_runs_signed_in_with_supergrok_and_its_token_is_refreshed() {
    let auth = providers::auth_server(0);
    let chat = providers::chat_behind(auth.state.clone());
    let mut home = Home::new("supergrok");
    home.cfg.instances.insert(
        "supergrok".into(),
        InstanceKind::XaiOauth { base_url: Some(chat.url.clone()), issuer: Some(auth.mock.url.clone()), client_id: None, scope: None, effort: None },
    );
    // Not signed in: refused before a session is made, with the fix.
    let reg = Registry::resolve(&home.cfg, &home.env());
    let cfg = HostConfig {
        sessions_dir: log::sessions_dir(&home.env()).unwrap(),
        cwd: home.root.join("repo"),
        registry: reg.clone(),
        krowk_version: "test".into(),
        pricer: Arc::new(|_, _, _| None),
        catalog: Arc::new(|_, _| None),
        credentials: home.credentials(),
        trust: krowk_harness::trust::allow_all(),
        publisher: None,
        permissions: Default::default(),
        agents: krowk_harness::subagent::AgentsConfig::none(),
    };
    let opts = headless::Options { prompt: "hi".into(), resume: None, model: Some(reg.parse_model("supergrok/grok-4.7").unwrap()), permission_mode: PermissionMode::Default, toolset: None, effort: None, budget: None, format: OutputFormat::Json };
    let outcome = headless::run(cfg, opts, &mut Vec::new());
    let e = outcome.error.unwrap();
    assert_eq!(e.code, "not_authenticated");
    assert!(e.message.contains("sign in with `krowk providers add supergrok`"), "{}", e.message);

    // Signed in, with a token already expired: refreshed before the first call.
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let http = krowk_harness::http::client().unwrap();
    let login = oauth::Login { issuer: auth.mock.url.clone(), client_id: None, scope: "openid offline_access".into() };
    let stored = rt.block_on(oauth::login_device(&http, &login, &mut |_| {})).unwrap();
    Store::new(home.credentials()).save("supergrok", &stored).unwrap();
    let (lines, r) = home.run("reword the README tagline", None, "supergrok/grok-4.7", None);
    assert_eq!(r.status, TurnStatus::Completed, "{:?}", r.error);
    assert_eq!(wire_of(&lines), WireApi::ChatCompletions);
    assert_eq!(home.readme(), "# krowk\n\nPermalinks for everything agents make.\n");
    assert!(auth.state.lock().unwrap().refreshes >= 1);
    let saved = Store::new(home.credentials()).load("supergrok").unwrap().unwrap();
    assert_eq!(saved.access_token, auth.state.lock().unwrap().access, "the refreshed token is the one saved");

    // A token the server stops taking mid-life is refreshed on its 401.
    auth.state.lock().unwrap().expires_in = 3600;
    let (_, _) = home.run("warm up a long-lived token", None, "supergrok/grok-4.7", None);
    let before = auth.state.lock().unwrap().refreshes;
    auth.state.lock().unwrap().access = "revoked-elsewhere".into();
    let (_, r) = home.run("reword it again", None, "supergrok/grok-4.7", None);
    assert_eq!(r.status, TurnStatus::Completed, "{:?}", r.error);
    assert_eq!(auth.state.lock().unwrap().refreshes, before + 1, "one refresh, on the refusal");
    // No token reaches the session log.
    let logs = log::sessions_dir(&home.env()).unwrap();
    for entry in std::fs::read_dir(&logs).unwrap() {
        let events = std::fs::read_to_string(entry.unwrap().path().join(log::EVENTS_FILE)).unwrap();
        assert!(!events.contains("xai-at-") && !events.contains("xai-rt-"));
    }
}
