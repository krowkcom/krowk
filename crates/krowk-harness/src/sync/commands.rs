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
//! on, or whose answer is on its way, is skipped until it is done. One that
//! does not open gets no answer (an answer would need a key the sender
//! proved it holds) and is said once in host.log; it is tried again once
//! the device list has been read again, since a device paired since, or a
//! user key rotated since, is not yet on the list this host holds. Those
//! are kept apart, `UNOPENED` at most, so bytes nobody can open never push
//! an answered id out.
//!
//! **Start.** On a thread of its own, so a slow start never holds the
//! refresh up, nor does an answer sent: the directory checked against what
//! this host offers, read again now; the model checked by the daemon
//! (`switchModel` with no session); then a background client of the daemon,
//! connected in that directory and answering approvals as a bridge's does,
//! sends the prompt and waits for the turn's `turn.started`, whose session
//! id is the answer. Automatic sync takes the session up at that first
//! turn, and the client stays until the session's bridge answers its
//! approvals (or `HOLD` has passed, or the turn is over): a request asked
//! with no answering client following is denied at once. Then it goes, and
//! the turn runs on in the daemon.

use super::announce::{Announcer, Options, FRESH_EVERY};
use super::auto::Config;
use crate::daemon::client::Client as Daemon;
use crate::protocol::{Command, LogBody, PermissionMode, StreamLine};
use krowk_api::sync::DeliveredCommand;
use krowk_client::e2e::DeviceId;
use krowk_client::host_command::{self, HostAnswer, HostCommand};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How many answered command ids are kept, as the bridge keeps its own.
pub const KEEP: usize = 4096;

/// How many ids that did not open are kept, apart from the answered ones.
const UNOPENED: usize = 256;

/// How long a started session's client stays for its bridge to come and
/// answer the turn's approvals.
const HOLD: Duration = Duration::from_secs(60);

/// How often a held client asks the daemon whether the bridge is there.
const HOLD_EVERY: Duration = Duration::from_millis(200);

/// What is kept of a command answered.
enum Kept {
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
    /// Ids answered, oldest first, at most `KEEP`.
    order: VecDeque<String>,
    kept: HashMap<String, Kept>,
    /// Ids that did not open, and when they were last tried.
    unopened: HashMap<String, Instant>,
    /// Ids a thread is working on, or sending the answer to, now.
    working: HashSet<String>,
}

impl State {
    fn keep(&mut self, id: &str, k: Kept) {
        self.unopened.remove(id);
        if self.kept.insert(id.to_string(), k).is_none() {
            self.order.push_back(id.to_string());
        }
        while self.order.len() > KEEP {
            if let Some(old) = self.order.pop_front() {
                self.kept.remove(&old);
            }
        }
    }

    fn not_opened(&mut self, id: &str) {
        if self.unopened.len() >= UNOPENED
            && let Some(oldest) = self.unopened.iter().min_by_key(|(_, at)| **at).map(|(id, _)| id.clone())
        {
            self.unopened.remove(&oldest);
        }
        self.unopened.insert(id.to_string(), Instant::now());
    }

    /// The answer to `id` sealed by `a`, if it is not yet, and kept so.
    fn sealed(&mut self, id: &str, a: &Announcer) -> Option<Vec<u8>> {
        let sealed = match self.kept.get(id)? {
            Kept::Sealed(b) => return Some(b.clone()),
            Kept::Answer { from, answer } => match host_command::seal_answer(answer, id, &a.device, from, &a.keys, a.chain.generation()) {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("krowk: the answer to command {id} could not be sealed: {e}; it is tried again at the next refresh");
                    return None;
                }
            },
        };
        self.keep(id, Kept::Sealed(sealed.clone()));
        Some(sealed)
    }
}

