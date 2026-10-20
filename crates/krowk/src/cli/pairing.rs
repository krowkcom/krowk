//! Adding a device by a short code (R-E2E-3; canon, engineering/devices.md
//! → Adding a device): `krowk devices add` on a device already on the
//! person's list (A), `krowk sync join` on the new machine (B). The crypto
//! is `krowk_client::pairing` and `device_chain`; this is the command line
//! around them and the registry's mailbox between them.
//!
//! What A seals to B once the person has said yes, built and checked in
//! `krowk_client::pairing_payload`, is the whole signed chain with the new
//! `add` entry on its end, the user key wrapped to B, and A's signing key.
//! B trusts it because the pairing authenticated it, not because the
//! registry served it, and keeps nothing until the chain verifies from seq
//! 0 and its last entry adds exactly B's keys, signed by A. A posts the
//! entry and the wrap once B has acked — and also once the reply has gone
//! out and B never acks: the reply carries the user key, so a machine that
//! may hold it is put on the list, where `krowk devices remove` can take it
//! off and rotate the key away from it, rather than left holding the key
//! where nothing can see it. devices.md states the same rule; a reply
//! that never went out posts nothing.
//!
//! **After any failure on B, the code is gone.** A step that fails, a
//! refusal, a timeout, a dropped connection — B ends the pairing, tells the
//! person to get a new code from A, and never starts again with the typed
//! string: each start gives a registry posing as A one more guess. Nothing
//! is retried on B. A, whose side holds no guess, reads the pairing back
//! after a step whose answer was lost rather than sending the step again,
//! since a step sent twice is out of turn and ends the pairing.
//!
//! B's side, from the hello to the claim, is `krowk_client::pairing_join`,
//! which holds every client to these rules; the CLI supplies its mailbox.
//!
//! What B cannot stop is the person typing the same code into a second
//! `krowk sync join`: A's screen still shows it, and a registry that ended
//! B's first run gets another guess from the second. So B says a code works
//! once and to get a new one from the other device. The typed line itself
//! is wiped once parsed; the line editor's own buffer is not, as crypto.md
//! accepts for the recovery kit.
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
use krowk_client::device_chain::{Action, Chain, Change, Entry, Kind, SignedEntry, Subject};
use krowk_client::e2e::{self, DeviceId, DeviceKey, DeviceSigner, SigningKey};
use krowk_client::keystore::{Keystore, Slot};
use krowk_client::pairing::{self, invite, Binding, NewDevice, PairA, PairingCode, PeerKind};
use krowk_client::pairing_join::{self, Field, JoinFailed, Mailbox};
use krowk_client::pairing_payload;
use krowk_client::Zeroizing;
use krowk_client::user_key::UserKeys;
use serde_json::json;
use std::time::{Duration, Instant};

/// How often A reads the pairing while it waits for B: a person is walking
/// between two machines. B waits by `pairing_join::Timing::default()`, the
/// same 500 ms and ten minutes.
const POLL: Duration = Duration::from_millis(500);
/// How long A waits for B: the registry's ten minutes.
const WAIT: Duration = Duration::from_secs(10 * 60);

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

