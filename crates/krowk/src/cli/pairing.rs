//! Adding a device by a short code (R-E2E-3; canon, engineering/devices.md
//! → Adding a device): `krowk devices add` on a device already on the
//! person's list (A), `krowk sync join` on the new machine (B). The crypto
//! is `krowk_client::pairing` and `device_chain`; this is the command line
//! around them and the registry's mailbox between them.
//!
//! What A seals to B, once the person has said yes, is the payload below:
//! the whole signed chain with the new `add` entry on its end, the user key
//! wrapped to B, and A's signing key. B trusts it because the pairing
//! authenticated it, not because the registry served it, and keeps nothing
//! until the chain verifies from seq 0 and its last entry adds exactly B's
//! keys, signed by A. A posts the entry and the wrap only after B's ack.
//!
//! **After any failure on B, the code is gone.** A step that fails, a
//! refusal, a timeout, a dropped connection — B ends the pairing, tells the
//! person to get a new code from A, and never starts again with the typed
//! string: each start gives a registry posing as A one more guess. Nothing
//! is retried on B. A, whose side holds no guess, reads the pairing back
//! after a step whose answer was lost rather than sending the step again,
//! since a step sent twice is out of turn and ends the pairing.
//!
//! Both commands need a person at a terminal: an agent told to run `krowk
//! devices add` must not hand a stranger's machine every session. Debug
//! builds let the test suite stand in for the person
//! (`KROWK_TEST_UNATTENDED_DEVICE_APPROVAL=1`, and the code on stdin); a
//! release build has no such path.

use super::Ctx;
use super::sync::{device_name, keyed_client, keystore, printable};
use crate::output::Format;
use krowk_api::sync::{ListPost, Pairing, PairingStep};
use krowk_api::{fail, Client, Error};
use krowk_client::device_chain::{Action, Chain, Change, Entry, Kind, SignedEntry, Subject, Trust};
use krowk_client::e2e::{self, DeviceId, DeviceKey, DeviceSigner, SigningKey, SigningPublic};
use krowk_client::keystore::{Keystore, Pin};
use krowk_client::pairing::{self, Binding, NewDevice, PairA, PairB, PairingCode, PeerKind};
use krowk_client::user_key::{UserKey, UserKeys};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::time::{Duration, Instant};

/// How often either side reads the pairing while it waits for the other: a
/// person is walking between two machines.
const POLL: Duration = Duration::from_millis(500);
/// How long either side waits for the other: the registry's ten minutes.
const WAIT: Duration = Duration::from_secs(10 * 60);
/// How long B waits for A to post the `add` entry once it has the ack.
const POSTED: Duration = Duration::from_secs(60);

fn unattended(ctx: &Ctx) -> bool {
    cfg!(debug_assertions) && ctx.env("KROWK_TEST_UNATTENDED_DEVICE_APPROVAL") == "1"
}

fn terminal(ctx: &Ctx, refused: &str) -> Result<(), Error> {
    if unattended(ctx) || (ctx.io.stdin_tty && ctx.io.err_tty) {
        return Ok(());
    }
    Err(fail("confirmation_required", refused.to_string()))
}

fn now() -> u64 {
    jiff::Timestamp::now().as_second().max(0) as u64
}

/// What a person calls this machine's OS on the prompt A shows.
fn os_name() -> &'static str {
    match std::env::consts::OS {
        "macos" => "macOS",
        "linux" => "Linux",
        "windows" => "Windows",
        "freebsd" => "FreeBSD",
        other => other,
    }
}

/// `s` cut to at most `max` bytes, on a character boundary.
fn clip(s: &str, max: usize) -> String {
    let mut end = s.len().min(max);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].trim().to_string()
}

#[derive(Serialize, Deserialize)]
struct Payload {
    v: u8,
    epoch: u64,
    entries: Vec<(String, String)>,
    /// When the registry received each entry, as A was served them (and now
    /// for the new one): what a new machine with no pin counts a recovery
    /// rotation's delay from, so a completed kit replacement verifies.
    received_at: Vec<u64>,
    /// Every older generation of the user key wrapped under the next, so
    /// the new machine opens sessions sealed before it joined.
    previous: Vec<String>,
    generation: u32,
    wrapped_key: String,
    signing_key: String,
}

