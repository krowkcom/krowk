//! A new machine's pairing request, answered by this host (R-EASE-5,
//! R-EASE-6, D6; canon, engineering/devices.md → Adding a device). A machine
//! the person signed in on, with no device keys, asks the registry for a
//! pairing; each host reads the request in its presence refresh
//! (`announce`), and one of them opens the pairing and shows the code. The
//! registry's one-pairing-per-person rule decides which: the others'
//! `open_pairing` is `pairing_open`, and they leave it be.
//!
//! **The roles are today's** (`krowk devices add`, `krowk sync join`): this
//! host is A. It makes the code, opens the pairing, and runs A's side of
//! the SPAKE2 exchange, never B's. A request carries no keys and opens
//! nothing: its name and OS are untrusted hints that are not even shown.
//! What the person is asked, as `devices add` asks it, is whether to add
//! the name and OS the pairing itself carries, which are known only once
//! the typed code's confirmation checked — a wrong code ends the pairing
//! and asks nothing. Only a listed device opens one: the host's device list
//! is read again, current and verified, before anything is opened.
//!
//! **The answer is a person's.** The code and the question are shown by the
//! terminal UI and `krowk machines`, which ask the daemon for the offer and
//! send the answer back over its socket (`ClientFrame::PairingOffer`,
//! `PairingAnswer`); nothing here answers for them. The question is asked
//! once, and an answer sent when none is asked is refused.
//!
//! **Once, and one at a time.** A request id is taken up once, however
//! many refreshes carry it. One pairing is open at a time (the person has
//! one at the registry); a second request is taken up after the first is
//! over, which it waits for in the refreshes that carry it. A pairing that
//! fails — a wrong code, a no, a lapse — is over: the new machine asks
//! again.
//!
//! As `devices add` does, once the reply, which carries the user key, has
//! gone out, the new machine's `add` entry is posted whether or not it
//! acknowledges, so a machine that may hold the key is on the list, where
//! it can be removed.

use super::announce::words;
use krowk_api::sync::{DeliveredPairingRequest, ListPost, PairingStep};
use krowk_api::{Client, Error};
use krowk_client::device_chain::{confusable_names, Change, Kind, SignedEntry, Subject};
use krowk_client::e2e::{self, DeviceId, SigningKey};
use krowk_client::pairing::{PairA, PeerKind};
use krowk_client::pairing_join::Field;
use krowk_client::pairing_payload;
use krowk_client::protocol::{OfferStage, PairingOffer};
use krowk_client::user_key::UserKeys;
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// How many request ids taken up are remembered.
const KEEP: usize = 256;

/// How long a finished offer (`added`, `failed`) is still told to a client
/// that asks.
const RESULT_KEPT: Duration = Duration::from_secs(60);

/// What one introduction is made with: the person's keys, the verified and
/// current device list, this device, its signing key and its signed client.
pub struct Introducer {
    pub api: Arc<Client>,
    pub keys: UserKeys,
    pub chain: krowk_client::device_chain::Chain,
    /// The list's entries as served, which `chain` was verified from.
    pub entries: Vec<SignedEntry>,
    pub device: DeviceId,
    pub signing: SigningKey,
}

/// Builds an `Introducer`, the list brought up to date from the registry
/// first. Blocking. Errors are `(code, message)`.
pub type Build = Box<dyn Fn() -> Result<Introducer, (String, String)> + Send + Sync>;

/// How often the pairing is read, and how long this host waits in all: the
/// registry's ten minutes. A test makes both short.
#[derive(Clone, Copy)]
pub struct Timing {
    pub poll: Duration,
    pub wait: Duration,
}

impl Default for Timing {
    fn default() -> Timing {
        Timing { poll: Duration::from_millis(500), wait: Duration::from_secs(10 * 60) }
    }
}

/// The pairings this host opens for new machines, shared by its publisher
/// (which delivers the requests) and its daemon (which tells clients the
/// offer and takes the answer).
pub struct Introductions {
    build: Build,
    timing: Timing,
    state: Mutex<State>,
    changed: Condvar,
}

#[derive(Default)]
struct State {
    /// Request ids taken up, oldest first.
    handled: VecDeque<String>,
    busy: bool,
    offer: Option<PairingOffer>,
    /// When a finished offer stops being told.
    over: Option<Instant>,
    answer: Option<bool>,
}

/// How a run ended.
enum Over {
    Added(String, String),
    Failed(String),
    /// Another host has the pairing open: nothing was made here.
    Elsewhere,
}

