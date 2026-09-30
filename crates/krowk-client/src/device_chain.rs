//! The device list: a person's devices as an append-only chain of signed
//! entries, which every client verifies itself, since the registry that
//! serves it is assumed hostile (engineering/devices.md in Canon → The
//! device list). A device wraps the user key only to devices on a chain it
//! has verified back to its pin, and adopts a user key only when its id is
//! the one the chain commits to.
//!
//! # An entry's bytes
//!
//! One canonical encoding, big-endian throughout, every field fixed or
//! length-prefixed:
//!
//! ```text
//! version     u8        1
//! seq         u64
//! prev        [32]      SHA-256 of the previous entry's bytes; zeros at seq 0
//! action      u8        1 add, 2 remove, 3 rotate-recovery,
//!                       4 cancel-recovery-rotation
//! subjects    u8        how many: 1, or 2 at seq 0 (first device, recovery)
//!   kind      u8          1 device, 2 recovery
//!   name      u16 + utf8  1–64 bytes (see "Names" below)
//!   os        u16 + utf8  0–32 bytes, the same characters refused
//!   x25519    [32]        the key the user key is wrapped to
//!   ed25519   [32]        the key it signs with
//! generation  u32       the user key generation the entry leaves current
//! key id      [16]      that generation's user key id (`user_key`)
//! time        u64       Unix seconds, as the signer's clock read it
//! signers     u8        how many (1–3), then each signer's device id [16],
//!                       strictly ascending
//! ```
//!
//! # The signatures
//!
//! Beside the bytes, not in them: a list keyed by signer, `count u8`, then
//! for each signer in the order the bytes list them, `device id [16] ‖
//! Ed25519 signature [64]` over `"krowk/device-list/v1" ‖ bytes` (strict
//! verification; no other signature in krowk starts with that label). The
//! ids must be exactly the bytes' `signers`, so the hash that chains and
//! pins an entry — SHA-256 over the bytes alone — also commits to who
//! signed it.
//!
//! Who must sign is one rule (`required_signers`):
//! - every device an entry adds as proof of possession — both subjects at
//!   seq 0, the new recovery device in `rotate-recovery` — signs it itself,
//!   so no one can build a chain around the person's real recovery device
//!   without the kit's words;
//! - except at seq 0, exactly one authorizer: a device on the list, not
//!   removed before this entry, that `may_authorize` the action — any
//!   device for every action but `cancel-recovery-rotation`, which only the
//!   recovery device authorizes.
//!
//! A reader decodes strictly — unknown versions, actions and kinds, bad
//! UTF-8, out-of-range lengths and counts, unsorted signers and trailing
//! bytes are refused — and then re-encodes and refuses anything that does
//! not come back byte for byte, so one entry has exactly one encoding.
//!
//! # What the verifier holds an entry to
//!
//! - `seq` is one more than the previous entry's, and `prev` is its hash.
//! - Seq 0 must `add` a `device`, may add one `recovery` device as its
//!   second subject, and leaves generation 1 with its key id.
//! - `add` adds a `device` whose keys this chain has never held (X25519
//!   and Ed25519 are one pool, so a key reused across the two kinds is
//!   refused too), and leaves the generation and key id as they were.
//! - `remove` names an active `device` exactly as the list holds it. A
//!   `recovery` device cannot be removed.
//! - `rotate-recovery` replaces the recovery device — at once when the
//!   recovery device itself authorizes it, or when there is none yet (setup
//!   skipped the kit). Authorized by any other device it is only
//!   **pending**: for `RECOVERY_ROTATION_DELAY` (7 days) the current kit
//!   stays the recovery device — wrapped to, able to sign, unremovable —
//!   and the proposed one has no wraps and no rights. After the delay a
//!   second `rotate-recovery` naming the same device, authorized by any
//!   device, completes it. `cancel-recovery-rotation`, authorized by the
//!   recovery device, voids it, and so does removing the device that
//!   proposed it. One rotation is pending at a time. `Chain::pending` says
//!   who proposed it and when it can take effect, for every device to warn.
//! - The delay is judged by the verifying device's own clock (`Trust`): a
//!   pinned device counts from when it first saw the proposal and refuses a
//!   new entry dated more than `CLOCK_SKEW` (1 hour) before it last
//!   verified the list, as backdated; a fresh machine counts from the
//!   registry's receipt time, which is thief defense only, and takes a
//!   completion only if the registry received it after the delay. Entries
//!   at or before a pin were judged when first verified and are not judged
//!   again. The backdating rule means an entry signed offline and posted
//!   more than an hour later is refused; the CLI signs at post time.
//! - A removal and a `rotate-recovery` that takes effect rotate the key:
//!   they must leave the generation exactly one higher, with a key id the
//!   chain has not held. Every other entry — a pending proposal and a
//!   cancellation included — carries the current key id unchanged. A
//!   proposal rotates nothing because nothing changes until it completes;
//!   the completion rotates, so the old kit opens nothing sealed after it.
//! - **Names** are what a person reads at every prompt and in recovery's
//!   review, so they refuse control characters, bidirectional marks and
//!   overrides, zero-width characters and line and paragraph separators,
//!   and a device may not take a name (ignoring case) a device on the list
//!   already has.
//!
//! A pinned client refuses a chain shorter than its pin, or whose entry at
//! the pin's seq has another hash: an older list, or a forked one.
//!
//! # A batch of removals
//!
//! N removals are N entries and N generations, g+1 … g+N (`Chain::batch`).
//! Only g+N is wrapped to devices — every device left on the list — and
//! each of g … g+N-1 is wrapped under the generation after it, so the
//! newest opens them all. The generations in between are never wrapped to
//! any device, so a device removed later in the same batch never holds a
//! generation after its own removal. The registry takes a batch whole.

use crate::e2e::{DeviceId, DevicePublic, Error, SigningKey, SigningPublic};
use crate::user_key::{UserKey, UserKeyId};
use sha2::{Digest, Sha256};

/// The entry format this krowk writes and reads.
pub const ENTRY_V1: u8 = 1;
/// What every entry's signature starts with.
pub const SIGNATURE_LABEL: &[u8] = b"krowk/device-list/v1";

const NAME_MAX: usize = 64;
const OS_MAX: usize = 32;

fn refuse(seq: u64, why: impl std::fmt::Display) -> Error {
    Error(format!("the device list is refused at entry {seq}: {why}"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Add = 1,
    Remove = 2,
    RotateRecovery = 3,
    CancelRecoveryRotation = 4,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Device = 1,
    Recovery = 2,
}

/// The device an entry is about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subject {
    pub kind: Kind,
    pub name: String,
    pub os: String,
    pub device: DevicePublic,
    pub signing: SigningPublic,
}

impl Subject {
    pub fn id(&self) -> DeviceId {
        self.device.id()
    }
}

/// One entry, before it is signed. `signers` is filled in by `sign`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub seq: u64,
    pub prev: [u8; 32],
    pub action: Action,
    pub subjects: Vec<Subject>,
    pub generation: u32,
    pub key_id: UserKeyId,
    pub time: u64,
    pub signers: Vec<DeviceId>,
}

impl Entry {
    /// The canonical bytes (the module doc has the layout). Refused for
    /// anything a reader would refuse.
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        let seq = self.seq;
        if self.subjects.is_empty() || self.subjects.len() > 2 {
            return Err(refuse(seq, "an entry is about one device, or two at entry 0"));
        }
        if self.signers.is_empty() || self.signers.len() > 3 || !self.signers.windows(2).all(|w| w[0].0 < w[1].0) {
            return Err(refuse(seq, "its signers are not one to three distinct ids in order"));
        }
        let mut b = Vec::with_capacity(320);
        b.push(ENTRY_V1);
        b.extend_from_slice(&seq.to_be_bytes());
        b.extend_from_slice(&self.prev);
        b.push(self.action as u8);
        b.push(self.subjects.len() as u8);
        for s in &self.subjects {
            put_subject(&mut b, s, seq)?;
        }
        b.extend_from_slice(&self.generation.to_be_bytes());
        b.extend_from_slice(&self.key_id.0);
        b.extend_from_slice(&self.time.to_be_bytes());
        b.push(self.signers.len() as u8);
        for s in &self.signers {
            b.extend_from_slice(&s.0);
        }
        Ok(b)
    }

    /// An entry from its bytes, strictly: see the module doc.
    pub fn decode(bytes: &[u8]) -> Result<Entry, Error> {
        let mut r = Reader { b: bytes, seq: 0 };
        let version = r.u8()?;
        if version != ENTRY_V1 {
            return Err(Error(format!("the device list entry is format {version}, which this krowk does not read — upgrade krowk")));
        }
        let seq = u64::from_be_bytes(r.array()?);
        r.seq = seq;
        let prev = r.array::<32>()?;
        let action = match r.u8()? {
            1 => Action::Add,
            2 => Action::Remove,
            3 => Action::RotateRecovery,
            4 => Action::CancelRecoveryRotation,
            a => return Err(refuse(seq, format!("unknown action {a}"))),
        };
        let n = r.u8()?;
        if !(1..=2).contains(&n) {
            return Err(refuse(seq, "an entry is about one device, or two at entry 0"));
        }
        let subjects = (0..n).map(|_| r.subject()).collect::<Result<Vec<_>, _>>()?;
        let generation = u32::from_be_bytes(r.array()?);
        let key_id = UserKeyId(r.array()?);
        let time = u64::from_be_bytes(r.array()?);
        let n = r.u8()?;
        let signers = (0..n).map(|_| r.array().map(DeviceId)).collect::<Result<Vec<_>, _>>()?;
        if !r.b.is_empty() {
            return Err(refuse(seq, "trailing bytes"));
        }
        let entry = Entry { seq, prev, action, subjects, generation, key_id, time, signers };
        if entry.encode()? != bytes {
            return Err(refuse(seq, "not in canonical form"));
        }
        Ok(entry)
    }

    /// The entry signed by each of `keys`, which become its signers.
    pub fn sign(&self, keys: &[(DeviceId, &SigningKey)]) -> Result<SignedEntry, Error> {
        let mut keys = keys.to_vec();
        keys.sort_by_key(|(id, _)| id.0);
        let mut entry = self.clone();
        entry.signers = keys.iter().map(|(id, _)| *id).collect();
        let bytes = entry.encode()?;
        let message = signed_message(&bytes);
        let signatures = keys.iter().map(|(id, k)| (*id, k.sign(&message))).collect();
        Ok(SignedEntry { bytes, signatures })
    }
}

fn signed_message(bytes: &[u8]) -> Vec<u8> {
    [SIGNATURE_LABEL, bytes].concat()
}

