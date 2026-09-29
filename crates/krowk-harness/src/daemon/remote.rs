//! A daemon client that outlives its daemon: what the TUI holds for as long
//! as it is open. A daemon can go while a TUI idles — `krowk host stop
//! --force`, `krowk host enable` handing over to the service, an upgrade's
//! old daemon killed, a crash — and the TUI's next command then reaches the
//! next one, started when none runs (`daemon::ensure`), rather than failing
//! with `host_gone` until krowk is restarted. A turn under way when its
//! connection goes — the daemon let it go for falling past what it can
//! catch up, or the daemon itself went — is re-followed on the next one
//! from where its stream stood: the log after its last logged event, the
//! running turn after its last `seq` (R-LAG-4). A turn its daemon took with
//! it is caught up from the log, and says it is no longer running.
//!
//! The frames of followed sessions arriving between turns (`watch`) come
//! through one channel of this client's, whichever connection they came
//! on.

use super::client::{Client, Cut};
use super::Spawn;
use crate::engine::EngineError;
use crate::protocol::{Command, HostStatus, RunResult, StreamLine};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::{broadcast, mpsc};

/// How many times one stream is re-followed after its connection went.
const RESUMES: usize = 3;

pub struct Remote {
    env: Box<dyn Fn(&str) -> String>,
    cwd: PathBuf,
    version: String,
    answers: bool,
    spawn: Box<Spawn<'static>>,
    client: Mutex<Arc<Client>>,
    watch: broadcast::Sender<StreamLine>,
    /// Said to the person once: a reconnection.
    note: Mutex<Option<String>>,
}

impl Remote {
    /// Connects to the daemon running, or starts one.
    pub async fn connect(env: Box<dyn Fn(&str) -> String>, cwd: PathBuf, version: String, answers: bool, spawn: Box<Spawn<'static>>) -> Result<Remote, EngineError> {
        let client = Arc::new(super::ensure(&*env, &cwd, &version, answers, &*spawn).await?);
        let watch = broadcast::channel(256).0;
        forward(&client, &watch);
        Ok(Remote { env, cwd, version, answers, spawn, client: Mutex::new(client), watch, note: Mutex::new(None) })
    }

    fn current(&self) -> Arc<Client> {
        self.client.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The client to send the next command on: the one there is, or — its
    /// daemon gone — one to the daemon that runs now.
    async fn fresh(&self) -> Result<Arc<Client>, EngineError> {
        let c = self.current();
        if !c.closed() {
            return Ok(c);
        }
        let next = Arc::new(super::ensure(&*self.env, &self.cwd, &self.version, self.answers, &*self.spawn).await?);
        forward(&next, &self.watch);
        *self.note.lock().unwrap_or_else(|e| e.into_inner()) = Some(format!("the host daemon went away; reconnected to the one running now (pid {})", next.pid));
        *self.client.lock().unwrap_or_else(|e| e.into_inner()) = next.clone();
        Ok(next)
    }

    /// What there is to tell the person about the connection, once.
    pub fn take_note(&self) -> Option<String> {
        self.note.lock().unwrap_or_else(|e| e.into_inner()).take()
    }

    pub fn pid(&self) -> u32 {
        self.current().pid
    }

    pub fn krowk_version(&self) -> String {
        self.current().krowk_version.clone()
    }

    pub async fn execute(&self, cmd: Command, out: mpsc::Sender<StreamLine>) -> Result<Option<RunResult>, EngineError> {
        let streams = matches!(cmd, Command::Prompt { .. } | Command::Continue { .. });
        let r = self.fresh().await?.execute_or_cut(cmd, out.clone()).await;
        self.carry_on(r, out, streams).await
    }

    pub async fn follow(&self, session_id: &str, after: Option<&str>, out: mpsc::Sender<StreamLine>) -> Result<Option<RunResult>, EngineError> {
        let c = self.fresh().await?;
        let from = super::client::Resume { session_id: session_id.to_string(), after_event_id: after.map(String::from), after_seq: 0, epoch: 0 };
        let r = c.resume(&from, out.clone()).await;
        self.carry_on(r, out, false).await
    }

    /// A stream cut with its connection, re-followed on the next from where
    /// it stood — a few times, not forever: a daemon that lets it go again
    /// and again says so.
    async fn carry_on(&self, mut r: Result<Option<RunResult>, Cut>, out: mpsc::Sender<StreamLine>, turn: bool) -> Result<Option<RunResult>, EngineError> {
        for _ in 0..RESUMES {
            let from = match r {
                Err(Cut::Dropped(from)) => from,
                other => return other.map_err(Cut::into_error),
            };
            let c = self.fresh().await?;
            r = match c.resume(&from, out.clone()).await {
                // Caught up, and the turn is not running there any more: its
                // daemon took it along, or it ended while the stream was cut
                // — its finished items are in what was caught up, its result
                // is not.
                Ok(None) if turn => Err(Cut::Failed(EngineError::new("host_gone", "the connection to the host daemon went during the turn; what it logged is caught up, and it is no longer running there"))),
                other => other,
            };
        }
        r.map_err(Cut::into_error)
    }

    pub async fn status(&self) -> Result<HostStatus, EngineError> {
        self.fresh().await?.status().await
    }

    pub fn reload_later(&self, changed: Option<String>, renamed: Option<(String, String)>) {
        self.current().reload_later(changed, renamed);
    }

    pub fn leave(&self, session_id: &str) {
        self.current().leave(session_id);
    }

    pub fn watch(&self) -> broadcast::Receiver<StreamLine> {
        self.watch.subscribe()
    }
}

/// `client`'s between-turns frames into `to`, for as long as it is open.
fn forward(client: &Client, to: &broadcast::Sender<StreamLine>) {
    let (mut rx, to) = (client.watch(), to.clone());
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(l) => {
                    let _ = to.send(l);
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => return,
            }
        }
    });
}