impl Commands {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Each delivered command taken: answered again, skipped, refused, or
    /// started on a thread of its own. `host` is this device's name on the
    /// list, named in a refusal. Nothing here waits on the network: an
    /// answer goes from a thread of its own.
    pub(crate) fn take(me: &Arc<Commands>, c: &Arc<Config>, a: &Announcer, host: &str, delivered: Vec<DeliveredCommand>) {
        for d in delivered {
            // The registry's, so said escaped.
            let id = d.id.to_ascii_lowercase();
            let shown = id.escape_debug().to_string();
            {
                let mut s = me.lock();
                if s.working.contains(&id) || s.unopened.get(&id).is_some_and(|at| at.elapsed() < FRESH_EVERY) {
                    continue;
                }
                if s.kept.contains_key(&id) {
                    // Not landed before: sent again as it was.
                    if let Some(sealed) = s.sealed(&id, a) {
                        drop(s);
                        send(me, &id, sealed, a.api.clone());
                    }
                    continue;
                }
            }
            let opened = DeviceId::parse(&d.from).ok_or_else(|| format!("the registry names its sender {:?}, which is no device id", d.from)).and_then(|from| host_command::open_command(&d.sealed, &id, &from, &a.device, &a.keys, &a.chain).map(|o| (from, o)));
            match opened {
                Err(e) => {
                    let mut s = me.lock();
                    if !s.unopened.contains_key(&id) {
                        eprintln!("krowk: a command delivered to this machine ({shown}) did not open, and is not answered: {e}");
                    }
                    s.not_opened(&id);
                }
                Ok((from, Err(answer))) => {
                    let sealed = {
                        let mut s = me.lock();
                        s.keep(&id, Kept::Answer { from, answer });
                        s.sealed(&id, a)
                    };
                    if let Some(sealed) = sealed {
                        send(me, &id, sealed, a.api.clone());
                    }
                }
                Ok((from, Ok(command))) => {
                    me.lock().working.insert(id.clone());
                    let (me2, c2, id2, host) = (me.clone(), c.clone(), id.clone(), host.to_string());
                    let spawned = std::thread::Builder::new().name("krowk-host-command".into()).spawn(move || {
                        let told = Mutex::new(false);
                        let tell = |answer: HostAnswer| {
                            *told.lock().unwrap_or_else(|e| e.into_inner()) = true;
                            finish(&me2, &c2, &id2, from, answer);
                        };
                        let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&c2, &host, command, &tell)));
                        if ran.is_err() && !*told.lock().unwrap_or_else(|e| e.into_inner()) {
                            tell(HostAnswer::refused("host_failed", "the host's start panicked — see host.log on it, and send the start again"));
                        }
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
        HostAnswer::Refused { code, message } => eprintln!("krowk: device {from} asked this machine to start a session, refused: {} ({})", message.escape_debug(), code.escape_debug()),
    }
    let a = c.announce.as_ref().and_then(|o| (o.keys)(false).ok());
    let sealed = {
        let mut s = me.lock();
        s.keep(id, Kept::Answer { from, answer });
        a.as_ref().and_then(|a| s.sealed(id, a))
    };
    // On a thread of its own already: sent from here, the id still worked on.
    if let (Some(sealed), Some(a)) = (sealed, a) {
        let _ = a.api.answer_host_command(id, &sealed);
    }
    me.lock().working.remove(id);
}

/// `sealed`, the answer to `id`, sent from a thread of its own, the id
/// skipped meanwhile. Not landed, it is sent again at the next delivery.
fn send(me: &Arc<Commands>, id: &str, sealed: Vec<u8>, api: Arc<krowk_api::Client>) {
    me.lock().working.insert(id.to_string());
    let (me2, id2) = (me.clone(), id.to_string());
    let spawned = std::thread::Builder::new().name("krowk-host-answer".into()).spawn(move || {
        let _ = api.answer_host_command(&id2, &sealed);
        me2.lock().working.remove(&id2);
    });
    if spawned.is_err() {
        me.lock().working.remove(id);
    }
}

/// The directories this host offers now: the trusted repositories, less
/// those the policy opts out.
pub(crate) fn offered(c: &Config, o: &Options, policy: &super::auto::Policy) -> Vec<String> {
    o.trusted.directories().into_iter().filter(|d| policy.opted_out(d, c.krowk_home.as_deref()).is_none()).map(|d| d.display().to_string()).collect()
}

/// A start, run: its answer told to `tell` once known, then held for the
/// bridge (`hold`).
fn run(c: &Config, host: &str, command: HostCommand, tell: &dyn Fn(HostAnswer)) {
    let Some(o) = c.announce.as_ref() else { return tell(HostAnswer::refused("host_failed", "this host takes no commands")) };
    let HostCommand::Start { cwd, model, prompt } = command;
    let policy = match (c.policy)() {
        Ok(p) if p.on => p,
        Ok(_) => return tell(HostAnswer::refused("sync_off", format!("automatic sync is off on {host} — turn it on there to start sessions from your other devices"))),
        Err(e) => return tell(HostAnswer::refused("sync_off", format!("{host} cannot read its sync settings: {e}"))),
    };
    let offered = offered(c, o, &policy);
    if !offered.contains(&cwd) {
        let pick = if offered.is_empty() { "it offers none yet".to_string() } else { format!("pick one of: {}", offered.join(", ")) };
        return tell(HostAnswer::refused("directory_not_offered", format!("{host} does not offer {cwd}: on {host}, run krowk there once and trust it, or {pick}")));
    }
    // The host's own default for the directory, never what the command
    // carried, and never a mode that asks nobody.
    let mode = match (o.mode_for)(Path::new(&cwd)) {
        Ok(m) if m.asks_nothing() => PermissionMode::Default,
        Ok(m) => m,
        Err((code, message)) => return tell(HostAnswer::refused(&code, message)),
    };
    let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else {
        return tell(HostAnswer::refused("host_failed", format!("{host} has no async runtime for the start — try again")));
    };
    rt.block_on(start(c, &cwd, model, prompt, mode, tell));
}

async fn start(c: &Config, cwd: &str, model: crate::protocol::ModelRef, prompt: String, mode: PermissionMode, tell: &dyn Fn(HostAnswer)) {
    let refused = |e: crate::engine::EngineError| HostAnswer::refused(&e.code, e.message);
    // Background, so it never keeps the daemon up; answering approvals, as
    // a bridge's client does, so the turn's are asked of its followers.
    let d = match Daemon::connect_as(&c.socket, Path::new(cwd), &c.version, true, true).await {
        Ok(d) => d,
        Err(crate::daemon::client::ConnectError::Failed(e)) => return tell(refused(e)),
        Err(crate::daemon::client::ConnectError::Absent) => return tell(HostAnswer::refused("host_unavailable", "the host daemon is not running — it starts again with the next krowk there")),
    };
    let (tx, _rx) = tokio::sync::mpsc::channel(1);
    if let Err(e) = d.execute(Command::SwitchModel { session_id: None, model: model.clone() }, tx).await {
        return tell(refused(e));
    }
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    let prompt = Command::Prompt { session_id: None, text: prompt, images: Vec::new(), model: Some(model), permission_mode: mode, toolset: None, effort: None, budget: None };
    let run = d.execute(prompt, tx);
    tokio::pin!(run);
    let started = |l: &StreamLine| match l {
        StreamLine::Log(ev) if matches!(ev.body, LogBody::TurnStarted { .. }) => Some(ev.session_id.clone()),
        _ => None,
    };
    // No deadline of its own: a refusal said while a turn may yet start
    // would have the sender start it again. A daemon that goes ends it.
    let session = loop {
        tokio::select! {
            // The turn's lines first: they all arrive before its end.
            biased;
            line = rx.recv() => match line {
                Some(l) => if let Some(s) = started(&l) { break s },
                None => return tell(HostAnswer::refused("host_gone", "the host daemon closed the connection before the turn started — `krowk host status` there says whether it is up")),
            },
            done = &mut run => {
                if let Some(s) = std::iter::from_fn(|| rx.try_recv().ok()).find_map(|l| started(&l)) {
                    return tell(HostAnswer::Started { session_id: s });
                }
                return tell(match done {
                    Err(e) => refused(e),
                    Ok(_) => HostAnswer::refused("not_started", "the prompt ended before a turn started — send it again"),
                });
            }
        }
    };
    // Told, and so sent, while this client still answers the turn's
    // approvals: sending blocks this thread's runtime, and the daemon
    // queues this client's lines meanwhile.
    tell(HostAnswer::Started { session_id: session.clone() });
    hold(&d, &session, &mut rx, run).await;
}

/// Stays, following the session, until another client answers its
/// approvals — its bridge — or the turn is over, or `HOLD` has passed.
async fn hold<F: std::future::Future>(d: &Daemon, session: &str, rx: &mut tokio::sync::mpsc::Receiver<StreamLine>, run: std::pin::Pin<&mut F>) {
    let until = Instant::now() + HOLD;
    let mut run = run;
    let (mut ended, mut closed) = (false, false);
    while !ended && Instant::now() < until {
        let tick = tokio::time::sleep(HOLD_EVERY);
        tokio::pin!(tick);
        // Its lines drained meanwhile, so its stream never backs up.
        loop {
            tokio::select! {
                line = rx.recv(), if !closed => closed = line.is_none(),
                _ = &mut run, if !ended => ended = true,
                () = &mut tick => break,
            }
        }
        let Ok(Ok(status)) = tokio::time::timeout(HOLD_EVERY * 5, d.status()).await else { return };
        match status.sessions.iter().find(|s| s.session_id == session) {
            Some(s) if s.running && s.answering < 2 => {}
            _ => return,
        }
    }
}
