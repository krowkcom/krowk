//! R-E2E-3, R-CLIENT-1: the phone's mailbox for krowk-client's joining
//! side, against the stand-in registry. It reads the person's one open
//! pairing, ends it, and once the join is cancelled stops waiting within a
//! second and refuses every call but `end` — as `cancelled`, never as the
//! status 0 the join would count as an ack that may have landed — until
//! the other device's reply is seen: from then on, a cancel is too late.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use common::{World, key};
use krowk_api::Client;
use krowk_api::sync::PairingStep;
use krowk_client::pairing_join::Mailbox;
use krowk_ffi::RegistryMailbox;

/// The host opens a pairing; the phone's key reaches it through the mailbox.
fn opened(w: &World) -> (RegistryMailbox, Arc<AtomicBool>, String) {
    let open = w.host.open_pairing().unwrap();
    let phone = Arc::new(Client::new(&w.api, &key("phone")));
    let cancel = Arc::new(AtomicBool::new(false));
    (RegistryMailbox::new(phone, open.id.clone(), cancel.clone()), cancel, open.id)
}

#[test]
fn r_e2e_3_the_mailbox_reads_the_open_pairing() {
    let w = World::new();
    let (mailbox, _, id) = opened(&w);
    let p = mailbox.read().unwrap();
    assert_eq!(p.id, id);
    assert_eq!(p.initiator_device.id, w.host_device.id().to_string(), "A is the host's device");
    assert!(p.joiner_message.is_empty(), "nothing joined yet");
    assert!(!mailbox.cancelled());
}

#[test]
fn r_e2e_3_a_cancel_cuts_a_pause_short_and_refuses_the_next_calls() {
    let w = World::new();
    let (mailbox, cancel, _) = opened(&w);
    let flag = cancel.clone();
    let canceller = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        flag.store(true, Ordering::SeqCst);
    });
    let started = Instant::now();
    mailbox.pause(Duration::from_secs(10));
    assert!(started.elapsed() < Duration::from_secs(1), "the pause took {:?}", started.elapsed());
    canceller.join().unwrap();
    assert!(mailbox.cancelled());

    let refused = [mailbox.read().map(|_| ()), mailbox.step(PairingStep::Join, b"hello"), mailbox.claim_device()];
    for r in refused {
        let e = r.unwrap_err();
        assert_eq!((e.status, e.code().as_str()), (499, "cancelled"), "{e:?}");
    }
    // Nothing reached the registry: the pairing is still open, unjoined.
    let p = w.host.find_open_pairing().unwrap();
    assert!(p.joiner_message.is_empty(), "{p:?}");
}

/// A cancel before the hello leaves the other device's pairing as it was:
/// nothing of the phone's is in it, and its code still works for a join.
#[test]
fn r_e2e_3_end_before_any_step_leaves_the_pairing_open() {
    let w = World::new();
    let (mailbox, cancel, id) = opened(&w);
    cancel.store(true, Ordering::SeqCst);
    assert!(mailbox.step(PairingStep::Join, &[7; 32]).is_err(), "a cancelled hello is never sent");
    mailbox.end();
    let p = w.host.find_open_pairing().unwrap();
    assert_eq!(p.id, id, "the same pairing is open");
    assert!(p.joiner_message.is_empty(), "{p:?}");
}

#[test]
fn r_e2e_3_end_ends_the_pairing_even_once_cancelled() {
    let w = World::new();
    let (mailbox, cancel, _) = opened(&w);
    // The registry takes an end from the pairing's parties only: the
    // phone's key is one once it has joined. The hello is opaque to it.
    mailbox.step(PairingStep::Join, &[7; 32]).unwrap();
    assert!(!mailbox.read().unwrap().joiner_message.is_empty());
    cancel.store(true, Ordering::SeqCst);
    mailbox.end();
    // The registry serves an ended pairing as gone until another opens.
    let e = w.host.find_open_pairing().unwrap_err();
    assert!(matches!(e.code().as_str(), "pairing_gone" | "no_pairing"), "{e:?}");
    assert!(w.host.open_pairing().is_ok(), "a new pairing can open: the old one is over");
}

#[test]
fn r_e2e_3_a_cancel_after_the_confirmation_looks_once_more_for_the_reply() {
    let w = World::new();
    let (mailbox, cancel, _) = opened(&w);
    // The stand-in takes the messages as opaque: a hello, A's answer, a
    // confirmation, and then the reply, as the two sides would send them.
    mailbox.step(PairingStep::Join, &[7; 32]).unwrap();
    w.host.pairing_step(PairingStep::Answer, &[8; 32]).unwrap();
    mailbox.step(PairingStep::Confirmation, &[9; 32]).unwrap();
    cancel.store(true, Ordering::SeqCst);
    // No reply yet: the last look finds none, and the join is cancelled.
    let e = mailbox.read().unwrap_err();
    assert_eq!((e.status, e.code().as_str()), (499, "cancelled"), "{e:?}");
    // A reply out by then is seen, and the join goes on.
    w.host.pairing_step(PairingStep::Reply, &[10; 64]).unwrap();
    let p = mailbox.read().expect("the reply is read despite the cancel");
    assert!(!p.sealed_reply.is_empty());
    assert!(!mailbox.cancelled(), "too late to cancel");
}

#[test]
fn r_e2e_3_before_the_confirmation_a_cancelled_read_fails_without_looking() {
    // Only a reply seen after this device confirmed makes a cancel too
    // late (the stand-in refuses a reply out of turn, so a registry that
    // serves one early is the mailbox's rule, not shown here).
    let w = World::new();
    let (mailbox, cancel, _) = opened(&w);
    mailbox.step(PairingStep::Join, &[7; 32]).unwrap();
    w.host.pairing_step(PairingStep::Answer, &[8; 32]).unwrap();
    assert!(!mailbox.read().unwrap().initiator_message.is_empty());
    cancel.store(true, Ordering::SeqCst);
    let e = mailbox.read().unwrap_err();
    assert_eq!((e.status, e.code().as_str()), (499, "cancelled"), "{e:?}");
    assert!(mailbox.cancelled());
}
