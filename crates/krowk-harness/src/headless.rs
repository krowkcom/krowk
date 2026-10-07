//! `krowk -p`: one prompt, run headless through the in-process protocol.
//! The runner is a client like any other — it sends `Command::Prompt`,
//! reads `StreamLine`s, and on Ctrl-C sends `Command::Interrupt` — so what
//! it prints is exactly what the protocol says.
//!
//! - `text` prints the turn's final answer.
//! - `json` prints the `result` event.
//! - `stream-json` prints every line of the stream as it happens: the
//!   logged events exactly as the log has them, the live
//!   `item.started`/`item.delta` frames, and the `result` last.
//!
//! One prompt is the whole run: an agent a backend runs in the background
//! (Claude Code's `Agent` tool) is not waited for, and stops with the run —
//! stderr says so.

use crate::engine::EngineError;
use crate::host::{Host, HostConfig};
use crate::protocol::{Command, LiveEvent, ModelRef, PermissionMode, RunResult, StreamLine};
use std::io::Write;
use tokio::sync::mpsc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Text,
    Json,
    StreamJson,
}

impl OutputFormat {
    pub const NAMES: [&'static str; 3] = ["text", "json", "stream-json"];

    pub fn parse(s: &str) -> Option<OutputFormat> {
        Some(match s {
            "" | "text" => OutputFormat::Text,
            "json" => OutputFormat::Json,
            "stream-json" => OutputFormat::StreamJson,
            _ => return None,
        })
    }
}

pub struct Options {
    pub prompt: String,
    pub resume: Option<String>,
    pub model: Option<ModelRef>,
    pub permission_mode: PermissionMode,
    /// `--toolset`: the preset for this prompt, instead of the model's.
    pub toolset: Option<String>,
    /// `--effort`: the reasoning effort for this prompt, on krowk's ladder.
    pub effort: Option<crate::protocol::Effort>,
    /// `--max-usd` and `--max-tokens`: what the session may spend.
    pub budget: Option<crate::protocol::BudgetLimits>,
    pub format: OutputFormat,
    /// `--worktree` (WT6): the worktree the session works in, finished
    /// once the run is over (`worktree::InUse::finish`) and before the
    /// result is printed, so a kept one is named in it
    /// (`RunResult::worktree`).
    pub worktree: Option<crate::worktree::InUse>,
}

/// How the run came out. `result` is set whenever a turn ran, failed or
/// not; `error` is set when none could.
pub struct Outcome {
    pub session_id: Option<String>,
    pub result: Option<RunResult>,
    pub error: Option<EngineError>,
    /// `Options::worktree`, and how finishing it went.
    pub worktree: Option<(crate::worktree::Worktree, Result<crate::worktree::Finished, crate::worktree::Error>)>,
}

/// Runs the prompt to its end on a runtime of its own: the rest of krowk is
/// blocking, and this is the one place it waits on async work.
pub fn run(cfg: HostConfig, opts: Options, stdout: &mut dyn Write) -> Outcome {
    let rt = match runtime() {
        Ok(rt) => rt,
        Err(e) => return Outcome { session_id: None, result: None, error: Some(e), worktree: None },
    };
    rt.block_on(drive(&Host::new(cfg), opts, stdout))
}

fn runtime() -> Result<tokio::runtime::Runtime, EngineError> {
    tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| EngineError::new("runtime_unavailable", format!("the async runtime could not start: {e}")))
}

