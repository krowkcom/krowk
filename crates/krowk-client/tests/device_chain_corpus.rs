//! The device-chain corpus (ticket D1): every accept and refuse rule of
//! `krowk_client::device_chain`'s verifier as a case another implementation
//! — the registry's structural check — can replay, written to
//! `tests/fixtures/device_chain_corpus.json`.
//!
//! Every key, user key and signature is deterministic, so the file is too.
//! This test fails when the committed file differs from what it would
//! write; `KROWK_UPDATE_CORPUS=1 cargo test -p krowk-client --test
//! device_chain_corpus` rewrites it after an intended change.
//!
//! A case holds its entries (`bytes` and the wire `signatures`, in hex), the
//! trust it is verified under, `now`, and two expectations: `verify` (the
//! whole chain accepted with its state, or refused with a reason code and
//! the seq) and `prefix` (`verify_prefix`: the chain as far as it verified
//! and the entry refused after it, or refused outright). The reason codes
//! are this file's contract: `code` below maps the verifier's refusals to
//! them, and a refusal it does not know fails the test.

use krowk_client::device_chain::{Action, Chain, Entry, Head, Kind, Pending, SignedEntry, Subject, Trust, CLOCK_SKEW, RECOVERY_ROTATION_DELAY, REFUSED_IN_NAMES};
use krowk_client::e2e::{hex, DeviceKey, Error, SigningKey};
use krowk_client::recovery::RecoveryKit;
use krowk_client::user_key::{UserKey, UserKeyId};
use serde_json::{json, Value};

const T: u64 = 1_790_000_000;
const DAY: u64 = 24 * 60 * 60;
const GENERATOR: &str = "crates/krowk-client/tests/device_chain_corpus.rs (krowk-cli)";
const PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/device_chain_corpus.json");

struct K {
    key: DeviceKey,
    signing: SigningKey,
    name: &'static str,
    kind: Kind,
}

impl K {
    fn device(n: u8, name: &'static str) -> K {
        K { key: DeviceKey::from_secret(&[n; 32]).unwrap(), signing: SigningKey::from_secret(&[n + 100; 32]).unwrap(), name, kind: Kind::Device }
    }

    fn kit(n: u8, name: &'static str) -> K {
        let d = RecoveryKit::from_bytes([n; 16]).device();
        K { key: d.key, signing: d.signing, name, kind: Kind::Recovery }
    }

    fn subject(&self) -> Subject {
        Subject { kind: self.kind, name: self.name.into(), os: if self.kind == Kind::Device { "linux".into() } else { String::new() }, device: self.key.public(), signing: self.signing.public() }
    }
}

fn uk(g: u32) -> UserKeyId {
    UserKey::from_bytes(g, [g as u8; 32]).unwrap().id()
}

fn sign(e: &Entry, by: &[&K]) -> SignedEntry {
    e.sign(&by.iter().map(|k| (k.key.id(), &k.signing)).collect::<Vec<_>>()).unwrap()
}

fn genesis(subjects: Vec<Subject>, generation: u32, time: u64) -> Entry {
    Entry { seq: 0, prev: [0; 32], action: Action::Add, subjects, generation, key_id: uk(generation.max(1)), time, signers: Vec::new() }
}

/// The entry after `prev`, carrying generation `g`.
fn after(prev: &SignedEntry, action: Action, subject: Subject, g: u32, time: u64) -> Entry {
    let seq = Entry::decode(&prev.bytes).unwrap().seq + 1;
    Entry { seq, prev: prev.hash(), action, subjects: vec![subject], generation: g, key_id: uk(g), time, signers: Vec::new() }
}

struct World {
    laptop: K,
    phone: K,
    tablet: K,
    kit: K,
    kit2: K,
    thief_kit: K,
    outsider: K,
}

fn world() -> World {
    World {
        laptop: K::device(1, "laptop"),
        phone: K::device(2, "phone"),
        tablet: K::device(3, "tablet"),
        kit: K::kit(1, "recovery kit"),
        kit2: K::kit(2, "new kit"),
        thief_kit: K::kit(3, "thief kit"),
        outsider: K::device(9, "outsider"),
    }
}

