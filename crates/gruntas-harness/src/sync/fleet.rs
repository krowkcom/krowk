//! The registry's side of placement (`placement::Fleet`): this machine's
//! capacity, the other machines' announcements, the job's storage, the
//! `run` command and its answer, and the child's log and result, as a
//! supervisor on this device reaches them. The rules are `placement`'s;
//! this is what they are run on.

use super::announce::{self, Announcer};
use super::handoff::{self, Bundle};
use super::placement::{Fleet, JobStore, LocalRun, TaskSpec};
use gruntas_client::device_chain::Kind;
use gruntas_client::e2e::{self, DeviceId, SessionKey, SessionKeys, SigningKey};
use gruntas_client::host_command::{self, HostAnswer, HostCommand, JobRef, ModelChoice, RunTask};
use gruntas_client::presence::{self, AnnouncedInstance, Announcement};
use gruntas_client::protocol::ModelRef;
use gruntas_client::session_record::{self, Signer};
use gruntas_client::sync::store;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How a model a call only half named is made whole: on this machine, from
/// its config.
pub type Resolver = Box<dyn Fn(&ModelChoice) -> Result<ModelRef, String> + Send + Sync>;

/// Runs a child in a local worktree (`Here::Worktree`).
pub type RunLocal = Box<dyn Fn(&TaskSpec, &ModelRef) -> Result<LocalRun, String> + Send + Sync>;

/// Stops a child through its session's interrupt, as a follower does
/// (needs the relay, which the caller knows).
pub type Stop = Box<dyn Fn(&str) -> Result<(), String> + Send + Sync>;

/// How long a short-lived job session's lease lasts: past a job's storage,
/// which is all it is held for.
const JOB_LEASE: u64 = 120;
/// A fresh command id: a random UUID, version 4, which is all the registry
/// takes for one (`krowk sync new` makes its the same way).
fn command_id() -> String {
    let mut b = e2e::random::<16>();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = e2e::hex(&b);
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}

/// How often a `run`'s answer is asked for, and how often a send is tried
/// again when the registry does not answer.
const ANSWER_EVERY: Duration = Duration::from_millis(500);
const SEND_RETRIES: u32 = 3;

pub struct Registry {
    announcer: Announcer,
    builds: crate::slots::Pool,
    agents: crate::slots::Pool,
    worktrees: Option<PathBuf>,
    resolve: Resolver,
    local: Option<RunLocal>,
    stop: Option<Stop>,
    /// This device's signing key: a job's session record is signed with it,
    /// as every session's is, or the registry refuses to make it.
    signing: Option<SigningKey>,
    /// The keys of each child session read, opened once.
    keys: Mutex<HashMap<String, SessionKeys>>,
}

impl Registry {
    /// The default pools of this machine and the models a call names whole
    /// (or by a model id alone, as a subagent's `--model` is read): what a
    /// caller with no config of its own gets.
    pub fn new(announcer: Announcer) -> Registry {
        let env = |k: &str| std::env::var(k).unwrap_or_default();
        let registry = crate::instances::Registry::resolve(&crate::instances::InstancesConfig::default(), &env);
        let builds = registry.builds.pool.clone().unwrap_or_else(|| crate::slots::Pool::new(crate::slots::runtime_dir(&env), crate::builds::POOL, 1));
        let agents = registry.agents.clone().unwrap_or_else(|| crate::slots::Pool::new(crate::slots::runtime_dir(&env), crate::children::AGENT_SLOTS, 1));
        let resolve: Resolver = Box::new(move |c| match (&c.instance, &c.model) {
            (Some(i), Some(m)) => Ok(ModelRef { instance: i.clone(), model: m.clone() }),
            (None, Some(m)) => registry.parse_model(m),
            (Some(i), None) => {
                let kind = registry.get(i)?.kind;
                crate::instances::default_model_of(kind).map(|m| ModelRef { instance: i.clone(), model: m.to_string() }).ok_or_else(|| format!("name a model for {i}: <instance>/<model>"))
            }
            (None, None) => Err("no model was named, and none is chosen for a task from here".to_string()),
        });
        Registry { announcer, builds, agents, worktrees: gruntas_api::home::worktrees_root(&env), resolve, local: None, stop: None, signing: None, keys: Mutex::default() }
    }

    /// The pools this machine's capacity is read from, when the caller's
    /// config sizes them.
    pub fn with_pools(mut self, builds: crate::slots::Pool, agents: crate::slots::Pool) -> Registry {
        self.builds = builds;
        self.agents = agents;
        self
    }

    pub fn with_resolver(mut self, resolve: Resolver) -> Registry {
        self.resolve = resolve;
        self
    }