/// `krowk -p --daemon`: the same run, sent to the host daemon — started
/// first when none is up (`daemon::ensure`, with `spawn`) — so the turn is
/// the daemon's and survives this process. What it prints is what the
/// in-process run prints.
#[cfg(unix)]
pub fn run_on_daemon(env: &dyn Fn(&str) -> String, cwd: &std::path::Path, version: &str, spawn: &crate::daemon::Spawn<'_>, opts: Options, stdout: &mut dyn Write) -> Outcome {
    let rt = match runtime() {
        Ok(rt) => rt,
        Err(e) => return Outcome { session_id: None, result: None, error: Some(e), worktree: None },
    };
    rt.block_on(async {
        // A `-p` client answers no approval request: its turns refuse what
        // would be asked, as they do in-process.
        match crate::daemon::ensure(env, cwd, version, false, spawn).await {
            Ok(client) => {
                if client.krowk_version != version {
                    let _ = writeln!(std::io::stderr(), "! the host daemon (pid {}) runs krowk {}, and this is {version} — its turns run on the old binary until it exits; `kill {}` once its sessions are done", client.pid, client.krowk_version, client.pid);
                }
                drive(&client, opts, stdout).await
            }
            Err(e) => Outcome { session_id: None, result: None, error: Some(e), worktree: None },
        }
    })
}

/// What a headless run sends its commands to: the in-process `Host`, or the
/// daemon's socket (`daemon::client::Client`). The same two calls either
/// way, which is the point of the protocol.
pub trait Transport {
    fn execute(&self, cmd: Command, out: mpsc::Sender<StreamLine>) -> impl std::future::Future<Output = Result<Option<RunResult>, EngineError>>;
    /// Lets the transport's backend processes go: the in-process host's.
    /// The daemon's are the daemon's, and stay.
    fn shutdown(&self) -> impl std::future::Future<Output = ()>;
}

impl Transport for Host {
    fn execute(&self, cmd: Command, out: mpsc::Sender<StreamLine>) -> impl std::future::Future<Output = Result<Option<RunResult>, EngineError>> {
        Host::execute(self, cmd, out)
    }

    fn shutdown(&self) -> impl std::future::Future<Output = ()> {
        Host::shutdown(self)
    }
}

#[cfg(unix)]
impl Transport for crate::daemon::client::Client {
    fn execute(&self, cmd: Command, out: mpsc::Sender<StreamLine>) -> impl std::future::Future<Output = Result<Option<RunResult>, EngineError>> {
        crate::daemon::client::Client::execute(self, cmd, out)
    }

    async fn shutdown(&self) {}
}

