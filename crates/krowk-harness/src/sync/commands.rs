//! The commands another of the person's devices sends this host (R-HOST-4;
//! engineering/harness.md → Sync → Starting a session from another device),
//! delivered in each presence refresh's answer (`announce`) until the host
//! answers. Today only `start`: a session in a directory this host
//! announced, on a model it has ready, in the host's own default
//! permission mode for that directory, whatever the command says.
//!
//! **Once.** The last `KEEP` ids answered are kept with the sealed answer
//! given, as the bridge keeps its command ids: a delivered id already
//! answered is answered the same again, and nothing runs. One being worked
//! on is skipped until it finishes. One that does not open gets no answer
//! (an answer would need a key the sender proved it holds) and is said once
//! in host.log.
//!
//! **Start.** On a thread of its own, so a slow start never holds the
//! refresh up: the directory checked against what this host offers, read
//! again now; the model checked by the daemon (`switchModel` with no
//! session); then a background client of the daemon, connected in that
//! directory and answering approvals as a bridge's does, sends the prompt
//! and waits for the turn's `turn.started`, whose session id is the answer.
//! The connection then goes, and the turn runs on in the daemon. Automatic
//! sync takes the session up at that first turn.

use super::announce::{Announcer, Options};
use super::auto::Config;
use crate::daemon::client::Client as Daemon;
use crate::protocol::{Command, LogBody, PermissionMode, StreamLine};
use krowk_api::sync::DeliveredCommand;
use krowk_client::e2e::DeviceId;
use krowk_client::host_command::{self, HostAnswer, HostCommand};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How many answered command ids are kept, as the bridge keeps its own.
pub const KEEP: usize = 4096;

/// How long a start waits for its turn to start: the instance's readiness
/// check, a vendor's login asked, comes first.
const START_WAIT: Duration = Duration::from_secs(60);

/// What is kept of a command once it is done with.
enum Kept {
    /// It did not open: never answered, said once.
    Unopened,
    /// Answered, not yet sealed: the keys were not to hand.
    Answer { from: DeviceId, answer: HostAnswer },
    /// The sealed answer given, sent again as it is whenever the command is
    /// delivered again: the registry takes the same bytes as a repeat.
    Sealed(Vec<u8>),
}

/// The commands this host has taken, across its publisher's runs.
#[derive(Default)]
pub(crate) struct Commands(Mutex<State>);

#[derive(Default)]
struct State {
    /// Ids done with, oldest first, at most `KEEP`.
    order: VecDeque<String>,
    kept: HashMap<String, Kept>,
    /// Ids a thread is working on now.
    working: HashSet<String>,
}

impl State {
    fn keep(&mut self, id: &str, k: Kept) {
        if self.kept.insert(id.to_string(), k).is_none() {
            self.order.push_back(id.to_string());
        }
        while self.order.len() > KEEP {
            if let Some(old) = self.order.pop_front() {
                self.kept.remove(&old);
            }
        }
    }
}

impl Commands {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Each delivered command taken: answered again, skipped, refused, or
    /// started on a thread of its own. `host` is this device's name on the
    /// list, named in a refusal. Blocking: an answer is sent from here.
    pub(crate) fn take(me: &Arc<Commands>, c: &Arc<Config>, a: &Announcer, host: &str, delivered: Vec<DeliveredCommand>) {
        for d in delivered {
            let id = d.id.to_ascii_lowercase();
            let resend = {
                let mut s = me.lock();
                if s.working.contains(&id) {
                    continue;
                }
                match s.kept.get(&id) {
                    Some(Kept::Unopened) => continue,
                    Some(Kept::Sealed(b)) => Some(b.clone()),
                    Some(Kept::Answer { from, answer }) => match host_command::seal_answer(answer, &id, &a.device, from, &a.keys, a.chain.generation()) {
                        Ok(b) => {
                            s.keep(&id, Kept::Sealed(b.clone()));
                            Some(b)
                        }
                        Err(_) => continue,
                    },
                    None => None,
                }
            };
            if let Some(sealed) = resend {
                // Not landed: tried again on the next refresh.
                let _ = a.api.answer_host_command(&id, &sealed);
                continue;
            }
            let opened = DeviceId::parse(&d.from).ok_or_else(|| format!("the registry names its sender {:?}, which is no device id", d.from)).and_then(|from| host_command::open_command(&d.sealed, &id, &from, &a.device, &a.keys, &a.chain).map(|o| (from, o)));
            match opened {
                Err(e) => {
                    eprintln!("krowk: a command delivered to this machine ({id}) did not open, and is not answered: {e}");
                    me.lock().keep(&id, Kept::Unopened);
                }
                Ok((from, Err(answer))) => {
                    me.lock().keep(&id, Kept::Answer { from, answer });
                    send(me, &id, a);
                }
                Ok((from, Ok(command))) => {
                    me.lock().working.insert(id.clone());
                    let (me2, c2, id2, host) = (me.clone(), c.clone(), id.clone(), host.to_string());
                    let spawned = std::thread::Builder::new().name("krowk-host-command".into()).spawn(move || {
                        let answer = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&c2, &host, command))).unwrap_or_else(|_| HostAnswer::refused("host_failed", "the host's start panicked — see host.log on it, and send the start again"));
                        finish(&me2, &c2, &id2, from, answer);
                    });
                    if spawned.is_err() {
                        // Not started: delivered again, it is tried again.
                        me.lock().working.remove(&id);
                    }
                }
            }
        }
    }
}