fn signer(store: &Keystore, device: &DeviceKey) -> Result<(SigningKey, std::sync::Arc<dyn krowk_api::client::RequestSigner>), Error> {
    let signing = store.signing_key().map_err(|e| fail("sync_setup_failed", e))?;
    let copy = SigningKey::from_secret(&*signing.secret_bytes()).map_err(|e| fail("sync_setup_failed", e.0))?;
    Ok((signing, DeviceSigner::new(device.id(), copy).shared()))
}

fn entries(list: &krowk_api::sync::DeviceList) -> Result<Vec<SignedEntry>, Error> {
    let bad = || fail("device_list_invalid", "the registry served a device list entry that is not hex — nothing was changed");
    list.entries
        .iter()
        .map(|e| SignedEntry::from_parts(e2e::unhex(&e.entry).ok_or_else(bad)?, &e2e::unhex(&e.signatures).ok_or_else(bad)?).map_err(|e| fail("device_list_invalid", e.0)))
        .collect()
}

fn hex(b: &[u8]) -> String {
    e2e::hex(b)
}

fn blob(field: &str, p: &Pairing) -> Option<Vec<u8>> {
    let v = match field {
        "joiner_message" => &p.joiner_message,
        "initiator_message" => &p.initiator_message,
        "joiner_confirmation" => &p.joiner_confirmation,
        "sealed_reply" => &p.sealed_reply,
        _ => &p.joiner_ack,
    };
    if v.is_empty() { None } else { e2e::unhex(v) }
}