async fn drive(host: &impl Transport, opts: Options, stdout: &mut dyn Write) -> Outcome {
    let format = opts.format;
    let mut worktree = opts.worktree;
    let mut held = HeldResult::default();
    let (tx, mut rx) = mpsc::channel::<StreamLine>(1024);
    // A resumed session's id is known up front; a new one's arrives with
    // its root event.
    let mut session_id: Option<String> = opts.resume.clone();
    let cmd = Command::Prompt { session_id: opts.resume, text: opts.prompt, images: Vec::new(), model: opts.model, permission_mode: opts.permission_mode, toolset: opts.toolset, effort: opts.effort, budget: opts.budget };
    let exec = host.execute(cmd, tx);
    tokio::pin!(exec);
    let mut done: Option<Result<Option<RunResult>, EngineError>> = None;
    // One signal stream for the whole run, so a Ctrl-C that lands between
    // two turns of the loop is queued, not lost.
    let mut sigint = Interrupts::new();
    let mut interrupts = 0u32;
    // Asked for and not yet accepted: the turn may not be running yet (the
    // session is still being opened), so it is asked again until it is.
    let mut want_interrupt = false;
    // The backend's own agents, as it last listed them.
    let mut agents = Vec::new();
    loop {
        let retry = async {
            if want_interrupt {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            } else {
                std::future::pending::<()>().await;
            }
        };
        tokio::select! {
            biased;
            Some(line) = rx.recv() => {
                if session_id.is_none() {
                    session_id = Some(line.session_id().to_string());
                }
                if let StreamLine::Live(LiveEvent::BackendAgents { session_id: s, agents: a }) = &line
                    && session_id.as_deref() == Some(s.as_str())
                {
                    agents.clone_from(a);
                }
                show(line, format, worktree.is_some().then_some(&mut held), stdout);
            }
            r = &mut exec, if done.is_none() => done = Some(r),
            _ = sigint.recv(), if done.is_none() => {
                interrupts += 1;
                // The first Ctrl-C asks the turn to stop and keeps what it
                // made; a second one does not wait.
                if interrupts > 1 {
                    // Not waited on, but not left running either: every
                    // backend's process group goes with krowk.
                    crate::group::kill_all();
                    if let Some(w) = worktree.take() {
                        finish_before_exit(w);
                    }
                    std::process::exit(130);
                }
                want_interrupt = true;
            }
            _ = retry, if done.is_none() => {}
            else => break,
        }
        if want_interrupt
            && done.is_none()
            && let Some(id) = &session_id
        {
            let (itx, _irx) = mpsc::channel(1);
            if host.execute(Command::Interrupt { session_id: id.clone() }, itx).await.is_ok() {
                want_interrupt = false;
            }
        }
    }
    if !agents.is_empty() {
        let names: Vec<String> = agents.iter().map(|a: &crate::protocol::BackendAgent| format!("“{}”", a.description.chars().map(|c| if c.is_control() { ' ' } else { c }).collect::<String>())).collect();
        let (what, verbs) = if agents.len() == 1 { ("background agent", "is still running, and stops") } else { ("background agents", "are still running, and stop") };
        let _ = writeln!(std::io::stderr(), "! Claude Code's {what} {} {verbs} with this run: krowk -p does not wait for background agents", names.join(", "));
    }
    // A backend's process is let go before the answer is reported, so its
    // transcript is whole when krowk exits.
    host.shutdown().await;
    // The session is over: its worktree is finished before the result is
    // printed, so a kept one is named in it.
    let (worktree, kept) = finish(worktree);
    let done = done.expect("the loop ends only once the command has").map(|r| r.map(|result| RunResult { worktree: kept.map(Box::new), ..result }));
    // The result held back for the worktree: the run's own, naming it, or
    // the last one there was when the run has none.
    if format == OutputFormat::StreamJson && worktree.is_some() {
        let last = match &done {
            Ok(Some(result)) => Some(StreamLine::Live(LiveEvent::Result(result.clone()))),
            _ => held.take(),
        };
        if let Some(last) = last {
            let _ = writeln!(stdout, "{}", serde_json::to_string(&last).expect("a stream line serializes"));
        }
    }
    match done {
        Ok(Some(result)) => {
            match format {
                OutputFormat::Text => {
                    if !result.result.is_empty() {
                        let _ = writeln!(stdout, "{}", result.result);
                    }
                }
                OutputFormat::Json => {
                    let _ = writeln!(stdout, "{}", serde_json::to_string(&LiveEvent::Result(result.clone())).expect("a result serializes"));
                }
                OutputFormat::StreamJson => {}
            }
            Outcome { session_id: Some(result.session_id.clone()), result: Some(result), error: None, worktree }
        }
        Ok(None) => Outcome { session_id, result: None, error: None, worktree },
        Err(e) => Outcome { session_id, result: None, error: Some(e), worktree },
    }
}

/// One line of the stream, as the run shows it: a notice on stderr, every
/// line on stdout under stream-json, and a `result` held back (`held`, a
/// `--worktree` run's) until a newer one comes, or the worktree is
/// finished.
fn show(line: StreamLine, format: OutputFormat, held: Option<&mut HeldResult>, stdout: &mut dyn Write) {
    // A notice is the person's alone (a claim token is a secret): the
    // terminal's stderr, never stdout, which a program reads.
    if let StreamLine::Live(LiveEvent::Notice { text, .. }) = &line {
        let _ = writeln!(std::io::stderr(), "! {text}");
        return;
    }
    let line = match held {
        Some(held) if matches!(line, StreamLine::Live(LiveEvent::Result(_))) => match held.hold(line) {
            Some(earlier) => earlier,
            None => return,
        },
        _ => line,
    };
    if format == OutputFormat::StreamJson {
        let _ = writeln!(stdout, "{}", serde_json::to_string(&line).expect("a stream line serializes"));
        let _ = stdout.flush();
    }
}