    pub fn with_local(mut self, run: RunLocal) -> Registry {
        self.local = Some(run);
        self
    }

    pub fn with_stop(mut self, stop: Stop) -> Registry {
        self.stop = Some(stop);
        self
    }

    pub fn with_signing(mut self, signing: SigningKey) -> Registry {
        self.signing = Some(signing);
        self
    }

    pub fn with_worktrees(mut self, root: PathBuf) -> Registry {
        self.worktrees = Some(root);
        self
    }

    fn me(&self) -> String {
        self.announcer.device.to_string()
    }

    fn named(&self) -> (String, String) {
        let id = self.announcer.device;
        self.announcer.chain.devices().iter().find(|d| d.kind == Kind::Device && d.id() == id).map_or_else(|| ("this machine".to_string(), std::env::consts::OS.to_string()), |d| (d.name.clone(), d.os.clone()))
    }

    fn session_keys(&self, child: &str) -> Result<SessionKeys, String> {
        if let Some(k) = self.keys.lock().unwrap_or_else(|e| e.into_inner()).get(child) {
            return Ok(k.clone());
        }
        let s = self.announcer.api.show_sync_session(child).map_err(|e| e.to_string())?;
        let keys = store::open_session_key(&s, child, &self.announcer.keys, &self.announcer.chain, Signer::EverHeld)?;
        self.keys.lock().unwrap_or_else(|e| e.into_inner()).insert(child.to_string(), keys.clone());
        Ok(keys)
    }
}

impl Fleet for Registry {
    fn device(&self) -> String {
        self.me()
    }

    fn resolve_model(&self, asked: &ModelChoice) -> Result<ModelRef, String> {
        (self.resolve)(asked)
    }

    fn here(&self, model: &ModelRef) -> super::placement::Candidate {
        let (name, os) = self.named();
        let instance = AnnouncedInstance { name: model.instance.clone(), kind: String::new(), ready: true, state: "ready".into(), fix: None, models: vec![model.model.clone()], limit: None };
        let announcement = Announcement {
            device: self.me(),
            name,
            os,
            version: "dev".into(),
            online_since_ms: 0,
            instances: vec![instance],
            directories: Vec::new(),
            sessions: Vec::new(),
            capacity: Some(announce::capacity(&self.builds, &self.agents)),
            repositories: Vec::new(),
            accepting: true,
        };
        super::placement::Candidate { announcement, online: true, here: true, placed_recently: 0 }
    }

    fn others(&self) -> Result<Vec<super::placement::Candidate>, String> {
        let a = &self.announcer;
        let me = self.me();
        let presences = match a.api.list_presences() {
            Ok(p) => p,
            Err(e) if e.status == 404 => Vec::new(),
            Err(e) => return Err(e.to_string()),
        };
        Ok(presences
            .into_iter()
            .filter(|p| !p.device.eq_ignore_ascii_case(&me))
            .filter_map(|p| {
                let from = DeviceId::parse(&p.device)?;
                let announcement = presence::open(&p.sealed, &from, &a.keys, &a.chain).ok()?;
                Some(super::placement::Candidate { announcement, online: p.online, here: false, placed_recently: 0 })
            })
            .collect())
    }

    fn repository(&self, cwd: &Path) -> Option<(String, String)> {
        let out = gruntas_api::git::query(cwd).and_then(|mut c| c.args(["remote", "get-url", "origin"]).output()).ok().filter(|o| o.status.success())?;
        let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
        presence::normalise_origin(&url).map(|origin| (origin, url))
    }

    fn open_store(&self, given: Option<&JobStore>) -> Result<JobStore, String> {
        if let Some(s) = given {
            return Ok(s.clone());
        }
        let a = &self.announcer;
        let signing = self.signing.as_ref().ok_or("this machine has no signing key to register the job's session with")?;
        let id = gruntas_store::new_id();
        let raw = crate::daemon::ws::uuid(&id);
        let keys = SessionKeys::from(SessionKey::generate());
        let sealed_key = e2e::seal_session_keys(&keys, &raw, &a.keys, a.chain.generation()).map_err(|e| e.to_string())?;
        // Signed as a bridge signs a session it makes: the registry makes
        // none without its record, and a reader opens none without it.
        let signature = session_record::sign(&raw, &sealed_key, session_record::SEAL_USER, a.keys.newest(), signing).map_err(|e| e.to_string())?;
        let index = store::Index { title: "task".into(), ..store::Index::default() };
        let sealed_index = e2e::hex(&e2e::seal_session_index(keys.current(), &raw, &serde_json::to_vec(&index).map_err(|e| e.to_string())?));
        a.api.put_sync_session(&id, &e2e::hex(&sealed_key), Some((&e2e::hex(&signature), &self.me())), Some(&sealed_index), None).map_err(|e| format!("the job could not be registered ({}): {}", e.code(), e.fix()))?;
        let env = |k: &str| std::env::var(k).unwrap_or_default();
        let lease = a.api.acquire_lease(&id, &self.me(), JOB_LEASE, gruntas_api::relay_env(&a.api.base_url, &env)).map_err(|e| format!("the job could not be stored ({}): {}", e.code(), e.fix()))?;
        Ok(JobStore { session: id, lease_token: lease.token })
    }