/// A reads the pairing until `field` has arrived. A pairing that ended, or
/// the window closing, is the failure `failed` names. A's side holds no
/// guess, so a read the network lost is only a read, asked again until the
/// window closes. B's wait, which ends at any failure, is
/// `krowk_client::pairing_join`'s.
fn wait_for(client: &Client, id: &str, field: Field, failed: &dyn Fn(Option<Error>) -> Error) -> Result<Vec<u8>, Error> {
    let deadline = Instant::now() + WAIT;
    loop {
        let p = match client.show_pairing(id) {
            Ok(p) => p,
            Err(e) if e.status == 0 && Instant::now() <= deadline && !interrupted() => {
                std::thread::sleep(POLL);
                continue;
            }
            Err(e) => return Err(failed(Some(e))),
        };
        if let Some(b) = field.blob(&p) {
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
/// asks the person and sends the reply. Once the reply, which carries the
/// user key, has gone out, posts the new machine's `add` entry and the user
/// key wrapped to it whether or not the machine acknowledges, so a machine
/// that may hold the key is listed and can be removed; with no ack it ends
/// the pairing and fails `pairing_unconfirmed`, the machine listed. A
/// pairing that ends before the reply posts nothing.
pub(super) fn add(ctx: &mut Ctx, args: &[String]) -> Result<(), Error> {
    if !args.is_empty() {
        return Err(fail("bad_argument", "`krowk devices add` takes no arguments — it shows a code for the new machine to type"));
    }
    terminal(ctx, ADD_OFF_TERMINAL)?;
    // ^C from here on ends the pairing rather than leaving it open.
    let interrupts = Interrupts::catch();
    let store = keystore(ctx)?;
    let setup_first = || fail("no_user_key", "this machine is not on your device list yet — set sync up with `krowk sync init`, or add it from another device with `krowk sync join`");
    let device = store.device().map_err(|e| fail("sync_setup_failed", e))?.ok_or_else(setup_first)?;
    if store.user_keys().map_err(|e| fail("sync_setup_failed", e))?.is_none() {
        return Err(setup_first());
    }
    if store.device_list().map_err(|e| fail("sync_setup_failed", e))?.is_none() {
        return Err(setup_first());
    }
    let (signing, shared) = signer(&store, &device)?;
    let client = keyed_client(ctx, "`krowk devices add`")?.signed_by(shared);

    let (served_entries, chain, keys) = current_list(&store, &client, &device)?;
    if !chain.devices().iter().any(|d| d.id() == device.id() && d.kind == Kind::Device) {
        return Err(fail("device_removed", "this machine is not on your device list — it was removed; pair it again with `krowk sync join`"));
    }

    let user = client.verify_key()?.user_id;
    let a = PairA::new(PeerKind::SamePersonDevice, user.clone(), device.id()).map_err(|e| fail("pairing_failed", e.0))?;
    let opened = client.open_pairing()?;
    let id = opened.id.clone();
    // Up to the reply, anything that goes wrong ends the pairing on the
    // registry too, so the new machine hears it at once, and adds nothing.
    let sealed = match seal_reply(ctx, &client, &id, (a, &user), (&chain, served_entries), &keys, (&device, &signing)) {
        Ok(sealed) => sealed,
        Err(e) => {
            let _ = client.end_pairing();
            return Err(e);
        }
    };
    // The reply went out with the user key in it: from here on the entry is
    // posted whatever B does, so a machine that may hold the key is listed.
    let acked = wait_for(&client, &id, Field::JoinerAck, &a_failed).and_then(|ack| sealed.await_ack.receive_ack(&ack).map_err(|_| a_failed(None)));
    if acked.is_err() {
        let _ = client.end_pairing();
    }
    let posted = post_add(&client, &sealed.batch);
    drop(interrupts);
    let (name, os) = (sealed.name, sealed.os);
    match (acked, posted) {
        (Ok(_), Ok(())) => {}
        (Err(_), Ok(())) => {
            let _ = store.save_device_list(&sealed.entries);
            return Err(fail(
                "pairing_unconfirmed",
                format!("'{name}' was sent your key but never confirmed it. It is on your devices so you can take it off and rotate your key away from it: `krowk devices remove '{name}'`"),
            ));
        }
        (_, Err(e)) => {
            return Err(fail(
                "pairing_unconfirmed",
                format!("'{name}' was sent your key, but it could not be added to your devices ({}) — it may hold your key without being listed. Check `krowk devices list`; rotate your key with the next `krowk devices remove`", e.code()),
            ));
        }
    }
    store.save_device_list(&sealed.entries).map_err(|e| fail("sync_setup_failed", e))?;
    let seq = sealed.next.head().seq;
    let summary = format!("Added. '{name}' ({os}) syncs in every workspace you're a member of.");
    if ctx.format == Format::Human {
        let _ = writeln!(ctx.io.stdout, "{summary}");
        return Ok(());
    }
    super::sessions::emit_data(ctx, json!({ "name": name, "os": os, "seq": seq }), summary)
}

/// The device list as the registry has it now, kept only if it extends
/// this machine's pin (a shorter, forked or started-over list is refused),
/// and this machine's user keys caught up to the generation it leaves
/// current. What a device wraps to or seals under is this, never the list
/// it kept from before, which a removal it slept through has left behind:
/// a registry that cannot be reached is a refusal, not a fall back to it.
pub(super) fn current_list(store: &Keystore, client: &Client, device: &DeviceKey) -> Result<(Vec<SignedEntry>, Chain, UserKeys), Error> {
    let served = client.device_list_all().map_err(|mut e| {
        e.body.insert("fix".into(), json!("the registry could not be asked for your device list, and nothing is sealed under a list that may be stale — try again when it answers"));
        e
    })?;
    let served = entries(&served)?;
    let chain = store.save_device_list(&served).map_err(|e| fail("device_list_refused", e))?;
    let keys = caught_up(store, client, &chain, device)?;
    Ok((served, chain, keys))
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
fn a_step(client: &Client, id: &str, step: PairingStep, field: Field, message: &[u8]) -> Result<(), Error> {
    match client.pairing_step(step, message) {
        Ok(_) => Ok(()),
        Err(e) if e.status == 0 => match client.show_pairing(id) {
            Ok(p) if field.blob(&p).as_deref() == Some(message) => Ok(()),
            _ => Err(a_failed(Some(e))),
        },
        Err(e) => Err(a_failed(Some(e))),
    }
}

/// The reply, sent once. It carries the user key, so when its answer is
/// lost and the read-back fails too, nobody can say it did not land: it is
/// taken as sent, and A goes on to the ack and the post, which list a
/// machine that may hold the key. Only a refusal, or a read-back that shows
/// it did not land, ends the pairing with nothing added.
fn send_reply(client: &Client, id: &str, reply: &[u8]) -> Result<(), Error> {
    match client.pairing_step(PairingStep::Reply, reply) {
        Ok(_) => Ok(()),
        // Lost on the way back, or a gateway's answer, which may come after
        // the registry stored it: either way it may have landed.
        Err(e) if matches!(e.status, 0 | 502..=504) => match client.show_pairing(id) {
            Ok(p) if Field::SealedReply.blob(&p).as_deref() != Some(reply) => Err(a_failed(Some(e))),
            _ => Ok(()),
        },
        Err(e) => Err(a_failed(Some(e))),
    }
}

/// The list A verified: the chain, and its entries as served.
type Verified<'a> = (&'a Chain, Vec<SignedEntry>);

/// A's side once the reply is out: what it posts, the chain it leaves,
/// and the wait for B's ack.
struct Sealed {
    batch: krowk_client::device_chain::Batch,
    /// The whole list with the new entry on its end, to keep once posted.
    entries: Vec<SignedEntry>,
    next: Chain,
    await_ack: krowk_client::pairing::AwaitAck,
    name: String,
    os: String,
}

/// From the hello to the reply: B's confirmation checked before the person
/// is asked anything, then the reply sealed and sent.
fn seal_reply(ctx: &mut Ctx, client: &Client, id: &str, (a, user): (PairA, &str), (chain, mut all): Verified<'_>, keys: &UserKeys, (device, signing): (&DeviceKey, &SigningKey)) -> Result<Sealed, Error> {
    let _ = writeln!(ctx.io.stderr, "Pair a new device\n  Code: {}          valid 10 minutes, once\nOn the new device run:  krowk sync join\nWaiting…", a.code());
    show_qr(ctx, a.code(), user, device.id());
    let _ = ctx.io.stderr.flush();
    let hello = wait_for(client, id, Field::JoinerMessage, &a_failed)?;
    let (await_confirm, spake) = a.receive_hello(&hello).map_err(|_| a_failed(None))?;
    a_step(client, id, PairingStep::Answer, Field::InitiatorMessage, &spake)?;
    let confirmation = wait_for(client, id, Field::JoinerConfirmation, &a_failed)?;
    // A wrong code fails here: the pairing ends, and the person is asked
    // nothing.
    let confirmed = await_confirm.receive_confirm(&confirmation).map_err(|_| {
        fail("pairing_failed", "the new device's confirmation did not check — the code was wrong, or someone else tried it; nothing was added. Run `krowk devices add` again for a new code")
    })?;
    let new = confirmed.device().clone();
    let (name, os) = (printable(&new.name), printable(&new.os));
    // Before the person is asked: a name that reads like a listed device's
    // is the new machine's to change, and the pairing ends here.
    if let Some(d) = chain.devices().iter().find(|d| krowk_client::device_chain::confusable_names(&d.name, &new.name)) {
        return Err(fail("device_name_taken", format!("the new device calls itself '{name}', which reads like '{}' already on your devices — nothing was added. On the new device run `krowk sync join --name <another name>`, and here `krowk devices add` again", printable(&d.name))));
    }
    ask(ctx, &format!("Add '{name}' ({os}) to your devices?"))?;
    if interrupted() {
        return Err(a_failed(None));
    }
    let subject = Subject { kind: Kind::Device, name: new.name.clone(), os: new.os.clone(), device: new.device, signing: new.signing };
    let (next, batch) = chain.batch(keys.newest(), vec![Change::Add(subject)], device.id(), signing, now()).map_err(|e| fail("device_list_refused", e.0))?;
    let wrapped = batch.wraps.iter().find(|(d, _)| *d == new.id()).map(|(_, w)| w.clone()).ok_or_else(|| fail("device_list_refused", "the new device's entry carried no wrap for it"))?;
    all.extend(batch.entries.iter().cloned());
    let bytes = pairing_payload::seal(&all, keys, next.generation(), &wrapped, &signing.public());
    let (await_ack, reply) = confirmed.approve(&bytes).map_err(|_| a_failed(None))?;
    send_reply(client, id, &reply)?;
    Ok(Sealed { batch, entries: all, next, await_ack, name, os })
}

/// The invite URI (`krowk_client::pairing::invite`) as a QR code under the
/// code, for the iPhone app to scan instead of typing it: on a terminal
/// only. Piped stderr, as in an agent's run or the test suite's stand-in,
/// gets nothing (`--json` at a terminal still draws it: the QR goes to
/// stderr, the JSON to stdout). Nor is it drawn on a terminal too narrow to
/// hold it, where every line would wrap and nothing could scan it, or for a
/// user id the URI cannot carry as it is: the code on the screen still
/// works. A terminal too short for it scrolls the code up off the window,
/// and is drawn on anyway: the person scanning needs the QR whole, and the
/// one typing scrolls back. The URI goes to the screen and nowhere else.
fn show_qr(ctx: &mut Ctx, code: &PairingCode, user: &str, device: DeviceId) {
    if !ctx.io.err_tty {
        return;
    }
    if !invite::carries(user) {
        return;
    }
    let uri = invite::uri(code, user, device);
    let lines = qr_lines(&uri);
    let wide = lines.first().map_or(0, |l| l.chars().count() - QR_SGR.len() - QR_RESET.len());
    if lines.is_empty() || columns().is_some_and(|c| c < wide) {
        return;
    }
    let _ = writeln!(ctx.io.stderr, "\nOr scan this with the krowk app on your iPhone:");
    for line in lines.iter() {
        let _ = writeln!(ctx.io.stderr, "{line}");
    }
}

/// Black on white, set explicitly on every line, so the code scans on a
/// dark terminal theme and a light one alike: a QR code needs dark modules
/// on a light ground, and the terminal's own colours may be either. It is
/// a functional image, not decoration, so `NO_COLOR` does not turn it off.
/// A theme that draws "white" as a light grey, or "black" as a dark one,
/// still leaves the contrast a reader needs. The half blocks are one column
/// wide but for terminals set to draw Unicode's ambiguous-width characters
/// wide, as some CJK setups are, where the code is drawn twice as wide.
const QR_SGR: &str = "\x1b[30;47m";
const QR_RESET: &str = "\x1b[0m";
/// The light margin a reader needs around the code, in modules, each side.
const QUIET: usize = 4;

/// `uri` as a QR code at medium error correction, in lines of text: two
/// module rows to a character row, drawn with the half blocks `▀ ▄ █` and
/// a space, a 4-module quiet zone around it, and each line wrapped in
/// `QR_SGR` … `QR_RESET`. Empty when the URI is too long for any QR code.
/// The lines are wiped on drop; qrcodegen's buffers, which hold the URI
/// and so the code, are not; as with what `spake2` holds (`krowk_client::pairing`), the code is
/// single-use and dies with the pairing, so that is accepted.
fn qr_lines(uri: &str) -> Zeroizing<Vec<String>> {
    use qrcodegen::{QrCode, QrCodeEcc, QrSegment, Version};
    let Ok(qr) = QrCode::encode_segments_advanced(&QrSegment::make_segments(uri), QrCodeEcc::Medium, Version::MIN, Version::MAX, None, false) else {
        return Zeroizing::new(Vec::new());
    };
    let size = qr.size();
    let quiet = QUIET as i32;
    // Outside the code is the quiet zone, light; qrcodegen says so too.
    let dark = |x: i32, y: i32| qr.get_module(x - quiet, y - quiet);
    let width = size + 2 * quiet;
    (0..width)
        .step_by(2)
        .map(|y| {
            let row: String = (0..width)
                .map(|x| match (dark(x, y), dark(x, y + 1)) {
                    (true, true) => '█',
                    (true, false) => '▀',
                    (false, true) => '▄',
                    (false, false) => ' ',
                })
                .collect();
            format!("{QR_SGR}{row}{QR_RESET}")
        })
        .collect::<Vec<_>>()
        .into()
}

/// The terminal's width in columns, when stderr is one that says.
#[cfg(unix)]
fn columns() -> Option<usize> {
    let mut ws = libc::winsize { ws_row: 0, ws_col: 0, ws_xpixel: 0, ws_ypixel: 0 };
    // SAFETY: TIOCGWINSZ writes one winsize through the pointer, which
    // points at one; on a closed or non-terminal stderr it fails, and
    // writes nothing.
    let ok = unsafe { libc::ioctl(libc::STDERR_FILENO, libc::TIOCGWINSZ, &mut ws) } == 0;
    (ok && ws.ws_col > 0).then_some(usize::from(ws.ws_col))
}

#[cfg(not(unix))]
fn columns() -> Option<usize> {
    None
}

/// Posts the new device's entry and wrap, once. When the answer is lost the
/// list is read back: the post landed if the entry is on it.
fn post_add(client: &Client, batch: &krowk_client::device_chain::Batch) -> Result<(), Error> {
    let post = ListPost {
        entries: batch.entries.iter().map(|e| (hex(&e.bytes), hex(&e.signatures_bytes()))).collect(),
        links: batch.links.iter().map(|l| hex(l)).collect(),
        wraps: batch.wraps.iter().map(|(d, w)| (d.to_string(), hex(w))).collect(),
        start_over: false,
    };
    match client.append_device_list(&post) {
        Ok(_) => Ok(()),
        Err(e) if e.status == 0 => {
            let first = hex(&batch.entries[0].bytes);
            match client.device_list_all() {
                Ok(list) if list.entries.iter().any(|x| x.entry == first) => Ok(()),
                _ => Err(e),
            }
        }
        Err(e) => Err(e),
    }
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
        format!("the pairing failed, and a code works once — get a new one from the other device: on '{from}' run `krowk devices add` again, then `krowk sync join` here. Nothing was kept"),
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
pub(super) fn join(ctx: &mut Ctx, args: &[String]) -> Result<(), Error> {
    if !args.is_empty() {
        return Err(fail("bad_argument", "`krowk sync join` takes the code at its prompt, never as an argument — one given here is in your shell history now, so get a new code with `krowk devices add` and type it when asked"));
    }
    terminal(ctx, JOIN_OFF_TERMINAL)?;
    let mut client = super::agent::new_client(ctx)?;
    if !client.authenticated() {
        super::auth::login(ctx, &[])?;
        client = keyed_client(ctx, "`krowk sync join`")?;
    }
    let key = client.verify_key()?;
    let who = if !key.email.is_empty() { &key.email } else if key.workspace_name.is_empty() { &key.workspace } else { &key.workspace_name };
    let _ = writeln!(ctx.io.stderr, "Signed in as {}", printable(who));
    let store = keystore(ctx)?;
    let device = store.device_key().map_err(|e| fail("sync_setup_failed", e))?;
    let (signing, shared) = signer(&store, &device)?;
    // Nothing here is forgotten on the registry's word: a user key left by
    // a pairing A never posted is replaced only once A's own chain says so.
    match listed(&client, &device.id())? {
        Listing::Active if store.user_keys().map_err(|e| fail("sync_setup_failed", e))?.is_some() => {
            return Err(fail("already_joined", "this machine is already on your device list — there is nothing to join"));
        }
        // Its keys are in the list's history, so no list takes them again.
        Listing::Removed => {
            return Err(fail(
                "device_removed",
                format!("this machine was removed from your devices, and its keys cannot be added again — move {} and {} aside, then run `krowk sync join`", store.describe(Slot::Device), store.describe(Slot::Signing)),
            ));
        }
        _ => {}
    }
    let open = client.find_open_pairing().map_err(|e| {
        if e.code() == "no_pairing" { fail("no_pairing", "none of your devices has a pairing open — run `krowk devices add` on one of them first, then `krowk sync join` here") } else { e }
    })?;
    let a_device = DeviceId::parse(&open.initiator_device.id).ok_or_else(|| fail("malformed_response", "the pairing names no device id"))?;
    // Only the paired device's id is bound by the pairing; its name is the
    // registry's, shown as a hint.
    let from = printable(&open.initiator_device.name);
    let code = typed_code(ctx)?;
    let _ = writeln!(ctx.io.stderr, "Waiting for approval on '{from}'…");
    let _ = ctx.io.stderr.flush();
    let me = NewDevice { device: device.public(), signing: signing.public(), name: clip(&device_name(ctx), pairing::MAX_NAME), os: os_name().into() };
    let binding = Binding { kind: PeerKind::SamePersonDevice, user_id: key.user_id.clone(), a_device, b_device: device.id() };
    // One client for the whole join, signed by this device for the claim;
    // B's steps still go with the key alone (`pairing_step` signs only A's).
    let mailbox = ClientMailbox { client: client.signed_by(shared), id: open.id.clone() };
    let joined = pairing_join::join(&mailbox, &store, &device, binding, code, me, &pairing_join::Timing::default());
    let pairing_join::Joined { seq, name } = match joined {
        Ok(joined) => joined,
        Err(JoinFailed::CodeUsedUp) => return Err(b_failed(&from)),
        Err(JoinFailed::NotAdded) => {
            return Err(fail(
                "pairing_failed",
                format!("'{from}' did not add this machine to your device list, or the registry could not be reached to see it — the keys were forgotten and the code is used up; on '{from}' run `krowk devices add` again for a new code"),
            ));
        }
    };
    let summary = format!("Joined as '{name}'. This machine syncs in every workspace you're a member of.");
    if ctx.format == Format::Human {
        let _ = writeln!(ctx.io.stdout, "{summary}");
        return Ok(());
    }
    super::sessions::emit_data(ctx, json!({ "device": device.id().to_string(), "name": name, "seq": seq, "joined": true }), summary)
}

#[derive(PartialEq, Eq)]
enum Listing {
    Never,
    Active,
    Removed,
}

/// Where `device` stands on the person's device list as the registry serves
/// it, replaying its adds and removes: only ever a reason to refuse, so the
/// registry's word is enough, and it never makes this machine forget.
fn listed(client: &Client, device: &DeviceId) -> Result<Listing, Error> {
    let list = client.device_list_all()?;
    let mut at = Listing::Never;
    for e in entries(&list)?.iter().filter_map(|e| Entry::decode(&e.bytes).ok()) {
        if e.subjects.iter().any(|s| s.id() == *device) {
            at = match e.action {
                Action::Add => Listing::Active,
                Action::Remove => Listing::Removed,
                _ => at,
            };
        }
    }
    Ok(at)
}

/// The registry's mailbox as `krowk sync join` reaches it: `client` is
/// the keyed client, signed by this device, and `id` the pairing it is in.
struct ClientMailbox {
    client: Client,
    id: String,
}

impl Mailbox for ClientMailbox {
    fn step(&self, step: PairingStep, message: &[u8]) -> Result<(), Error> {
        self.client.pairing_step(step, message).map(|_| ())
    }

    fn read(&self) -> Result<Pairing, Error> {
        self.client.show_pairing(&self.id)
    }

    fn end(&self) {
        let _ = self.client.end_pairing();
    }

    fn claim_device(&self) -> Result<(), Error> {
        self.client.claim_key_device().map(|_| ())
    }

    fn pause(&self, d: Duration) {
        std::thread::sleep(d);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qrcodegen::{QrCode, QrCodeEcc, QrSegment, Version};

    const URI: &str = "krowk://pair?v=1&code=K7QF9M3X&user=0192f5a0-7e1c-7cc3-9d1a-5b6f00000001&device=0123456789abcdef0123456789abcdef";

    /// R-E2E-3 (a device added "by QR or code"): what `devices add` draws
    /// is the crate's matrix for the URI at medium error correction, two
    /// rows to a line, inside a 4-module quiet zone, each line black on
    /// white.
    #[test]
    fn r_e2e_3_the_qr_lines_are_the_modules_of_the_uri() {
        let qr = QrCode::encode_segments_advanced(&QrSegment::make_segments(URI), QrCodeEcc::Medium, Version::MIN, Version::MAX, None, false).unwrap();
        assert_eq!(qr.error_correction_level(), QrCodeEcc::Medium);
        let size = qr.size() as usize;
        let lines = qr_lines(URI);
        assert_eq!(lines.len(), (size + 8).div_ceil(2), "rows");
        for (row, line) in lines.iter().enumerate() {
            let body = line.strip_prefix("\x1b[30;47m").and_then(|l| l.strip_suffix("\x1b[0m")).unwrap_or_else(|| panic!("line {row} is not black on white: {line:?}"));
            let cells: Vec<char> = body.chars().collect();
            assert_eq!(cells.len(), size + 8, "columns of line {row}");
            for (x, cell) in cells.iter().enumerate() {
                let dark = |y: usize| x >= 4 && y >= 4 && x < size + 4 && y < size + 4 && qr.get_module(x as i32 - 4, y as i32 - 4);
                let want = match (dark(2 * row), dark(2 * row + 1)) {
                    (true, true) => '█',
                    (true, false) => '▀',
                    (false, true) => '▄',
                    (false, false) => ' ',
                };
                assert_eq!(*cell, want, "line {row}, column {x}");
            }
        }
        // The quiet zone: two blank lines on top, at least two below, four
        // blank columns each side.
        let blank = |l: &String| l.chars().all(|c| !matches!(c, '▀' | '▄' | '█'));
        assert!(blank(&lines[0]) && blank(&lines[1]) && blank(&lines[lines.len() - 1]) && blank(&lines[lines.len() - 2]));
        assert!(lines.iter().all(|l| { let c: Vec<char> = l.chars().collect(); let b = &c[8..c.len() - 4]; b[..4] == [' '; 4] && b[b.len() - 4..] == [' '; 4] }));
    }

    /// Too long for any QR code: nothing drawn rather than a panic.
    #[test]
    fn a_uri_no_qr_code_holds_draws_nothing() {
        assert!(qr_lines(&"x".repeat(4000)).is_empty());
    }
}