/// Characters a device name or os may not hold: what a terminal would act
/// on, reorder or hide. The bidirectional set is pairing's; zero-width
/// characters and line and paragraph separators are refused here as well.
fn forbidden(c: char) -> bool {
    c.is_control() || matches!(c, '\u{061C}' | '\u{200B}'..='\u{200F}' | '\u{2028}'..='\u{202E}' | '\u{2060}'..='\u{2069}' | '\u{FEFF}')
}

fn check_text(seq: u64, what: &str, s: &str, min: usize, max: usize) -> Result<(), Error> {
    if s.len() < min || s.len() > max {
        return Err(refuse(seq, format!("a device {what} is {min}–{max} bytes")));
    }
    if s.chars().any(forbidden) {
        return Err(refuse(seq, format!("a device {what} has a control, bidirectional or invisible character")));
    }
    Ok(())
}

fn put_subject(b: &mut Vec<u8>, s: &Subject, seq: u64) -> Result<(), Error> {
    check_text(seq, "name", &s.name, 1, NAME_MAX)?;
    check_text(seq, "os", &s.os, 0, OS_MAX)?;
    b.push(s.kind as u8);
    for t in [&s.name, &s.os] {
        b.extend_from_slice(&(t.len() as u16).to_be_bytes());
        b.extend_from_slice(t.as_bytes());
    }
    b.extend_from_slice(&s.device.0);
    b.extend_from_slice(&s.signing.0);
    Ok(())
}

struct Reader<'a> {
    b: &'a [u8],
    seq: u64,
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], Error> {
        if self.b.len() < n {
            return Err(refuse(self.seq, "the entry is cut short"));
        }
        let (head, rest) = self.b.split_at(n);
        self.b = rest;
        Ok(head)
    }

    fn u8(&mut self) -> Result<u8, Error> {
        Ok(self.take(1)?[0])
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], Error> {
        Ok(self.take(N)?.try_into().expect("N bytes"))
    }

    fn text(&mut self) -> Result<String, Error> {
        let n = u16::from_be_bytes(self.array()?) as usize;
        let seq = self.seq;
        String::from_utf8(self.take(n)?.to_vec()).map_err(|_| refuse(seq, "a device name or os is not UTF-8"))
    }

    fn subject(&mut self) -> Result<Subject, Error> {
        let kind = match self.u8()? {
            1 => Kind::Device,
            2 => Kind::Recovery,
            k => return Err(refuse(self.seq, format!("unknown device kind {k}"))),
        };
        let name = self.text()?;
        let os = self.text()?;
        Ok(Subject { kind, name, os, device: DevicePublic(self.array()?), signing: SigningPublic(self.array()?) })
    }
}

/// An entry as the registry holds and serves it: its canonical bytes and
/// the signatures, keyed by signer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedEntry {
    pub bytes: Vec<u8>,
    pub signatures: Vec<(DeviceId, [u8; 64])>,
}

impl SignedEntry {
    /// What the next entry's `prev` and a pin hold.
    pub fn hash(&self) -> [u8; 32] {
        Sha256::digest(&self.bytes).into()
    }

    /// The signature list as the wire carries it: `count u8`, then `id [16]
    /// ‖ signature [64]` each.
    pub fn signatures_bytes(&self) -> Vec<u8> {
        let mut b = vec![self.signatures.len() as u8];
        for (id, sig) in &self.signatures {
            b.extend_from_slice(&id.0);
            b.extend_from_slice(sig);
        }
        b
    }

    /// An entry from the bytes and the signature list as the wire carries
    /// them; nothing is verified until it is extended onto a chain.
    pub fn from_parts(bytes: Vec<u8>, signatures: &[u8]) -> Result<SignedEntry, Error> {
        let bad = || Error("the device list entry's signatures are malformed".into());
        let (&n, rest) = signatures.split_first().ok_or_else(bad)?;
        if rest.len() != n as usize * 80 {
            return Err(bad());
        }
        let signatures = rest.chunks(80).map(|c| (DeviceId(c[..16].try_into().expect("16 bytes")), c[16..].try_into().expect("64 bytes"))).collect();
        Ok(SignedEntry { bytes, signatures })
    }
}

/// The head of a verified chain: what a client pins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Head {
    pub seq: u64,
    pub hash: [u8; 32],
}

/// A device on the list, as of the head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    pub kind: Kind,
    pub name: String,
    pub os: String,
    pub device: DevicePublic,
    pub signing: SigningPublic,
    /// The seq of the entry that added it.
    pub added: u64,
}

impl Device {
    pub fn id(&self) -> DeviceId {
        self.device.id()
    }

    fn matches(&self, s: &Subject) -> bool {
        self.kind == s.kind && self.name == s.name && self.os == s.os && self.device == s.device && self.signing == s.signing
    }
}

/// Which current devices may authorize an action: only the recovery
/// device cancels a pending recovery rotation; any device on the list
/// authorizes the rest. Whether a `rotate-recovery` takes effect at once
/// or waits is state, not eligibility (`Chain::step`).
fn may_authorize(action: Action, signer: &Device) -> bool {
    action != Action::CancelRecoveryRotation || signer.kind == Kind::Recovery
}

/// How long a `rotate-recovery` a device authorized, rather than the
/// recovery device itself, waits before it can take effect.
pub const RECOVERY_ROTATION_DELAY: u64 = 7 * 24 * 60 * 60;

/// How far a new entry's `time` may fall before the moment this device
/// last verified its head, for clocks that disagree, before the entry is
/// refused as backdated.
pub const CLOCK_SKEW: u64 = 60 * 60;

/// What this device knows about the list before it verifies it, and so
/// what a recovery rotation's delay is counted from. An entry's `time` is
/// written by its signer and never counted from alone: a thief would
/// backdate it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trust {
    /// A device that has verified this list before: its pinned head, when
    /// (by its own clock) it last verified it, and, for a pending rotation
    /// it saw then, `(seq, since)` from `Chain::pending`. An entry after
    /// the pin whose `time` is more than `CLOCK_SKEW` before `verified_at`
    /// is refused as backdated. A pending rotation's delay runs from when
    /// this device first saw it: `first_seen` if it matches, else now.
    Pinned { head: Head, verified_at: u64, first_seen: Option<(u64, u64)> },
    /// A fresh machine with no pin — recovery. Its delay runs from the
    /// registry's receipt time for each entry, `received_at[seq]`, as the
    /// registry returns it. Under a hostile registry that is no control at
    /// all: the delay defends against a thief, as the fresh sign-in does,
    /// and not against the registry.
    Fresh { received_at: Vec<u64> },
}
/// What `Chain::verify_prefix` returns: the chain as far as every entry
/// verified, and the first entry it refused, if any, with why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified {
    pub chain: Chain,
    pub rejected: Option<(u64, Error)>,
}

/// A `rotate-recovery` a device authorized, waiting out its delay. Until it
/// takes effect the current recovery device stays the recovery device —
/// wrapped to, able to sign, unremovable — and the new one is nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    /// The recovery device proposed.
    pub subject: Subject,
    /// The device that authorized it: removing it voids the rotation.
    pub authorizer: DeviceId,
    /// The entry that proposed it.
    pub seq: u64,
    /// The entry's own time, as its signer's clock read it.
    pub time: u64,
    /// When the delay started for this device: when it first saw the entry
    /// (pinned), or the later of the entry's time and the registry's
    /// receipt time (fresh).
    pub since: u64,
    /// When, by this device's clock, the rotation may be completed:
    /// `since` + `RECOVERY_ROTATION_DELAY`. Clock skew between devices
    /// moves this by the skew and no more; the registry cannot move it on
    /// a pinned device.
    pub effective_at: u64,
}

/// What an entry does to the key: nothing, a rotation, or a pending
/// recovery rotation, which changes nothing yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Plain,
    Rotate,
    Propose,
}

/// Who must sign an entry: the subjects that prove possession of their
/// own keys, and how many authorizers from the list besides them.
fn required_signers(e: &Entry) -> (&[Subject], usize) {
    match (e.seq, e.action) {
        (0, _) => (&e.subjects[..], 0),
        (_, Action::RotateRecovery) => (&e.subjects[..], 1),
        (_, Action::CancelRecoveryRotation) => (&[], 1),
        _ => (&[], 1),
    }
}

/// A change in a batch (`Chain::batch`).
pub enum Change<'a> {
    Add(Subject),
    Remove(Subject),
    /// The new recovery device, and its signing key for its own signature.
    RotateRecovery(Subject, &'a SigningKey),
    /// Signed by the recovery device: voids the pending rotation to this one.
    CancelRecoveryRotation(Subject),
}

/// What a batch or a new chain posts, all at once: the signed entries, the
/// newest user key, each generation it made wrapped under the next
/// (`links`, oldest first), and the newest wrapped to devices (`wraps`).
pub struct Batch {
    pub entries: Vec<SignedEntry>,
    pub newest: UserKey,
    pub links: Vec<Vec<u8>>,
    pub wraps: Vec<(DeviceId, Vec<u8>)>,
}

/// A chain verified up to its head: the devices on it now, and the user
/// key generation and id it leaves current.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chain {
    head: Head,
    /// Every generation's key id, generation 1 first.
    key_ids: Vec<UserKeyId>,
    devices: Vec<Device>,
    /// Every X25519 and Ed25519 key this chain has ever added, so none is
    /// added twice, a removed device included.
    seen: Vec<[u8; 32]>,
    pending: Option<Pending>,
    trust: Trust,
    /// This device's clock, Unix seconds.
    now: u64,
}

impl Chain {
    /// Verifies a whole chain from seq 0, by this device's clock `now`.
    /// With a pin, it must also reach at least the pin's seq and hold the
    /// pinned hash there: a shorter chain is an older list, another hash a
    /// forked one, and both are refused. Any entry refused refuses the lot;
    /// `verify_prefix` says how far it got.
    pub fn verify(entries: &[SignedEntry], trust: Trust, now: u64) -> Result<Chain, Error> {
        let v = Chain::verify_prefix(entries, trust, now)?;
        match v.rejected {
            Some((_, e)) => Err(e),
            None => Ok(v.chain),
        }
    }