/// Entry 0 (laptop and kit) and entry 1 (the laptop adds the phone).
fn base(w: &World) -> Vec<SignedEntry> {
    let g = sign(&genesis(vec![w.laptop.subject(), w.kit.subject()], 1, T), &[&w.laptop, &w.kit]);
    let a = sign(&after(&g, Action::Add, w.phone.subject(), 1, T + 1), &[&w.laptop]);
    vec![g, a]
}

fn push(mut v: Vec<SignedEntry>, e: SignedEntry) -> Vec<SignedEntry> {
    v.push(e);
    v
}

fn pinned_at(entries: &[SignedEntry], seq: usize, verified_at: u64, first_seen: Option<(u64, u64)>) -> Trust {
    Trust::Pinned { head: Head { seq: seq as u64, hash: entries[seq].hash() }, verified_at, first_seen }
}

fn fresh(n: usize) -> Trust {
    Trust::Fresh { received_at: vec![T; n] }
}

/// The corpus's reason code for a refusal, from the verifier's message.
fn code(e: &Error) -> &'static str {
    let m = e.0.as_str();
    let table: &[(&str, &str)] = &[
        ("the device list is empty", "empty"),
        ("which this krowk does not read", "unknown_version"),
        ("unknown action", "unknown_action"),
        ("unknown device kind", "unknown_kind"),
        ("bad recovery flag", "malformed"),
        ("an entry is about one device", "subject_count"),
        ("only entry 0 is about two devices", "subject_count"),
        ("its signers are not one to three", "signers_malformed"),
        ("trailing bytes", "trailing_bytes"),
        ("not in canonical form", "non_canonical"),
        ("cut short", "truncated"),
        ("not UTF-8", "bad_utf8"),
        ("has a control, bidirectional or invisible character", "name_characters"),
        ("–64 bytes", "name_length"),
        ("–32 bytes", "os_length"),
        ("does not start at entry 0", "genesis_seq"),
        ("entry 0 names a previous entry", "genesis_prev"),
        ("entry 0 must add the first device", "genesis_action"),
        ("second device must be the recovery device", "genesis_second_subject"),
        ("entry 0 must leave user key generation 1", "genesis_generation"),
        ("has not signed it", "missing_possession_signature"),
        ("has a signer beyond the devices it adds", "extra_signer"),
        ("needs exactly one signer already on the list", "authorizer_count"),
        ("its signer is not a device on the list", "signer_not_listed"),
        ("may not sign", "signer_not_eligible"),
        ("a signature does not verify", "bad_signature"),
        ("its signatures are not the signers it names", "signature_list_mismatch"),
        ("it does not follow entry", "seq_gap"),
        ("it does not extend entry", "prev_mismatch"),
        ("it is backdated", "backdated"),
        ("keys or name of the recovery device a rotation is pending to", "pending_kit_taken"),
        ("a recovery device is added only by rotate-recovery", "add_recovery_kind"),
        ("removes a device that is not on the list", "remove_unknown"),
        ("cannot be removed", "remove_recovery"),
        ("names a device other than the one on the list", "remove_mismatch"),
        ("rotate-recovery must add a recovery device", "rotate_not_recovery"),
        ("rotates to a user key id this list has held before", "key_id_reused"),
        ("adds a key this list has held before", "key_reused"),
        ("is already called", "duplicate_name"),
        ("cancels a recovery rotation that is not pending", "cancel_not_pending"),
        ("takes effect only at", "rotation_not_due"),
        ("before its delay was over", "rotation_received_early"),
        ("already pending", "rotation_already_pending"),
        ("gave no receipt time", "missing_receipt"),
        ("its time is out of range", "time_out_of_range"),
        (", and a ", "generation_not_next"),
        ("and its key id as they were", "generation_changed"),
        ("older than entry", "pin_older"),
        ("from the one this device last verified", "pin_forked"),
    ];
    table.iter().find(|(needle, _)| m.contains(needle)).map(|(_, c)| *c).unwrap_or_else(|| panic!("no corpus code for the refusal {m:?}"))
}

/// The seq a refusal names: the entry refused, the pin a list is older
/// than, or where it forks; null for an empty list.
fn refused_seq(e: &Error) -> Value {
    let m = e.0.as_str();
    let num_after = |pat: &str| m.find(pat).map(|i| m[i + pat.len()..].chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse::<u64>().unwrap());
    let seq = num_after("older than entry ").or_else(|| num_after("differs at entry ")).or_else(|| num_after("refused at entry "));
    seq.map_or(Value::Null, Value::from)
}