impl Introductions {
    pub fn new(build: Build) -> Arc<Introductions> {
        Introductions::with_timing(build, Timing::default())
    }

    pub fn with_timing(build: Build, timing: Timing) -> Arc<Introductions> {
        Arc::new(Introductions { build, timing, state: Mutex::default(), changed: Condvar::new() })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The requests a presence refresh carried: the first not taken up
    /// starts a pairing on a thread of its own, unless one is under way.
    pub fn deliver(self: &Arc<Self>, requests: &[DeliveredPairingRequest]) {
        let id = {
            let mut s = self.lock();
            if s.busy {
                return;
            }
            let Some(r) = requests.iter().find(|r| !r.id.is_empty() && !s.handled.contains(&r.id)) else { return };
            s.busy = true;
            s.offer = None;
            s.over = None;
            s.answer = None;
            r.id.clone()
        };
        let this = self.clone();
        let spawned = std::thread::Builder::new().name("krowk-introduce".into()).spawn(move || this.run(&id));
        if let Err(e) = spawned {
            eprintln!("krowk: a new machine asked for a pairing, and no thread could answer it: {e}");
            self.lock().busy = false;
        }
    }

    /// The offer to tell a client: the code, or the question, or how it
    /// ended for a minute.
    pub fn offer(&self) -> Option<PairingOffer> {
        let mut s = self.lock();
        if s.over.is_some_and(|at| at.elapsed() > RESULT_KEPT) {
            s.offer = None;
            s.over = None;
        }
        s.offer.clone()
    }

    /// The person's answer to the question about `pairing`. False when none
    /// is asked, or it is about another.
    pub fn answer(&self, pairing: &str, yes: bool) -> bool {
        let mut s = self.lock();
        let asked = s.offer.as_ref().is_some_and(|o| o.stage == OfferStage::Confirm && o.pairing == pairing) && s.answer.is_none();
        if asked {
            s.answer = Some(yes);
            self.changed.notify_all();
        }
        asked
    }

    fn set(&self, offer: PairingOffer) {
        self.lock().offer = Some(offer);
    }

    fn run(&self, id: &str) {
        let over = self.drive();
        let mut s = self.lock();
        s.busy = false;
        s.offer = match over {
            // Nothing was made: the request is delivered again until a
            // pairing opens for it or it lapses.
            Over::Elsewhere => None,
            Over::Added(name, os) => Some(PairingOffer { pairing: String::new(), code: String::new(), stage: OfferStage::Added, note: format!("Added '{name}' ({os})."), name, os }),
            Over::Failed(note) => Some(PairingOffer { pairing: String::new(), code: String::new(), stage: OfferStage::Failed, name: String::new(), os: String::new(), note }),
        };
        if s.offer.is_some() {
            // Taken up, whatever came of it.
            s.over = Some(Instant::now());
            s.handled.push_back(id.to_string());
            while s.handled.len() > KEEP {
                s.handled.pop_front();
            }
        }
    }

    /// A's whole side, as `krowk devices add` runs it.
    fn drive(&self) -> Over {
        let failed = |why: String| Over::Failed(why);
        let i = match (self.build)() {
            Ok(i) => i,
            Err((code, message)) => return failed(format!("this machine could not open a pairing: {message} ({code})")),
        };
        // Only a listed device opens a pairing.
        if !i.chain.devices().iter().any(|d| d.id() == i.device && d.kind == Kind::Device) {
            return failed("this machine is not on your device list, so it cannot add another".into());
        }
        let user = match i.api.verify_key() {
            Ok(k) => k.user_id,
            Err(e) => return failed(format!("this machine could not read your account: {}", words(&e))),
        };
        let a = match PairA::new(PeerKind::SamePersonDevice, user, i.device) {
            Ok(a) => a,
            Err(e) => return failed(format!("the pairing could not start: {}", e.0)),
        };
        let opened = match i.api.open_pairing() {
            Ok(p) => p,
            Err(e) if e.code() == "pairing_open" => return Over::Elsewhere,
            Err(e) => return failed(format!("the pairing could not be opened: {}", words(&e))),
        };
        self.set(PairingOffer { pairing: opened.id.clone(), code: a.code().to_string(), stage: OfferStage::Waiting, name: String::new(), os: String::new(), note: String::new() });
        // Up to the reply, anything that goes wrong ends the pairing on the
        // registry too, so the new machine hears it at once, and adds nothing.
        let sealed = match self.seal_reply(&i, &opened.id, a) {
            Ok(s) => s,
            Err(why) => {
                let _ = i.api.end_pairing();
                return failed(why);
            }
        };
        // The reply went out with the user key in it: from here on the entry
        // is posted whatever the new machine does, so one that may hold the
        // key is listed.
        let acked = self.wait_for(&i.api, &opened.id, Field::JoinerAck).and_then(|ack| sealed.await_ack.receive_ack(&ack).map_err(|_| "the new machine's acknowledgement did not check".to_string()));
        if acked.is_err() {
            let _ = i.api.end_pairing();
        }
        let posted = post_add(&i.api, &sealed.batch);
        let (name, os) = (sealed.name, sealed.os);
        match (acked, posted) {
            (Ok(_), Ok(())) => Over::Added(name, os),
            (Err(_), Ok(())) => failed(format!("'{name}' was sent your key but never confirmed it. It is on your devices so you can take it off and rotate your key away from it: krowk devices remove '{name}'")),
            (_, Err(e)) => failed(format!("'{name}' was sent your key, but it could not be added to your devices ({}) — it may hold your key without being listed. Check krowk devices list; rotate your key with the next krowk devices remove", e.code())),
        }
    }

    /// From the hello to the reply: the new machine's confirmation checked
    /// before the person is asked anything, then the reply sealed and sent.
    fn seal_reply(&self, i: &Introducer, id: &str, a: PairA) -> Result<Sealed, String> {
        let ended = |e: Option<Error>| format!("the pairing ended before the new machine was added{} — nothing was added", e.map(|e| format!(" ({})", e.code())).unwrap_or_default());
        let hello = self.wait_for(&i.api, id, Field::JoinerMessage)?;
        let (await_confirm, spake) = a.receive_hello(&hello).map_err(|_| ended(None))?;
        a_step(&i.api, id, PairingStep::Answer, Field::InitiatorMessage, &spake).map_err(|e| ended(Some(e)))?;
        let confirmation = self.wait_for(&i.api, id, Field::JoinerConfirmation)?;
        // A wrong code fails here: the pairing ends, and the person is asked
        // nothing.
        let confirmed = await_confirm.receive_confirm(&confirmation).map_err(|_| "the new machine's confirmation did not check — the code was wrong, or someone else tried it; nothing was added".to_string())?;
        let new = confirmed.device().clone();
        let (name, os) = (printable(&new.name), printable(&new.os));
        // Before the person is asked: a name that reads like a listed
        // device's is the new machine's to change.
        if let Some(d) = i.chain.devices().iter().find(|d| confusable_names(&d.name, &new.name)) {
            return Err(format!("the new machine calls itself '{name}', which reads like '{}' already on your devices — nothing was added; on the new machine run krowk sync join --name <another name>", printable(&d.name)));
        }
        self.ask(&i.api, id, &name, &os)?;
        let subject = Subject { kind: Kind::Device, name: new.name.clone(), os: new.os.clone(), device: new.device, signing: new.signing };
        let (next, batch) = i.chain.batch(i.keys.newest(), vec![Change::Add(subject)], i.device, &i.signing, now()).map_err(|e| e.0)?;
        let wrapped = batch.wraps.iter().find(|(d, _)| *d == new.id()).map(|(_, w)| w.clone()).ok_or("the new machine's entry carried no wrap for it")?;
        // The entries the verified chain was built from, with the new one.
        let mut all = i.entries.clone();
        all.extend(batch.entries.iter().cloned());
        let bytes = pairing_payload::seal(&all, &i.keys, next.generation(), &wrapped, &i.signing.public());
        let (await_ack, reply) = confirmed.approve(&bytes).map_err(|_| ended(None))?;
        send_reply(&i.api, id, &reply).map_err(|e| ended(Some(e)))?;
        Ok(Sealed { batch, await_ack, name, os })
    }

    /// The question, asked once: told to clients, and answered by one of
    /// them. The new machine is waited for meanwhile: a pairing that ended
    /// is no longer worth a yes.
    fn ask(&self, api: &Client, id: &str, name: &str, os: &str) -> Result<(), String> {
        let deadline = Instant::now() + self.timing.wait;
        {
            let mut s = self.lock();
            s.answer = None;
            s.offer = Some(PairingOffer { pairing: id.into(), code: String::new(), stage: OfferStage::Confirm, name: name.into(), os: os.into(), note: String::new() });
        }
        loop {
            let mut s = self.lock();
            if let Some(yes) = s.answer.take() {
                return if yes { Ok(()) } else { Err("not added — the new machine was told, and nothing reached your device list".into()) };
            }
            if Instant::now() > deadline {
                return Err("no answer came in time — nothing was added".into());
            }
            let (guard, _) = self.changed.wait_timeout(s, self.timing.poll).unwrap_or_else(|e| e.into_inner());
            drop(guard);
            match api.show_pairing(id) {
                Ok(p) if p.state != "dead" => {}
                Err(e) if e.status == 0 => {}
                _ => return Err("the pairing ended before you answered — nothing was added".into()),
            }
        }
    }

    /// The pairing read until `field` has arrived. A pairing that ended, or
    /// the window closing, is the failure. This side holds no guess, so a
    /// read the network lost is only a read, asked again.
    fn wait_for(&self, api: &Client, id: &str, field: Field) -> Result<Vec<u8>, String> {
        let deadline = Instant::now() + self.timing.wait;
        let ended = |e: Option<Error>| format!("the pairing ended before the new machine was added{} — nothing was added", e.map(|e| format!(" ({})", e.code())).unwrap_or_default());
        loop {
            let p = match api.show_pairing(id) {
                Ok(p) => p,
                Err(e) if e.status == 0 && Instant::now() <= deadline => {
                    std::thread::sleep(self.timing.poll);
                    continue;
                }
                Err(e) => return Err(ended(Some(e))),
            };
            if let Some(b) = field.blob(&p) {
                return Ok(b);
            }
            if p.state == "dead" || Instant::now() > deadline {
                return Err(ended(None));
            }
            std::thread::sleep(self.timing.poll);
        }
    }
}

/// A's side once the reply is out.
struct Sealed {
    batch: krowk_client::device_chain::Batch,
    await_ack: krowk_client::pairing::AwaitAck,
    name: String,
    os: String,
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// `s` without control characters: it is printed on a terminal.
fn printable(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect::<String>().trim().to_string()
}

/// One step, sent once. When its answer is lost, the pairing is read back:
/// the step landed if it holds what was sent, and the pairing is over
/// otherwise — sending it again would be out of turn.
fn a_step(api: &Client, id: &str, step: PairingStep, field: Field, message: &[u8]) -> Result<(), Error> {
    match api.pairing_step(step, message) {
        Ok(_) => Ok(()),
        Err(e) if e.status == 0 => match api.show_pairing(id) {
            Ok(p) if field.blob(&p).as_deref() == Some(message) => Ok(()),
            _ => Err(e),
        },
        Err(e) => Err(e),
    }
}

/// The reply, sent once. It carries the user key, so when its answer is
/// lost and the read-back fails too, nobody can say it did not land: it is
/// taken as sent, and the ack and the post follow, which list a machine
/// that may hold the key. Only a refusal, or a read-back that shows it did
/// not land, ends the pairing with nothing added.
fn send_reply(api: &Client, id: &str, reply: &[u8]) -> Result<(), Error> {
    match api.pairing_step(PairingStep::Reply, reply) {
        Ok(_) => Ok(()),
        Err(e) if matches!(e.status, 0 | 502..=504) => match api.show_pairing(id) {
            Ok(p) if Field::SealedReply.blob(&p).as_deref() != Some(reply) => Err(e),
            _ => Ok(()),
        },
        Err(e) => Err(e),
    }
}

/// Posts the new device's entry and wrap, once. When the answer is lost the
/// list is read back: the post landed if the entry is on it.
fn post_add(api: &Client, batch: &krowk_client::device_chain::Batch) -> Result<(), Error> {
    let hex = e2e::hex;
    let post = ListPost {
        entries: batch.entries.iter().map(|e| (hex(&e.bytes), hex(&e.signatures_bytes()))).collect(),
        links: batch.links.iter().map(|l| hex(l)).collect(),
        wraps: batch.wraps.iter().map(|(d, w)| (d.to_string(), hex(w))).collect(),
        start_over: false,
    };
    match api.append_device_list(&post) {
        Ok(_) => Ok(()),
        Err(e) if e.status == 0 => {
            let first = hex(&batch.entries[0].bytes);
            match api.device_list_all() {
                Ok(list) if list.entries.iter().any(|x| x.entry == first) => Ok(()),
                _ => Err(e),
            }
        }
        Err(e) => Err(e),
    }
}