    /// As `verify`, but an entry after the pin that is refused ends the
    /// chain there rather than refusing it all: the chain up to it comes
    /// back with the seq and the reason. An entry at or before the pin, a
    /// shorter or forked list, or a bad entry 0 still refuses everything.
    ///
    /// This is how a client that was offline through a recovery rotation's
    /// whole delay, and so receives the proposal and its completion at once,
    /// gets past it: the completion is refused (seen just now), the prefix
    /// ends with the proposal pending, and the client pins that prefix and
    /// stores `(pending.seq, pending.since)` as `first_seen`. Seven days on
    /// by its own clock, the completion verifies. Recovery on a fresh
    /// machine works from the prefix the same way, so the kit can cancel a
    /// rotation whose completion it cannot yet accept. A client only ever
    /// wraps to, adopts keys from and pins the prefix. When it pins a
    /// prefix whose tail was refused it keeps its `verified_at` as it was:
    /// the tail it saw is older than now, and should not read as backdated
    /// when it verifies again.
    pub fn verify_prefix(entries: &[SignedEntry], trust: Trust, now: u64) -> Result<Verified, Error> {
        let (first, rest) = entries.split_first().ok_or_else(|| Error("the device list is empty".into()))?;
        let pin = match &trust {
            Trust::Pinned { head, .. } => Some(*head),
            Trust::Fresh { .. } => None,
        };
        if let Some(pin) = pin
            && (entries.len() as u64) <= pin.seq
        {
            return Err(Error(format!("the device list ends at entry {}, older than entry {} this device last verified — the registry served an old list", entries.len() - 1, pin.seq)));
        }
        let pinned = |c: &Chain| match pin {
            Some(p) if c.head.seq == p.seq && c.head.hash != p.hash => Err(Error(format!("the device list differs at entry {} from the one this device last verified — the registry served a forked list", p.seq))),
            _ => Ok(()),
        };
        let mut chain = Chain::genesis(first, trust, now)?;
        pinned(&chain)?;
        for (i, e) in rest.iter().enumerate() {
            let seq = i as u64 + 1;
            match chain.extend(e) {
                Ok(next) => chain = next,
                Err(err) if pin.is_none_or(|p| seq > p.seq) => return Ok(Verified { chain, rejected: Some((seq, err)) }),
                Err(err) => return Err(err),
            }
            pinned(&chain)?;
        }
        Ok(Verified { chain, rejected: None })
    }

    /// Verifies seq 0 alone; `trust` and `now` are kept for what follows.
    pub fn genesis(entry: &SignedEntry, trust: Trust, now: u64) -> Result<Chain, Error> {
        let e = Entry::decode(&entry.bytes)?;
        if e.seq != 0 {
            return Err(refuse(e.seq, "the list does not start at entry 0"));
        }
        if e.prev != [0; 32] {
            return Err(refuse(0, "entry 0 names a previous entry"));
        }
        if e.action != Action::Add || e.subjects[0].kind != Kind::Device {
            return Err(refuse(0, "entry 0 must add the first device"));
        }
        if e.subjects.get(1).is_some_and(|r| r.kind != Kind::Recovery) {
            return Err(refuse(0, "entry 0's second device must be the recovery device"));
        }
        if e.generation != 1 {
            return Err(refuse(0, "entry 0 must leave user key generation 1"));
        }
        let mut chain = Chain { head: Head { seq: 0, hash: entry.hash() }, key_ids: vec![e.key_id], devices: Vec::new(), seen: Vec::new(), pending: None, trust, now };
        chain.check_signatures(&e, entry)?;
        for s in &e.subjects {
            chain.add(s, 0)?;
        }
        Ok(chain)
    }

    /// This chain with one more entry, or refused — the chain it was is
    /// unchanged either way. This is how a pinned client checks what came
    /// after its head: the entry must extend exactly this head.
    pub fn extend(&self, entry: &SignedEntry) -> Result<Chain, Error> {
        let e = Entry::decode(&entry.bytes)?;
        let seq = e.seq;
        if Some(seq) != self.head.seq.checked_add(1) {
            return Err(refuse(seq, format!("it does not follow entry {}", self.head.seq)));
        }
        if e.prev != self.head.hash {
            return Err(refuse(seq, format!("it does not extend entry {} as this device verified it — a forked list", self.head.seq)));
        }
        if e.subjects.len() != 1 {
            return Err(refuse(seq, "only entry 0 is about two devices"));
        }
        // Entries at or before the pin were checked when this device first
        // verified them; neither the backdating nor the delay check runs on
        // them again.
        if let Trust::Pinned { head, verified_at, .. } = &self.trust
            && seq > head.seq
            && e.time.saturating_add(CLOCK_SKEW) < *verified_at
        {
            return Err(refuse(seq, "its time is before this device last verified the list — it is backdated"));
        }
        let authorizer = self.check_signatures(&e, entry)?;
        let subject = &e.subjects[0];
        let step = self.step(e.action, &authorizer, subject, seq)?;
        let mut next = self.clone();
        next.head = Head { seq, hash: entry.hash() };
        match e.action {
            Action::Add => {
                if subject.kind != Kind::Device {
                    return Err(refuse(seq, "a recovery device is added only by rotate-recovery"));
                }
                // Nor may a device take the pending recovery device's keys or
                // name, which would jam the rotation's completion.
                if let Some(p) = &self.pending
                    && ([subject.device.0, subject.signing.0].iter().any(|k| *k == p.subject.device.0 || *k == p.subject.signing.0) || subject.name.to_lowercase() == p.subject.name.to_lowercase())
                {
                    return Err(refuse(seq, "it adds the keys or name of the recovery device a rotation is pending to"));
                }
                next.add(subject, seq)?;
            }
            Action::Remove => {
                let at = next.devices.iter().position(|d| d.id() == subject.id()).ok_or_else(|| refuse(seq, "it removes a device that is not on the list"))?;
                if next.devices[at].kind == Kind::Recovery || subject.kind == Kind::Recovery {
                    return Err(refuse(seq, "the recovery device cannot be removed, only replaced by rotate-recovery"));
                }
                if !next.devices[at].matches(subject) {
                    return Err(refuse(seq, "it names a device other than the one on the list"));
                }
                next.devices.remove(at);
                // Removing the device that proposed a recovery rotation voids it.
                if next.pending.as_ref().is_some_and(|p| p.authorizer == subject.id()) {
                    next.pending = None;
                }
            }
            Action::RotateRecovery if step == Step::Propose => {
                if subject.kind != Kind::Recovery {
                    return Err(refuse(seq, "rotate-recovery must add a recovery device"));
                }
                if next.seen.contains(&subject.device.0) || next.seen.contains(&subject.signing.0) || subject.device.0 == subject.signing.0 {
                    return Err(refuse(seq, "it adds a key this list has held before"));
                }
                // The kit it would replace aside, its name may not be one a
                // device on the list has, as it may not be at completion.
                if next.devices.iter().any(|d| d.kind != Kind::Recovery && d.name.to_lowercase() == subject.name.to_lowercase()) {
                    return Err(refuse(seq, format!("a device on the list is already called {:?}", subject.name)));
                }
                let since = match &self.trust {
                    Trust::Pinned { first_seen: Some((s, t)), .. } if *s == seq => *t,
                    Trust::Pinned { .. } => self.now,
                    Trust::Fresh { received_at } => e.time.max(*received_at.get(seq as usize).ok_or_else(|| refuse(seq, "the registry gave no receipt time for it"))?),
                };
                let effective_at = since.checked_add(RECOVERY_ROTATION_DELAY).ok_or_else(|| refuse(seq, "its time is out of range"))?;
                next.pending = Some(Pending { subject: subject.clone(), authorizer: authorizer.expect("rotate-recovery has an authorizer"), seq, time: e.time, since, effective_at });
            }
            Action::RotateRecovery => {
                if subject.kind != Kind::Recovery {
                    return Err(refuse(seq, "rotate-recovery must add a recovery device"));
                }
                next.devices.retain(|d| d.kind != Kind::Recovery);
                next.add(subject, seq)?;
                next.pending = None;
            }
            Action::CancelRecoveryRotation => {
                if next.pending.as_ref().map(|p| &p.subject) != Some(subject) {
                    return Err(refuse(seq, "it cancels a recovery rotation that is not pending"));
                }
                next.pending = None;
            }
        }
        let g = self.generation();
        if step == Step::Rotate {
            if Some(e.generation) != g.checked_add(1) {
                return Err(refuse(seq, format!("it leaves user key generation {}, and a {:?} must leave {}", e.generation, e.action, g as u64 + 1)));
            }
            if next.key_ids.contains(&e.key_id) {
                return Err(refuse(seq, "it rotates to a user key id this list has held before"));
            }
            next.key_ids.push(e.key_id);
        } else if e.generation != g || e.key_id != self.key_id() {
            return Err(refuse(seq, format!("it must leave user key generation {g} and its key id as they were")));
        }
        Ok(next)
    }

    /// What an entry does to the key, from the state before it. A
    /// `rotate-recovery` takes effect at once when there is no recovery
    /// device yet or the recovery device authorized it; authorized by any
    /// other device it is only proposed, and a second one naming the same
    /// recovery device completes it once the delay has passed by this
    /// device's clock. One rotation is pending at a time.
    fn step(&self, action: Action, authorizer: &Option<DeviceId>, subject: &Subject, seq: u64) -> Result<Step, Error> {
        Ok(match action {
            Action::Add | Action::CancelRecoveryRotation => Step::Plain,
            Action::Remove => Step::Rotate,
            Action::RotateRecovery => {
                let by_recovery = self.recovery().is_some_and(|r| Some(r.id()) == *authorizer);
                match (&self.pending, self.recovery().is_none() || by_recovery) {
                    (_, true) => Step::Rotate,
                    (Some(p), false) if p.subject == *subject => {
                        match &self.trust {
                            Trust::Pinned { head, .. } if seq <= head.seq => {}
                            _ if self.now < p.effective_at => {
                                return Err(refuse(seq, format!("the recovery rotation proposed at entry {} takes effect only at {} by this device's clock", p.seq, p.effective_at)));
                            }
                            // A fresh machine also needs the registry to have
                            // received the completion no earlier than the
                            // delay allows: judged by its own clock alone, a
                            // completion posted on day one would pass on day
                            // eight.
                            Trust::Fresh { received_at } if received_at.get(seq as usize).is_none_or(|r| *r < p.effective_at) => {
                                return Err(refuse(seq, format!("the registry received the completion of the recovery rotation proposed at entry {} before its delay was over", p.seq)));
                            }
                            _ => {}
                        }
                        Step::Rotate
                    }
                    (Some(p), false) => return Err(refuse(seq, format!("a recovery rotation is already pending, from entry {}", p.seq))),
                    (None, false) => Step::Propose,
                }
            }
        })
    }