/// Whether the person pressed ^C while `devices add` waits: the wait stops,
/// and the pairing is ended on the registry rather than left open for its
/// ten minutes.
static INTERRUPTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(unix)]
extern "C" fn on_interrupt(_: libc::c_int) {
    INTERRUPTED.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// ^C noted rather than fatal while it lives, and fatal again after.
struct Interrupts;

impl Interrupts {
    fn catch() -> Interrupts {
        INTERRUPTED.store(false, std::sync::atomic::Ordering::SeqCst);
        // SAFETY: the handler only stores to an atomic, which is
        // async-signal-safe.
        #[cfg(unix)]
        unsafe {
            libc::signal(libc::SIGINT, on_interrupt as extern "C" fn(libc::c_int) as libc::sighandler_t);
        }
        Interrupts
    }
}

impl Drop for Interrupts {
    fn drop(&mut self) {
        // SAFETY: restoring the default disposition.
        #[cfg(unix)]
        unsafe {
            libc::signal(libc::SIGINT, libc::SIG_DFL);
        }
    }
}

fn interrupted() -> bool {
    INTERRUPTED.load(std::sync::atomic::Ordering::SeqCst)
}

/// Reads the pairing until `field` has arrived. A pairing that ended, or
/// the window closing, is the failure `failed` names.
fn wait_for(client: &Client, id: &str, field: &str, failed: &dyn Fn(Option<Error>) -> Error) -> Result<Vec<u8>, Error> {
    let deadline = Instant::now() + WAIT;
    loop {
        let p = client.show_pairing(id).map_err(|e| failed(Some(e)))?;
        if let Some(b) = blob(field, &p) {
            return Ok(b);
        }
        if p.state == "dead" || Instant::now() > deadline || interrupted() {
            return Err(failed(None));
        }
        std::thread::sleep(POLL);
    }
}

// ------------------------------------------------------------ A: devices add

const ADD_OFF_TERMINAL: &str = "`krowk devices add` hands your key to another machine once a person has said yes to it, so it needs a person at a terminal — run it in one";

/// A: shows a code, waits for the new machine, checks its confirmation,
/// asks the person, and only once the new machine has acknowledged posts
/// its `add` entry and the user key wrapped to it.
pub(super) fn add(ctx: &mut Ctx) -> Result<(), Error> {
    terminal(ctx, ADD_OFF_TERMINAL)?;
    let store = keystore(ctx)?;
    let setup_first = || fail("no_user_key", "this machine is not on your device list yet — set sync up with `krowk sync init`, or add it from another device with `krowk sync join`");
    let device = store.device().map_err(|e| fail("sync_setup_failed", e))?.ok_or_else(setup_first)?;
    if store.user_keys().map_err(|e| fail("sync_setup_failed", e))?.is_none() {
        return Err(setup_first());
    }
    let pin = store.pin().map_err(|e| fail("sync_setup_failed", e))?.ok_or_else(setup_first)?;
    let (signing, shared) = signer(&store, &device)?;
    let client = keyed_client(ctx, "`krowk devices add`")?.signed_by(shared);

    // The list this device wraps to is the one it verifies back to its pin,
    // never what the registry says it is.
    let served = client.device_list_all()?;
    if served.epoch != pin.epoch {
        return Err(fail("device_list_reset", "your device list was started over since this machine last saw it — pair this machine again with `krowk sync join`"));
    }
    let served_entries = entries(&served)?;
    let received: Vec<u64> = served.entries.iter().map(|e| e.received_at).collect();
    let chain = Chain::verify(&served_entries, pin.trust().map_err(|e| fail("sync_setup_failed", e))?, now()).map_err(|e| fail("device_list_refused", e.0))?;
    let keys = caught_up(&store, &client, &chain, &device)?;
    if !chain.devices().iter().any(|d| d.id() == device.id() && d.kind == Kind::Device) {
        return Err(fail("device_removed", "this machine is not on your device list — it was removed; pair it again with `krowk sync join`"));
    }
    store.save_pin(&Pin::new(served.epoch, chain.head(), now(), chain.pending().map(|p| (p.seq, p.since)))).map_err(|e| fail("sync_setup_failed", e))?;

    let user = client.verify_key()?.user_id;
    let a = PairA::new(PeerKind::SamePersonDevice, user, device.id()).map_err(|e| fail("pairing_failed", e.0))?;
    let interrupts = Interrupts::catch();
    let opened = client.open_pairing()?;
    let id = opened.id.clone();
    // From here on, anything that goes wrong ends the pairing on the
    // registry too, so the new machine hears it at once.
    let result = add_steps(ctx, &client, &id, a, (&chain, served_entries, received, served.epoch), &keys, (&device, &signing));
    if result.is_err() {
        let _ = client.end_pairing(&id);
    }
    drop(interrupts);
    let (name, os, seq) = result?;
    let summary = format!("Added. '{name}' ({os}) syncs in every workspace you're a member of.");
    if ctx.format == Format::Human {
        let _ = writeln!(ctx.io.stdout, "{summary}");
        return Ok(());
    }
    super::sessions::emit_data(ctx, json!({ "name": name, "os": os, "seq": seq }), summary)
}

/// This device's user keys at the generation the verified chain leaves
/// current. A device a rotation left behind takes up the newer generation
/// here, as every device does (`UserKeys::adopt`): its wrap must open to the
/// id the chain names, and lead back down to the key it holds. Every older
/// generation is then held to the chain's ids too (`verified_by`).
fn caught_up(store: &Keystore, client: &Client, chain: &Chain, device: &DeviceKey) -> Result<UserKeys, Error> {
    let setup = |e: String| fail("sync_setup_failed", e);
    let held = store.user_keys().map_err(setup)?.ok_or_else(|| fail("no_user_key", "this machine holds no user key — add it from another device with `krowk sync join`"))?;
    if held.newest().generation() < chain.generation() {
        let behind = || fail("user_key_behind", "the registry holds no wrap of your current user key for this machine — it may have been removed; check `krowk devices list`");
        let served = client.user_key()?;
        let g = chain.generation();
        let wrap = served.wraps.iter().find(|w| w.generation == g).and_then(|w| e2e::unhex(&w.wrapped_key)).ok_or_else(behind)?;
        let links = served.generations.iter().filter(|x| !x.wrapped_previous.is_empty()).filter_map(|x| e2e::unhex(&x.wrapped_previous));
        let adopted = UserKeys::adopt(held.newest(), &wrap, g, chain.key_id(), device, links).map_err(|e| fail("user_key_refused", e.0))?;
        store.save_user_keys(&adopted).map_err(setup)?;
    }
    let keys = store.user_keys().map_err(setup)?.ok_or_else(|| setup("the user keys went missing".into()))?;
    keys.verified_by(chain).map_err(|e| fail("user_key_refused", e.0))
}

fn a_failed(e: Option<Error>) -> Error {
    let why = e.map(|e| format!(" ({})", e.code())).unwrap_or_default();
    fail("pairing_failed", format!("the pairing ended before the new device was added{why} — nothing was added; run `krowk devices add` again for a new code"))
}

/// One of A's steps, sent once. When its answer is lost, the pairing is
/// read back: the step landed if it holds what was sent, and the pairing is
/// over otherwise — sending it again would be out of turn.
fn a_step(client: &Client, id: &str, step: PairingStep, field: &str, message: &[u8]) -> Result<(), Error> {
    match client.pairing_step(id, step, message) {
        Ok(_) => Ok(()),
        Err(e) if e.status == 0 => match client.show_pairing(id) {
            Ok(p) if blob(field, &p).as_deref() == Some(message) => Ok(()),
            _ => Err(a_failed(Some(e))),
        },
        Err(e) => Err(a_failed(Some(e))),
    }
}

/// The list A verified: the chain, its entries as served with when the
/// registry received each, and its epoch.
type Verified<'a> = (&'a Chain, Vec<SignedEntry>, Vec<u64>, u64);