fn refusal(e: &Error) -> Value {
    json!({ "code": code(e), "seq": refused_seq(e) })
}

fn pending(p: &Pending) -> Value {
    json!({ "subject": p.subject.id().to_string(), "authorizer": p.authorizer.to_string(), "seq": p.seq, "time": p.time, "since": p.since, "effective_at": p.effective_at })
}

fn state(c: &Chain) -> Value {
    let devices: Vec<Value> = c
        .devices()
        .iter()
        .map(|d| json!({ "id": d.id().to_string(), "kind": if d.kind == Kind::Device { "device" } else { "recovery" }, "name": d.name, "os": d.os, "x25519": hex(&d.device.0), "ed25519": hex(&d.signing.0), "added": d.added }))
        .collect();
    json!({
        "head": { "seq": c.head().seq, "hash": hex(&c.head().hash) },
        "generation": c.generation(),
        "key_id": c.key_id().to_string(),
        "devices": devices,
        "pending": c.pending().map_or(Value::Null, pending),
    })
}

fn trust_json(t: &Trust) -> Value {
    match t {
        Trust::Pinned { head, verified_at, first_seen } => json!({ "mode": "pinned", "head": { "seq": head.seq, "hash": hex(&head.hash) }, "verified_at": verified_at, "first_seen": first_seen.map(|(s, t)| json!([s, t])) }),
        Trust::Fresh { received_at } => json!({ "mode": "fresh", "received_at": received_at }),
    }
}

fn case(name: &str, rule: &str, entries: Vec<SignedEntry>, trust: Trust, now: u64) -> Value {
    let verify = match Chain::verify(&entries, trust.clone(), now) {
        Ok(c) => json!({ "accept": state(&c) }),
        Err(e) => json!({ "refuse": refusal(&e) }),
    };
    let prefix = match Chain::verify_prefix(&entries, trust.clone(), now) {
        Ok(v) => json!({ "chain": state(&v.chain), "rejected": v.rejected.as_ref().map(|(_, e)| refusal(e)) }),
        Err(e) => json!({ "refuse": refusal(&e) }),
    };
    json!({
        "name": name,
        "rule": rule,
        "entries": entries.iter().map(|e| json!({ "bytes": hex(&e.bytes), "signatures": hex(&e.signatures_bytes()) })).collect::<Vec<_>>(),
        "trust": trust_json(&trust),
        "now": now,
        "expect": { "verify": verify, "prefix": prefix },
    })
}