    /// The signers an entry needs, then each signature: see the module doc.
    /// `self` is the chain before the entry.
    fn check_signatures(&self, e: &Entry, entry: &SignedEntry) -> Result<Option<DeviceId>, Error> {
        let seq = e.seq;
        let (possession, authorizers) = required_signers(e);
        let ids: Vec<DeviceId> = entry.signatures.iter().map(|(id, _)| *id).collect();
        if ids != e.signers {
            return Err(refuse(seq, "its signatures are not the signers it names"));
        }
        let mut keys = Vec::new();
        for s in possession {
            if !e.signers.contains(&s.id()) {
                return Err(refuse(seq, format!("the device it adds, {:?}, has not signed it", s.name)));
            }
            keys.push((s.id(), s.signing));
        }
        let others: Vec<DeviceId> = e.signers.iter().filter(|id| !keys.iter().any(|(k, _)| k == *id)).copied().collect();
        if others.len() != authorizers {
            return Err(refuse(seq, if authorizers == 0 { "it has a signer beyond the devices it adds" } else { "it needs exactly one signer already on the list" }));
        }
        let authorizer = others.first().copied();
        for id in others {
            let d = self.devices.iter().find(|d| d.id() == id).ok_or_else(|| refuse(seq, "its signer is not a device on the list"))?;
            if !may_authorize(e.action, d) {
                return Err(refuse(seq, format!("{:?} may not sign a {:?}", d.name, e.action)));
            }
            keys.push((id, d.signing));
        }
        let message = signed_message(&entry.bytes);
        for (id, sig) in &entry.signatures {
            let key = keys.iter().find(|(k, _)| k == id).expect("every signer has a key").1;
            key.verify(&message, sig).map_err(|_| refuse(seq, "a signature does not verify"))?;
        }
        Ok(authorizer)
    }

    fn add(&mut self, s: &Subject, seq: u64) -> Result<(), Error> {
        if self.seen.contains(&s.device.0) || self.seen.contains(&s.signing.0) || s.device.0 == s.signing.0 {
            return Err(refuse(seq, "it adds a key this list has held before"));
        }
        if self.devices.iter().any(|d| d.name.to_lowercase() == s.name.to_lowercase()) {
            return Err(refuse(seq, format!("a device on the list is already called {:?}", s.name)));
        }
        self.seen.extend([s.device.0, s.signing.0]);
        self.devices.push(Device { kind: s.kind, name: s.name.clone(), os: s.os.clone(), device: s.device, signing: s.signing, added: seq });
        Ok(())
    }

    /// The head to pin.
    pub fn head(&self) -> Head {
        self.head
    }

    /// The user key generation the chain leaves current.
    pub fn generation(&self) -> u32 {
        self.key_ids.len() as u32
    }

    /// The current generation's key id: the only id a device adopts a
    /// wrapped user key under (`UserKey::unwrap`).
    pub fn key_id(&self) -> UserKeyId {
        *self.key_ids.last().expect("a chain has generation 1")
    }

    /// Generation `generation`'s key id, as the chain committed to it.
    pub fn key_id_at(&self, generation: u32) -> Option<UserKeyId> {
        self.key_ids.get((generation as usize).checked_sub(1)?).copied()
    }

    /// Every device on the list, the recovery device included: exactly the
    /// devices the current user key is wrapped to.
    pub fn devices(&self) -> &[Device] {
        &self.devices
    }

    pub fn recovery(&self) -> Option<&Device> {
        self.devices.iter().find(|d| d.kind == Kind::Recovery)
    }

    /// A recovery rotation waiting out its delay, for every device to warn
    /// about until it is completed, cancelled or voided. The caller stores
    /// `(seq, since)` as its `Trust::Pinned::first_seen` the first time it
    /// sees one.
    pub fn pending(&self) -> Option<&Pending> {
        self.pending.as_ref()
    }

    /// This chain judged by a later reading of this device's clock, as a
    /// long-running client does before completing a pending rotation.
    pub fn at(&self, now: u64) -> Chain {
        Chain { now, ..self.clone() }
    }

    /// The next entry after this head, unsigned, carrying the current
    /// generation and key id; a rotation's caller sets both.
    pub fn next_entry(&self, action: Action, subject: Subject, time: u64) -> Entry {
        Entry { seq: self.head.seq + 1, prev: self.head.hash, action, subjects: vec![subject], generation: self.generation(), key_id: self.key_id(), time, signers: Vec::new() }
    }

    /// A new chain: seq 0 adding the first device and, if the person made a
    /// kit, the recovery device, each signing it; generation 1 made and
    /// wrapped to both.
    pub fn start(first: Subject, first_key: &SigningKey, recovery: Option<(Subject, &SigningKey)>, time: u64) -> Result<(Chain, Batch), Error> {
        let newest = UserKey::first();
        let mut subjects = vec![first.clone()];
        let mut keys = vec![(first.id(), first_key)];
        if let Some((r, k)) = &recovery {
            subjects.push(r.clone());
            keys.push((r.id(), *k));
        }
        let entry = Entry { seq: 0, prev: [0; 32], action: Action::Add, subjects, generation: 1, key_id: newest.id(), time, signers: Vec::new() }.sign(&keys)?;
        let head = Head { seq: 0, hash: entry.hash() };
        let chain = Chain::genesis(&entry, Trust::Pinned { head, verified_at: time, first_seen: None }, time)?;
        let wraps = chain.devices.iter().map(|d| Ok((d.id(), newest.wrap_to(&d.device)?))).collect::<Result<_, Error>>()?;
        Ok((chain, Batch { entries: vec![entry], newest, links: Vec::new(), wraps }))
    }