fn add_steps(ctx: &mut Ctx, client: &Client, id: &str, a: PairA, (chain, mut all, mut received, epoch): Verified<'_>, keys: &UserKeys, (device, signing): (&DeviceKey, &SigningKey)) -> Result<(String, String, u64), Error> {
    let _ = writeln!(ctx.io.stderr, "Pair a new device\n  Code: {}          valid 10 minutes, once\nOn the new device run:  krowk sync join\nWaiting…", a.code());
    let _ = ctx.io.stderr.flush();
    let hello = wait_for(client, id, "joiner_message", &a_failed)?;
    let (await_confirm, spake) = a.receive_hello(&hello).map_err(|_| a_failed(None))?;
    a_step(client, id, PairingStep::Answer, "initiator_message", &spake)?;
    let confirmation = wait_for(client, id, "joiner_confirmation", &a_failed)?;
    // A wrong code fails here: the pairing ends, and the person is asked
    // nothing.
    let confirmed = await_confirm.receive_confirm(&confirmation).map_err(|_| {
        fail("pairing_failed", "the new device's confirmation did not check — the code was wrong, or someone else tried it; nothing was added. Run `krowk devices add` again for a new code")
    })?;
    let new = confirmed.device().clone();
    let (name, os) = (printable(&new.name), printable(&new.os));
    ask(ctx, &format!("Add '{name}' ({os}) to your devices?"))?;
    let subject = Subject { kind: Kind::Device, name: new.name.clone(), os: new.os.clone(), device: new.device, signing: new.signing };
    let (next, batch) = chain.batch(keys.newest(), vec![Change::Add(subject)], device.id(), signing, now()).map_err(|e| fail("device_list_refused", e.0))?;
    let wrapped = batch.wraps.iter().find(|(d, _)| *d == new.id()).map(|(_, w)| w.clone()).ok_or_else(|| fail("device_list_refused", "the new device's entry carried no wrap for it"))?;
    all.extend(batch.entries.iter().cloned());
    received.extend(batch.entries.iter().map(|_| now()));
    let payload = Payload {
        v: 1,
        epoch,
        entries: all.iter().map(|e| (hex(&e.bytes), hex(&e.signatures_bytes()))).collect(),
        received_at: received,
        previous: keys.wraps().map(|(_, w)| hex(w)).collect(),
        generation: next.generation(),
        wrapped_key: hex(&wrapped),
        signing_key: hex(&signing.public().0),
    };
    let bytes = krowk_client::Zeroizing::new(serde_json::to_vec(&payload).expect("the payload serializes"));
    let (await_ack, reply) = confirmed.approve(&bytes).map_err(|_| a_failed(None))?;
    a_step(client, id, PairingStep::Reply, "sealed_reply", &reply)?;
    let ack = wait_for(client, id, "joiner_ack", &a_failed)?;
    let paired = await_ack.receive_ack(&ack).map_err(|_| a_failed(None))?;
    // Both sides have confirmed: only now does anything reach the chain.
    let post = ListPost {
        entries: batch.entries.iter().map(|e| (hex(&e.bytes), hex(&e.signatures_bytes()))).collect(),
        links: batch.links.iter().map(|l| hex(l)).collect(),
        wraps: batch.wraps.iter().map(|(d, w)| (d.to_string(), hex(w))).collect(),
        start_over: false,
    };
    client.append_device_list(&post)?;
    let store = keystore(ctx)?;
    store.save_pin(&Pin::new(epoch, next.head(), now(), next.pending().map(|p| (p.seq, p.since)))).map_err(|e| fail("sync_setup_failed", e))?;
    drop(paired);
    Ok((name, os, next.head().seq))
}