fn cases() -> Vec<Value> {
    let w = world();
    let b = base(&w);
    let now = T + 100;
    let mut out = Vec::new();
    let mut add = |name: &str, rule: &str, entries: Vec<SignedEntry>, trust: Trust, now: u64| out.push(case(name, rule, entries, trust, now));

    // Seq 0.
    add("genesis_device_only", "seq 0 adds the first device, self-signed, generation 1", vec![sign(&genesis(vec![w.laptop.subject()], 1, T), &[&w.laptop])], fresh(1), now);
    add("genesis_with_kit", "seq 0 may add the recovery device, which signs too", b[..1].to_vec(), fresh(1), now);
    add("empty", "an empty list is refused", vec![], fresh(0), now);
    add("genesis_not_self_signed", "seq 0 must be signed by the device it adds", vec![sign(&genesis(vec![w.laptop.subject()], 1, T), &[&w.phone])], fresh(1), now);
    add("genesis_kit_unsigned", "the recovery device seq 0 adds signs it itself", vec![sign(&genesis(vec![w.laptop.subject(), w.kit.subject()], 1, T), &[&w.laptop])], fresh(1), now);
    add("genesis_extra_signer", "seq 0 has no signer beyond the devices it adds", vec![sign(&genesis(vec![w.laptop.subject()], 1, T), &[&w.laptop, &w.phone])], fresh(1), now);
    add("genesis_generation_2", "seq 0 leaves generation 1", vec![sign(&genesis(vec![w.laptop.subject()], 2, T), &[&w.laptop])], fresh(1), now);
    add("genesis_kit_alone", "seq 0 adds a device first", vec![sign(&genesis(vec![w.kit.subject()], 1, T), &[&w.kit])], fresh(1), now);
    add("genesis_second_device", "seq 0's second subject is a recovery device", vec![sign(&genesis(vec![w.laptop.subject(), w.phone.subject()], 1, T), &[&w.laptop, &w.phone])], fresh(1), now);
    {
        let mut g = genesis(vec![w.laptop.subject()], 1, T);
        g.prev = [1; 32];
        add("genesis_prev", "seq 0 names no previous entry", vec![sign(&g, &[&w.laptop])], fresh(1), now);
        let mut g = genesis(vec![w.laptop.subject()], 1, T);
        g.seq = 1;
        add("genesis_seq", "a list starts at seq 0", vec![sign(&g, &[&w.laptop])], fresh(1), now);
    }
    // Encoding.
    {
        let mut t = b[0].clone();
        t.bytes.push(0);
        add("trailing_bytes", "an entry has no trailing bytes", vec![t], fresh(1), now);
        let mut v = b[0].clone();
        v.bytes[0] = 2;
        add("unknown_version", "an entry's version is 1", vec![v], fresh(1), now);
        let mut a = b.clone();
        a[1].bytes[41] = 9;
        add("unknown_action", "actions are 1–4", a, fresh(2), now);
        let mut bidi = b[0].clone();
        let at = bidi.bytes.windows(6).position(|x| x == b"laptop").unwrap();
        bidi.bytes[at..at + 6].copy_from_slice("lap\u{202E}".as_bytes());
        add("name_bidi", "names refuse control, bidirectional and invisible characters", vec![bidi], fresh(1), now);
        let mut sig = b[0].clone();
        sig.signatures[0].1[0] ^= 1;
        add("bad_signature", "every signature verifies", vec![sig], fresh(1), now);
        let mut ids = b.clone();
        ids[1].signatures[0].0 = w.phone.key.id();
        add("signature_list_mismatch", "the signature list's ids are the signers the bytes name", ids, fresh(2), now);
    }
    // Add, remove, signers.
    add("add_device", "a listed device adds a device, generation unchanged", b.clone(), fresh(2), now);
    add("remove_device", "a removal rotates the generation by one", push(b.clone(), sign(&after(&b[1], Action::Remove, w.laptop.subject(), 2, T + 2), &[&w.phone])), fresh(3), now);
    add("signer_not_listed", "the authorizer is a device on the list", push(b.clone(), sign(&after(&b[1], Action::Add, w.tablet.subject(), 1, T + 2), &[&w.outsider])), fresh(3), now);
    {
        let mut forged = sign(&after(&b[1], Action::Add, w.tablet.subject(), 1, T + 2), &[&w.laptop]);
        let other = sign(&after(&b[1], Action::Add, w.tablet.subject(), 1, T + 2), &[&w.outsider]);
        forged.signatures[0].1 = other.signatures[0].1;
        add("forged_signer", "a listed device's id with another key's signature is refused", push(b.clone(), forged), fresh(3), now);
    }
    add("two_authorizers", "exactly one authorizer", push(b.clone(), sign(&after(&b[1], Action::Add, w.tablet.subject(), 1, T + 2), &[&w.laptop, &w.phone])), fresh(3), now);
    {
        let r = push(b.clone(), sign(&after(&b[1], Action::Remove, w.laptop.subject(), 2, T + 2), &[&w.phone]));
        let after_removal = push(r.clone(), sign(&after(&r[2], Action::Add, w.tablet.subject(), 2, T + 3), &[&w.laptop]));
        add("removed_signer", "a removed device cannot sign", after_removal, fresh(4), now);
        let back = push(r.clone(), sign(&after(&r[2], Action::Add, w.laptop.subject(), 2, T + 3), &[&w.phone]));
        add("readd_removed_keys", "no key is added twice", back, fresh(4), now);
    }
    add("remove_recovery", "the recovery device cannot be removed", push(b.clone(), sign(&after(&b[1], Action::Remove, w.kit.subject(), 2, T + 2), &[&w.laptop])), fresh(3), now);
    {
        let mut disguised = w.kit.subject();
        disguised.kind = Kind::Device;
        add("remove_recovery_disguised", "the recovery device cannot be removed as a device either", push(b.clone(), sign(&after(&b[1], Action::Remove, disguised, 2, T + 2), &[&w.laptop])), fresh(3), now);
        let mut wrong = w.phone.subject();
        wrong.name = "other".into();
        add("remove_mismatch", "a removal names the device exactly as listed", push(b.clone(), sign(&after(&b[1], Action::Remove, wrong, 2, T + 2), &[&w.laptop])), fresh(3), now);
    }
    add("remove_unknown", "a removal names a listed device", push(b.clone(), sign(&after(&b[1], Action::Remove, w.tablet.subject(), 2, T + 2), &[&w.laptop])), fresh(3), now);
    add("add_recovery_kind", "a recovery device is added only by rotate-recovery", push(b.clone(), sign(&after(&b[1], Action::Add, w.kit2.subject(), 1, T + 2), &[&w.laptop])), fresh(3), now);
    {
        let mut twin = w.tablet.subject();
        twin.name = "Laptop".into();
        add("duplicate_name", "no two listed devices share a name, ignoring case", push(b.clone(), sign(&after(&b[1], Action::Add, twin, 1, T + 2), &[&w.laptop])), fresh(3), now);
    }
    // Generations and key ids.
    add("remove_generation_same", "a removal must move the generation up by one", push(b.clone(), sign(&after(&b[1], Action::Remove, w.laptop.subject(), 1, T + 2), &[&w.phone])), fresh(3), now);
    add("remove_generation_skip", "a removal must move the generation up by exactly one", push(b.clone(), sign(&after(&b[1], Action::Remove, w.laptop.subject(), 3, T + 2), &[&w.phone])), fresh(3), now);
    add("add_generation_moves", "an add leaves the generation", push(b.clone(), sign(&after(&b[1], Action::Add, w.tablet.subject(), 2, T + 2), &[&w.laptop])), fresh(3), now);
    {
        let mut e = after(&b[1], Action::Add, w.tablet.subject(), 1, T + 2);
        e.key_id = uk(7);
        add("add_key_id_changes", "every entry carries the current key id", push(b.clone(), sign(&e, &[&w.laptop])), fresh(3), now);
        let mut e = after(&b[1], Action::Remove, w.laptop.subject(), 2, T + 2);
        e.key_id = uk(1);
        add("rotation_key_id_reused", "a rotation takes a key id the list has not held", push(b.clone(), sign(&e, &[&w.phone])), fresh(3), now);
    }
    {
        let r1 = sign(&after(&b[1], Action::Remove, w.phone.subject(), 2, T + 2), &[&w.laptop]);
        let t = sign(&after(&r1, Action::Add, w.tablet.subject(), 2, T + 3), &[&w.laptop]);
        let r2 = sign(&after(&t, Action::Remove, w.tablet.subject(), 3, T + 4), &[&w.laptop]);
        add("batch_of_removals", "N removals are N generations", [b.clone(), vec![r1, t, r2]].concat(), fresh(5), now);
    }
    // Sequence and pins.
    {
        let mut gap = after(&b[1], Action::Add, w.tablet.subject(), 1, T + 2);
        gap.seq = 3;
        add("seq_gap", "seq is one more than the previous entry's", push(b.clone(), sign(&gap, &[&w.laptop])), fresh(3), now);
        let mut fork = after(&b[1], Action::Add, w.tablet.subject(), 1, T + 2);
        fork.prev = b[0].hash();
        add("prev_mismatch", "prev is the previous entry's hash", push(b.clone(), sign(&fork, &[&w.laptop])), fresh(3), now);
        let longer = push(b.clone(), sign(&after(&b[1], Action::Add, w.tablet.subject(), 1, T + 2), &[&w.laptop]));
        add("pinned_same_head", "a pinned device takes its own head", b.clone(), pinned_at(&b, 1, T + 1, None), now);
        add("pinned_longer", "a pinned device takes a list that extends its pin", longer.clone(), pinned_at(&b, 1, T + 1, None), now);
        add("pinned_older", "a pinned device refuses a list shorter than its pin", b.clone(), pinned_at(&longer, 2, T + 2, None), now);
        let rogue = push(b[..1].to_vec(), sign(&after(&b[0], Action::Add, w.tablet.subject(), 1, T + 1), &[&w.laptop]));
        add("pinned_forked", "a pinned device refuses a list with another entry at its pin", rogue, pinned_at(&b, 1, T + 1, None), now);
        let old = push(b.clone(), sign(&after(&b[1], Action::Add, w.tablet.subject(), 1, T - 2 * CLOCK_SKEW), &[&w.laptop]));
        add("backdated", "a pinned device refuses a new entry dated before it last verified, less the skew", old, pinned_at(&b, 1, T, None), now);
    }
    // Recovery rotation.
    let propose = |prev: &SignedEntry, kit: &K, by: &K, g: u32, time: u64| sign(&after(prev, Action::RotateRecovery, kit.subject(), g, time), &[by, kit]);
    add("rotate_by_kit", "the recovery device replaces itself at once, rotating the key", push(b.clone(), propose(&b[1], &w.kit2, &w.kit, 2, T + 2)), fresh(3), now);
    add("rotate_new_kit_unsigned", "the new recovery device signs its own entry", push(b.clone(), sign(&after(&b[1], Action::RotateRecovery, w.kit2.subject(), 2, T + 2), &[&w.kit])), fresh(3), now);
    {
        let g = sign(&genesis(vec![w.laptop.subject()], 1, T), &[&w.laptop]);
        add("rotate_no_kit", "with no recovery device a device makes one at once", vec![g.clone(), propose(&g, &w.kit2, &w.laptop, 2, T + 1)], fresh(2), now);
    }
    let p = propose(&b[1], &w.thief_kit, &w.laptop, 1, T + 10);
    let proposed = push(b.clone(), p.clone());
    add("propose", "a device-authorized rotation is pending; the old kit stays", proposed.clone(), fresh(3), now);
    add("propose_rotating", "a proposal leaves the generation", push(b.clone(), propose(&b[1], &w.thief_kit, &w.laptop, 2, T + 10)), fresh(3), now);
    {
        let mut clash = w.thief_kit.subject();
        clash.name = "Phone".into();
        add("propose_name_clash", "a proposed kit may not take a listed device's name", push(b.clone(), sign(&after(&b[1], Action::RotateRecovery, clash, 1, T + 10), &[&w.laptop, &w.thief_kit])), fresh(3), now);
    }
    add("pending_second_proposal", "one rotation is pending at a time", push(proposed.clone(), propose(&p, &w.kit2, &w.phone, 1, T + 11)), fresh(4), now);
    add("pending_kit_has_no_rights", "a proposed kit cannot sign", push(proposed.clone(), sign(&after(&p, Action::Add, w.tablet.subject(), 1, T + 11), &[&w.thief_kit])), fresh(4), now);
    add("pending_kit_unremovable", "the old kit stays unremovable while a rotation is pending", push(proposed.clone(), sign(&after(&p, Action::Remove, w.kit.subject(), 2, T + 11), &[&w.laptop])), fresh(4), now);
    {
        let mut taken = w.thief_kit.subject();
        taken.kind = Kind::Device;
        taken.name = "other".into();
        add("pending_kit_taken", "an add may not take the pending kit's keys or name", push(proposed.clone(), sign(&after(&p, Action::Add, taken, 1, T + 11), &[&w.laptop])), fresh(4), now);
    }
    let cancel = sign(&after(&p, Action::CancelRecoveryRotation, w.thief_kit.subject(), 1, T + 11), &[&w.kit]);
    add("cancel_by_kit", "the recovery device cancels a pending rotation", push(proposed.clone(), cancel.clone()), fresh(4), now);
    add("cancel_by_device", "only the recovery device cancels", push(proposed.clone(), sign(&after(&p, Action::CancelRecoveryRotation, w.thief_kit.subject(), 1, T + 11), &[&w.phone])), fresh(4), now);
    add("cancel_nothing_pending", "a cancellation names the pending rotation", push(b.clone(), sign(&after(&b[1], Action::CancelRecoveryRotation, w.thief_kit.subject(), 1, T + 11), &[&w.kit])), fresh(3), now);
    add("remove_authorizer_voids", "removing the proposer voids the rotation", push(proposed.clone(), sign(&after(&p, Action::Remove, w.laptop.subject(), 2, T + 11), &[&w.phone])), fresh(4), now);
    let done_at = T + 10 + RECOVERY_ROTATION_DELAY;
    let completion = propose(&p, &w.thief_kit, &w.laptop, 2, done_at);
    let completed = push(proposed.clone(), completion.clone());
    add("complete_early_pinned", "a completion before the delay, by this device's clock, is refused; the prefix ends at the proposal", completed.clone(), pinned_at(&b, 1, T + 5, None), T + 20 * DAY);
    add("complete_pinned_first_seen", "a pinned device takes the completion seven days after it first saw the proposal", completed.clone(), pinned_at(&proposed, 2, T + 5, Some((2, T + 10))), done_at);
    add("complete_pinned_first_seen_early", "and not a second sooner", completed.clone(), pinned_at(&proposed, 2, T + 5, Some((2, T + 10))), done_at - 1);
    add("complete_reverify_past_pin", "entries at or before the pin are not judged by the clock again", completed.clone(), pinned_at(&completed, 3, done_at + 1, None), done_at + 100 * DAY);
    add("complete_fresh_on_time", "a fresh machine takes a completion the registry received after the delay", completed.clone(), Trust::Fresh { received_at: vec![T, T, T + 10, done_at] }, done_at + DAY);
    add("complete_fresh_received_early", "a fresh machine refuses a completion the registry received before the delay", completed.clone(), Trust::Fresh { received_at: vec![T, T, T + 10, T + DAY] }, done_at + DAY);
    add("complete_fresh_not_due", "a fresh machine refuses a completion before the delay by its own clock", completed.clone(), Trust::Fresh { received_at: vec![T, T, T + 10, done_at] }, T + DAY);
    add("complete_fresh_missing_receipt", "a fresh machine needs a receipt time for a proposal", completed.clone(), Trust::Fresh { received_at: vec![T, T] }, done_at + DAY);
    {
        let backdated = propose(&b[1], &w.thief_kit, &w.laptop, 1, T - 8 * DAY);
        add("propose_backdated_pinned", "a backdated proposal is refused on a pinned device", push(b.clone(), backdated.clone()), pinned_at(&b, 1, T, None), now);
        add("propose_backdated_within_skew", "within the skew it is pending from first sight", push(b.clone(), backdated), pinned_at(&b, 1, T - 8 * DAY + CLOCK_SKEW, None), now);
    }
    // Unicode. Names are compared with ASCII A–Z folded and every other
    // byte exact, and refused characters are a literal code-point list, so
    // no case here depends on a runtime's Unicode tables. Each pair: the
    // laptop at seq 0 takes the first name, the phone added at seq 1 the
    // second; the names' UTF-8 is in the entry bytes, and in `rule`.
    let named = |first: &str, second: &str| {
        let mut l = w.laptop.subject();
        l.name = first.into();
        let mut p = w.phone.subject();
        p.name = second.into();
        let g = sign(&genesis(vec![l], 1, T), &[&w.laptop]);
        let a = sign(&after(&g, Action::Add, p, 1, T + 1), &[&w.laptop]);
        vec![g, a]
    };
    let utf8 = |s: &str| hex(s.as_bytes());
    for (name, first, second, rule) in [
        ("unicode_final_sigma", "ΑΣ", "ας", "Greek capitals and final-sigma lowercase are distinct names"),
        ("unicode_sigma", "Σ", "σ", "Σ and σ are distinct names"),
        ("unicode_dotted_i", "İ", "i\u{307}", "İ (U+0130) and i + U+0307 are distinct names"),
        ("unicode_sharp_s", "ẞ", "ß", "ẞ (U+1E9E) and ß are distinct names"),
        ("unicode_ogonek", "Ą", "ą", "Ą and ą are distinct names"),
        ("unicode_kelvin", "K", "\u{212A}", "ASCII K and the Kelvin sign are distinct names"),
        ("ascii_case_duplicate", "Laptop", "lAPTOP", "names equal with ASCII A–Z folded are one name"),
        ("ascii_case_duplicate_mixed", "Ąb", "ĄB", "the ASCII letters of a name fold, the rest compare exact"),
    ] {
        add(name, &format!("{rule} ({} vs {})", utf8(first), utf8(second)), named(first, second), fresh(2), now);
    }
    // One refused name per end of each refused range: "x" and the code
    // point, patched into a valid entry's bytes (the encoder refuses to
    // write it), so the refusal is the reader's.
    for &(lo, hi) in REFUSED_IN_NAMES {
        for cp in if lo == hi { vec![lo] } else { vec![lo, hi] } {
            let c = char::from_u32(cp).unwrap();
            let bad = format!("x{c}");
            let placeholder = format!("x{}", "q".repeat(c.len_utf8()));
            let mut l = w.laptop.subject();
            l.name = placeholder.clone();
            let mut g = sign(&genesis(vec![l], 1, T), &[&w.laptop]);
            let at = g.bytes.windows(placeholder.len()).position(|x| x == placeholder.as_bytes()).unwrap();
            g.bytes[at..at + placeholder.len()].copy_from_slice(bad.as_bytes());
            add(&format!("refused_u{cp:04x}"), &format!("a name may not hold U+{cp:04X} (range U+{lo:04X}–{hi:04X}; name bytes {})", utf8(&bad)), vec![g], fresh(1), now);
        }
    }
    // And the OS field refuses the same list.
    {
        let mut g = sign(&genesis(vec![w.laptop.subject()], 1, T), &[&w.laptop]);
        let at = g.bytes.windows(5).position(|x| x == b"linux").unwrap();
        g.bytes[at + 4] = 0x07;
        add("refused_in_os", "an OS may not hold a refused code point (U+0007)", vec![g], fresh(1), now);
    }
    out
}

