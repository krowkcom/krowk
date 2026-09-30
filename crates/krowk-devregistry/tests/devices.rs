//! Devices a person owns, against the stand-in: the signed device list, the
//! user key wrapped to it, keys bound to their device, and pairing a new
//! machine over the mailbox — driven by krowk-client's own chain and
//! pairing, through krowk-api's calls, since those are what `krowk devices
//! add` and `krowk sync join` run (canon, engineering/devices.md).

mod common;

use common::{Server, request};
use jiff::SignedDuration;
use krowk_api::sync::{ListPost, PairingStep};
use krowk_api::Client;
use krowk_client::device_chain::{Chain, Change, Kind, SignedEntry, Subject};
use krowk_client::e2e::{self, DeviceKey, DeviceSigner, SigningKey};
use krowk_client::pairing::{Binding, NewDevice, PairA, PairB, PairingCode, PeerKind};
use krowk_client::recovery::RecoveryKit;
use krowk_client::user_key::UserKey;

/// Two keys of one person in one workspace, the first freshly signed in.
const LAPTOP: &str = "krowk_sk_owner#laptop-fresh";
const DESKTOP: &str = "krowk_sk_owner#desktop";

struct Dev {
    key: DeviceKey,
    signing: SigningKey,
    name: &'static str,
}

impl Dev {
    fn new(name: &'static str) -> Dev {
        Dev { key: DeviceKey::generate(), signing: SigningKey::generate(), name }
    }

    fn subject(&self) -> Subject {
        Subject { kind: Kind::Device, name: self.name.into(), os: "linux".into(), device: self.key.public(), signing: self.signing.public() }
    }

    fn client(&self, server: &Server, token: &str) -> Client {
        let signing = SigningKey::from_secret(&*self.signing.secret_bytes()).unwrap();
        Client::new(&server.at("/v1"), token).signed_by(DeviceSigner::new(self.key.id(), signing).shared())
    }
}

fn now() -> u64 {
    jiff::Timestamp::now().as_second() as u64
}

fn post(batch: &krowk_client::device_chain::Batch, start_over: bool) -> ListPost {
    ListPost {
        entries: batch.entries.iter().map(|e| (e2e::hex(&e.bytes), e2e::hex(&e.signatures_bytes()))).collect(),
        links: batch.links.iter().map(|l| e2e::hex(l)).collect(),
        wraps: batch.wraps.iter().map(|(d, w)| (d.to_string(), e2e::hex(w))).collect(),
        start_over,
    }
}

/// A laptop that set sync up with a recovery kit: the chain and the user key.
fn init(server: &Server, laptop: &Dev) -> (Chain, UserKey) {
    let kit = RecoveryKit::generate().device();
    let recovery = Subject { kind: Kind::Recovery, name: "recovery kit".into(), os: String::new(), device: kit.key.public(), signing: kit.signing.public() };
    let (chain, batch) = Chain::start(laptop.subject(), &laptop.signing, Some((recovery, &kit.signing)), None, now()).unwrap();
    laptop.client(server, LAPTOP).init_device_list(&post(&batch, false)).unwrap();
    (chain, batch.newest)
}

fn served(client: &Client) -> Vec<SignedEntry> {
    let list = client.device_list_all().unwrap();
    list.entries.iter().map(|e| SignedEntry::from_parts(e2e::unhex(&e.entry).unwrap(), &e2e::unhex(&e.signatures).unwrap()).unwrap()).collect()
}

/// The chain as posted comes back byte for byte, with the registry's
/// receipt time and the epoch, and verifies from seq 0 on a fresh machine.
#[test]
fn a_chain_posted_at_init_reads_back_with_its_receipt_time_and_epoch() {
    let server = Server::new();
    let laptop = Dev::new("laptop");
    let (chain, _) = init(&server, &laptop);
    let list = laptop.client(&server, DESKTOP).device_list_all().unwrap();
    assert_eq!(list.epoch, 1);
    assert_eq!(list.entries.len(), 1);
    assert!(list.entries[0].received_at.abs_diff(now()) < 60);
    assert_eq!(list.head.unwrap().hash, e2e::hex(&chain.head().hash));
    let again = Chain::verify(&served(&laptop.client(&server, DESKTOP)), None).unwrap();
    assert_eq!(again.head(), chain.head());
}