    fn close_store(&self, store: &JobStore) {
        let _ = self.announcer.api.release_lease(&store.session, &store.lease_token);
    }

    fn pack_job(&self, cwd: &Path, session: &str, prompt: &str) -> Result<Bundle, String> {
        handoff::pack_job(cwd, session, prompt)
    }

    fn put_job(&self, store: &JobStore, job: &Bundle) -> Result<JobRef, String> {
        let key: [u8; 32] = e2e::random();
        let sealed = e2e::seal_transport(&key, &crate::daemon::ws::uuid(&store.session), &job.encode());
        let t = self.announcer.api.put_transport(&store.session, &sealed, &store.lease_token).map_err(|e| format!("the job could not be stored ({}): {}", e.code(), e.fix()))?;
        Ok(JobRef { session: store.session.clone(), transport: t.slug, key: handoff::key_hex(&key) })
    }

    fn spend_job(&self, job: &JobRef) {
        let _ = self.announcer.api.spend_transport(&job.transport);
    }

    fn send_run(&self, to: &Announcement, task: &RunTask, wait: Duration) -> Result<Option<HostAnswer>, String> {
        let a = &self.announcer;
        let target = DeviceId::parse(&to.device).ok_or_else(|| format!("{} announces no device id this krowk reads", to.name))?;
        let id = command_id();
        let sealed = host_command::seal_command(&HostCommand::Run(task.clone()), &id, &a.device, &target, &a.keys, a.chain.generation())?;
        let mut tries = 0;
        loop {
            match a.api.send_host_command(&target.to_string(), &id, &sealed) {
                Ok(_) => break,
                Err(e) if e.status == 0 && tries < SEND_RETRIES => {
                    tries += 1;
                    std::thread::sleep(ANSWER_EVERY * tries);
                }
                Err(e) => return Err(format!("it could not be reached ({}): {}", e.code(), e.fix())),
            }
        }
        let deadline = Instant::now() + wait;
        loop {
            match a.api.host_command(&target.to_string(), &id) {
                Ok(s) if s.answered => {
                    let sealed = s.answer.ok_or_else(|| format!("{} answered with nothing this krowk reads", to.name))?;
                    return host_command::open_answer(&sealed, &id, &target, &a.device, &a.keys, &a.chain).map(Some);
                }
                _ => {}
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            std::thread::sleep(ANSWER_EVERY);
        }
    }

    fn watch(&self, child: &str) -> Result<Vec<Value>, String> {
        let keys = self.session_keys(child)?;
        store::read_all(&self.announcer.api, &keys, child, None).map(|(events, _, _)| events)
    }

    fn seen(&self, device: &str) -> Option<i64> {
        let presences = self.announcer.api.list_presences().ok()?;
        let p = presences.into_iter().find(|p| p.device.eq_ignore_ascii_case(device))?;
        crate::http::rfc3339_ms(&p.updated_at)
    }

    fn read_result(&self, child: &str, transport: &str, key: &str) -> Result<Bundle, String> {
        let key = handoff::key_from_hex(key).ok_or("the result's key is not one krowk reads")?;
        let sealed = self.announcer.api.read_transport(transport).map_err(|e| format!("{} ({})", e.fix(), e.code()))?;
        let plain = e2e::open_transport(&sealed, &crate::daemon::ws::uuid(child), &key).map_err(|e| e.to_string())?;
        Bundle::decode(&plain, child)
    }

    fn land(&self, job: &Bundle, result: &Bundle, to: &Path) -> Result<(crate::worktree::Worktree, crate::worktree::apply::Applied), String> {
        let root = self.worktrees.as_deref().ok_or("krowk found no home directory to make worktrees under: set HOME or XDG_DATA_HOME")?;
        handoff::apply_result(job, result, to, root)
    }

    fn stop(&self, child: &str) -> Result<(), String> {
        match &self.stop {
            Some(stop) => stop(child),
            None => Err("this machine has no way to stop a child".into()),
        }
    }

    fn run_local(&self, spec: &TaskSpec, model: &ModelRef) -> Result<LocalRun, String> {
        match &self.local {
            Some(run) => run(spec, model),
            None => Err("this machine has no way to run a child of its own here".into()),
        }
    }
}