fn corpus() -> String {
    let v = json!({
        "generator": GENERATOR,
        "about": "Device-chain verifier cases (engineering/devices.md in Canon). Entry bytes and the wire signature list are hex; expect.verify is Chain::verify, expect.prefix is Chain::verify_prefix. Codes and seqs are the contract; see the generator's module doc.",
        "constants": { "recovery_rotation_delay": RECOVERY_ROTATION_DELAY, "clock_skew": CLOCK_SKEW },
        "cases": cases(),
    });
    serde_json::to_string_pretty(&v).unwrap() + "\n"
}

/// The committed corpus is exactly what this generator writes.
#[test]
fn d1_the_device_chain_corpus_is_current() {
    let fresh = corpus();
    if std::env::var_os("KROWK_UPDATE_CORPUS").is_some() {
        std::fs::write(PATH, &fresh).unwrap();
        return;
    }
    let committed = std::fs::read_to_string(PATH).unwrap_or_default();
    assert!(committed == fresh, "tests/fixtures/device_chain_corpus.json is stale: rerun with KROWK_UPDATE_CORPUS=1 and commit it");
}

/// Every case the corpus says is accepted, or refused, is — a sanity check
/// on the cases themselves, so a rule is not recorded the wrong way round.
#[test]
fn d1_the_corpus_covers_both_outcomes_of_each_rule() {
    let v: Value = serde_json::from_str(&corpus()).unwrap();
    let cases = v["cases"].as_array().unwrap();
    let get = |n: &str| cases.iter().find(|c| c["name"] == n).unwrap_or_else(|| panic!("no case {n}"));
    for n in ["genesis_device_only", "genesis_with_kit", "add_device", "remove_device", "batch_of_removals", "rotate_by_kit", "rotate_no_kit", "propose", "cancel_by_kit", "remove_authorizer_voids", "complete_pinned_first_seen", "complete_reverify_past_pin", "complete_fresh_on_time", "pinned_same_head", "pinned_longer", "propose_backdated_within_skew"] {
        assert!(get(n)["expect"]["verify"]["accept"].is_object(), "{n} should be accepted");
    }
    let refused = cases.iter().filter(|c| c["expect"]["verify"]["refuse"].is_object()).count();
    assert!(refused >= 40, "{refused} refusals");
    assert_eq!(get("propose")["expect"]["verify"]["accept"]["pending"]["authorizer"], get("propose")["expect"]["verify"]["accept"]["devices"][0]["id"]);
    assert_eq!(get("complete_early_pinned")["expect"]["prefix"]["rejected"]["code"], "rotation_not_due");
    assert_eq!(get("remove_recovery")["expect"]["verify"]["refuse"]["code"], "remove_recovery");
    for n in ["unicode_final_sigma", "unicode_sigma", "unicode_dotted_i", "unicode_sharp_s", "unicode_ogonek", "unicode_kelvin"] {
        assert!(get(n)["expect"]["verify"]["accept"].is_object(), "{n} should be distinct names");
    }
    for n in ["ascii_case_duplicate", "ascii_case_duplicate_mixed"] {
        assert_eq!(get(n)["expect"]["verify"]["refuse"]["code"], "duplicate_name", "{n}");
    }
    for c in cases.iter().filter(|c| c["name"].as_str().unwrap().starts_with("refused_")) {
        assert_eq!(c["expect"]["verify"]["refuse"]["code"], "name_characters", "{}", c["name"]);
    }
}