/// One chain per person: a second init is `chain_exists`; a start-over is
/// a new epoch, never a shorter chain; and init needs a fresh sign-in.
#[test]
fn a_second_init_is_refused_unless_it_starts_over_into_a_new_epoch() {
    let server = Server::new();
    let laptop = Dev::new("laptop");
    init(&server, &laptop);
    let other = Dev::new("other");
    let (_, batch) = Chain::start(other.subject(), &other.signing, None, None, now()).unwrap();
    let refused = other.client(&server, "krowk_sk_owner#other-fresh").init_device_list(&post(&batch, false)).unwrap_err();
    assert_eq!(refused.code(), "chain_exists");
    let stale = other.client(&server, "krowk_sk_owner#other").init_device_list(&post(&batch, true)).unwrap_err();
    assert_eq!(stale.code(), "fresh_sign_in_required");
    other.client(&server, "krowk_sk_owner#other-fresh").init_device_list(&post(&batch, true)).unwrap();
    assert_eq!(other.client(&server, DESKTOP).device_list(None).unwrap().epoch, 2);
    // The old laptop's devices are gone with the old epoch.
    let gone = laptop.client(&server, LAPTOP).user_key().unwrap_err();
    assert_eq!(gone.code(), "unauthorized", "its key was revoked with it");
}

/// An append must extend the head, carry exactly the wraps it makes, and
/// be signed by the device posting it; a remove needs a fresh sign-in.
#[test]
fn an_append_is_held_to_the_head_the_wraps_and_a_fresh_sign_in() {
    let server = Server::new();
    let laptop = Dev::new("laptop");
    let (chain, key) = init(&server, &laptop);
    let desktop = Dev::new("desktop");
    let (_, add) = chain.batch(&key, vec![Change::Add(desktop.subject())], laptop.key.id(), &laptop.signing, now()).unwrap();

    let mut no_wrap = post(&add, false);
    no_wrap.wraps.clear();
    assert_eq!(laptop.client(&server, LAPTOP).append_device_list(&no_wrap).unwrap_err().code(), "device_list_invalid");
    laptop.client(&server, LAPTOP).append_device_list(&post(&add, false)).unwrap();
    let again = laptop.client(&server, LAPTOP).append_device_list(&post(&add, false)).unwrap_err();
    assert_eq!(again.code(), "device_list_stale");

    let chain = Chain::verify(&served(&laptop.client(&server, LAPTOP)), Some(chain.head())).unwrap();
    let (_, remove) = chain.batch(&key, vec![Change::Remove(desktop.subject())], laptop.key.id(), &laptop.signing, now()).unwrap();
    // A key the laptop signs with that has no sign-in from the last five
    // minutes cannot remove: a thief holds the stored key, not the password.
    let plain = laptop.client(&server, "krowk_sk_owner#laptop").append_device_list(&post(&remove, false)).unwrap_err();
    assert_eq!(plain.code(), "fresh_sign_in_required");
}

/// A key speaks for one device: bound at init to the first device, it is
/// refused signed as another; a new machine's key binds on its first
/// signed call, and a removed device takes its keys with it.
#[test]
fn a_key_speaks_for_one_device_and_goes_with_it() {
    let server = Server::new();
    let laptop = Dev::new("laptop");
    let (chain, key) = init(&server, &laptop);
    let desktop = Dev::new("desktop");
    let (chain2, add) = chain.batch(&key, vec![Change::Add(desktop.subject())], laptop.key.id(), &laptop.signing, now()).unwrap();
    laptop.client(&server, LAPTOP).append_device_list(&post(&add, false)).unwrap();

    let crossed = desktop.client(&server, LAPTOP).claim_key_device().unwrap_err();
    assert_eq!(crossed.code(), "device_mismatch", "the laptop's key cannot be moved to the desktop");
    assert_eq!(Client::new(&server.at("/v1"), DESKTOP).open_pairing().unwrap_err().code(), "device_signature_missing");
    assert_eq!(desktop.client(&server, DESKTOP).open_pairing().unwrap_err().code(), "key_has_no_device", "unclaimed, the desktop's key is no device's");
    desktop.client(&server, DESKTOP).claim_key_device().unwrap();
    let wraps = desktop.client(&server, DESKTOP).user_key().unwrap();
    assert_eq!(wraps.wraps.len(), 1);
    let opened = UserKey::unwrap(&e2e::unhex(&wraps.wraps[0].wrapped_key).unwrap(), 1, key.id(), &desktop.key).unwrap();
    assert_eq!(opened, key);

    let (_, remove) = chain2.batch(&key, vec![Change::Remove(desktop.subject())], laptop.key.id(), &laptop.signing, now()).unwrap();
    laptop.client(&server, LAPTOP).append_device_list(&post(&remove, false)).unwrap();
    assert_eq!(desktop.client(&server, DESKTOP).user_key().unwrap_err().code(), "unauthorized", "the desktop's key went with it");
}

