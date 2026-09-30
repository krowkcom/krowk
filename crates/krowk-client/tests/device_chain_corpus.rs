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
//! A case holds its entries (`bytes` and the wire `signatures`, in hex),
//! the pin it is verified against (`null` to verify from seq 0), and what
//! `Chain::verify` returns: the chain accepted with its state, or refused
//! with a reason code and the seq. The reason codes are this file's
//! contract: `code` below maps the verifier's refusals to them, and a
//! refusal it does not know fails the test.

use krowk_client::device_chain::{Action, Chain, Entry, Head, Kind, SignedEntry, Subject, REFUSED_IN_NAMES};
use krowk_client::e2e::{hex, DeviceKey, Error, SigningKey};
use krowk_client::recovery::RecoveryKit;
use krowk_client::user_key::{UserKey, UserKeyId};
use serde_json::{json, Value};

const T: u64 = 1_790_000_000;
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

/// A pin at `entries[seq]`.
fn pin(entries: &[SignedEntry], seq: usize) -> Option<Head> {
    Some(Head { seq: seq as u64, hash: entries[seq].hash() })
}

/// The corpus's reason code for a refusal, from the verifier's message.
fn code(e: &Error) -> &'static str {
    let m = e.0.as_str();
    let table: &[(&str, &str)] = &[
        ("the device list is empty", "empty"),
        ("which this krowk does not read", "unknown_version"),
        ("unknown action", "unknown_action"),
        ("unknown device kind", "unknown_kind"),
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
        ("a recovery device is added only by rotate-recovery", "add_recovery_kind"),
        ("removes a device that is not on the list", "remove_unknown"),
        ("cannot be removed", "remove_recovery"),
        ("names a device other than the one on the list", "remove_mismatch"),
        ("rotate-recovery must add a recovery device", "rotate_not_recovery"),
        ("rotates to a user key id this list has held before", "key_id_reused"),
        ("adds a key this list has held before", "key_reused"),
        ("is already called", "duplicate_name"),
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
    })
}

fn case(name: &str, rule: &str, entries: Vec<SignedEntry>, pin: Option<Head>) -> Value {
    let expect = match Chain::verify(&entries, pin) {
        Ok(c) => json!({ "accept": state(&c) }),
        Err(e) => json!({ "refuse": refusal(&e) }),
    };
    json!({
        "name": name,
        "rule": rule,
        "entries": entries.iter().map(|e| json!({ "bytes": hex(&e.bytes), "signatures": hex(&e.signatures_bytes()) })).collect::<Vec<_>>(),
        "pin": pin.map(|h| json!({ "seq": h.seq, "hash": hex(&h.hash) })),
        "expect": expect,
    })
}

