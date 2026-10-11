//! The subagent view, live (SV15): what a person sees in tmux when a turn
//! runs three agents in parallel, one of them `sleep 120` in bash. Each step
//! is read off the screen, and captured when `KROWK_LIVE_CAPTURES` names a
//! directory:
//!
//! 1. the rows say `bash 1m…` and `quiet …` (R-SUB-11);
//! 2. Ctrl-G's Agents overlay lists the three;
//! 3. Enter on the sleeper opens its child view, its header first;
//! 4. → shows a sibling;
//! 5. `x` on the sleeper stops it alone: it ends interrupted (R-SUB-10)
//!    while the other two finish;
//! 6. Esc goes back, and the conversation is as it was.
//!
//! Then the same session, synced through `krowk relay serve` and a
//! stand-in registry, is attached from a second machine's TUI (`krowk sync
//! attach`), which opens a child of a turn it prompted and stops it on the
//! host (R-SUB-12).
//!
//! Twice: on the Claude Code backend, and on a native session. Both are
//! ignored by default — each takes minutes, the Claude Code one spends real
//! tokens — and are run by hand:
//!
//!     KROWK_LIVE_CLAUDE_CONFIG=~/.krowk/accounts/claude-x \
//!     KROWK_LIVE_CAPTURES=/tmp/captures \
//!       cargo test -p krowk --features harness --test subagents_live -- --ignored --nocapture
//!
//! The Claude Code run needs `claude` on PATH and a Claude Code config
//! directory that is signed in (`KROWK_LIVE_CLAUDE_CONFIG`, an instance's
//! `configDir`; its model `KROWK_LIVE_CLAUDE_MODEL`, Haiku unless named);
//! without them it is skipped, saying so. The native run has a stand-in
//! Anthropic API play the model, so its moves are scripted while the TUI,
//! host, engine, bash and sync are real.

#![cfg(all(feature = "harness", unix))]

#[path = "common/device_list.rs"]
mod device_list;
#[path = "../../krowk-harness/tests/common/mock.rs"]
mod mock;
#[path = "common/tmux.rs"]
mod tmux;

use device_list::People;
use krowk_client::e2e::{self, AccountKey};
use krowk_client::keystore::Keystore;
use serde_json::{json, Value};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tmux::Tmux;

const TOKEN: &str = "krowk_sk_subagents_live_0000000000000000";

/// The first turn: three agents in parallel, one asleep for two minutes,
/// the other two long enough to still run when it is stopped.
const THREE: &str = "Use the Agent tool three times in ONE message, so the three agents run in parallel in the foreground (not in the background). \
Agent 1: subagent_type general-purpose, description \"sleeper\", prompt: \"Run exactly this Bash command in the foreground (not in the background), with timeout 300000: sleep 120. Then reply: slept.\" \
Agent 2: subagent_type general-purpose, description \"counter\", prompt: \"Run this Bash command in the foreground: sleep 95 && echo counted. Then reply with its output.\" \
Agent 3: subagent_type general-purpose, description \"reader\", prompt: \"Run this Bash command in the foreground: sleep 100 && cat notes.txt. Then reply with what it printed.\" \
Do nothing else yourself. When they end, say in one line which finished and which did not.";

/// The second turn, prompted from the attached machine: two agents, one
/// for it to stop.
const TWO: &str = "Use the Agent tool twice in ONE message, so the two agents run in parallel in the foreground (not in the background). \
Agent 1: subagent_type general-purpose, description \"napper\", prompt: \"Run exactly this Bash command in the foreground (not in the background), with timeout 300000: sleep 90. Then reply: napped.\" \
Agent 2: subagent_type general-purpose, description \"waker\", prompt: \"Run this Bash command in the foreground: sleep 30 && echo awake. Then reply with its output.\" \
Do nothing else yourself. When they end, say in one line which finished and which did not.";

/// One machine's view of the run: its home, its repository, the variables
/// its krowk runs with.
struct Machine {
    home: PathBuf,
    repo: PathBuf,
    run: PathBuf,
    env: Vec<(String, String)>,
}