/// The one `[Y/n]` naming the new device. Off a terminal only a debug
/// build's test stand-in answers, and it says yes.
fn ask(ctx: &mut Ctx, question: &str) -> Result<(), Error> {
    if unattended(ctx) {
        let _ = writeln!(ctx.io.stderr, "{question} [Y/n] y");
        return Ok(());
    }
    match inquire::Confirm::new(question).with_default(true).prompt() {
        Ok(true) => Ok(()),
        _ => Err(fail("selection_cancelled", "not added — the new device was told, and nothing reached your device list")),
    }
}

// --------------------------------------------------------------- B: sync join

const JOIN_OFF_TERMINAL: &str = "`krowk sync join` takes a pairing code typed by a person, never an argument or a pipe, so it needs a person at a terminal — run it in one";

/// The failure every step on B ends in: the code is used up.
fn b_failed(from: &str) -> Error {
    fail(
        "pairing_failed",
        format!("the pairing failed and this code is used up — on '{from}' run `krowk devices add` again for a new code, then `krowk sync join` here. Nothing was kept"),
    )
}

/// The code, typed at a prompt: never an argument, so it stays out of shell
/// history and process lists. A debug build's tests type it on stdin.
fn typed_code(ctx: &Ctx) -> Result<PairingCode, Error> {
    let typed = if unattended(ctx) {
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).map_err(|_| fail("selection_cancelled", "no pairing code was entered"))?;
        krowk_client::Zeroizing::new(line)
    } else {
        krowk_client::Zeroizing::new(inquire::Text::new("Pairing code:").prompt().map_err(|_| fail("selection_cancelled", "no pairing code was entered"))?)
    };
    // Parsed once: the string is dropped here, and the code it made is
    // consumed by the one pairing it starts.
    PairingCode::parse(&typed).ok_or_else(|| fail("bad_pairing_code", "that is not a pairing code — it is the eight characters `krowk devices add` shows, like K7QF-9M3X; nothing was sent"))
}