fn cases() -> Vec<Value> {
    let w = world();
    let b = base(&w);
    let mut out = Vec::new();
    let mut add = |name: &str, rule: &str, entries: Vec<SignedEntry>, pin: Option<Head>| out.push(case(name, rule, entries, pin));

    // Seq 0.
    add("genesis_device_only", "seq 0 adds the first device, self-signed, generation 1", vec![sign(&genesis(vec![w.laptop.subject()], 1, T), &[&w.laptop])], None);
    add("genesis_with_kit", "seq 0 may add the recovery device, which signs too", b[..1].to_vec(), None);
    add("empty", "an empty list is refused", vec![], None);
    add("genesis_not_self_signed", "seq 0 must be signed by the device it adds", vec![sign(&genesis(vec![w.laptop.subject()], 1, T), &[&w.phone])], None);
    add("genesis_kit_unsigned", "the recovery device seq 0 adds signs it itself", vec![sign(&genesis(vec![w.laptop.subject(), w.kit.subject()], 1, T), &[&w.laptop])], None);
    add("genesis_extra_signer", "seq 0 has no signer beyond the devices it adds", vec![sign(&genesis(vec![w.laptop.subject()], 1, T), &[&w.laptop, &w.phone])], None);
    add("genesis_generation_2", "seq 0 leaves generation 1", vec![sign(&genesis(vec![w.laptop.subject()], 2, T), &[&w.laptop])], None);
    add("genesis_kit_alone", "seq 0 adds a device first", vec![sign(&genesis(vec![w.kit.subject()], 1, T), &[&w.kit])], None);
    add("genesis_second_device", "seq 0's second subject is a recovery device", vec![sign(&genesis(vec![w.laptop.subject(), w.phone.subject()], 1, T), &[&w.laptop, &w.phone])], None);
    {
        let mut g = genesis(vec![w.laptop.subject()], 1, T);
        g.prev = [1; 32];
        add("genesis_prev", "seq 0 names no previous entry", vec![sign(&g, &[&w.laptop])], None);
        let mut g = genesis(vec![w.laptop.subject()], 1, T);
        g.seq = 1;
        add("genesis_seq", "a list starts at seq 0", vec![sign(&g, &[&w.laptop])], None);
    }
    // Encoding.
    {
        let mut t = b[0].clone();
        t.bytes.push(0);
        add("trailing_bytes", "an entry has no trailing bytes", vec![t], None);
        let mut v = b[0].clone();
        v.bytes[0] = 2;
        add("unknown_version", "an entry's version is 1", vec![v], None);
        let mut a = b.clone();
        a[1].bytes[41] = 9;
        add("unknown_action", "actions are 1–3", a, None);
        let mut bidi = b[0].clone();
        let at = bidi.bytes.windows(6).position(|x| x == b"laptop").unwrap();
        bidi.bytes[at..at + 6].copy_from_slice("lap\u{202E}".as_bytes());
        add("name_bidi", "names refuse control, bidirectional and invisible characters", vec![bidi], None);
        let mut sig = b[0].clone();
        sig.signatures[0].1[0] ^= 1;
        add("bad_signature", "every signature verifies", vec![sig], None);
        let mut ids = b.clone();
        ids[1].signatures[0].0 = w.phone.key.id();
        add("signature_list_mismatch", "the signature list's ids are the signers the bytes name", ids, None);
    }
    // Add, remove, signers.
    add("add_device", "a listed device adds a device, generation unchanged", b.clone(), None);
    add("remove_device", "a removal rotates the generation by one", push(b.clone(), sign(&after(&b[1], Action::Remove, w.laptop.subject(), 2, T + 2), &[&w.phone])), None);
    add("signer_not_listed", "the authorizer is a device on the list", push(b.clone(), sign(&after(&b[1], Action::Add, w.tablet.subject(), 1, T + 2), &[&w.outsider])), None);
    {
        let mut forged = sign(&after(&b[1], Action::Add, w.tablet.subject(), 1, T + 2), &[&w.laptop]);
        let other = sign(&after(&b[1], Action::Add, w.tablet.subject(), 1, T + 2), &[&w.outsider]);
        forged.signatures[0].1 = other.signatures[0].1;
        add("forged_signer", "a listed device's id with another key's signature is refused", push(b.clone(), forged), None);
    }
    add("two_authorizers", "exactly one authorizer", push(b.clone(), sign(&after(&b[1], Action::Add, w.tablet.subject(), 1, T + 2), &[&w.laptop, &w.phone])), None);
    {
        let r = push(b.clone(), sign(&after(&b[1], Action::Remove, w.laptop.subject(), 2, T + 2), &[&w.phone]));
        let after_removal = push(r.clone(), sign(&after(&r[2], Action::Add, w.tablet.subject(), 2, T + 3), &[&w.laptop]));
        add("removed_signer", "a removed device cannot sign", after_removal, None);
        let back = push(r.clone(), sign(&after(&r[2], Action::Add, w.laptop.subject(), 2, T + 3), &[&w.phone]));
        add("readd_removed_keys", "no key is added twice", back, None);
    }
    add("remove_recovery", "the recovery device cannot be removed", push(b.clone(), sign(&after(&b[1], Action::Remove, w.kit.subject(), 2, T + 2), &[&w.laptop])), None);
    {
        let mut disguised = w.kit.subject();
        disguised.kind = Kind::Device;
        add("remove_recovery_disguised", "the recovery device cannot be removed as a device either", push(b.clone(), sign(&after(&b[1], Action::Remove, disguised, 2, T + 2), &[&w.laptop])), None);
        let mut wrong = w.phone.subject();
        wrong.name = "other".into();
        add("remove_mismatch", "a removal names the device exactly as listed", push(b.clone(), sign(&after(&b[1], Action::Remove, wrong, 2, T + 2), &[&w.laptop])), None);
    }
    add("remove_unknown", "a removal names a listed device", push(b.clone(), sign(&after(&b[1], Action::Remove, w.tablet.subject(), 2, T + 2), &[&w.laptop])), None);
    add("add_recovery_kind", "a recovery device is added only by rotate-recovery", push(b.clone(), sign(&after(&b[1], Action::Add, w.kit2.subject(), 1, T + 2), &[&w.laptop])), None);
    {
        let mut twin = w.tablet.subject();
        twin.name = "Laptop".into();
        add("duplicate_name", "no two listed devices share a name, ignoring case", push(b.clone(), sign(&after(&b[1], Action::Add, twin, 1, T + 2), &[&w.laptop])), None);
    }
    // Generations and key ids.
    add("remove_generation_same", "a removal must move the generation up by one", push(b.clone(), sign(&after(&b[1], Action::Remove, w.laptop.subject(), 1, T + 2), &[&w.phone])), None);
    add("remove_generation_skip", "a removal must move the generation up by exactly one", push(b.clone(), sign(&after(&b[1], Action::Remove, w.laptop.subject(), 3, T + 2), &[&w.phone])), None);
    add("add_generation_moves", "an add leaves the generation", push(b.clone(), sign(&after(&b[1], Action::Add, w.tablet.subject(), 2, T + 2), &[&w.laptop])), None);
    {
        let mut e = after(&b[1], Action::Add, w.tablet.subject(), 1, T + 2);
        e.key_id = uk(7);
        add("add_key_id_changes", "every entry carries the current key id", push(b.clone(), sign(&e, &[&w.laptop])), None);
        let mut e = after(&b[1], Action::Remove, w.laptop.subject(), 2, T + 2);
        e.key_id = uk(1);
        add("rotation_key_id_reused", "a rotation takes a key id the list has not held", push(b.clone(), sign(&e, &[&w.phone])), None);
    }
    {
        let r1 = sign(&after(&b[1], Action::Remove, w.phone.subject(), 2, T + 2), &[&w.laptop]);
        let t = sign(&after(&r1, Action::Add, w.tablet.subject(), 2, T + 3), &[&w.laptop]);
        let r2 = sign(&after(&t, Action::Remove, w.tablet.subject(), 3, T + 4), &[&w.laptop]);
        add("batch_of_removals", "N removals are N generations", [b.clone(), vec![r1, t, r2]].concat(), None);
    }
    // Sequence and pins.
    {
        let mut gap = after(&b[1], Action::Add, w.tablet.subject(), 1, T + 2);
        gap.seq = 3;
        add("seq_gap", "seq is one more than the previous entry's", push(b.clone(), sign(&gap, &[&w.laptop])), None);
        let mut fork = after(&b[1], Action::Add, w.tablet.subject(), 1, T + 2);
        fork.prev = b[0].hash();
        add("prev_mismatch", "prev is the previous entry's hash", push(b.clone(), sign(&fork, &[&w.laptop])), None);
        let longer = push(b.clone(), sign(&after(&b[1], Action::Add, w.tablet.subject(), 1, T + 2), &[&w.laptop]));
        add("pinned_same_head", "a pinned device takes its own head", b.clone(), pin(&b, 1));
        add("pinned_longer", "a pinned device takes a list that extends its pin", longer.clone(), pin(&b, 1));
        add("pinned_older", "a pinned device refuses a list shorter than its pin", b.clone(), pin(&longer, 2));
        let rogue = push(b[..1].to_vec(), sign(&after(&b[0], Action::Add, w.tablet.subject(), 1, T + 1), &[&w.laptop]));
        add("pinned_forked", "a pinned device refuses a list with another entry at its pin", rogue, pin(&b, 1));
    }
    // Recovery rotation.
    let propose = |prev: &SignedEntry, kit: &K, by: &K, g: u32, time: u64| sign(&after(prev, Action::RotateRecovery, kit.subject(), g, time), &[by, kit]);
    add("rotate_by_kit", "the recovery device replaces itself at once, rotating the key", push(b.clone(), propose(&b[1], &w.kit2, &w.kit, 2, T + 2)), None);
    add("rotate_new_kit_unsigned", "the new recovery device signs its own entry", push(b.clone(), sign(&after(&b[1], Action::RotateRecovery, w.kit2.subject(), 2, T + 2), &[&w.kit])), None);
    {
        let g = sign(&genesis(vec![w.laptop.subject()], 1, T), &[&w.laptop]);
        add("rotate_no_kit", "with no recovery device a device makes one at once", vec![g.clone(), propose(&g, &w.kit2, &w.laptop, 2, T + 1)], None);
    }
    add("rotate_by_device_while_kit", "while a kit exists, only the kit authorizes rotate-recovery", push(b.clone(), propose(&b[1], &w.thief_kit, &w.laptop, 2, T + 10)), None);
    {
        let mut clash = w.thief_kit.subject();
        clash.name = "Phone".into();
        add("rotate_name_clash", "a new kit may not take a listed device's name", push(b.clone(), sign(&after(&b[1], Action::RotateRecovery, clash, 2, T + 10), &[&w.kit, &w.thief_kit])), None);
        let replaced = push(b.clone(), propose(&b[1], &w.kit2, &w.kit, 2, T + 2));
        add("rotated_kit_cannot_sign", "the replaced kit is no longer on the list", push(replaced.clone(), sign(&after(&replaced[2], Action::Add, w.tablet.subject(), 2, T + 3), &[&w.kit])), None);
    }
    add("time_is_informational", "nothing checks an entry's time", push(b.clone(), sign(&after(&b[1], Action::Add, w.tablet.subject(), 1, 1), &[&w.laptop])), pin(&b, 1));
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
        add(name, &format!("{rule} ({} vs {})", utf8(first), utf8(second)), named(first, second), None);
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
            add(&format!("refused_u{cp:04x}"), &format!("a name may not hold U+{cp:04X} (range U+{lo:04X}–{hi:04X}; name bytes {})", utf8(&bad)), vec![g], None);
        }
    }
    // And the OS field refuses the same list.
    {
        let mut g = sign(&genesis(vec![w.laptop.subject()], 1, T), &[&w.laptop]);
        let at = g.bytes.windows(5).position(|x| x == b"linux").unwrap();
        g.bytes[at + 4] = 0x07;
        add("refused_in_os", "an OS may not hold a refused code point (U+0007)", vec![g], None);
    }
    out
}

