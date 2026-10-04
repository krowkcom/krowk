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
//! action      u8        1 add, 2 remove, 3 rotate-recovery
//! subjects    u8        how many: 1, or 2 at seq 0 (first device, recovery)
//!   kind      u8          1 device, 2 recovery
//!   name      u16 + utf8  1–64 bytes (see "Names" below)
//!   os        u16 + utf8  0–32 bytes, the same characters refused
//!   x25519    [32]        the key the user key is wrapped to
//!   ed25519   [32]        the key it signs with
//! generation  u32       the user key generation the entry leaves current
//! key id      [16]      that generation's user key id (`user_key`)
//! time        u64       Unix seconds, as the signer's clock read it;
//!                       informational, nothing checks it
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
//!   device, except that `rotate-recovery` is authorized only by the
//!   current recovery device, or by any device when there is none.
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
//! - `rotate-recovery` replaces the recovery device at once (or makes the
//!   first, when setup skipped the kit).
//! - A removal and a `rotate-recovery` rotate the key: they must leave the
//!   generation exactly one higher, with a key id the chain has not held.
//!   Every other entry carries the current key id unchanged.
//! - **Names** are what a person reads at every prompt and in recovery's
//!   review. A name or OS may not hold any code point in these inclusive
//!   ranges (`REFUSED_IN_NAMES`, Unicode 16's `Cc`, `Cf`, `Zl` and `Zp`):
//!   U+0000–001F, U+007F–009F, U+00AD, U+0600–0605, U+061C, U+06DD,
//!   U+070F, U+0890–0891, U+08E2, U+180E, U+200B–200F, U+2028–202E,
//!   U+2060–206F, U+FEFF, U+FFF9–FFFB, U+110BD, U+110CD, U+13430–1343F,
//!   U+1BCA0–1BCA3, U+1D173–1D17A, U+E0001, U+E0020–E007F. A device may
//!   not take a name a device on the list already has, compared with ASCII
//!   `A`–`Z` folded and every other byte exact (`same_name`). No rule of
//!   the chain's validity reads the runtime's Unicode tables.
//! - A name that only *reads* like one on the list — a Cyrillic `а` for a
//!   Latin `a`, `ｌａｐｔｏｐ`, `1` for `l` — is refused where an entry is
//!   written (`Chain::batch`, `confusable_names`), not where one is
//!   verified: NFKC and the confusables table are the runtime's Unicode
//!   tables, which two implementations of the verifier need not share.
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
}

impl Action {
    fn from_byte(a: u8, seq: u64) -> Result<Action, Error> {
        match a {
            1 => Ok(Action::Add),
            2 => Ok(Action::Remove),
            3 => Ok(Action::RotateRecovery),
            a => Err(refuse(seq, format!("unknown action {a}"))),
        }
    }

    /// Whether the entry moves the user key up a generation.
    fn rotates(self) -> bool {
        self != Action::Add
    }
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

    /// A device on the list as the subject an entry about it names — what
    /// a `remove` must name exactly.
    pub fn of(d: &Device) -> Subject {
        Subject { kind: d.kind, name: d.name.clone(), os: d.os.clone(), device: d.device, signing: d.signing }
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
        self.check_counts()?;
        let mut b = Vec::with_capacity(320);
        b.push(ENTRY_V1);
        b.extend_from_slice(&self.seq.to_be_bytes());
        b.extend_from_slice(&self.prev);
        b.push(self.action as u8);
        b.push(self.subjects.len() as u8);
        for s in &self.subjects {
            put_subject(&mut b, s, self.seq)?;
        }
        b.extend_from_slice(&self.generation.to_be_bytes());
        b.extend_from_slice(&self.key_id.0);
        b.extend_from_slice(&self.time.to_be_bytes());
        b.push(self.signers.len() as u8);
        self.signers.iter().for_each(|s| b.extend_from_slice(&s.0));
        Ok(b)
    }