/// The run's worktree finished, with how that went, and as its result
/// names it when it is kept.
#[allow(clippy::type_complexity)]
fn finish(worktree: Option<crate::worktree::InUse>) -> (Option<(crate::worktree::Worktree, Result<crate::worktree::Finished, crate::worktree::Error>)>, Option<crate::protocol::KeptWorktree>) {
    let Some(w) = worktree else { return (None, None) };
    let wt = w.worktree.clone();
    let finished = w.finish();
    let kept = finished.clone().ok().and_then(|f| crate::worktree::kept(&wt, f));
    (Some((wt, finished)), kept)
}

/// The `result` frames of a `--worktree` run, held back one at a time: a
/// turn that fails over to another instance (R-INST-7) sends one per turn,
/// and only the last is the run's end, which names the worktree. An
/// earlier one goes out as soon as a newer one arrives.
#[derive(Default)]
struct HeldResult(Option<StreamLine>);

impl HeldResult {
    /// Holds `line`: the one held before it, to write now.
    fn hold(&mut self, line: StreamLine) -> Option<StreamLine> {
        self.0.replace(line)
    }

    fn take(&mut self) -> Option<StreamLine> {
        self.0.take()
    }
}

/// How long a second Ctrl-C waits for the worktree to be finished.
const FINISH_ON_EXIT: std::time::Duration = std::time::Duration::from_secs(5);

/// The worktree of a run left at once (a second Ctrl-C), finished on the
/// way out as far as `FINISH_ON_EXIT` allows, so an unchanged one is not
/// left behind and a kept one is named. Best effort: one not finished in
/// time is left as it is, which `krowk worktrees` lists.
fn finish_before_exit(w: crate::worktree::InUse) {
    let wt = w.worktree.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(w.finish());
    });
    match rx.recv_timeout(FINISH_ON_EXIT) {
        Ok(Ok(crate::worktree::Finished::Removed)) => {}
        Ok(Ok(crate::worktree::Finished::Kept { .. })) => {
            let _ = writeln!(std::io::stderr(), "{}", crate::worktree::kept_line(&wt));
        }
        _ => {
            let _ = writeln!(std::io::stderr(), "! the worktree {} (branch {}) was left as it is — `krowk worktrees` lists it", wt.path.display(), wt.branch());
        }
    }
}

/// SIGINT as a stream: registered once, so none is missed between polls.
struct Interrupts {
    #[cfg(unix)]
    inner: Option<tokio::signal::unix::Signal>,
}

impl Interrupts {
    fn new() -> Interrupts {
        #[cfg(unix)]
        return Interrupts { inner: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()).ok() };
        #[cfg(not(unix))]
        Interrupts {}
    }

    async fn recv(&mut self) {
        #[cfg(unix)]
        match &mut self.inner {
            Some(s) => {
                if s.recv().await.is_none() {
                    std::future::pending::<()>().await;
                }
            }
            None => std::future::pending::<()>().await,
        }
        #[cfg(not(unix))]
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(turn: &str) -> StreamLine {
        let r = RunResult {
            session_id: "s".into(),
            turn_id: turn.into(),
            status: crate::protocol::TurnStatus::Failed,
            is_error: true,
            result: String::new(),
            model: ModelRef { instance: "anthropic".into(), model: "m".into() },
            usage: crate::protocol::Usage::default(),
            cost_usd: None,
            duration_ms: 1,
            num_model_calls: 1,
            error: None,
            unread_steers: Vec::new(),
            switch_offer: None,
            worktree: None,
        };
        StreamLine::Live(LiveEvent::Result(r))
    }

    /// A run that fails over sends a result per turn: each but the last
    /// goes out when the next arrives, and the last is held for the
    /// worktree.
    #[test]
    fn wt6_only_the_latest_result_is_held_back() {
        let mut held = HeldResult::default();
        assert!(held.hold(result("t1")).is_none());
        let out = held.hold(result("t2")).expect("the failed turn's result goes out");
        assert!(matches!(&out, StreamLine::Live(LiveEvent::Result(r)) if r.turn_id == "t1"));
        assert!(matches!(held.take(), Some(StreamLine::Live(LiveEvent::Result(r))) if r.turn_id == "t2"));
        assert!(held.take().is_none());
    }
}