/// The whole pairing through the mailbox, with krowk-client's two sides:
/// B finds the person's one open pairing, the five messages go in order,
/// and nothing reaches the chain until A has B's ack.
#[test]
fn a_pairing_runs_krowk_clients_five_messages_in_order() {
    let server = Server::new();
    let laptop = Dev::new("laptop");
    let (chain, key) = init(&server, &laptop);
    let a = laptop.client(&server, LAPTOP);
    let user = a.verify_key().unwrap().user_id;
    let opened = a.open_pairing().unwrap();
    assert_eq!(opened.state, "open");
    let pair_a = PairA::new(PeerKind::SamePersonDevice, &user, laptop.key.id()).unwrap();
    let typed = PairingCode::parse(&pair_a.code().to_string()).unwrap();

    let desktop = Dev::new("desktop");
    let b = Client::new(&server.at("/v1"), DESKTOP);
    let found = b.find_open_pairing().unwrap();
    assert_eq!((found.id.as_str(), found.initiator_device.id.as_str()), (opened.id.as_str(), laptop.key.id().to_string().as_str()));
    assert_eq!(b.verify_key().unwrap().user_id, user, "one person, two keys");
    let me = NewDevice { device: desktop.key.public(), signing: desktop.signing.public(), name: "desktop".into(), os: "linux".into() };
    let binding = Binding { kind: PeerKind::SamePersonDevice, user_id: user.clone(), a_device: laptop.key.id(), b_device: desktop.key.id() };
    let (pair_b, hello) = PairB::start(binding, typed, me).unwrap();
    b.pairing_step(&found.id, PairingStep::Join, &hello).unwrap();
    assert_eq!(b.find_open_pairing().unwrap_err().code(), "no_pairing", "joined, it is no longer open to anyone");

    let seen = a.show_pairing(&opened.id).unwrap();
    let (await_confirm, spake) = pair_a.receive_hello(&e2e::unhex(&seen.joiner_message).unwrap()).unwrap();
    a.pairing_step(&opened.id, PairingStep::Answer, &spake).unwrap();
    let seen = b.show_pairing(&found.id).unwrap();
    let (await_reply, confirm) = pair_b.receive_spake(&e2e::unhex(&seen.initiator_message).unwrap()).unwrap();
    assert_eq!(b.pairing_step(&found.id, PairingStep::Confirmation, &confirm).unwrap().state, "confirmed");

    let seen = a.show_pairing(&opened.id).unwrap();
    let confirmed = await_confirm.receive_confirm(&e2e::unhex(&seen.joiner_confirmation).unwrap()).unwrap();
    let new = confirmed.device().clone();
    let subject = Subject { kind: Kind::Device, name: new.name.clone(), os: new.os.clone(), device: new.device, signing: new.signing };
    let (_, add) = chain.batch(&key, vec![Change::Add(subject)], laptop.key.id(), &laptop.signing, now()).unwrap();
    let (await_ack, reply) = confirmed.approve(b"the chain, the wrap and the signing key").unwrap();
    a.pairing_step(&opened.id, PairingStep::Reply, &reply).unwrap();

    let seen = b.show_pairing(&found.id).unwrap();
    let received = await_reply.receive_reply(&e2e::unhex(&seen.sealed_reply).unwrap()).unwrap();
    let (_, ack) = received.acknowledge();
    assert_eq!(b.pairing_step(&found.id, PairingStep::Acknowledgement, &ack).unwrap().state, "done");

    let seen = a.show_pairing(&opened.id).unwrap();
    await_ack.receive_ack(&e2e::unhex(&seen.joiner_ack).unwrap()).unwrap();
    a.append_device_list(&post(&add, false)).unwrap();
    desktop.client(&server, DESKTOP).claim_key_device().unwrap();
    assert_eq!(desktop.client(&server, DESKTOP).user_key().unwrap().wraps.len(), 1, "the new machine's key speaks for it now");
}