    fn check_counts(&self) -> Result<(), Error> {
        if self.subjects.is_empty() || self.subjects.len() > 2 {
            return Err(refuse(self.seq, "an entry is about one device, or two at entry 0"));
        }
        let ordered = self.signers.windows(2).all(|w| w[0].0 < w[1].0);
        if self.signers.is_empty() || self.signers.len() > 3 || !ordered {
            return Err(refuse(self.seq, "its signers are not one to three distinct ids in order"));
        }
        Ok(())
    }

    /// An entry from its bytes, strictly: see the module doc.
    pub fn decode(bytes: &[u8]) -> Result<Entry, Error> {
        let mut r = Reader { b: bytes, seq: 0 };
        let version = r.u8()?;
        if version != ENTRY_V1 {
            return Err(Error(format!("the device list entry is format {version}, which this krowk does not read — upgrade krowk")));
        }
        r.seq = u64::from_be_bytes(r.array()?);
        let entry = r.entry_after_seq()?;
        if !r.b.is_empty() {
            return Err(refuse(entry.seq, "trailing bytes"));
        }
        if entry.encode()? != bytes {
            return Err(refuse(entry.seq, "not in canonical form"));
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

/// The code points a device name or OS may not hold, as inclusive ranges:
/// what a terminal would act on, reorder or hide. A literal list, never a
/// lookup in the runtime's Unicode tables, so every implementation of the
/// chain — the registry's included — refuses exactly these and no others
/// whatever its Unicode version. It is Unicode 16's `Cc`, `Cf`, `Zl` and
/// `Zp` (U+2065, unassigned, rides along in U+2060–206F). Pairing refuses
/// the same list.
pub const REFUSED_IN_NAMES: &[(u32, u32)] = &[
    (0x0000, 0x001F),
    (0x007F, 0x009F),
    (0x00AD, 0x00AD),
    (0x0600, 0x0605),
    (0x061C, 0x061C),
    (0x06DD, 0x06DD),
    (0x070F, 0x070F),
    (0x0890, 0x0891),
    (0x08E2, 0x08E2),
    (0x180E, 0x180E),
    (0x200B, 0x200F),
    (0x2028, 0x202E),
    (0x2060, 0x206F),
    (0xFEFF, 0xFEFF),
    (0xFFF9, 0xFFFB),
    (0x110BD, 0x110BD),
    (0x110CD, 0x110CD),
    (0x13430, 0x1343F),
    (0x1BCA0, 0x1BCA3),
    (0x1D173, 0x1D17A),
    (0xE0001, 0xE0001),
    (0xE0020, 0xE007F),
];

/// Whether `c` is one of `REFUSED_IN_NAMES`.
pub fn refused_in_name(c: char) -> bool {
    let c = c as u32;
    REFUSED_IN_NAMES.iter().any(|&(lo, hi)| (lo..=hi).contains(&c))
}

/// Two names are one name when they are equal with ASCII `A`–`Z` folded to
/// `a`–`z`, and every other byte compared exactly: `Σ` and `σ`, `Ą` and
/// `ą`, `ẞ` and `ß` are distinct. No Unicode case mapping, which differs
/// between runtimes (Ruby's `downcase` has no final sigma), decides
/// whether a chain is valid.
pub fn same_name(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

/// Whether two names read alike to a person: equal once each is NFKC
/// normalised and reduced to its UTS #39 confusable skeleton, case folded
/// between two passes so `I` and `l` meet. A writer's check, never a rule of
/// validity (the module doc says why).
pub fn confusable_names(a: &str, b: &str) -> bool {
    fn fold(s: &str) -> String {
        use unicode_normalization::UnicodeNormalization;
        let nfkc: String = s.nfkc().collect();
        // Runs of space of any kind read as one, and none at the ends; what
        // draws nothing (a Hangul filler, U+034F) does not count.
        let spaced = nfkc.split(char::is_whitespace).filter(|w| !w.is_empty()).collect::<Vec<_>>().join(" ");
        let shown: String = spaced.chars().filter(|&c| !invisible(c)).collect();
        let once: String = unicode_security::skeleton(&shown).collect();
        unicode_security::skeleton(&once.to_lowercase()).collect()
    }
    fold(a) == fold(b)
}

/// What draws nothing beside text: Unicode's default-ignorable code points
/// the refused list leaves out (Hangul fillers, the combining grapheme
/// joiner, variation selectors).
fn invisible(c: char) -> bool {
    matches!(c as u32, 0x034F | 0x115F | 0x1160 | 0x17B4 | 0x17B5 | 0x3164 | 0xFFA0 | 0xFE00..=0xFE0F | 0xE0100..=0xE01EF | 0x180B..=0x180D | 0x180F)
}

fn check_text(seq: u64, what: &str, s: &str, min: usize, max: usize) -> Result<(), Error> {
    if s.len() < min || s.len() > max {
        return Err(refuse(seq, format!("a device {what} is {min}–{max} bytes")));
    }
    if s.chars().any(refused_in_name) {
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

    /// Everything after `version` and `seq` (already read into `self.seq`).
    fn entry_after_seq(&mut self) -> Result<Entry, Error> {
        let seq = self.seq;
        let prev = self.array::<32>()?;
        let action = Action::from_byte(self.u8()?, seq)?;
        let n = self.u8()?;
        if !(1..=2).contains(&n) {
            return Err(refuse(seq, "an entry is about one device, or two at entry 0"));
        }
        let subjects = (0..n).map(|_| self.subject()).collect::<Result<Vec<_>, _>>()?;
        let generation = u32::from_be_bytes(self.array()?);
        let key_id = UserKeyId(self.array()?);
        let time = u64::from_be_bytes(self.array()?);
        let n = self.u8()?;
        let signers = (0..n).map(|_| self.array().map(DeviceId)).collect::<Result<Vec<_>, _>>()?;
        Ok(Entry { seq, prev, action, subjects, generation, key_id, time, signers })
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

/// Which current devices may authorize an action: any device on the list,
/// except that only the recovery device replaces itself — or, when there
/// is none (setup skipped the kit), any device makes the first.
fn may_authorize(action: Action, signer: &Device, recovery: Option<&Device>) -> bool {
    action != Action::RotateRecovery || recovery.is_none() || signer.kind == Kind::Recovery
}

/// Who must sign an entry: the subjects that prove possession of their
/// own keys, and how many authorizers from the list besides them.
fn required_signers(e: &Entry) -> (&[Subject], usize) {
    match (e.seq, e.action) {
        (0, _) => (&e.subjects[..], 0),
        (_, Action::RotateRecovery) => (&e.subjects[..], 1),
        _ => (&[], 1),
    }
}

/// A change in a batch (`Chain::batch`).
pub enum Change<'a> {
    Add(Subject),
    Remove(Subject),
    /// The new recovery device, and its signing key for its own signature.
    RotateRecovery(Subject, &'a SigningKey),
}

impl<'a> Change<'a> {
    fn parts(self) -> (Action, Subject, Option<&'a SigningKey>) {
        match self {
            Change::Add(s) => (Action::Add, s, None),
            Change::Remove(s) => (Action::Remove, s, None),
            Change::RotateRecovery(s, k) => (Action::RotateRecovery, s, Some(k)),
        }
    }
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
    /// Every device this list has ever held, removed ones included, with
    /// its signing key: who may have signed a session record (`signer`).
    held: Vec<(DeviceId, SigningPublic)>,
    /// Entry 0's hash: which list this is, the same at every head.
    root: [u8; 32],
}

impl Chain {
    /// Verifies a whole chain from seq 0. With a pin, it must also reach at
    /// least the pin's seq and hold the pinned hash there: a shorter chain
    /// is an older list, another hash a forked one, and both are refused.
    pub fn verify(entries: &[SignedEntry], pin: Option<Head>) -> Result<Chain, Error> {
        let (first, rest) = entries.split_first().ok_or_else(|| Error("the device list is empty".into()))?;
        if let Some(p) = pin
            && (entries.len() as u64) <= p.seq
        {
            return Err(Error(format!("the device list ends at entry {}, older than entry {} this device last verified — the registry served an old list", entries.len() - 1, p.seq)));
        }
        let mut chain = Chain::genesis(first)?;
        chain.check_pin(pin)?;
        for e in rest {
            chain = chain.extend(e)?;
            chain.check_pin(pin)?;
        }
        Ok(chain)
    }

    fn check_pin(&self, pin: Option<Head>) -> Result<(), Error> {
        match pin {
            Some(p) if self.head.seq == p.seq && self.head.hash != p.hash => Err(Error(format!("the device list differs at entry {} from the one this device last verified — the registry served a forked list", p.seq))),
            _ => Ok(()),
        }
    }

    /// Verifies seq 0 alone: `verify`'s first step, and only reached through
    /// it, so no caller holds a chain that was not verified from its start.
    fn genesis(entry: &SignedEntry) -> Result<Chain, Error> {
        let e = Entry::decode(&entry.bytes)?;
        check_genesis(&e)?;
        let mut chain = Chain { head: Head { seq: 0, hash: entry.hash() }, key_ids: vec![e.key_id], devices: Vec::new(), seen: Vec::new(), held: Vec::new(), root: entry.hash() };
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
        self.check_follows(&e)?;
        self.check_signatures(&e, entry)?;
        let mut next = self.clone();
        next.head = Head { seq: e.seq, hash: entry.hash() };
        next.apply(e.action, &e.subjects[0], e.seq)?;
        next.check_generation(self, &e)?;
        Ok(next)
    }

    fn check_follows(&self, e: &Entry) -> Result<(), Error> {
        if Some(e.seq) != self.head.seq.checked_add(1) {
            return Err(refuse(e.seq, format!("it does not follow entry {}", self.head.seq)));
        }
        if e.prev != self.head.hash {
            return Err(refuse(e.seq, format!("it does not extend entry {} as this device verified it — a forked list", self.head.seq)));
        }
        if e.subjects.len() != 1 {
            return Err(refuse(e.seq, "only entry 0 is about two devices"));
        }
        Ok(())
    }

    fn apply(&mut self, action: Action, subject: &Subject, seq: u64) -> Result<(), Error> {
        match action {
            Action::Add => self.apply_add(subject, seq),
            Action::Remove => self.apply_remove(subject, seq),
            Action::RotateRecovery => self.apply_rotate_recovery(subject, seq),
        }
    }

    fn apply_add(&mut self, subject: &Subject, seq: u64) -> Result<(), Error> {
        if subject.kind != Kind::Device {
            return Err(refuse(seq, "a recovery device is added only by rotate-recovery"));
        }
        self.add(subject, seq)
    }

    fn apply_remove(&mut self, subject: &Subject, seq: u64) -> Result<(), Error> {
        let at = self.devices.iter().position(|d| d.id() == subject.id()).ok_or_else(|| refuse(seq, "it removes a device that is not on the list"))?;
        if self.devices[at].kind == Kind::Recovery || subject.kind == Kind::Recovery {
            return Err(refuse(seq, "the recovery device cannot be removed, only replaced by rotate-recovery"));
        }
        if !self.devices[at].matches(subject) {
            return Err(refuse(seq, "it names a device other than the one on the list"));
        }
        self.devices.remove(at);
        Ok(())
    }

    fn apply_rotate_recovery(&mut self, subject: &Subject, seq: u64) -> Result<(), Error> {
        if subject.kind != Kind::Recovery {
            return Err(refuse(seq, "rotate-recovery must add a recovery device"));
        }
        self.devices.retain(|d| d.kind != Kind::Recovery);
        self.add(subject, seq)
    }

    /// The generation and key id `e` leaves, checked against `before`.
    fn check_generation(&mut self, before: &Chain, e: &Entry) -> Result<(), Error> {
        let g = before.generation();
        if !e.action.rotates() {
            if e.generation != g || e.key_id != before.key_id() {
                return Err(refuse(e.seq, format!("it must leave user key generation {g} and its key id as they were")));
            }
            return Ok(());
        }
        if Some(e.generation) != g.checked_add(1) {
            return Err(refuse(e.seq, format!("it leaves user key generation {}, and a {:?} must leave {}", e.generation, e.action, g as u64 + 1)));
        }
        if self.key_ids.contains(&e.key_id) {
            return Err(refuse(e.seq, "it rotates to a user key id this list has held before"));
        }
        self.key_ids.push(e.key_id);
        Ok(())
    }

    /// The signers an entry needs, then each signature: see the module doc.
    /// `self` is the chain before the entry.
    fn check_signatures(&self, e: &Entry, entry: &SignedEntry) -> Result<(), Error> {
        let ids: Vec<DeviceId> = entry.signatures.iter().map(|(id, _)| *id).collect();
        if ids != e.signers {
            return Err(refuse(e.seq, "its signatures are not the signers it names"));
        }
        let keys = self.signer_keys(e)?;
        let message = signed_message(&entry.bytes);
        for (id, sig) in &entry.signatures {
            let key = keys.iter().find(|(k, _)| k == id).expect("every signer has a key").1;
            key.verify(&message, sig).map_err(|_| refuse(e.seq, "a signature does not verify"))?;
        }
        Ok(())
    }

    /// The key for each signer `e` needs: every subject proving possession,
    /// then exactly the authorizers `required_signers` asks for.
    fn signer_keys(&self, e: &Entry) -> Result<Vec<(DeviceId, SigningPublic)>, Error> {
        let (possession, authorizers) = required_signers(e);
        let mut keys = Vec::new();
        for s in possession {
            if !e.signers.contains(&s.id()) {
                return Err(refuse(e.seq, format!("the device it adds, {:?}, has not signed it", s.name)));
            }
            keys.push((s.id(), s.signing));
        }
        let others: Vec<DeviceId> = e.signers.iter().filter(|id| !keys.iter().any(|(k, _)| k == *id)).copied().collect();
        if others.len() != authorizers {
            return Err(refuse(e.seq, if authorizers == 0 { "it has a signer beyond the devices it adds" } else { "it needs exactly one signer already on the list" }));
        }
        for id in others {
            keys.push((id, self.authorizer(e, id)?.signing));
        }
        Ok(keys)
    }

    fn authorizer(&self, e: &Entry, id: DeviceId) -> Result<&Device, Error> {
        let d = self.devices.iter().find(|d| d.id() == id).ok_or_else(|| refuse(e.seq, "its signer is not a device on the list"))?;
        if !may_authorize(e.action, d, self.recovery()) {
            return Err(refuse(e.seq, format!("{:?} may not sign a {:?}", d.name, e.action)));
        }
        Ok(d)
    }

    fn add(&mut self, s: &Subject, seq: u64) -> Result<(), Error> {
        if self.seen.contains(&s.device.0) || self.seen.contains(&s.signing.0) || s.device.0 == s.signing.0 {
            return Err(refuse(seq, "it adds a key this list has held before"));
        }
        if !s.signing.is_strong() {
            return Err(refuse(seq, "it adds a signing key no one can verify with (not a canonical point of the curve, or of small order)"));
        }
        if self.devices.iter().any(|d| same_name(&d.name, &s.name)) {
            return Err(refuse(seq, format!("a device on the list is already called {:?}", s.name)));
        }
        self.seen.extend([s.device.0, s.signing.0]);
        self.held.push((s.id(), s.signing));
        self.devices.push(Device { kind: s.kind, name: s.name.clone(), os: s.os.clone(), device: s.device, signing: s.signing, added: seq });
        Ok(())
    }

    /// Entry 0's hash: which list this is, at any head.
    pub fn root(&self) -> [u8; 32] {
        self.root
    }

    /// The signing key of `device` if this list has ever held it, removed
    /// or not: whose signature a session record may carry.
    pub fn signer(&self, device: DeviceId) -> Option<SigningPublic> {
        self.held.iter().find(|(d, _)| *d == device).map(|(_, k)| *k)
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
        let chain = Chain::genesis(&entry)?;
        let wraps = chain.wrap_to(&newest, |_| true)?;
        Ok((chain, Batch { entries: vec![entry], newest, links: Vec::new(), wraps }))
    }

    /// Changes made as one post at `time`, signed by `signer`, a device on
    /// the list that stays on it, holding `held`, the current generation.
    /// Adds leave the generation; each removal and each rotate-recovery
    /// makes the next. Only the last generation is wrapped to devices —
    /// every device left on the list when anything rotated, else just the
    /// devices added — and each earlier generation is wrapped under the one
    /// after it. Each entry is verified onto the chain as it is made, so a
    /// batch this returns is one every client accepts.
    pub fn batch(&self, held: &UserKey, changes: Vec<Change<'_>>, signer: DeviceId, key: &SigningKey, time: u64) -> Result<(Chain, Batch), Error> {
        if held.generation() != self.generation() || held.id() != self.key_id() {
            return Err(Error("the user key held is not the one the device list leaves current".into()));
        }
        let mut b = Building { chain: self.clone(), current: held.clone(), entries: Vec::new(), links: Vec::new(), added: Vec::new() };
        for change in changes {
            b.push(change, signer, key, time)?;
        }
        let rotated = !b.links.is_empty();
        let wraps = b.chain.wrap_to(&b.current, |d| rotated || b.added.contains(&d.id()))?;
        Ok((b.chain, Batch { entries: b.entries, newest: b.current, links: b.links, wraps }))
    }

    fn wrap_to(&self, key: &UserKey, to: impl Fn(&Device) -> bool) -> Result<Vec<(DeviceId, Vec<u8>)>, Error> {
        self.devices.iter().filter(|d| to(d)).map(|d| Ok((d.id(), key.wrap_to(&d.device)?))).collect()
    }
}

fn check_genesis(e: &Entry) -> Result<(), Error> {
    let why = if e.seq != 0 {
        "the list does not start at entry 0"
    } else if e.prev != [0; 32] {
        "entry 0 names a previous entry"
    } else if e.action != Action::Add || e.subjects[0].kind != Kind::Device {
        "entry 0 must add the first device"
    } else if e.subjects.get(1).is_some_and(|r| r.kind != Kind::Recovery) {
        "entry 0's second device must be the recovery device"
    } else if e.generation != 1 {
        "entry 0 must leave user key generation 1"
    } else {
        return Ok(());
    };
    Err(refuse(e.seq, why))
}

/// A batch as it is built: the chain so far, the newest key, and what the
/// post will carry.
struct Building {
    chain: Chain,
    current: UserKey,
    entries: Vec<SignedEntry>,
    links: Vec<Vec<u8>>,
    added: Vec<DeviceId>,
}

impl Building {
    fn push(&mut self, change: Change<'_>, signer: DeviceId, key: &SigningKey, time: u64) -> Result<(), Error> {
        let (action, subject, own) = change.parts();
        // An add brings a name; a rotate-recovery's kit takes the name the
        // kit it replaces had.
        if action == Action::Add
            && let Some(d) = self.chain.devices.iter().find(|d| confusable_names(&d.name, &subject.name))
        {
            return Err(Error(format!("{:?} reads like {:?}, already on your device list — name this device something that doesn't look like another", subject.name, d.name)));
        }
        let mut entry = self.chain.next_entry(action, subject.clone(), time);
        let next = if action.rotates() { Some(self.current.next()?) } else { None };
        if let Some(n) = &next {
            entry.generation = n.generation();
            entry.key_id = n.id();
        }
        let mut keys = vec![(signer, key)];
        keys.extend(own.map(|k| (subject.id(), k)));
        let signed = entry.sign(&keys)?;
        self.chain = self.chain.extend(&signed)?;
        self.entries.push(signed);
        if let Some(n) = next {
            self.links.push(n.wrap_previous(&self.current)?);
            self.current = n;
        }
        if action == Action::Add {
            self.added.push(subject.id());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::e2e::{self, DeviceKey};
    use crate::recovery::RecoveryKit;
    use crate::user_key::UserKeys;

    const T: u64 = 1_790_000_000;

    fn gen0(e: &SignedEntry) -> Result<Chain, Error> {
        Chain::genesis(e)
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
        Chain::verify(entries, None).unwrap()
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
        assert!(Chain::verify(&[], None).is_err());
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
        // rotate-recovery through a batch, by the kit: the new kit signs
        // its own entry, and the key rotates to every device but the old kit.
        let kit2 = Dev::recovery(&RecoveryKit::generate());
        let (c2, b2) = before.batch(&g1, vec![Change::RotateRecovery(kit2.subject(), &kit2.signing)], kit.id(), &kit.signing, T).unwrap();
        assert_eq!(c2.recovery().unwrap().id(), kit2.id());
        assert_eq!(b2.entries[0].signatures.len(), 2);
        assert!(b2.wraps.iter().any(|w| w.0 == kit2.id()) && !b2.wraps.iter().any(|w| w.0 == kit.id()));
        // By a device, while a kit exists: refused.
        let err = before.batch(&g1, vec![Change::RotateRecovery(kit2.subject(), &kit2.signing)], laptop.id(), &laptop.signing, T).err().unwrap();
        assert!(err.0.contains("may not sign"), "{err}");
    }

    #[test]
    fn d1_with_no_kit_a_device_makes_one_at_once() {
        let laptop = Dev::new("laptop");
        let (c, s) = Chain::start(laptop.subject(), &laptop.signing, None, T).unwrap();
        let kit = Dev::recovery(&RecoveryKit::generate());
        let (c, b) = c.batch(&s.newest, vec![Change::RotateRecovery(kit.subject(), &kit.signing)], laptop.id(), &laptop.signing, T + 1).unwrap();
        assert_eq!(c.recovery().unwrap().id(), kit.id());
        assert_eq!((c.generation(), b.links.len()), (2, 1));
    }

    #[test]
    fn d1_a_pinned_client_refuses_an_older_or_shorter_chain() {
        let (laptop, _kit, phone, mut entries, _) = three();
        entries.push(signed(&entries, Action::Remove, laptop.subject(), &[&phone], rotate));
        let pin = chain(&entries).head();
        let e = Chain::verify(&entries[..2], Some(pin)).unwrap_err();
        assert!(e.0.contains("old list"), "{e}");
        assert_eq!(Chain::verify(&entries, Some(pin)).unwrap().head(), pin);
        entries.push(signed(&entries, Action::Add, Dev::new("new").subject(), &[&phone], |_| {}));
        assert!(Chain::verify(&entries, Some(pin)).is_ok());
    }

    #[test]
    fn d1_a_pinned_client_refuses_a_forked_chain() {
        let (laptop, kit, phone, entries, _) = three();
        let pin = chain(&entries).head();
        let mut fork = entries[..1].to_vec();
        fork.push(signed(&fork, Action::Add, Dev::new("rogue").subject(), &[&laptop], |_| {}));
        assert!(Chain::verify(&fork, Some(pin)).unwrap_err().0.contains("forked"));
        // A fork at seq 0: another genesis entirely, by the same devices.
        let (c, s) = Chain::start(laptop.subject(), &laptop.signing, Some((kit.subject(), &kit.signing)), T + 5).unwrap();
        let (_, add) = c.batch(&s.newest, vec![Change::Add(phone.subject())], laptop.id(), &laptop.signing, T).unwrap();
        let other = [s.entries, add.entries].concat();
        assert!(Chain::verify(&other, Some(pin)).unwrap_err().0.contains("forked"));
        assert!(Chain::verify(&other, Some(Head { seq: 0, hash: entries[0].hash() })).unwrap_err().0.contains("forked"));
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

    /// A name that only reads like one on the list — another script's
    /// letter, a full-width form, `1` or `I` for `l` — is not written; one
    /// that merely shares letters is. A chain holding such a name, written
    /// some other way, still verifies: it is the writer's check, not the
    /// chain's.
    #[test]
    fn d1_a_name_that_reads_like_one_on_the_list_is_not_written() {
        let (laptop, _kit, phone, entries, g1) = three();
        for alike in ["l\u{430}ptop", "\u{FF4C}\u{FF41}\u{FF50}\u{FF54}\u{FF4F}\u{FF50}", "1aptop", "Iaptop", "PHONE", "laptop ", "laptop\u{A0}", "lap\u{3164}top", "lap\u{34F}top"] {
            let mut s = Dev::new("x").subject();
            s.name = alike.into();
            let e = chain(&entries).batch(&g1, vec![Change::Add(s)], laptop.id(), &laptop.signing, T).err().expect("refused").0;
            assert!(e.contains("reads like"), "{alike:?}: {e}");
        }
        for different in ["laptop 2", "lapdog", "phones"] {
            let mut s = Dev::new("x").subject();
            s.name = different.into();
            assert!(chain(&entries).batch(&g1, vec![Change::Add(s)], laptop.id(), &laptop.signing, T).is_ok(), "{different:?}");
        }
        let mut s = Dev::new("x").subject();
        s.name = "l\u{430}ptop".into();
        let e = signed(&entries, Action::Add, s, &[&phone], |_| {});
        assert!(chain(&entries).extend(&e).is_ok(), "verifying does not read the confusables table");
    }

    /// Names compare with ASCII folded only, and the refused list is
    /// literal: it holds every `Cc` character, as `char::is_control` agrees
    /// today, and the doc's ranges are the constant's.
    #[test]
    fn d1_names_fold_ascii_only_and_refuse_a_literal_list() {
        assert!(same_name("Laptop", "lAPTOP"));
        for (a, b) in [("ΑΣ", "ας"), ("Σ", "σ"), ("İ", "i\u{307}"), ("ẞ", "ß"), ("Ą", "ą"), ("K", "\u{212A}")] {
            assert!(!same_name(a, b), "{a} {b}");
        }
        for c in (0..=0x10FFFF).filter_map(char::from_u32) {
            if c.is_control() {
                assert!(refused_in_name(c), "U+{:04X}", c as u32);
            }
        }
        assert!(!refused_in_name('a') && !refused_in_name('Ą') && !refused_in_name(' '));
        assert!(refused_in_name('\u{2065}'), "unassigned, inside U+2060–206F");
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

    /// A device whose signing key verifies nothing is refused, as the
    /// registry refuses it: the identity point, of small order.
    #[test]
    fn an_add_with_a_small_order_signing_key_is_refused() {
        let laptop = Dev::new("laptop");
        let (chain, start) = Chain::start(laptop.subject(), &laptop.signing, None, T).unwrap();
        let mut identity = [0u8; 32];
        identity[0] = 1;
        let weak = Subject { kind: Kind::Device, name: "weak".into(), os: "linux".into(), device: DeviceKey::generate().public(), signing: SigningPublic(identity) };
        let Err(err) = chain.batch(&start.newest, vec![Change::Add(weak)], laptop.id(), &laptop.signing, T + 1) else { panic!("a small-order signing key was added") };
        assert!(err.0.contains("signing key"), "{err:?}");
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
        assert!(Chain::verify(&[g], None).is_err());
    }

    /// M1: a stolen laptop cannot replace the kit with its own: only the
    /// kit authorizes rotate-recovery while one exists.
    #[test]
    fn m1_a_thief_cannot_replace_the_kit() {
        let (lk, ls) = (DeviceKey::generate(), SigningKey::generate());
        let (pk, ps) = (DeviceKey::generate(), SigningKey::generate());
        let kit = RecoveryKit::generate().device();
        let rec = Subject { kind: Kind::Recovery, name: "recovery kit".into(), os: String::new(), device: kit.key.public(), signing: kit.signing.public() };
        let (c, start) = Chain::start(subj(Kind::Device, "phone", &pk, &ps), &ps, Some((rec, &kit.signing)), 1).unwrap();
        let (c, _) = c.batch(&start.newest, vec![Change::Add(subj(Kind::Device, "laptop", &lk, &ls))], pk.id(), &ps, 2).unwrap();
        let thief_kit = RecoveryKit::generate().device();
        let trec = Subject { kind: Kind::Recovery, name: "thief kit".into(), os: String::new(), device: thief_kit.key.public(), signing: thief_kit.signing.public() };
        let r = c.batch(&start.newest, vec![Change::RotateRecovery(trec, &thief_kit.signing)], lk.id(), &ls, 3);
        assert!(r.err().unwrap().0.contains("may not sign"));
        assert_eq!(c.recovery().unwrap().device, kit.key.public());
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