fn corpus() -> String {
    let v = json!({
        "generator": GENERATOR,
        "about": "Device-chain verifier cases (engineering/devices.md in Canon). Entry bytes and the wire signature list are hex; pin is the head the verifier holds, or null to verify from seq 0; expect is Chain::verify's result. Codes and seqs are the contract; see the generator's module doc.",
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
    for n in ["genesis_device_only", "genesis_with_kit", "add_device", "remove_device", "batch_of_removals", "rotate_by_kit", "rotate_no_kit", "pinned_same_head", "pinned_longer", "time_is_informational"] {
        assert!(get(n)["expect"]["accept"].is_object(), "{n} should be accepted");
    }
    let refused = cases.iter().filter(|c| c["expect"]["refuse"].is_object()).count();
    assert!(refused >= 40, "{refused} refusals");
    assert_eq!(get("remove_recovery")["expect"]["refuse"]["code"], "remove_recovery");
    assert_eq!(get("rotate_by_device_while_kit")["expect"]["refuse"]["code"], "signer_not_eligible");
    for n in ["unicode_final_sigma", "unicode_sigma", "unicode_dotted_i", "unicode_sharp_s", "unicode_ogonek", "unicode_kelvin"] {
        assert!(get(n)["expect"]["accept"].is_object(), "{n} should be distinct names");
    }
    for n in ["ascii_case_duplicate", "ascii_case_duplicate_mixed"] {
        assert_eq!(get(n)["expect"]["refuse"]["code"], "duplicate_name", "{n}");
    }
    for c in cases.iter().filter(|c| c["name"].as_str().unwrap().starts_with("refused_")) {
        assert_eq!(c["expect"]["refuse"]["code"], "name_characters", "{}", c["name"]);
    }
}