/// A worker's answer kept, then sealed and sent with keys of its own; one
/// that cannot be is sent from the next refresh.
fn finish(me: &Arc<Commands>, c: &Config, id: &str, from: DeviceId, answer: HostAnswer) {
    match &answer {
        HostAnswer::Started { session_id } => eprintln!("krowk: device {from} started session {session_id} on this machine"),
        HostAnswer::Refused { code, message } => eprintln!("krowk: device {from} asked this machine to start a session, refused: {message} ({code})"),
    }
    {
        let mut s = me.lock();
        s.working.remove(id);
        s.keep(id, Kept::Answer { from, answer });
    }
    if let Some(a) = c.announce.as_ref().and_then(|o| (o.keys)(false).ok()) {
        send(me, id, &a);
    }
}

/// The kept answer to `id` sealed, if it is not yet, and sent.
fn send(me: &Arc<Commands>, id: &str, a: &Announcer) {
    let sealed = {
        let mut s = me.lock();
        let sealed = match s.kept.get(id) {
            Some(Kept::Sealed(b)) => b.clone(),
            Some(Kept::Answer { from, answer }) => match host_command::seal_answer(answer, id, &a.device, from, &a.keys, a.chain.generation()) {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("krowk: the answer to command {id} could not be sealed: {e}; it is tried again at the next refresh");
                    return;
                }
            },
            _ => return,
        };
        s.keep(id, Kept::Sealed(sealed.clone()));
        sealed
    };
    let _ = a.api.answer_host_command(id, &sealed);
}

/// The directories this host offers now: the trusted repositories, less
/// those the policy opts out.
pub(crate) fn offered(c: &Config, o: &Options, policy: &super::auto::Policy) -> Vec<String> {
    o.trusted.directories().into_iter().filter(|d| policy.opted_out(d, c.krowk_home.as_deref()).is_none()).map(|d| d.display().to_string()).collect()
}

/// A start, run: the answer to give.
fn run(c: &Config, host: &str, command: HostCommand) -> HostAnswer {
    let Some(o) = c.announce.as_ref() else { return HostAnswer::refused("host_failed", "this host takes no commands") };
    let HostCommand::Start { cwd, model, prompt } = command;
    let policy = match (c.policy)() {
        Ok(p) if p.on => p,
        Ok(_) => return HostAnswer::refused("sync_off", format!("automatic sync is off on {host} — turn it on there to start sessions from your other devices")),
        Err(e) => return HostAnswer::refused("sync_off", format!("{host} cannot read its sync settings: {e}")),
    };
    let offered = offered(c, o, &policy);
    if !offered.contains(&cwd) {
        let pick = if offered.is_empty() { "it offers none yet".to_string() } else { format!("pick one of: {}", offered.join(", ")) };
        return HostAnswer::refused("directory_not_offered", format!("{host} does not offer {cwd}: on {host}, run krowk there once and trust it, or {pick}"));
    }
    // The host's own default for the directory, never what the command
    // carried, and never a mode that asks nobody.
    let mode = match (o.mode_for)(Path::new(&cwd)) {
        Ok(m) if m.asks_nothing() => PermissionMode::Default,
        Ok(m) => m,
        Err((code, message)) => return HostAnswer::refused(&code, message),
    };
    let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else {
        return HostAnswer::refused("host_failed", format!("{host} has no async runtime for the start — try again"));
    };
    rt.block_on(start(c, &cwd, model, prompt, mode))
}

async fn start(c: &Config, cwd: &str, model: crate::protocol::ModelRef, prompt: String, mode: PermissionMode) -> HostAnswer {
    let refused = |e: crate::engine::EngineError| HostAnswer::refused(&e.code, e.message);
    // Background, so it never keeps the daemon up; answering approvals, as
    // a bridge's client does, so the turn's are asked of its followers.
    let d = match Daemon::connect_as(&c.socket, Path::new(cwd), &c.version, true, true).await {
        Ok(d) => d,
        Err(crate::daemon::client::ConnectError::Failed(e)) => return refused(e),
        Err(crate::daemon::client::ConnectError::Absent) => return HostAnswer::refused("host_unavailable", "the host daemon is not running — it starts again with the next krowk there"),
    };
    let (tx, _rx) = tokio::sync::mpsc::channel(1);
    if let Err(e) = d.execute(Command::SwitchModel { session_id: None, model: model.clone() }, tx).await {
        return refused(e);
    }
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    let prompt = Command::Prompt { session_id: None, text: prompt, images: Vec::new(), model: Some(model), permission_mode: mode, toolset: None, effort: None, budget: None };
    let run = d.execute(prompt, tx);
    tokio::pin!(run);
    let wait = tokio::time::sleep(START_WAIT);
    tokio::pin!(wait);
    loop {
        tokio::select! {
            line = rx.recv() => match line {
                Some(StreamLine::Log(ev)) if matches!(ev.body, LogBody::TurnStarted { .. }) => return HostAnswer::Started { session_id: ev.session_id },
                Some(_) => {}
                None => return HostAnswer::refused("host_gone", "the host daemon closed the connection before the turn started — `krowk host status` there says whether it is up"),
            },
            done = &mut run => return match done {
                Err(e) => refused(e),
                Ok(_) => HostAnswer::refused("not_started", "the prompt ended before a turn started — send it again"),
            },
            () = &mut wait => return HostAnswer::refused("start_timeout", format!("the turn did not start within {} s — it may still; the host's session list says", START_WAIT.as_secs())),
        }
    }
}