impl Machine {
    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_krowk"));
        c.args(args).env_clear().envs(self.env.iter().map(|(k, v)| (k, v))).current_dir(&self.repo);
        c
    }

    fn sessions(&self) -> PathBuf {
        self.home.join(".krowk/sessions")
    }

    /// Every event of a session's log on this machine.
    fn log(&self, id: &str) -> Vec<Value> {
        let text = std::fs::read_to_string(self.sessions().join(id).join("events.jsonl")).unwrap_or_default();
        text.lines().filter_map(|l| serde_json::from_str(l).ok()).collect()
    }

    /// The child whose `subagent.started` in `parent`'s log has
    /// `description`, and how its own log says its turn ended.
    fn child_end(&self, parent: &str, description: &str) -> (String, Option<String>) {
        let started = self.log(parent).into_iter().find(|e| e["type"] == "subagent.started" && e["description"] == description);
        let child = started.and_then(|e| e["subagentSessionId"].as_str().map(String::from)).unwrap_or_else(|| panic!("no subagent {description:?} in {parent}"));
        let end = self.log(&child).into_iter().rev().find(|e| e["type"] == "turn.completed").and_then(|e| e["status"].as_str().map(String::from));
        (child, end)
    }

    /// Waits until `n` turns of the session `id` have ended in its log.
    fn turns_ended(&self, id: &str, n: usize, secs: u64) {
        let deadline = Instant::now() + Duration::from_secs(secs);
        let ended = || self.log(id).iter().filter(|e| e["type"] == "turn.completed").count();
        while ended() < n {
            assert!(Instant::now() < deadline, "{n} turns of {id} ended within {secs}s: {}", ended());
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    fn tui(&self, name: &str, args: &[&str]) -> Option<Tmux> {
        let mut env = self.env.clone();
        env.push(("TERM".into(), "xterm-256color".into()));
        Tmux::start(name, 140, 40, &self.repo, &env, args)
    }
}

impl Drop for Machine {
    fn drop(&mut self) {
        let _ = self.command(&["host", "stop", "--force"]).stdout(Stdio::null()).stderr(Stdio::null()).status();
        let _ = std::fs::remove_dir_all(&self.run);
    }
}

/// A stand-in registry, `krowk relay serve` and one person's keys, for two
/// machines to sync a session through.
struct World {
    name: String,
    root: PathBuf,
    api: String,
    relay: String,
    account: AccountKey,
    people: std::sync::Mutex<People>,
    _registry: krowk_devregistry::Running,
    relay_serve: Child,
}

impl World {
    fn new(name: &str) -> World {
        let root = std::env::temp_dir().join(format!("krowk-live-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let registry = krowk_devregistry::start(TcpListener::bind("127.0.0.1:0").unwrap(), Default::default()).unwrap();
        let api = format!("{}/v1", registry.url());
        // The reference relay as a person runs it, trusting the stand-in's
        // ticket key.
        let keys = root.join("ticket-keys.json");
        std::fs::write(&keys, json!({"ticketKeys": {e2e::hex(&krowk_devregistry::TICKET_KID): e2e::hex(&krowk_devregistry::ticket_public_key())}}).to_string()).unwrap();
        let addr = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
        let relay_serve = Command::new(env!("CARGO_BIN_EXE_krowk"))
            .args(["relay", "serve", "--ticket-keys"])
            .arg(&keys)
            .args(["--addr", &addr.to_string()])
            .env_clear()
            .env("HOME", root.join("relay-home"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(root.join("relay.log")).unwrap())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        while std::net::TcpStream::connect(addr).is_err() {
            assert!(Instant::now() < deadline, "krowk relay serve listens: {}", std::fs::read_to_string(root.join("relay.log")).unwrap_or_default());
            std::thread::sleep(Duration::from_millis(50));
        }
        World { name: name.into(), root, api, relay: format!("ws://{addr}"), account: AccountKey::generate(), people: Default::default(), _registry: registry, relay_serve }
    }

    /// A machine on the person's device list, with `config` as its
    /// config.json and `extra` among its variables. Its repository is
    /// trusted, as a person's answer to the trust question leaves it.
    fn machine(&self, name: &str, config: Value, extra: &[(&str, String)]) -> Machine {
        let home = self.root.join(name);
        let repo = home.join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::write(repo.join("notes.txt"), "the notes say: periwinkle\n").unwrap();
        std::fs::create_dir_all(home.join(".krowk")).unwrap();
        std::fs::write(home.join(".krowk/config.json"), config.to_string()).unwrap();
        std::fs::write(home.join(".krowk/trusted.json"), json!({"directories": [repo]}).to_string()).unwrap();
        // Short, as a socket path must be.
        let run = PathBuf::from(format!("/tmp/krowk-live-{}-{name}-{}", self.name, std::process::id()));
        let _ = std::fs::remove_dir_all(&run);
        std::fs::create_dir_all(&run).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o700)).unwrap();
        let ks = Keystore::new(&home.join(".krowk"));
        ks.recover(AccountKey::from_bytes(*self.account.as_bytes())).unwrap();
        {
            let mut people = self.people.lock().unwrap();
            people.enlist(&ks, name);
            people.publish(&self.api, TOKEN);
        }
        let mut env: Vec<(String, String)> = vec![
            ("HOME".into(), home.display().to_string()),
            ("XDG_RUNTIME_DIR".into(), run.display().to_string()),
            ("KROWK_NO_UPDATE_CHECK".into(), "1".into()),
            ("KROWK_HOST_IDLE".into(), "5".into()),
            ("KROWK_API_URL".into(), self.api.clone()),
            ("KROWK_TOKEN".into(), device_list::key(TOKEN, name)),
            ("KROWK_RELAY_URL".into(), self.relay.clone()),
            ("KROWK_TAILSCALE_SOCKET".into(), run.join("no-tailscaled.sock").display().to_string()),
        ];
        env.extend(extra.iter().map(|(k, v)| (k.to_string(), v.clone())));
        Machine { home, repo, run, env }
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = self.relay_serve.kill();
        let _ = self.relay_serve.wait();
        if std::env::var_os("KROWK_LIVE_KEEP").is_none() {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
}

/// Where the captures go, if anywhere.
fn captures() -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var_os("KROWK_LIVE_CAPTURES")?);
    std::fs::create_dir_all(&dir).unwrap();
    Some(dir)
}

/// One step's screen: kept as `<run>-<step>.txt`, and printed.
fn capture(run: &str, step: &str, screen: &str) {
    let trimmed: Vec<&str> = screen.lines().map(str::trim_end).collect();
    let text = trimmed.join("\n").trim_matches('\n').to_string() + "\n";
    eprintln!("── {run} {step}\n{text}");
    if let Some(dir) = captures() {
        std::fs::write(dir.join(format!("{run}-{step}.txt")), text).unwrap();
    }
}

/// Waits until the screen passes `ok`, failing with the screen after
/// `secs`.
fn until(tm: &Tmux, what: &str, secs: u64, ok: impl Fn(&str) -> bool) -> String {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let screen = tm.screen();
        if ok(&screen) {
            return screen;
        }
        assert!(Instant::now() < deadline, "no {what} within {secs}s:\n{screen}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The first row with anything on it: an open child view's header.
fn header(screen: &str) -> String {
    screen.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or_default().to_string()
}

fn in_view(screen: &str) -> bool {
    screen.contains("esc back · ← → siblings · x stop")
}

/// The row the Agents overlay has selected: drawn reversed.
fn selected(tm: &Tmux) -> String {
    let styled = tm.tmux(&["capture-pane", "-p", "-e", "-t", "t"]);
    styled.lines().find(|l| sgr_params(l).iter().any(|p| p == "7")).map(strip).unwrap_or_default()
}

fn sgr_params(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = line;
    while let Some(at) = rest.find("\x1b[") {
        let tail = &rest[at + 2..];
        let Some(end) = tail.find(|c: char| c.is_ascii_alphabetic()) else { break };
        if tail.as_bytes()[end] == b'm' {
            out.extend(tail[..end].split(';').map(String::from));
        }
        rest = &tail[end + 1..];
    }
    out
}

fn strip(line: &str) -> String {
    let mut out = String::new();
    let mut rest = line;
    while let Some(at) = rest.find('\x1b') {
        out.push_str(&rest[..at]);
        let tail = &rest[at + 1..];
        let skip = if tail.starts_with('[') { tail.find(|c: char| c.is_ascii_alphabetic()).map_or(tail.len(), |e| e + 1) } else { 1.min(tail.len()) };
        rest = &tail[skip..];
    }
    out.push_str(rest);
    out.trim().to_string()
}

/// Types a prompt and sends it.
fn prompt(tm: &Tmux, text: &str) {
    tm.keys(&["-l", text]);
    std::thread::sleep(Duration::from_millis(300));
    tm.keys(&["Enter"]);
}

/// From the Agents overlay, the child named `name` opened, in the order a
/// person would: ↓ to its row, Enter.
fn open(tm: &Tmux, name: &str) -> String {
    tm.keys(&["C-g"]);
    until(tm, "the Agents overlay", 10, |s| s.contains("enter opens"));
    for _ in 0..6 {
        if selected(tm).contains(&format!("Agent {name} ")) {
            break;
        }
        tm.keys(&["Down"]);
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(selected(tm).contains(&format!("Agent {name} ")), "{name} is selected: {}", tm.screen());
    tm.keys(&["Enter"]);
    until(tm, &format!("{name}'s view"), 10, |s| in_view(s) && header(s).starts_with(&format!("{name} ·")))
}

/// What a run checks, for one backend.
struct Run {
    /// `claude` or `native`: names its captures.
    name: &'static str,
    /// What its header says ran the children.
    by: &'static str,
}

/// The whole check: A's TUI runs the three agents and stops the sleeper;
/// B, attached through the relay, prompts two, opens the napper and stops
/// it.
fn check(run: &Run, w: &World, a: &Machine, b: &Machine, model: &str) {
    let made = a.command(&["-p", "say ok", "--model", model]).stdin(Stdio::null()).output().unwrap();
    assert!(made.status.success(), "A makes the session: {}", String::from_utf8_lossy(&made.stderr));
    let session = std::fs::read_dir(a.sessions()).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).find(|n| n.len() == 36).expect("a session was logged");
    let host_log = std::fs::File::create(w.root.join("sync-host.log")).unwrap();
    let mut host = a.command(&["sync", "host", &session]).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(host_log).spawn().unwrap();
    let ta = a.tui(&format!("{}-a", run.name), &["--resume", &session]).expect("tmux");
    until(&ta, "A's prompt", 30, |s| s.contains("Plan, search, build anything"));

    // 1. Three agents, the rows' facts once the sleeper's bash has run a minute.
    prompt(&ta, THREE);
    let rows = until(&ta, "the sleeper's bash at a minute, and quiet", 180, |s| {
        s.lines().any(|l| l.contains("Agent sleeper") && l.contains("bash 1m") && l.contains("quiet"))
    });
    for name in ["sleeper", "counter", "reader"] {
        assert!(rows.contains(&format!("Agent {name}")), "{name}'s row: {rows}");
    }
    capture(run.name, "01-rows", &rows);

    // 2. The overlay, 3. the sleeper's view.
    ta.keys(&["C-g"]);
    let overlay = until(&ta, "the Agents overlay", 10, |s| s.contains("enter opens") && s.matches("· running").count() >= 3);
    capture(run.name, "02-overlay", &overlay);
    ta.keys(&["Escape"]);
    until(&ta, "the overlay closed", 10, |s| !s.contains("enter opens"));
    let view = open(&ta, "sleeper");
    let head = header(&view);
    assert!(head.contains(&format!("· by {} · running", run.by)) && head.contains("bash 1m"), "the sleeper's header: {head}");
    assert!(view.contains("sleep 120"), "its prompt and its call: {view}");
    capture(run.name, "03-sleeper-view", &view);

    // 4. → to a sibling, ← back.
    ta.keys(&["Right"]);
    let sibling = until(&ta, "a sibling's view", 10, |s| in_view(s) && !header(s).starts_with("sleeper ·"));
    assert!(header(&sibling).starts_with("counter ·") || header(&sibling).starts_with("reader ·"), "{}", header(&sibling));
    capture(run.name, "04-sibling", &sibling);
    ta.keys(&["Left"]);
    until(&ta, "the sleeper again", 10, |s| in_view(s) && header(s).starts_with("sleeper ·"));

    // 5. x: the sleeper alone ends interrupted, the others finish.
    ta.keys(&["x"]);
    let stopped = until(&ta, "the sleeper interrupted", 30, |s| in_view(s) && header(s).starts_with("sleeper ·") && header(s).contains("interrupted after"));
    capture(run.name, "05-sleeper-stopped", &stopped);
    a.turns_ended(&session, 2, 120);
    let done = until(&ta, "the view at the turn's end", 10, |s| in_view(s) && !s.contains("esc to interrupt"));
    capture(run.name, "06-turn-ended", &done);
    for (name, end) in [("sleeper", "interrupted"), ("counter", "completed"), ("reader", "completed")] {
        assert_eq!(a.child_end(&session, name).1.as_deref(), Some(end), "{name}'s log");
    }

    // 6. Esc back to the overlay, Esc again to the conversation.
    ta.keys(&["Escape"]);
    let after = until(&ta, "the overlay after", 10, |s| !in_view(s) && s.contains("enter opens"));
    assert!(after.contains("Agent sleeper") && after.matches("done in").count() >= 2, "{after}");
    capture(run.name, "07-overlay-after", &after);
    ta.keys(&["Escape"]);
    let conversation = until(&ta, "the conversation", 10, |s| !s.contains("enter opens"));
    let history = ta.history();
    for want in ["Agent sleeper", "interrupted after", "Agent counter", "Agent reader", "Worked for"] {
        assert!(conversation.contains(want), "{want} in the conversation: {conversation}");
    }
    assert_eq!(history.matches("When they end, say in one line").count(), 1, "the prompt once: {history}");
    capture(run.name, "08-conversation", &conversation);

    // The attached client: B's TUI on the same session, through the relay.
    let tb = b.tui(&format!("{}-b", run.name), &["sync", "attach", &session]).expect("tmux");
    let attached = until(&tb, "B attached", 60, |s| s.contains("Attached to") && s.contains("Agent sleeper"));
    capture(run.name, "b1-attached", &attached);
    prompt(&tb, TWO);
    // A prompt from another machine runs in the host's `default` mode at
    // most: each agent's bash waits on an approval, answered here.
    let waiting = until(&tb, "the agents waiting on B", 120, |s| s.contains("Agent napper") && s.contains("waiting on you") && s.contains("allow once"));
    capture(run.name, "b2-waiting-on-you", &waiting);
    let mut answered = 0;
    while answered < 4 {
        std::thread::sleep(Duration::from_millis(800));
        if !tb.screen().contains("allow once") {
            std::thread::sleep(Duration::from_secs(3));
            if !tb.screen().contains("allow once") {
                break;
            }
            continue;
        }
        tb.keys(&["y"]);
        answered += 1;
    }
    assert!(answered >= 2, "both agents' calls approved on B");
    // The napper's bash running, then opened and stopped from B.
    until(&tb, "the napper's bash on B", 60, |s| s.lines().any(|l| l.contains("Agent napper") && l.contains("bash ")));
    let view = open(&tb, "napper");
    assert!(header(&view).contains(&format!("· by {} · running", run.by)), "{}", header(&view));
    assert!(view.contains("sleep 90") && !view.contains("could not be read"), "its history, read on B: {view}");
    capture(run.name, "b3-napper-view", &view);
    tb.keys(&["x"]);
    let stopped = until(&tb, "the napper interrupted on B", 30, |s| in_view(s) && header(s).contains("interrupted after"));
    capture(run.name, "b4-napper-stopped", &stopped);
    let host_side = until(&ta, "the napper interrupted on A", 30, |s| s.lines().any(|l| l.contains("Agent napper") && l.contains("interrupted after")));
    capture(run.name, "b5-host-sees-it", &host_side);
    tb.keys(&["Escape"]);
    tb.keys(&["Escape"]);
    a.turns_ended(&session, 3, 120);
    let ended = until(&tb, "the second turn's end on B", 30, |s| !in_view(s) && s.lines().any(|l| l.contains("Agent waker") && l.contains("done in")));
    capture(run.name, "b6-turn-ended", &ended);
    assert_eq!(a.child_end(&session, "napper").1.as_deref(), Some("interrupted"), "the napper's log on A");
    assert_eq!(a.child_end(&session, "waker").1.as_deref(), Some("completed"), "the waker's log on A");
    assert!(host.try_wait().unwrap().is_none(), "A still hosts: {}", std::fs::read_to_string(w.root.join("sync-host.log")).unwrap_or_default());
    let _ = host.kill();
    let _ = host.wait();
}

/// `claude` on PATH: the directory it is in.
fn claude_dir() -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?).find(|d| d.join("claude").is_file())
}

#[test]
#[ignore = "live: minutes long, spends Claude Code tokens; run with --ignored and KROWK_LIVE_CLAUDE_CONFIG"]
fn r_sub_10_r_sub_11_r_sub_12_live_claude_code_three_agents_one_stopped_in_the_tui_and_one_from_an_attached_tui() {
    let Some(config) = std::env::var_os("KROWK_LIVE_CLAUDE_CONFIG").map(PathBuf::from).filter(|d| d.is_dir()) else {
        eprintln!("skipped: KROWK_LIVE_CLAUDE_CONFIG names no signed-in Claude Code config directory");
        return;
    };
    let Some(bin) = claude_dir() else {
        eprintln!("skipped: claude is not on PATH");
        return;
    };
    let model = std::env::var("KROWK_LIVE_CLAUDE_MODEL").unwrap_or_else(|_| "haiku".into());
    let w = World::new("claude");
    let path = format!("{}:/usr/bin:/bin", bin.display());
    let instances = json!({"claude:live": {"kind": "claude-code", "configDir": config}});
    // A in the fullscreen TUI, B inline: the view in both screens.
    let a = w.machine("a", json!({"instances": instances, "tui": {"screen": "fullscreen"}, "permissions": {"defaultMode": "bypassPermissions"}}), &[("PATH", path.clone())]);
    let b = w.machine("b", json!({"tui": {"screen": "inline"}}), &[("PATH", path)]);
    check(&Run { name: "claude", by: "Claude Code" }, &w, &a, &b, &format!("claude:live/{model}"));
}

/// The stand-in model of the native run: each turn's agents as THREE and
/// TWO ask, each running its own bash command and then saying so.
fn native(body: &Value, _: usize) -> mock::Reply {
    let messages = body["messages"].as_array().cloned().unwrap_or_default();
    let answered = messages.last().and_then(|m| m["content"].as_array()).is_some_and(|c| c.iter().any(|b| b["type"] == "tool_result"));
    if body["system"].to_string().contains("You are a subagent") {
        let task = messages.first().map(Value::to_string).unwrap_or_default();
        if answered {
            return mock::Reply::sse(&mock::text_stream("Done: the command ran."));
        }
        let command = ["sleep 120", "sleep 95 && echo counted", "sleep 100 && cat notes.txt", "sleep 90", "sleep 30 && echo awake"].into_iter().find(|c| task.contains(c)).unwrap_or("true");
        return mock::Reply::sse(&mock::tool_use("toolu_bash", "bash", &json!({"command": command, "timeout_ms": 300000})));
    }
    if answered {
        return mock::Reply::sse(&mock::text_stream("The agents ended; each one's line says how."));
    }
    let last = messages.last().map(Value::to_string).unwrap_or_default();
    let agents: &[(&str, &str)] = if last.contains("description \\\"sleeper\\\"") {
        &[("sleeper", "Run exactly this: sleep 120"), ("counter", "Run this: sleep 95 && echo counted"), ("reader", "Run this: sleep 100 && cat notes.txt")]
    } else if last.contains("description \\\"napper\\\"") {
        &[("napper", "Run exactly this: sleep 90"), ("waker", "Run this: sleep 30 && echo awake")]
    } else {
        return mock::Reply::sse(&mock::text_stream("ok"));
    };
    let calls: Vec<(String, Value)> = agents.iter().enumerate().map(|(i, (d, p))| (format!("toolu_agent{i}"), json!({"description": d, "prompt": p}))).collect();
    mock::Reply::sse(&tool_calls(&calls))
}

/// One Messages-API response calling `subagent` once per entry.
fn tool_calls(calls: &[(String, Value)]) -> String {
    let mut out = format!(
        "event: message_start\ndata: {}\n\n",
        json!({"type": "message_start", "message": {"id": "msg_calls", "type": "message", "role": "assistant", "model": "claude-sonnet-4-6", "content": [], "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 20, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0, "output_tokens": 1}}})
    );
    for (i, (id, input)) in calls.iter().enumerate() {
        out += &format!("event: content_block_start\ndata: {}\n\n", json!({"type": "content_block_start", "index": i, "content_block": {"type": "tool_use", "id": id, "name": "subagent", "input": {}}}));
        out += &format!("event: content_block_delta\ndata: {}\n\n", json!({"type": "content_block_delta", "index": i, "delta": {"type": "input_json_delta", "partial_json": input.to_string()}}));
        out += &format!("event: content_block_stop\ndata: {}\n\n", json!({"type": "content_block_stop", "index": i}));
    }
    out + "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":40}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
}

#[test]
#[ignore = "live: minutes long; run with --ignored"]
fn r_sub_10_r_sub_11_r_sub_12_live_native_three_agents_one_stopped_in_the_tui_and_one_from_an_attached_tui() {
    let m = mock::serve(native);
    let w = World::new("native");
    let path = "/usr/bin:/bin".to_string();
    let model = [("PATH", path), ("ANTHROPIC_API_KEY", "sk-test".to_string()), ("ANTHROPIC_BASE_URL", m.url.clone())];
    // A inline, B in the fullscreen TUI: the other way round from Claude Code's run.
    let a = w.machine("a", json!({"tui": {"screen": "inline"}, "permissions": {"defaultMode": "bypassPermissions"}}), &model);
    let b = w.machine("b", json!({"tui": {"screen": "fullscreen"}}), &model);
    check(&Run { name: "native", by: "krowk" }, &w, &a, &b, "claude-sonnet-4-6");
}
