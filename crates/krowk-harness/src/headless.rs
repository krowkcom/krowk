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
}

/// How the run came out. `result` is set whenever a turn ran, failed or
/// not; `error` is set when none could.
pub struct Outcome {
    pub session_id: Option<String>,
    pub result: Option<RunResult>,
    pub error: Option<EngineError>,
}

/// Runs the prompt to its end on a runtime of its own: the rest of krowk is
/// blocking, and this is the one place it waits on async work.
pub fn run(cfg: HostConfig, opts: Options, stdout: &mut dyn Write) -> Outcome {
    let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => return Outcome { session_id: None, result: None, error: Some(EngineError::new("runtime_unavailable", format!("the async runtime could not start: {e}"))) },
    };
    rt.block_on(drive(Host::new(cfg), opts, stdout))
}

async fn drive(host: Host, opts: Options, stdout: &mut dyn Write) -> Outcome {
    let format = opts.format;
    let (tx, mut rx) = mpsc::channel::<StreamLine>(1024);
    // A resumed session's id is known up front; a new one's arrives with
    // its root event.
    let mut session_id: Option<String> = opts.resume.clone();
    let cmd = Command::Prompt { session_id: opts.resume, text: opts.prompt, model: opts.model, permission_mode: opts.permission_mode, toolset: opts.toolset, effort: opts.effort, budget: opts.budget };
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
                    session_id = Some(line_session(&line).to_string());
                }
                if let StreamLine::Live(LiveEvent::BackendAgents { session_id: s, agents: a }) = &line
                    && session_id.as_deref() == Some(s.as_str())
                {
                    agents.clone_from(a);
                }
                // A notice is the person's alone (a claim token is a secret):
                // the terminal's stderr, never stdout, which a program reads.
                if let StreamLine::Live(LiveEvent::Notice { text, .. }) = &line {
                    let _ = writeln!(std::io::stderr(), "! {text}");
                } else if format == OutputFormat::StreamJson {
                    let _ = writeln!(stdout, "{}", serde_json::to_string(&line).expect("a stream line serializes"));
                    let _ = stdout.flush();
                }
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
    match done.expect("the loop ends only once the command has") {
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
            Outcome { session_id: Some(result.session_id.clone()), result: Some(result), error: None }
        }
        Ok(None) => Outcome { session_id, result: None, error: None },
        Err(e) => Outcome { session_id, result: None, error: Some(e) },
    }
}

fn line_session(line: &StreamLine) -> &str {
    match line {
        StreamLine::Log(ev) => &ev.session_id,
        StreamLine::Live(LiveEvent::ItemStarted { session_id, .. } | LiveEvent::ItemDelta { session_id, .. }) => session_id,
        StreamLine::Live(LiveEvent::Cost { session_id, .. } | LiveEvent::Notice { session_id, .. } | LiveEvent::Limits { session_id, .. }) => session_id,
        StreamLine::Live(LiveEvent::BackendAgents { session_id, .. } | LiveEvent::TurnUnprompted { session_id, .. }) => session_id,
        StreamLine::Live(LiveEvent::ApprovalRequested(r)) => &r.session_id,
        StreamLine::Live(LiveEvent::ApprovalResolved { session_id, .. }) => session_id,
        StreamLine::Live(LiveEvent::Result(r)) => &r.session_id,
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