/// B: signs in if it has to, finds the person's one open pairing, takes
/// the code at a prompt, and keeps the keys and the pin only once the
/// chain A sealed adds exactly this machine.
pub(super) fn join(ctx: &mut Ctx) -> Result<(), Error> {
    terminal(ctx, JOIN_OFF_TERMINAL)?;
    let mut client = super::agent::new_client(ctx)?;
    if !client.authenticated() {
        super::auth::login(ctx, &[])?;
        client = keyed_client(ctx, "`krowk sync join`")?;
    }
    let key = client.verify_key()?;
    let _ = writeln!(ctx.io.stderr, "Signed in to {}", printable(if key.workspace_name.is_empty() { &key.workspace } else { &key.workspace_name }));
    let store = keystore(ctx)?;
    let device = store.device_key().map_err(|e| fail("sync_setup_failed", e))?;
    let (signing, shared) = signer(&store, &device)?;
    // A user key here is a join that finished only if this machine is on the
    // list; one left by a pairing A never posted is replaced.
    if store.user_keys().map_err(|e| fail("sync_setup_failed", e))?.is_some() {
        if listed(&client, &device.id())? {
            return Err(fail("already_joined", "this machine is already on your device list — there is nothing to join"));
        }
        store.forget_user_keys().map_err(|e| fail("sync_setup_failed", e))?;
    }
    let open = client.find_open_pairing().map_err(|e| {
        if e.code() == "no_pairing" { fail("no_pairing", "none of your devices has a pairing open — run `krowk devices add` on one of them first, then `krowk sync join` here") } else { e }
    })?;
    let a_device = DeviceId::parse(&open.initiator_device.id).ok_or_else(|| fail("malformed_response", "the pairing names no device id"))?;
    let from = printable(&open.initiator_device.name);
    let code = typed_code(ctx)?;
    let _ = writeln!(ctx.io.stderr, "Waiting for approval on '{from}'…");
    let _ = ctx.io.stderr.flush();
    let me = NewDevice { device: device.public(), signing: signing.public(), name: clip(&device_name(ctx), pairing::MAX_NAME), os: os_name().into() };
    let binding = Binding { kind: PeerKind::SamePersonDevice, user_id: key.user_id.clone(), a_device, b_device: device.id() };
    let result = join_steps(&client, &open.id, binding, code, me, &device, &store);
    let (seq, name) = match result {
        Ok(done) => done,
        Err(()) => {
            let _ = client.end_pairing(&open.id);
            let _ = store.forget_user_keys();
            return Err(b_failed(&from));
        }
    };
    // A posts the entry once it has the ack; this key then claims the
    // device, which the registry allows only once it is on the list. That,
    // not whether the ack's answer arrived, says whether the join finished:
    // an ack whose answer was lost may well have landed.
    let signed = client.signed_by(shared);
    let deadline = Instant::now() + POSTED;
    while signed.claim_key_device().is_err() {
        if Instant::now() > deadline {
            let _ = store.forget_user_keys();
            return Err(fail(
                "pairing_failed",
                format!("'{from}' did not add this machine to your device list, or the registry could not be reached to see it — the keys were forgotten and the code is used up; on '{from}' run `krowk devices add` again for a new code"),
            ));
        }
        std::thread::sleep(POLL);
    }
    let summary = format!("Joined as '{name}'. This machine syncs in every workspace you're a member of.");
    if ctx.format == Format::Human {
        let _ = writeln!(ctx.io.stdout, "{summary}");
        return Ok(());
    }
    super::sessions::emit_data(ctx, json!({ "device": device.id().to_string(), "name": name, "seq": seq, "joined": true }), summary)
}

/// Whether `device` is on the person's device list as the registry serves
/// it: only ever a reason to refuse, so the registry's word is enough.
fn listed(client: &Client, device: &DeviceId) -> Result<bool, Error> {
    let list = client.device_list_all()?;
    Ok(entries(&list)?.iter().filter_map(|e| Entry::decode(&e.bytes).ok()).any(|e| e.action == Action::Add && e.subjects.iter().any(|s| s.id() == *device)))
}