/// One live pairing per person; a step out of turn or sent twice ends it,
/// and an ended one is gone for good — as is one ten minutes old.
#[test]
fn a_pairing_is_one_per_person_and_ends_at_the_first_misstep_or_ten_minutes() {
    let server = Server::new();
    let laptop = Dev::new("laptop");
    init(&server, &laptop);
    let a = laptop.client(&server, LAPTOP);
    let b = Client::new(&server.at("/v1"), DESKTOP);
    let first = a.open_pairing().unwrap();
    assert_eq!(a.open_pairing().unwrap_err().code(), "pairing_open");

    b.pairing_step(&first.id, PairingStep::Join, b"hello").unwrap();
    let twice = b.pairing_step(&first.id, PairingStep::Join, b"hello").unwrap_err();
    assert_eq!(twice.code(), "pairing_out_of_turn", "a second join is somebody else trying the code");
    assert_eq!(a.show_pairing(&first.id).unwrap_err().code(), "pairing_gone");
    assert_eq!(a.pairing_step(&first.id, PairingStep::Answer, b"spake").unwrap_err().code(), "pairing_gone");

    let second = a.open_pairing().unwrap();
    b.pairing_step(&second.id, PairingStep::Join, b"hello").unwrap();
    let early = b.pairing_step(&second.id, PairingStep::Confirmation, b"early").unwrap_err();
    assert_eq!(early.code(), "pairing_out_of_turn", "a confirmation before A's answer");

    let third = a.open_pairing().unwrap();
    b.pairing_step(&third.id, PairingStep::Join, b"hello").unwrap();
    b.end_pairing(&third.id).unwrap();
    assert_eq!(b.show_pairing(&third.id).unwrap_err().code(), "pairing_gone");

    let fourth = a.open_pairing().unwrap();
    server.advance(SignedDuration::from_mins(10));
    assert_eq!(b.find_open_pairing().unwrap_err().code(), "no_pairing");
    assert_eq!(a.show_pairing(&fourth.id).unwrap_err().code(), "pairing_gone");
    a.open_pairing().unwrap();

    // Another person's pairing is not there to be found or stepped on.
    let stranger = Client::new(&server.at("/v1"), "krowk_sk_stranger");
    assert_eq!(stranger.find_open_pairing().unwrap_err().code(), "no_pairing");
}

/// A's steps are the opening device's own, signed, and B's the joining
/// key's: another of the person's keys cannot step in.
#[test]
fn a_pairing_step_from_the_wrong_side_ends_it() {
    let server = Server::new();
    let laptop = Dev::new("laptop");
    init(&server, &laptop);
    let a = laptop.client(&server, LAPTOP);
    let opened = a.open_pairing().unwrap();
    let b = Client::new(&server.at("/v1"), DESKTOP);
    b.pairing_step(&opened.id, PairingStep::Join, b"hello").unwrap();
    // Joined, it is its two parties' alone: another of the person's keys
    // reads it as not existing.
    let other = Client::new(&server.at("/v1"), "krowk_sk_owner#third");
    assert_eq!(other.pairing_step(&opened.id, PairingStep::Confirmation, b"x").unwrap_err().code(), "not_found");
    assert_eq!(other.show_pairing(&opened.id).unwrap_err().code(), "not_found");
    // The joiner answering for A is out of turn, and ends it.
    assert_eq!(b.pairing_step(&opened.id, PairingStep::Reply, b"x").unwrap_err().code(), "device_signature_missing");
    assert_eq!(b.pairing_step(&opened.id, PairingStep::Acknowledgement, b"x").unwrap_err().code(), "pairing_out_of_turn");
    assert_eq!(a.show_pairing(&opened.id).unwrap_err().code(), "pairing_gone");
}

/// The approval mailbox is gone: whatever an old client asks of it is
/// `410 sync_reset`, with the fix in the message.
#[test]
fn the_retired_approval_endpoints_answer_sync_reset() {
    let server = Server::new();
    for (method, path) in [("GET", "/v1/device_approvals"), ("POST", "/v1/device_approvals"), ("GET", "/v1/device_approvals/dap_x"), ("PUT", "/v1/device_approvals/dap_x/approval")] {
        let r = request(method, &server.at(path), "krowk_sk_old", "application/json", "{}");
        assert_eq!((r.status, r.code()), (410, "sync_reset".to_owned()), "{method} {path}");
        assert!(r.text().contains("krowk sync init"));
    }
}
