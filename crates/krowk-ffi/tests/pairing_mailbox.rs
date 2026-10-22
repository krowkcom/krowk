//! R-E2E-3, R-CLIENT-1: the phone's mailbox for krowk-client's joining
//! side, against the stand-in registry. It reads the person's one open
//! pairing, ends it, and once the join is cancelled stops waiting within a
//! second and refuses every call but `end` — as `cancelled`, never as the
//! status 0 the join would count as an ack that may have landed — until
//! the keys are kept: from the ack on, a cancel is too late.

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
fn r_e2e_3_from_the_ack_on_a_cancel_is_too_late() {
    let w = World::new();
    let (mailbox, cancel, _) = opened(&w);
    // The ack is out of turn here, which the registry refuses: what counts
    // is that it went to the registry, cancel or no cancel.
    cancel.store(true, Ordering::SeqCst);
    let e = mailbox.step(PairingStep::Acknowledgement, &[7; 32]).unwrap_err();
    assert_ne!(e.code(), "cancelled", "{e:?}");
    assert!(!mailbox.cancelled());
    let e = mailbox.claim_device().unwrap_err();
    assert_ne!(e.code(), "cancelled", "the claim reaches the registry: {e:?}");
    let started = Instant::now();
    mailbox.pause(Duration::from_millis(400));
    assert!(started.elapsed() >= Duration::from_millis(400), "the pause is not cut short");
}