/// B's side from the hello to the ack. Every failure is `Err(())`: which
/// check it was is nothing the person or an attacker needs, and the caller
/// ends the pairing and discards the code whatever it was.
fn join_steps(client: &Client, id: &str, binding: Binding, code: PairingCode, me: NewDevice, device: &DeviceKey, store: &Keystore) -> Result<(u64, String), ()> {
    let a_device = binding.a_device;
    let (b, hello) = PairB::start(binding, code, me.clone()).map_err(|_| ())?;
    client.pairing_step(id, PairingStep::Join, &hello).map_err(|_| ())?;
    let none = |_: Option<Error>| fail("pairing_failed", "");
    let spake = wait_for(client, id, "initiator_message", &none).map_err(|_| ())?;
    let (await_reply, confirm) = b.receive_spake(&spake).map_err(|_| ())?;
    client.pairing_step(id, PairingStep::Confirmation, &confirm).map_err(|_| ())?;
    let reply = wait_for(client, id, "sealed_reply", &none).map_err(|_| ())?;
    let received = await_reply.receive_reply(&reply).map_err(|_| ())?;
    let (key, pin, seq) = check_payload(received.payload(), a_device, &me, device).map_err(|_| ())?;
    // Kept before the ack, so an ack that lands always leaves the keys
    // here; a failure after this forgets them again.
    store.save_user_keys(&key).map_err(|_| ())?;
    store.save_pin(&pin).map_err(|_| ())?;
    let (_, ack) = received.acknowledge();
    match client.pairing_step(id, PairingStep::Acknowledgement, &ack) {
        // Lost on the way back, it may have landed: the caller learns which
        // from the list. Nothing is sent again either way.
        Ok(_) => Ok((seq, me.name)),
        Err(e) if e.status == 0 => Ok((seq, me.name)),
        Err(_) => Err(()),
    }
}

/// The caller's checks on what A sealed (`Received::payload`): the chain
/// verifies from seq 0 — a new machine has no pin, and this authenticated
/// reply is where its trust comes from — and its last entry adds exactly
/// this machine's keys, name and OS, signed by A alone, whose signing key
/// the payload names; then the user key opens as the id the chain names.
pub(super) fn check_payload(payload: &[u8], a_device: DeviceId, me: &NewDevice, device: &DeviceKey) -> Result<(UserKeys, Pin, u64), String> {
    let p: Payload = serde_json::from_slice(payload).map_err(|_| "the reply is not a payload krowk reads")?;
    if p.v != 1 {
        return Err("the reply is from a newer krowk".into());
    }
    let entries = p
        .entries
        .iter()
        .map(|(e, s)| SignedEntry::from_parts(e2e::unhex(e).ok_or("not hex")?, &e2e::unhex(s).ok_or("not hex")?).map_err(|e| e.0))
        .collect::<Result<Vec<_>, String>>()?;
    let at = now();
    if p.received_at.len() != entries.len() {
        return Err("the reply's receipt times do not match its entries".into());
    }
    let chain = Chain::verify(&entries, Trust::Fresh { received_at: p.received_at.clone() }, at).map_err(|e| e.0)?;
    let last = Entry::decode(&entries.last().ok_or("the chain is empty")?.bytes).map_err(|e| e.0)?;
    let mine = Subject { kind: Kind::Device, name: me.name.clone(), os: me.os.clone(), device: me.device, signing: me.signing };
    if last.action != Action::Add || last.subjects != [mine] || last.signers != [a_device] || last.seq == 0 {
        return Err("the chain's last entry does not add exactly this device, signed by the device that paired it".into());
    }
    let signing = e2e::unhex(&p.signing_key).and_then(|b| <[u8; 32]>::try_from(b).ok()).map(SigningPublic).ok_or("no signing key")?;
    if !chain.devices().iter().any(|d| d.id() == a_device && d.kind == Kind::Device && d.signing == signing) {
        return Err("the device that paired this one is not on the chain with the signing key it sent".into());
    }
    if p.generation != chain.generation() {
        return Err("the user key sent is not the generation the chain leaves current".into());
    }
    let wrapped = e2e::unhex(&p.wrapped_key).ok_or("not hex")?;
    let newest = UserKey::unwrap(&wrapped, chain.generation(), chain.key_id(), device).map_err(|e| e.0)?;
    let previous = p.previous.iter().map(|w| e2e::unhex(w).ok_or("not hex")).collect::<Result<Vec<_>, _>>()?;
    let key = UserKeys::new(newest, previous).and_then(|k| k.verified_by(&chain)).map_err(|e| e.0)?;
    let pin = Pin::new(p.epoch, chain.head(), at, chain.pending().map(|q| (q.seq, q.since)));
    Ok((key, pin, chain.head().seq))
}