    /// Changes made as one post at `time`, this device's clock, signed by
    /// `signer`, a device on the list
    /// that stays on it, holding `held`, the current generation. Adds, a
    /// proposed recovery rotation and a cancellation leave the generation;
    /// each removal and each rotate-recovery that takes effect makes the
    /// next. A proposed recovery device is wrapped nothing.
    /// Only the last generation is wrapped to devices — every device left
    /// on the list when anything rotated, else just the devices added —
    /// and each earlier generation is wrapped under the one after it. Each
    /// entry is verified onto the chain as it is made, so a batch this
    /// returns is one every client accepts.
    pub fn batch(&self, held: &UserKey, changes: Vec<Change<'_>>, signer: DeviceId, key: &SigningKey, time: u64) -> Result<(Chain, Batch), Error> {
        if held.generation() != self.generation() || held.id() != self.key_id() {
            return Err(Error("the user key held is not the one the device list leaves current".into()));
        }
        // This device makes these entries now, so it judges them as a pinned
        // device at its own head; a registry's receipt times play no part.
        let mut chain = Chain { trust: Trust::Pinned { head: self.head, verified_at: 0, first_seen: None }, now: time, ..self.clone() };
        let mut current = held.clone();
        let (mut entries, mut links, mut added) = (Vec::new(), Vec::new(), Vec::new());
        for change in changes {
            let (action, subject, own) = match change {
                Change::Add(s) => (Action::Add, s, None),
                Change::Remove(s) => (Action::Remove, s, None),
                Change::RotateRecovery(s, k) => (Action::RotateRecovery, s, Some(k)),
                Change::CancelRecoveryRotation(s) => (Action::CancelRecoveryRotation, s, None),
            };
            let mut entry = chain.next_entry(action, subject.clone(), time);
            let mut next = None;
            if chain.step(action, &Some(signer), &subject, entry.seq)? == Step::Rotate {
                let n = current.next()?;
                entry.generation = n.generation();
                entry.key_id = n.id();
                next = Some(n);
            }
            let mut keys = vec![(signer, key)];
            if let Some(k) = own {
                keys.push((subject.id(), k));
            }
            let signed = entry.sign(&keys)?;
            chain = chain.extend(&signed)?;
            entries.push(signed);
            if let Some(n) = next {
                links.push(n.wrap_previous(&current)?);
                current = n;
            }
            if action == Action::Add {
                added.push(subject.id());
            }
        }
        // The chain handed back is pinned at its new head as of `time`: this
        // device made and verified every entry in it, and whatever it
        // extends next is judged as a pinned device judges it — backdating,
        // and the delay from first sight — never by the trust it was built
        // under, which for a fresh machine would drop the receipt check.
        chain.trust = Trust::Pinned { head: chain.head, verified_at: time, first_seen: chain.pending.as_ref().map(|p| (p.seq, p.since)) };
        chain.now = time;
        let rotated = !links.is_empty();
        let wraps = chain.devices.iter().filter(|d| rotated || added.contains(&d.id())).map(|d| Ok((d.id(), current.wrap_to(&d.device)?))).collect::<Result<_, Error>>()?;
        Ok((chain, Batch { entries, newest: current, links, wraps }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::e2e::{self, DeviceKey};
    use crate::recovery::RecoveryKit;
    use crate::user_key::UserKeys;

    const T: u64 = 1_790_000_000;

    /// This device's clock in most tests: a little after every entry.
    const NOW: u64 = T + 100;

    /// A fresh machine, the registry saying it received every entry at T.
    fn fresh() -> Trust {
        Trust::Fresh { received_at: vec![T; 64] }
    }

    fn gen0(e: &SignedEntry) -> Result<Chain, Error> {
        Chain::genesis(e, fresh(), NOW)
    }

    /// A device that last verified `head` at T.
    fn pinned(head: Head) -> Trust {
        Trust::Pinned { head, verified_at: T, first_seen: None }
    }

    struct Dev {
        key: DeviceKey,
        signing: SigningKey,
        name: &'static str,
        kind: Kind,
    }

    impl Dev {
        fn new(name: &'static str) -> Dev {
            Dev { key: DeviceKey::generate(), signing: SigningKey::generate(), name, kind: Kind::Device }
        }

        fn recovery(kit: &RecoveryKit) -> Dev {
            let d = kit.device();
            Dev { key: d.key, signing: d.signing, name: "recovery kit", kind: Kind::Recovery }
        }

        fn subject(&self) -> Subject {
            Subject { kind: self.kind, name: self.name.into(), os: if self.kind == Kind::Device { "linux".into() } else { String::new() }, device: self.key.public(), signing: self.signing.public() }
        }

        fn id(&self) -> DeviceId {
            self.key.id()
        }

        fn signs(&self) -> (DeviceId, &SigningKey) {
            (self.id(), &self.signing)
        }
    }

    /// A laptop and a recovery kit at seq 0, then a phone added by the
    /// laptop; the user key at generation 1.
    fn three() -> (Dev, Dev, Dev, Vec<SignedEntry>, UserKey) {
        let laptop = Dev::new("laptop");
        let kit = Dev::recovery(&RecoveryKit::generate());
        let phone = Dev::new("phone");
        let (chain, start) = Chain::start(laptop.subject(), &laptop.signing, Some((kit.subject(), &kit.signing)), T).unwrap();
        let (_, add) = chain.batch(&start.newest, vec![Change::Add(phone.subject())], laptop.id(), &laptop.signing, T + 1).unwrap();
        let entries = [start.entries, add.entries].concat();
        (laptop, kit, phone, entries, start.newest)
    }

    fn chain(entries: &[SignedEntry]) -> Chain {
        Chain::verify(entries, fresh(), NOW).unwrap()
    }

    /// An entry after `entries`, signed by `by`, adjusted by `edit` first.
    fn signed(entries: &[SignedEntry], action: Action, subject: Subject, by: &[&Dev], edit: impl FnOnce(&mut Entry)) -> SignedEntry {
        let mut e = chain(entries).next_entry(action, subject, T + 10);
        edit(&mut e);
        e.sign(&by.iter().map(|d| d.signs()).collect::<Vec<_>>()).unwrap()
    }

    /// A rotation to a fresh key id one generation up.
    fn rotate(e: &mut Entry) {
        e.generation += 1;
        e.key_id = UserKey::from_bytes(e.generation, e2e::random()).unwrap().id();
    }

    #[test]
    fn d1_a_chain_verifies_from_seq_0_to_its_devices_generation_and_key_id() {
        let (laptop, kit, phone, entries, g1) = three();
        let c = chain(&entries);
        assert_eq!((c.generation(), c.key_id()), (1, g1.id()));
        assert_eq!(c.devices().iter().map(Device::id).collect::<Vec<_>>(), [laptop.id(), kit.id(), phone.id()]);
        assert_eq!(c.recovery().unwrap().id(), kit.id());
        assert_eq!(c.head(), Head { seq: 1, hash: entries[1].hash() });
        // The wire form of the signatures round-trips.
        let back = SignedEntry::from_parts(entries[0].bytes.clone(), &entries[0].signatures_bytes()).unwrap();
        assert_eq!(back, entries[0]);
        assert_eq!(entries[0].signatures.len(), 2, "the first device and the kit");
    }

    #[test]
    fn d1_seq_0_must_be_signed_by_every_device_it_adds() {
        let (laptop, other) = (Dev::new("laptop"), Dev::new("other"));
        let kit = Dev::recovery(&RecoveryKit::generate());
        let id = UserKey::first().id();
        let genesis = |subjects: Vec<Subject>| Entry { seq: 0, prev: [0; 32], action: Action::Add, subjects, generation: 1, key_id: id, time: T, signers: Vec::new() };
        let e = genesis(vec![laptop.subject()]);
        assert!(gen0(&e.sign(&[laptop.signs()]).unwrap()).is_ok());
        assert!(gen0(&e.sign(&[other.signs()]).unwrap()).unwrap_err().0.contains("has not signed"));
        assert!(gen0(&e.sign(&[laptop.signs(), other.signs()]).unwrap()).unwrap_err().0.contains("beyond"));
        // A forged signature under the right id.
        let mut forged = e.sign(&[laptop.signs()]).unwrap();
        forged.signatures[0].1 = other.signing.sign(&signed_message(&forged.bytes));
        assert!(gen0(&forged).unwrap_err().0.contains("does not verify"));
        // With a kit, the kit signs too.
        let e = genesis(vec![laptop.subject(), kit.subject()]);
        assert!(gen0(&e.sign(&[laptop.signs()]).unwrap()).unwrap_err().0.contains("has not signed"));
        assert!(gen0(&e.sign(&[laptop.signs(), kit.signs()]).unwrap()).is_ok());
        let mut g2 = genesis(vec![laptop.subject()]);
        g2.generation = 2;
        assert!(gen0(&g2.sign(&[laptop.signs()]).unwrap()).is_err());
        // A kit alone cannot start a list, nor a second device of kind device.
        assert!(gen0(&genesis(vec![kit.subject()]).sign(&[kit.signs()]).unwrap()).is_err());
        assert!(gen0(&genesis(vec![laptop.subject(), other.subject()]).sign(&[laptop.signs(), other.signs()]).unwrap()).is_err());
        assert!(Chain::verify(&[], fresh(), NOW).is_err());
    }

    /// C2 (review of #199): the registry knows the recovery device's public
    /// keys and the person's device names, and builds a chain of its own
    /// around them. Without the kit's words it cannot sign as the kit, so
    /// a fresh machine recovering refuses it.
    #[test]
    fn d1_a_chain_the_registry_builds_around_the_real_kit_is_refused() {
        let kit = Dev::recovery(&RecoveryKit::generate());
        let fake = Dev::new("laptop");
        let e = Entry { seq: 0, prev: [0; 32], action: Action::Add, subjects: vec![fake.subject(), kit.subject()], generation: 1, key_id: UserKey::first().id(), time: T, signers: Vec::new() };
        assert!(gen0(&e.sign(&[fake.signs()]).unwrap()).is_err());
        // Nor can it claim the kit signed, with a signature of its own.
        let claimed = e.sign(&[fake.signs(), (kit.id(), &fake.signing)]).unwrap();
        assert!(gen0(&claimed).unwrap_err().0.contains("does not verify"));
        // Nor slip in a new kit by rotate-recovery without that kit's signature.
        let (laptop, _kit, _phone, entries, _) = three();
        let other_kit = Dev::recovery(&RecoveryKit::generate());
        let e = signed(&entries, Action::RotateRecovery, other_kit.subject(), &[&laptop], rotate);
        assert!(chain(&entries).extend(&e).unwrap_err().0.contains("has not signed"));
    }

    #[test]
    fn d1_a_forged_signer_is_refused() {
        let (laptop, _kit, phone, entries, _) = three();
        let thief = Dev::new("thief");
        let c = chain(&entries);
        // Signed by a key not on the list, claiming the laptop's id.
        let e = c.next_entry(Action::Add, thief.subject(), T).sign(&[(laptop.id(), &thief.signing)]).unwrap();
        assert!(c.extend(&e).unwrap_err().0.contains("does not verify"));
        // Signed by a device not on the list, as itself.
        let e = c.next_entry(Action::Add, thief.subject(), T).sign(&[thief.signs()]).unwrap();
        assert!(c.extend(&e).unwrap_err().0.contains("not a device on the list"));
        // Signatures that are not the signers the bytes name.
        let mut e = c.next_entry(Action::Add, thief.subject(), T).sign(&[laptop.signs()]).unwrap();
        e.signatures.push((thief.id(), [0; 64]));
        assert!(c.extend(&e).unwrap_err().0.contains("not the signers"));
        // Two authorizers where one is needed.
        let e = c.next_entry(Action::Add, thief.subject(), T).sign(&[laptop.signs(), phone.signs()]).unwrap();
        assert!(c.extend(&e).unwrap_err().0.contains("exactly one"));
    }

    #[test]
    fn d1_a_removed_device_can_no_longer_sign() {
        let (laptop, _kit, phone, mut entries, _) = three();
        entries.push(signed(&entries, Action::Remove, laptop.subject(), &[&phone], rotate));
        let c = chain(&entries);
        let thief = Dev::new("thief");
        let e = c.next_entry(Action::Add, thief.subject(), T).sign(&[laptop.signs()]).unwrap();
        assert!(c.extend(&e).unwrap_err().0.contains("not a device on the list"));
        // Nor can it come back under its old keys.
        let e = c.next_entry(Action::Add, laptop.subject(), T).sign(&[phone.signs()]).unwrap();
        assert!(c.extend(&e).unwrap_err().0.contains("held before"));
    }

    /// The rule a stolen laptop would otherwise break first.
    #[test]
    fn d1_the_recovery_device_cannot_be_removed_only_rotated() {
        let (laptop, kit, _phone, mut entries, _) = three();
        let c = chain(&entries);
        let e = signed(&entries, Action::Remove, kit.subject(), &[&laptop], rotate);
        assert!(c.extend(&e).unwrap_err().0.contains("cannot be removed"));
        let mut disguised = kit.subject();
        disguised.kind = Kind::Device;
        let e = signed(&entries, Action::Remove, disguised, &[&laptop], rotate);
        assert!(c.extend(&e).unwrap_err().0.contains("cannot be removed"));
        // An `add` of a second recovery device is refused.
        let kit2 = Dev::recovery(&RecoveryKit::generate());
        let e = c.next_entry(Action::Add, kit2.subject(), T).sign(&[laptop.signs()]).unwrap();
        assert!(c.extend(&e).is_err());
        // rotate-recovery, authorized by the kit itself and signed by the
        // new kit, replaces it at once.
        entries.push(signed(&entries, Action::RotateRecovery, kit2.subject(), &[&kit, &kit2], rotate));
        let c = chain(&entries);
        assert_eq!(c.recovery().unwrap().id(), kit2.id());
        assert_eq!(c.devices().iter().filter(|d| d.kind == Kind::Recovery).count(), 1);
        assert_eq!(c.generation(), 2);
        // The old kit, gone, cannot sign.
        let e = c.next_entry(Action::Add, Dev::new("x").subject(), T).sign(&[kit.signs()]).unwrap();
        assert!(c.extend(&e).is_err());
    }

    /// C1 (review of #199): every entry commits to the current user key id.
    #[test]
    fn d1_every_entry_commits_to_the_user_key_id() {
        let (laptop, _kit, phone, entries, g1) = three();
        let c = chain(&entries);
        for g in [1, 3, 0] {
            let e = signed(&entries, Action::Remove, laptop.subject(), &[&phone], |e| {
                e.generation = g;
                e.key_id = UserKey::from_bytes(g.max(1), [9; 32]).unwrap().id();
            });
            assert!(c.extend(&e).unwrap_err().0.contains("generation"), "generation {g}");
        }
        // A rotation that keeps the id it already has.
        let e = signed(&entries, Action::Remove, laptop.subject(), &[&phone], |e| e.generation = 2);
        assert!(c.extend(&e).unwrap_err().0.contains("held before"));
        // An add that moves the generation, or swaps the key id.
        let e = signed(&entries, Action::Add, Dev::new("x").subject(), &[&phone], rotate);
        assert!(c.extend(&e).unwrap_err().0.contains("as they were"));
        let e = signed(&entries, Action::Add, Dev::new("x").subject(), &[&phone], |e| e.key_id = UserKey::from_bytes(1, [9; 32]).unwrap().id());
        assert!(c.extend(&e).unwrap_err().0.contains("as they were"));
        assert_eq!(c.key_id_at(1), Some(g1.id()));
        assert_eq!(c.key_id_at(2), None);
        assert_eq!(c.key_id_at(0), None);
    }

    /// Batched removal: N removals are N generations; only the last is
    /// wrapped to devices, and it opens every one before it.
    #[test]
    fn d1_a_batch_of_removals_wraps_only_the_newest_generation_to_devices() {
        let (laptop, kit, phone, entries, g1) = three();
        let (tablet, desk) = (Dev::new("tablet"), Dev::new("desk"));
        let (c, adds) = chain(&entries).batch(&g1, vec![Change::Add(tablet.subject()), Change::Add(desk.subject())], laptop.id(), &laptop.signing, T).unwrap();
        assert!(adds.links.is_empty());
        assert_eq!(adds.wraps.iter().map(|w| w.0).collect::<Vec<_>>(), [tablet.id(), desk.id()], "an add wraps only to the devices added");
        let before = c.clone();
        let (c, b) = c.batch(&g1, vec![Change::Remove(phone.subject()), Change::Remove(tablet.subject()), Change::Remove(desk.subject())], laptop.id(), &laptop.signing, T).unwrap();
        assert_eq!((c.generation(), b.newest.generation(), c.key_id()), (4, 4, b.newest.id()));
        assert_eq!((b.entries.len(), b.links.len()), (3, 3));
        // Only generation 4, and only to the devices left.
        assert_eq!(b.wraps.iter().map(|w| w.0).collect::<Vec<_>>(), [laptop.id(), kit.id()]);
        let keys = UserKeys::adopt(&g1, &b.wraps[0].1, 4, c.key_id(), &laptop.key, b.links.clone()).unwrap();
        for g in 1..=4 {
            assert_eq!(keys.open(g).unwrap().id(), c.key_id_at(g).unwrap());
        }
        // Posted as it is, every client accepts it.
        let posted = [entries.clone(), adds.entries, b.entries].concat();
        let again = chain(&posted);
        assert_eq!((again.head(), again.devices(), again.key_id()), (c.head(), c.devices(), c.key_id()));
        // A signer removing itself before the batch ends, or a stale held
        // key, is refused.
        assert!(before.batch(&g1, vec![Change::Remove(laptop.subject())], laptop.id(), &laptop.signing, T).is_ok());
        assert!(before.batch(&g1, vec![Change::Remove(laptop.subject()), Change::Remove(phone.subject())], laptop.id(), &laptop.signing, T).is_err());
        assert!(before.batch(&UserKey::first(), vec![Change::Remove(phone.subject())], laptop.id(), &laptop.signing, T).is_err());
        // rotate-recovery through a batch, by a device: the new kit signs
        // its own entry, and it is only proposed — no generation, no wrap.
        let kit2 = Dev::recovery(&RecoveryKit::generate());
        let (c2, b2) = before.batch(&g1, vec![Change::RotateRecovery(kit2.subject(), &kit2.signing)], laptop.id(), &laptop.signing, T).unwrap();
        assert_eq!(c2.recovery().unwrap().id(), kit.id());
        assert_eq!(c2.pending().unwrap().subject, kit2.subject());
        assert_eq!(b2.entries[0].signatures.len(), 2);
        assert!(b2.links.is_empty() && b2.wraps.is_empty());
    }

    #[test]
    fn d1_a_pinned_client_refuses_an_older_or_shorter_chain() {
        let (laptop, _kit, phone, mut entries, _) = three();
        entries.push(signed(&entries, Action::Remove, laptop.subject(), &[&phone], rotate));
        let pin = chain(&entries).head();
        let e = Chain::verify(&entries[..2], pinned(pin), NOW).unwrap_err();
        assert!(e.0.contains("old list"), "{e}");
        assert_eq!(Chain::verify(&entries, pinned(pin), NOW).unwrap().head(), pin);
        entries.push(signed(&entries, Action::Add, Dev::new("new").subject(), &[&phone], |_| {}));
        assert!(Chain::verify(&entries, pinned(pin), NOW).is_ok());
    }

    #[test]
    fn d1_a_pinned_client_refuses_a_forked_chain() {
        let (laptop, kit, phone, entries, _) = three();
        let pin = chain(&entries).head();
        let mut fork = entries[..1].to_vec();
        fork.push(signed(&fork, Action::Add, Dev::new("rogue").subject(), &[&laptop], |_| {}));
        assert!(Chain::verify(&fork, pinned(pin), NOW).unwrap_err().0.contains("forked"));
        // A fork at seq 0: another genesis entirely, by the same devices.
        let (c, s) = Chain::start(laptop.subject(), &laptop.signing, Some((kit.subject(), &kit.signing)), T + 5).unwrap();
        let (_, add) = c.batch(&s.newest, vec![Change::Add(phone.subject())], laptop.id(), &laptop.signing, T).unwrap();
        let other = [s.entries, add.entries].concat();
        assert!(Chain::verify(&other, pinned(pin), NOW).unwrap_err().0.contains("forked"));
        assert!(Chain::verify(&other, pinned(Head { seq: 0, hash: entries[0].hash() }), NOW).unwrap_err().0.contains("forked"));
        // Extending from a pinned state: an entry that does not follow the
        // head, by seq or by hash, is refused.
        let c = chain(&entries);
        let mut stale = c.next_entry(Action::Add, Dev::new("x").subject(), T);
        stale.prev = entries[0].hash();
        assert!(c.extend(&stale.sign(&[laptop.signs()]).unwrap()).unwrap_err().0.contains("forked"));
        let mut gap = c.next_entry(Action::Add, Dev::new("x").subject(), T);
        gap.seq += 1;
        assert!(c.extend(&gap.sign(&[laptop.signs()]).unwrap()).unwrap_err().0.contains("does not follow"));
    }

    #[test]
    fn d1_an_entry_has_one_encoding() {
        let (_laptop, _kit, _phone, entries, _) = three();
        let e = Entry::decode(&entries[1].bytes).unwrap();
        assert_eq!(e.encode().unwrap(), entries[1].bytes);
        let mut trailing = entries[0].clone();
        trailing.bytes.push(0);
        assert!(gen0(&trailing).is_err());
        for at in 0..entries[0].bytes.len() {
            let mut bad = entries[0].clone();
            bad.bytes[at] ^= 0x80;
            assert!(gen0(&bad).is_err(), "byte {at}");
        }
        let mut bad_sig = entries[0].clone();
        bad_sig.signatures[1].1[0] ^= 1;
        assert!(gen0(&bad_sig).is_err());
        assert!(SignedEntry::from_parts(entries[0].bytes.clone(), &[1; 80]).is_err());
    }

    /// m2 (review of #199): a name is what recovery's review shows, so none
    /// may hide, reorder or repeat another.
    #[test]
    fn d1_device_names_refuse_invisible_characters_and_duplicates() {
        let (_laptop, _kit, phone, entries, _) = three();
        for bad in ["laptop\u{202E}kcabpu", "lap\u{200B}top", "a\u{2028}b", "x\u{FEFF}", "\u{2066}x", "laptop\x1b[2J", "a\u{061C}"] {
            let mut s = Dev::new("x").subject();
            s.name = bad.into();
            assert!(chain(&entries).next_entry(Action::Add, s, T).sign(&[phone.signs()]).is_err(), "{bad:?}");
        }
        let mut s = Dev::new("x").subject();
        s.name = "x".repeat(65);
        assert!(chain(&entries).next_entry(Action::Add, s, T).sign(&[phone.signs()]).is_err());
        let twin = Dev::new("Laptop");
        let e = signed(&entries, Action::Add, twin.subject(), &[&phone], |_| {});
        assert!(chain(&entries).extend(&e).unwrap_err().0.contains("already called"));
    }

    const DAY: u64 = 24 * 60 * 60;

    /// The three devices, and the laptop (a thief, say) proposing a kit of
    /// its own at `at`, as a device with no record of it first sees it.
    fn proposed(at: u64) -> (Dev, Dev, Dev, Dev, Chain, UserKey) {
        let (laptop, kit, phone, entries, g1) = three();
        let thief_kit = Dev::recovery(&RecoveryKit::generate());
        let (c, _) = chain(&entries).batch(&g1, vec![Change::RotateRecovery(thief_kit.subject(), &thief_kit.signing)], laptop.id(), &laptop.signing, at).unwrap();
        (laptop, kit, phone, thief_kit, c, g1)
    }

    #[test]
    fn d1_a_device_authorized_recovery_rotation_waits_seven_days() {
        let (laptop, kit, _phone, thief_kit, c, g1) = proposed(T + 10);
        let p = c.pending().unwrap().clone();
        assert_eq!((p.authorizer, p.seq, p.since, p.effective_at), (laptop.id(), 2, T + 10, T + 10 + 7 * DAY));
        // Meanwhile the old kit is the recovery device, the new one nothing.
        assert_eq!(c.recovery().unwrap().id(), kit.id());
        assert!(!c.devices().iter().any(|d| d.id() == thief_kit.id()));
        assert_eq!(c.generation(), 1);
        let e = c.next_entry(Action::Add, Dev::new("x").subject(), T + 20).sign(&[thief_kit.signs()]).unwrap();
        assert!(c.extend(&e).unwrap_err().0.contains("not a device on the list"), "the new kit has no rights yet");
        let e = c.next_entry(Action::Remove, kit.subject(), T + 20);
        let mut e = e;
        rotate(&mut e);
        assert!(c.extend(&e.sign(&[laptop.signs()]).unwrap()).unwrap_err().0.contains("cannot be removed"));
        // One pending at a time.
        let other = Dev::recovery(&RecoveryKit::generate());
        assert!(c.batch(&g1, vec![Change::RotateRecovery(other.subject(), &other.signing)], laptop.id(), &laptop.signing, T + 20).err().unwrap().0.contains("already pending"));
        // After the delay, by this device's clock, it can be completed: the
        // kit is replaced and the key rotated away from the old one.
        let early = c.batch(&g1, vec![Change::RotateRecovery(thief_kit.subject(), &thief_kit.signing)], laptop.id(), &laptop.signing, p.effective_at - 1);
        assert!(early.is_err());
        let (done, b) = c.batch(&g1, vec![Change::RotateRecovery(thief_kit.subject(), &thief_kit.signing)], laptop.id(), &laptop.signing, p.effective_at).unwrap();
        assert_eq!(done.recovery().unwrap().id(), thief_kit.id());
        assert!(done.pending().is_none());
        assert_eq!(done.generation(), 2);
        assert!(b.wraps.iter().any(|(id, _)| *id == thief_kit.id()) && !b.wraps.iter().any(|(id, _)| *id == kit.id()));
    }

    #[test]
    fn d1_the_old_kit_cancels_a_pending_rotation() {
        let (laptop, kit, _phone, thief_kit, c, g1) = proposed(T + 10);
        // Only the recovery device may cancel.
        assert!(c.batch(&g1, vec![Change::CancelRecoveryRotation(thief_kit.subject())], laptop.id(), &laptop.signing, T + 20).is_err());
        let (c, b) = c.batch(&g1, vec![Change::CancelRecoveryRotation(thief_kit.subject()), Change::Remove(laptop.subject())], kit.id(), &kit.signing, T + 20).unwrap();
        assert!(c.pending().is_none());
        assert_eq!(c.recovery().unwrap().id(), kit.id());
        assert!(!b.wraps.iter().any(|(id, _)| *id == laptop.id()));
        // Nothing left to cancel.
        assert!(c.batch(&b.newest, vec![Change::CancelRecoveryRotation(thief_kit.subject())], kit.id(), &kit.signing, T + 30).is_err());
        // The kit itself rotates to a new kit at once.
        let kit2 = Dev::recovery(&RecoveryKit::generate());
        let (c, _) = c.batch(&b.newest, vec![Change::RotateRecovery(kit2.subject(), &kit2.signing)], kit.id(), &kit.signing, T + 30).unwrap();
        assert_eq!(c.recovery().unwrap().id(), kit2.id());
    }

    #[test]
    fn d1_removing_the_authorizer_voids_a_pending_rotation() {
        let (laptop, kit, phone, thief_kit, c, g1) = proposed(T + 10);
        let (c, b) = c.batch(&g1, vec![Change::Remove(laptop.subject())], phone.id(), &phone.signing, T + 20).unwrap();
        assert!(c.pending().is_none());
        assert_eq!(c.recovery().unwrap().id(), kit.id());
        // Its completion, even after the delay, is now a new proposal by
        // whoever signs it, not a replacement.
        let later = c.at(T + 30 * DAY);
        let (c, _) = later.batch(&b.newest, vec![Change::RotateRecovery(thief_kit.subject(), &thief_kit.signing)], phone.id(), &phone.signing, T + 30 * DAY).unwrap();
        assert_eq!(c.recovery().unwrap().id(), kit.id());
        assert_eq!(c.pending().unwrap().authorizer, phone.id());
    }

    #[test]
    fn d1_with_no_kit_a_device_makes_one_at_once() {
        let laptop = Dev::new("laptop");
        let (c, s) = Chain::start(laptop.subject(), &laptop.signing, None, T).unwrap();
        let kit = Dev::recovery(&RecoveryKit::generate());
        let (c, b) = c.batch(&s.newest, vec![Change::RotateRecovery(kit.subject(), &kit.signing)], laptop.id(), &laptop.signing, T + 1).unwrap();
        assert_eq!(c.recovery().unwrap().id(), kit.id());
        assert!(c.pending().is_none());
        assert_eq!((c.generation(), b.links.len()), (2, 1));
    }

    /// The delay is counted from this device's own first sight of the
    /// entry, never its `time` alone: a thief backdating the proposal by
    /// eight days gains nothing on a pinned device.
    #[test]
    fn d1_a_backdated_rotation_is_refused_or_stays_pending_on_a_pinned_device() {
        let (laptop, _kit, _phone, entries, g1) = three();
        let pin = chain(&entries).head();
        let thief_kit = Dev::recovery(&RecoveryKit::generate());
        let mut e = chain(&entries).next_entry(Action::RotateRecovery, thief_kit.subject(), T - 8 * DAY);
        e.time = T - 8 * DAY;
        let backdated = e.sign(&[laptop.signs(), thief_kit.signs()]).unwrap();
        let all = [entries.clone(), vec![backdated.clone()]].concat();
        // Verified at T: refused as backdated.
        let e = Chain::verify(&all, pinned(pin), NOW).unwrap_err();
        assert!(e.0.contains("backdated"), "{e}");
        // Within the skew allowance it is taken, and pending from now.
        let near = Trust::Pinned { head: pin, verified_at: T - 8 * DAY + CLOCK_SKEW, first_seen: None };
        let c = Chain::verify(&all, near.clone(), NOW).unwrap();
        assert_eq!(c.pending().unwrap().effective_at, NOW + RECOVERY_ROTATION_DELAY);
        // Its completion posted at once stays refused on this device …
        let (_, done) = c.batch(&g1, vec![Change::RotateRecovery(thief_kit.subject(), &thief_kit.signing)], laptop.id(), &laptop.signing, NOW + RECOVERY_ROTATION_DELAY).unwrap();
        let with_completion = [all.clone(), done.entries].concat();
        assert!(Chain::verify(&with_completion, near.clone(), NOW + DAY).is_err());
        // … and is taken once seven days have passed since it first saw the
        // proposal, which it recorded.
        let recorded = Trust::Pinned { head: c.head(), verified_at: NOW, first_seen: Some((c.pending().unwrap().seq, NOW)) };
        assert!(Chain::verify(&with_completion, recorded.clone(), NOW + DAY).is_err());
        assert!(Chain::verify(&with_completion, recorded, NOW + RECOVERY_ROTATION_DELAY).is_ok());
        // A fresh machine counts from the registry's receipt time: honest
        // receipts keep it pending, and a hostile registry's, made up to
        // fit the delay, make it no control — the documented limit.
        let honest = Trust::Fresh { received_at: vec![T, T, NOW, NOW] };
        assert!(Chain::verify(&with_completion, honest, NOW + DAY).is_err());
        let hostile = Trust::Fresh { received_at: vec![T - 8 * DAY, T - 8 * DAY, T - 8 * DAY, T - DAY] };
        assert!(Chain::verify(&with_completion, hostile, NOW + DAY).is_ok());
        assert!(Chain::verify(&with_completion, Trust::Fresh { received_at: vec![T; 2] }, NOW + DAY).unwrap_err().0.contains("receipt"));
    }

    /// The three devices, a proposal by the laptop at T+10, and its
    /// completion at T+10+7 days.
    fn completed() -> (Vec<SignedEntry>, Dev, Dev) {
        let (laptop, _kit, _phone, entries, g1) = three();
        let thief_kit = Dev::recovery(&RecoveryKit::generate());
        let (c, p) = chain(&entries).batch(&g1, vec![Change::RotateRecovery(thief_kit.subject(), &thief_kit.signing)], laptop.id(), &laptop.signing, T + 10).unwrap();
        let (_, done) = c.at(T + 10 + 7 * DAY).batch(&g1, vec![Change::RotateRecovery(thief_kit.subject(), &thief_kit.signing)], laptop.id(), &laptop.signing, T + 10 + 7 * DAY).unwrap();
        ([entries, p.entries, done.entries].concat(), laptop, thief_kit)
    }

    /// R1 (re-check of #199): once a device has pinned past a completed
    /// rotation, it re-verifies from seq 0 with nothing pending recorded.
    #[test]
    fn d1_a_pinned_device_re_verifies_past_a_completed_rotation() {
        let (all, _, thief_kit) = completed();
        let done = T + 10 + 7 * DAY;
        let first = Trust::Pinned { head: Head { seq: 1, hash: all[1].hash() }, verified_at: T + 10, first_seen: Some((2, T + 10)) };
        let c = Chain::verify(&all, first, done + 1).unwrap();
        let later = Trust::Pinned { head: c.head(), verified_at: done + 1, first_seen: None };
        let again = Chain::verify(&all, later, done + 100 * DAY).unwrap();
        assert_eq!(again.recovery().unwrap().id(), thief_kit.id());
    }

    /// R2 (re-check of #199): a device offline through the whole delay gets
    /// the proposal and the completion at once. It verifies the prefix, pins
    /// it with the proposal recorded, and takes the completion seven days
    /// later by its own clock.
    #[test]
    fn d1_an_offline_device_takes_a_completed_rotation_seven_days_after_it_sees_it() {
        let (all, laptop, thief_kit) = completed();
        let pin = Head { seq: 1, hash: all[1].hash() };
        let now = T + 20 * DAY;
        assert!(Chain::verify(&all, Trust::Pinned { head: pin, verified_at: T + 5, first_seen: None }, now).is_err());
        let v = Chain::verify_prefix(&all, Trust::Pinned { head: pin, verified_at: T + 5, first_seen: None }, now).unwrap();
        let (seq, why) = v.rejected.clone().unwrap();
        assert_eq!(seq, 3);
        assert!(why.0.contains("takes effect only"), "{why}");
        let p = v.chain.pending().unwrap().clone();
        assert_eq!((p.seq, p.since, p.authorizer), (2, now, laptop.id()));
        // It pins the prefix but keeps its last clean verification time: the
        // refused completion is older than `now`, and is not backdated.
        let recorded = Trust::Pinned { head: v.chain.head(), verified_at: T + 5, first_seen: Some((p.seq, p.since)) };
        assert!(Chain::verify(&all, recorded.clone(), now + 6 * DAY).is_err());
        assert_eq!(Chain::verify(&all, recorded, now + 7 * DAY).unwrap().recovery().unwrap().id(), thief_kit.id());
        // A refused entry at or before the pin still refuses everything.
        let mut bad = all.clone();
        bad[1].signatures[0].1[0] ^= 1;
        assert!(Chain::verify_prefix(&bad, Trust::Pinned { head: pin, verified_at: T + 5, first_seen: None }, now).is_err());
    }

    /// R3 (re-check of #199): on a fresh machine a completion the registry
    /// received before the delay was over is refused, however late the
    /// machine verifies; the prefix leaves the old kit able to cancel.
    #[test]
    fn d1_a_fresh_machine_refuses_a_completion_received_early() {
        let (laptop, kit, _phone, entries, g1) = three();
        let thief_kit = Dev::recovery(&RecoveryKit::generate());
        let (c, p) = chain(&entries).batch(&g1, vec![Change::RotateRecovery(thief_kit.subject(), &thief_kit.signing)], laptop.id(), &laptop.signing, T + 10).unwrap();
        let (_, done) = c.batch(&g1, vec![Change::RotateRecovery(thief_kit.subject(), &thief_kit.signing)], laptop.id(), &laptop.signing, T + 10 + 7 * DAY).unwrap();
        let all = [entries, p.entries, done.entries].concat();
        let early = Trust::Fresh { received_at: vec![T, T, T + 10, T + DAY] };
        assert!(Chain::verify(&all, early.clone(), T + 8 * DAY).unwrap_err().0.contains("before its delay was over"));
        let v = Chain::verify_prefix(&all, early, T + 8 * DAY).unwrap();
        assert_eq!(v.chain.recovery().unwrap().id(), kit.id());
        assert!(v.chain.pending().is_some());
        // Received after the delay, it is taken.
        let on_time = Trust::Fresh { received_at: vec![T, T, T + 10, T + 10 + 7 * DAY] };
        assert_eq!(Chain::verify(&all, on_time, T + 8 * DAY).unwrap().recovery().unwrap().id(), thief_kit.id());
    }

    /// Minor (re-check of #199): a pending rotation cannot be jammed by
    /// adding a device with its kit's keys or name.
    #[test]
    fn d1_an_add_cannot_take_the_pending_kits_keys_or_name() {
        let (laptop, _kit, _phone, thief_kit, c, g1) = proposed(T + 10);
        let mut same_keys = thief_kit.subject();
        same_keys.kind = Kind::Device;
        same_keys.name = "other".into();
        assert!(c.batch(&g1, vec![Change::Add(same_keys)], laptop.id(), &laptop.signing, T + 20).err().unwrap().0.contains("pending"));
        let mut same_name = Dev::new("x").subject();
        same_name.name = "Recovery Kit".into();
        assert!(c.batch(&g1, vec![Change::Add(same_name)], laptop.id(), &laptop.signing, T + 20).err().unwrap().0.contains("pending"));
    }

    /// Re-check 2 of #199: the chain `batch` hands back is pinned at its
    /// new head, so a fresh machine that recovered through it still judges
    /// what comes next by its own clock — an early completion is refused.
    #[test]
    fn d1_a_batch_hands_back_a_chain_pinned_at_its_new_head() {
        let (laptop, kit, _phone, entries, g1) = three();
        let fresh = Chain::verify(&entries, Trust::Fresh { received_at: vec![T, T] }, NOW).unwrap();
        let recovered = Dev::new("recovered");
        let (c, _) = fresh.batch(&g1, vec![Change::Add(recovered.subject())], kit.id(), &kit.signing, NOW).unwrap();
        let head = c.head();
        // A thief's proposal and its completion arrive together a day later.
        let thief_kit = Dev::recovery(&RecoveryKit::generate());
        let mut p = c.next_entry(Action::RotateRecovery, thief_kit.subject(), NOW + DAY);
        p.seq = head.seq + 1;
        let proposal = p.sign(&[laptop.signs(), thief_kit.signs()]).unwrap();
        let c1 = c.extend(&proposal).unwrap();
        assert_eq!(c1.pending().unwrap().since, NOW, "counted from this device's clock, not a receipt");
        let completion = c1.next_entry(Action::RotateRecovery, thief_kit.subject(), NOW + DAY);
        let mut completion = completion;
        rotate(&mut completion);
        let completion = completion.sign(&[laptop.signs(), thief_kit.signs()]).unwrap();
        assert!(c1.at(NOW + 2 * DAY).extend(&completion).unwrap_err().0.contains("takes effect only"));
        // A backdated entry after the new head is refused.
        let mut old = c.next_entry(Action::Add, Dev::new("old").subject(), NOW - 2 * CLOCK_SKEW);
        old.seq = head.seq + 1;
        assert!(c.extend(&old.sign(&[kit.signs()]).unwrap()).unwrap_err().0.contains("backdated"));
    }

    /// Re-check 2 nit: a proposed kit may not take a listed device's name.
    #[test]
    fn d1_a_proposed_kit_may_not_take_a_listed_devices_name() {
        let (laptop, _kit, _phone, entries, g1) = three();
        let thief_kit = Dev::recovery(&RecoveryKit::generate());
        let mut clash = thief_kit.subject();
        clash.name = "Phone".into();
        let r = chain(&entries).batch(&g1, vec![Change::RotateRecovery(clash, &thief_kit.signing)], laptop.id(), &laptop.signing, T + 10);
        assert!(r.err().unwrap().0.contains("already called"));
    }

    /// Known answer, frozen: a genesis entry over fixed keys and a fixed
    /// user key, its bytes, hash and (deterministic) Ed25519 signatures.
    /// Another implementation of the format — the registry's structural
    /// check — must agree.
    #[test]
    fn d1_the_entry_encoding_and_signature_known_answer() {
        let (entry, first, kit) = kat();
        let signed = entry.sign(&[(entry.subjects[0].id(), &first), (entry.subjects[1].id(), &kit)]).unwrap();
        assert_eq!(e2e::hex(&signed.bytes), KAT_BYTES);
        assert_eq!(e2e::hex(&signed.hash()), KAT_HASH);
        assert_eq!(e2e::hex(&signed.signatures_bytes()), KAT_SIGNATURES);
        assert!(gen0(&signed).is_ok());
    }

    fn kat() -> (Entry, SigningKey, SigningKey) {
        let device = DeviceKey::from_secret(&[2; 32]).unwrap();
        let signing = SigningKey::from_secret(&[1; 32]).unwrap();
        let kit = RecoveryKit::from_bytes([0; 16]).device();
        let first = Subject { kind: Kind::Device, name: "elvinas-arch".into(), os: "linux".into(), device: device.public(), signing: signing.public() };
        let recovery = Subject { kind: Kind::Recovery, name: "recovery kit".into(), os: String::new(), device: kit.key.public(), signing: kit.signing.public() };
        let key_id = UserKey::from_bytes(1, [0x42; 32]).unwrap().id();
        (Entry { seq: 0, prev: [0; 32], action: Action::Add, subjects: vec![first, recovery], generation: 1, key_id, time: T, signers: Vec::new() }, signing, kit.signing)
    }

    const KAT_BYTES: &str = "0100000000000000000000000000000000000000000000000000000000000000000000000000000000010201000c656c76696e61732d6172636800056c696e7578ce8d3ad1ccb633ec7b70c17814a5c76ecd029685050d344745ba05870e587d598a88e3dd7409f195fd52db2d3cba5d72ca6709bf1d94121bf3748801b40f6f5c02000c7265636f76657279206b6974000099431817513a8a27a56fef4349664cd4cfaada795fd8c5fa1e5720d05d554f3bdce4476f44aa387f22d70ae566ba8619e62aa418fc878b6c99166d5a80f5c4f30000000144a301e5d533c38411bc62b3f83654bb000000006ab13b80028321f34eb2572c7a3cf59b2b4cc83579add03dec5d1aa3cbb5a9dd321d2c028d";
    const KAT_HASH: &str = "fbde1c49577f61b1d1d4d3afd122aad39a8cc4bc1382dcd2ce23d4e0a0fa33dd";
    const KAT_SIGNATURES: &str = "028321f34eb2572c7a3cf59b2b4cc83579a765f0efce33c8001e7ae5fc8d8cc79fd8a3c25e7655f5929cde72aa8ae0622af72674badd438d7d23038627fe588890b5509bed73548014d2a3984e2c1ba20dadd03dec5d1aa3cbb5a9dd321d2c028d0615562c0c5831b6f737c0e431d1982a3fca4e811917d36af2e5a72615b5f37d8cc598f982b7761b98dacd6868a2cc351c56aa18d76691bb557f78ddc3ac4d0a";

    /// Prints the known answer above: run once with `--ignored
    /// --nocapture` when the format is meant to change.
    #[test]
    #[ignore]
    fn print_entry_known_answer() {
        let (entry, first, kit) = kat();
        let s = entry.sign(&[(entry.subjects[0].id(), &first), (entry.subjects[1].id(), &kit)]).unwrap();
        println!("bytes {}\nhash {}\nsigs {}", e2e::hex(&s.bytes), e2e::hex(&s.hash()), e2e::hex(&s.signatures_bytes()));
    }
}

/// The review of #199's attacks (C1, C2, M1, m2), kept as regression tests:
/// each now fails where it once succeeded, except M1, which stands until
/// the rule for who may authorize rotate-recovery is decided.
#[cfg(test)]
mod review_attacks {
    use super::*;
    use crate::e2e::DeviceKey;
    use crate::recovery::RecoveryKit;
    use crate::user_key::{UserKey, UserKeys};

    fn subj(kind: Kind, name: &str, k: &DeviceKey, s: &SigningKey) -> Subject {
        Subject { kind, name: name.into(), os: "linux".into(), device: k.public(), signing: s.public() }
    }

    /// C1: the registry, knowing only a device's public key, delivers its
    /// own user key as generation 2 after a genuine rotation.
    #[test]
    fn c1_a_forged_wrap_is_refused() {
        let phone = DeviceKey::generate();
        let held = UserKey::first();
        let real = held.next().unwrap();
        let attacker_key = UserKey::from_bytes(2, [0xaa; 32]).unwrap();
        let blob = attacker_key.wrap_to(&phone.public()).unwrap();
        assert!(UserKey::unwrap(&blob, 2, real.id(), &phone).is_err());
        assert!(UserKeys::adopt(&held, &blob, 2, attacker_key.id(), &phone, []).is_err());
    }

    /// C2: a chain the registry built from the recovery device's public
    /// keys and a copied name does not verify.
    #[test]
    fn c2_a_fabricated_chain_with_the_real_kit_is_refused() {
        let rd = RecoveryKit::generate().device();
        let (rk, rs) = (DeviceKey::generate(), SigningKey::generate());
        let fake_first = subj(Kind::Device, "elvinas-arch", &rk, &rs);
        let rec = Subject { kind: Kind::Recovery, name: "recovery kit".into(), os: String::new(), device: rd.key.public(), signing: rd.signing.public() };
        let e = Entry { seq: 0, prev: [0; 32], action: Action::Add, subjects: vec![fake_first, rec], generation: 1, key_id: UserKey::first().id(), time: 1, signers: Vec::new() };
        let g = e.sign(&[(rk.id(), &rs)]).unwrap();
        assert!(Chain::verify(&[g], Trust::Fresh { received_at: vec![1] }, 2).is_err());
    }

    /// M1: a stolen laptop proposes a kit of its own and removes the
    /// owner's other device. Within the delay the owner's kit is still the
    /// recovery device, still wrapped to, and cannot be removed.
    #[test]
    fn m1_a_thief_cannot_dislodge_the_kit_within_the_delay() {
        let (lk, ls) = (DeviceKey::generate(), SigningKey::generate());
        let (pk, ps) = (DeviceKey::generate(), SigningKey::generate());
        let kit = RecoveryKit::generate().device();
        let rec = Subject { kind: Kind::Recovery, name: "recovery kit".into(), os: String::new(), device: kit.key.public(), signing: kit.signing.public() };
        let (c, start) = Chain::start(subj(Kind::Device, "phone", &pk, &ps), &ps, Some((rec, &kit.signing)), 1).unwrap();
        let (c, _) = c.batch(&start.newest, vec![Change::Add(subj(Kind::Device, "laptop", &lk, &ls))], pk.id(), &ps, 2).unwrap();
        let thief_kit = RecoveryKit::generate().device();
        let trec = Subject { kind: Kind::Recovery, name: "thief kit".into(), os: String::new(), device: thief_kit.key.public(), signing: thief_kit.signing.public() };
        let (c, b) = c.batch(&start.newest, vec![Change::RotateRecovery(trec.clone(), &thief_kit.signing), Change::Remove(subj(Kind::Device, "phone", &pk, &ps))], lk.id(), &ls, 3).unwrap();
        assert_eq!(c.recovery().unwrap().device, kit.key.public());
        assert!(b.wraps.iter().any(|(id, _)| *id == kit.key.id()), "the old kit still gets the new generation");
        assert!(!b.wraps.iter().any(|(id, _)| *id == thief_kit.key.id()));
        // Completing it early is refused.
        let early = c.batch(&b.newest, vec![Change::RotateRecovery(trec, &thief_kit.signing)], lk.id(), &ls, 4);
        assert!(early.err().unwrap().0.contains("takes effect only"));
    }

    /// m2: a bidirectional override no longer passes the name check.
    #[test]
    fn m2_a_bidi_name_is_refused() {
        let (k, s) = (DeviceKey::generate(), SigningKey::generate());
        let mut sub = subj(Kind::Device, "x", &k, &s);
        sub.name = "laptop\u{202E}kcabpu".into();
        assert!(Chain::start(sub, &s, None, 1).is_err());
    }
}
