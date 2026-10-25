//! R-IOS-3, R-CLIENT-1, R-PERF-6: a paired phone lists the person's synced
//! sessions from their sealed indexes and opens one from its latest
//! checkpoint, or from the place its last reading stopped (R-IOS-1), all
//! in krowk-client behind `Account`, never in Swift.
//! Against the stand-in registry, with a host that writes the session at
//! rest through the harness's own writer, and the phone on the device list
//! the host started.


mod common;

use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use common::*;
use krowk_client::e2e::{self, DeviceKey, SigningKey};
use krowk_client::keystore::Keystore;
use krowk_client::sync::store::{self, ReadPlace};
use krowk_ffi::{Account, HeadRecord, KrowkError, ReadPlaceRecord};
use serde_json::json;


fn refused<T>(r: Result<T, KrowkError>) -> String {
    match r {
        Err(KrowkError::Refused { code, .. }) => code,
        Err(e) => panic!("not a refusal: {e:?}"),
        Ok(_) => panic!("not refused"),
    }
}

const SESSION: &str = "01a0ec7b-3333-7000-8000-000000000079";

#[test]
fn r_ios_3_list_sessions_shows_the_index_and_counts_a_record_signed_off_the_list_unreadable() {
    let w = World::new();
    let _host = w.host_session(SESSION, "fix the build", &[event(SESSION, 1, "first")], &[]);

    let list = w.phone.list_sessions().unwrap();
    assert_eq!(list.unreadable, 0, "{list:?}");
    assert_eq!(list.sessions.len(), 1, "{list:?}");
    let s = &list.sessions[0];
    assert_eq!((s.id.as_str(), s.title.as_str(), s.cwd.as_str()), (SESSION, "fix the build", "/src/app"));
    assert!(s.updated_ms > 0, "the index's write time: {s:?}");
    assert_eq!(s.hosted_by.as_deref(), Some(w.host_device.id().to_string().as_str()), "the lease holder: {s:?}");
    assert_eq!(s.host_device.as_deref(), Some(w.host_device.id().to_string().as_str()), "the index's host: {s:?}");
    assert!(s.head.as_ref().is_some_and(|h| h.index == 1 && h.digest.len() == 32), "{s:?}");

    // A record a device the list never held signed: left out, counted.
    let (stranger, stranger_signing) = (DeviceKey::generate(), SigningKey::generate());
    let forged = "01a0ec7b-3333-7000-8000-0000000000ff";
    w.record(forged, "forged", (stranger.id(), &stranger_signing));
    let list = w.phone.list_sessions().unwrap();
    assert_eq!(list.unreadable, 1, "{list:?}");
    assert_eq!(list.sessions.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), [SESSION]);
    // And it does not open.
    assert_eq!(refused(w.phone.open_session(forged.into(), None, None)), "session_unreadable");
}

#[test]
fn r_ios_3_open_session_reads_from_the_checkpoint_and_catch_up_returns_only_whats_new() {
    let w = World::new();
    let mut host = w.host_session(SESSION, "fix the build", &[event(SESSION, 1, "before")], &[event(SESSION, 2, "after")]);

    let opened = w.phone.open_session(SESSION.into(), None, None).unwrap();
    assert_eq!(opened.id(), SESSION);
    assert_eq!(opened.title(), "fix the build");
    // The checkpoint's compacted context, then the tail after it.
    assert_eq!(runs(&opened.events().unwrap()), ["before", "after"]);
    let head = opened.head().expect("a head");
    // Chunks 0 (events), 1 (checkpoint), 2 (the tail).
    assert_eq!(head.index, 2);
    assert!(opened.catch_up().unwrap().is_empty(), "nothing written since");

    host.write(&[event(SESSION, 3, "later"), event(SESSION, 4, "latest")]);
    assert_eq!(runs(&opened.catch_up().unwrap()), ["later", "latest"]);
    assert_eq!(runs(&opened.events().unwrap()), ["before", "after", "later", "latest"]);
    assert_eq!(opened.head().unwrap().index, 3);
    assert!(opened.catch_up().unwrap().is_empty());

    // Two at once take turns: each new event comes back once, in order.
    host.write(&[event(SESSION, 5, "one"), event(SESSION, 6, "two")]);
    let both: Vec<_> = std::thread::scope(|t| {
        let racing: Vec<_> = (0..2).map(|_| t.spawn(|| runs(&opened.catch_up().unwrap()))).collect();
        racing.into_iter().flat_map(|r| r.join().unwrap()).collect()
    });
    assert_eq!(both, ["one", "two"], "read once, between the two");
    assert_eq!(runs(&opened.events().unwrap()), ["before", "after", "later", "latest", "one", "two"]);

    // Opened again, knowing the head it saw: the same log reads.
    let again = w.phone.open_session(SESSION.into(), opened.head(), None).unwrap();
    assert_eq!(again.head(), opened.head());
}

#[test]
fn r_ios_3_open_session_beyond_what_is_served_is_session_rolled_back() {
    let w = World::new();
    let host = w.host_session(SESSION, "t", &[event(SESSION, 1, "before")], &[]);
    let served = w.phone.open_session(SESSION.into(), None, None).unwrap().head().unwrap();
    let beyond = HeadRecord { index: served.index + 3, digest: vec![7; 32] };
    assert_eq!(refused(w.phone.open_session(SESSION.into(), Some(beyond), None)), "session_rolled_back");
    // Another chunk at the head it knows: another log.
    let forked = HeadRecord { index: served.index, digest: vec![7; 32] };
    assert_eq!(refused(w.phone.open_session(SESSION.into(), Some(forked), None)), "session_rolled_back");
    let short = HeadRecord { index: 0, digest: vec![7; 31] };
    assert!(matches!(w.phone.open_session(SESSION.into(), Some(short), None), Err(KrowkError::InvalidInput { .. })));

    // The registry's sealed index names a head past the log it serves (the
    // tail withheld): refused the same way, with no `known` at all.
    let raw = krowk_client::sync::uuid(SESSION);
    let mut index = host.writer.index.clone();
    index.head = Some(krowk_client::sync::store::Head { index: served.index + 2, digest: [9; 32] });
    let sealed = e2e::hex(&e2e::seal_session_index(host.keys.current(), &raw, &serde_json::to_vec(&index).unwrap()));
    w.host.put_sync_session(SESSION, &host.wrapped, None, Some(&sealed), Some(&host.token)).unwrap();
    assert_eq!(refused(w.phone.open_session(SESSION.into(), None, None)), "session_rolled_back");
}

#[test]
fn r_client_1_relay_ticket_and_the_relay_the_phone_uses() {
    let w = World::new();
    let _host = w.host_session(SESSION, "t", &[event(SESSION, 1, "before")], &[]);
    let opened = w.phone.open_session(SESSION.into(), None, Some("ws://127.0.0.1:9".into())).unwrap();
    assert!(!opened.relay_ticket().unwrap().is_empty());

    assert_ne!(w.phone.relay_url(None), "wss://relay.krowk.com", "the stand-in's API is not krowk's");
    assert_eq!(w.phone.relay_url(Some(" ws://127.0.0.1:9 ".into())), "ws://127.0.0.1:9");
    assert_eq!(w.phone.relay_env(), "development");
    let hosted = Account::open(krowk_api::DEFAULT_BASE_URL.into(), signed_in(TOKEN)).unwrap();
    assert_eq!(hosted.relay_url(None), "wss://relay.krowk.com");
    assert_eq!(hosted.relay_url(Some(String::new())), "wss://relay.krowk.com");
    assert_eq!(hosted.relay_env(), if cfg!(debug_assertions) { "development" } else { "production" });
}

#[test]
fn r_client_1_an_account_with_no_user_keys_is_not_paired() {
    let w = World::new();
    let unpaired = Account::open(w.api.clone(), signed_in(&key("phone"))).unwrap();
    assert_eq!(refused(unpaired.list_sessions()), "not_paired");
    assert_eq!(refused(unpaired.open_session(SESSION.into(), None, None)), "not_paired");

    // The device list held and the user keys not, as a pairing cut short
    // between the two writes leaves it.
    Keystore::with(Arc::new(Bridge(w.storage.clone()))).forget_user_keys().unwrap();
    let half = Account::open(w.api.clone(), w.storage.clone()).unwrap();
    assert!(!half.has_user_keys().unwrap());
    assert_eq!(refused(half.list_sessions()), "not_paired");
}

#[test]
fn r_proto_2_an_event_this_build_cannot_decode_is_a_decode_error_and_read_again() {
    let w = World::new();
    let mut host = w.host_session(SESSION, "t", &[event(SESSION, 1, "before")], &[]);
    let opened = w.phone.open_session(SESSION.into(), None, None).unwrap();
    let head = opened.head();

    // A newer host's event, then one this build reads.
    let newer = json!({"id": "01a0ec7b-2222-7000-8000-0000000000aa", "sessionId": SESSION, "timeMs": 9, "type": "turn.teleported"});
    host.write(&[newer, event(SESSION, 10, "known")]);
    for _ in 0..2 {
        match opened.catch_up() {
            // Its place counted from the checkpoint: "before" is event 0.
            Err(KrowkError::Decode { detail }) => assert!(detail.contains("event 1 read since the session's checkpoint"), "{detail}"),
            other => panic!("refused, every time, not skipped: {:?}", other.map(|e| e.len())),
        }
        assert_eq!(opened.head(), head, "nothing kept of a read that did not decode");
        assert_eq!(runs(&opened.events().unwrap()), ["before"]);
    }
    // Opened afresh, the attached events hold it: `events` refuses too.
    let again = w.phone.open_session(SESSION.into(), None, None).unwrap();
    assert!(matches!(again.events(), Err(KrowkError::Decode { .. })));
}

/// A TCP proxy to `to` that keeps every byte its clients sent, so a test
/// reads the request lines of what was asked, in order. Its `/v1` URL.
fn recording_proxy(to: std::net::SocketAddr) -> (String, Arc<Mutex<Vec<u8>>>) {
    use std::io::{Read, Write};
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let sent = Arc::new(Mutex::new(Vec::new()));
    let log = sent.clone();
    std::thread::spawn(move || {
        for client in listener.incoming().map_while(Result::ok) {
            let upstream = std::net::TcpStream::connect(to).unwrap();
            let (mut c_in, mut u_out) = (client.try_clone().unwrap(), upstream.try_clone().unwrap());
            let (mut u_in, mut c_out) = (upstream, client);
            let log = log.clone();
            std::thread::spawn(move || {
                let mut buf = [0u8; 16 * 1024];
                while let Ok(n @ 1..) = c_in.read(&mut buf) {
                    log.lock().unwrap().extend_from_slice(&buf[..n]);
                    if u_out.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
                let _ = u_out.shutdown(std::net::Shutdown::Write);
            });
            std::thread::spawn(move || {
                let _ = std::io::copy(&mut u_in, &mut c_out);
                let _ = c_out.shutdown(std::net::Shutdown::Write);
            });
        }
    });
    (url, sent)
}

#[test]
fn r_perf_6_open_session_starts_at_the_checkpoint_never_at_chunk_0() {
    let w = World::new();
    let _host = w.host_session(SESSION, "t", &[event(SESSION, 1, "before")], &[event(SESSION, 2, "after")]);
    let chunks = w.host.list_chunks(SESSION, None, 100).unwrap().chunks;
    assert_eq!(chunks.iter().map(|c| c.index).collect::<Vec<_>>(), [0, 1, 2]);

    // The phone through a proxy that records what it asks the registry.
    // The chunks' bytes come from where the listing points, not through
    // it, so what shows the reading's start is the chunk listing it asks
    // for: after chunk 0, never from the start.
    let (proxy, sent) = recording_proxy(w.registry.addr());
    let phone = Account::open(proxy, w.storage.clone()).unwrap();
    let opened = phone.open_session(SESSION.into(), None, None).unwrap();
    assert_eq!(runs(&opened.events().unwrap()), ["before", "after"]);
    let sent = String::from_utf8_lossy(&sent.lock().unwrap()).into_owned();
    let listings: Vec<&str> = sent.lines().filter(|l| l.starts_with("GET ") && l.contains("/chunks?")).collect();
    assert!(!listings.is_empty(), "{sent}");
    assert!(listings[0].contains("after=0"), "the first listing starts after chunk 0, at the checkpoint (chunk 1): {listings:#?}");
    assert!(listings.iter().all(|l| l.contains("after=")), "no listing from the start of the log: {listings:#?}");
}

/// A session of event chunks only, one event each, run `c1`, `c2`…: chunk
/// index `n - 1` holds `c{n}`.
fn chunked(w: &World, n: u32) -> Hosted {
    let (key, wrapped, index) = w.record(SESSION, "t", (w.host_device.id(), &w.host_signing));
    let keys = e2e::SessionKeys::from(key);
    let lease = w.host.acquire_lease(SESSION, &w.host_device.id().to_string(), 60, "development").unwrap();
    let writer = krowk_harness::sync::store::Writer::take_up(w.host.clone(), keys.clone(), SESSION, wrapped.clone(), index, lease.fence).unwrap();
    let mut h = Hosted { writer, token: lease.token, keys, wrapped };
    more(&mut h, 1..=n);
    h
}

fn more(h: &mut Hosted, chunks: std::ops::RangeInclusive<u32>) {
    for c in chunks {
        h.write(&[event(SESSION, c, &format!("c{c}"))]);
    }
}

/// The session's sealed index, as the registry serves it now.
fn index(w: &World, h: &Hosted) -> store::Index {
    store::open_index(&h.keys, SESSION, &w.host.show_sync_session(SESSION).unwrap().sealed_index).unwrap()
}

fn run_names(events: &[serde_json::Value]) -> Vec<String> {
    events.iter().map(|e| e["run"].as_str().unwrap().to_string()).collect()
}

/// Ticket 126: a device that kept where its reading stopped reads only the
/// chunks after it on its next attach, not the log from the checkpoint
/// (R-PERF-6, R-IOS-1: catch up from what the phone saw).
#[test]
fn r_perf_6_resume_from_a_place_reads_only_the_chunks_after_it() {
    let w = World::new();
    let mut h = chunked(&w, 6);
    let a = store::attach(&w.host, &h.keys, SESSION, index(&w, &h), None).unwrap();
    assert_eq!(run_names(&a.events), ["c1", "c2", "c3", "c4", "c5", "c6"]);
    let place = a.place().expect("chunks were read");
    assert_eq!((place.next, place.head), (6, a.head.unwrap()));
    // Kept on disk as JSON, as the phone's cache keeps it.
    let place: ReadPlace = serde_json::from_str(&serde_json::to_string(&place).unwrap()).unwrap();

    more(&mut h, 7..=10);
    let r = store::resume(&w.host, &h.keys, SESSION, index(&w, &h), place, a.head).unwrap();
    assert_eq!(run_names(&r.events), ["c7", "c8", "c9", "c10"], "only what came after the place");
    assert_eq!(r.head, h.writer.head());
    assert_eq!(r.place().unwrap().next, 10);

    // Nothing written since: nothing read.
    let again = store::resume(&w.host, &h.keys, SESSION, index(&w, &h), r.place().unwrap(), None).unwrap();
    assert!(again.events.is_empty());
    assert_eq!(again.head, r.head);

    // An index behind the place, as between a holder's chunk and its index
    // write, with the log served on past the place: only behind, read.
    let behind = store::Index { head: Some(a.heads[3]), ..index(&w, &h) };
    let r = store::resume(&w.host, &h.keys, SESSION, behind, place, None).unwrap();
    assert_eq!(run_names(&r.events), ["c7", "c8", "c9", "c10"]);
}

#[test]
fn r_ios_1_a_read_place_round_trips_through_serde() {
    let place = ReadPlace { next: 7, previous: [3; 32], fence: 2, epoch: 1, head: store::Head { index: 6, digest: [3; 32] } };
    let json = serde_json::to_value(place).unwrap();
    assert_eq!(json["previous"], serde_json::json!(e2e::hex(&[3; 32])));
    assert_eq!(serde_json::from_value::<ReadPlace>(json).unwrap(), place);
    assert!(place.check().is_ok());
    assert!(ReadPlace { next: 8, ..place }.check().is_err(), "the next chunk is the one after the head");
    assert!(ReadPlace { previous: [4; 32], ..place }.check().is_err(), "the next chunk binds the head's digest");
}

/// An index older than the place — the registry rolled back past what this
/// device read — and a chunk after the place that does not follow it are
/// refused for good, as a rollback.
#[test]
fn r_perf_6_resume_refuses_an_index_older_than_its_place_and_a_fork() {
    let w = World::new();
    let h = chunked(&w, 6);
    let a = store::attach(&w.host, &h.keys, SESSION, index(&w, &h), None).unwrap();
    let place = a.place().unwrap();

    let old = store::Index { head: Some(a.heads[3]), ..index(&w, &h) };
    let e = store::resume(&w.host, &h.keys, SESSION, old, place, None).unwrap_err();
    assert!(e.permanent && e.rolled_back, "{e:?}");
    let none = store::Index { head: None, ..index(&w, &h) };
    assert!(store::resume(&w.host, &h.keys, SESSION, none, place, None).unwrap_err().rolled_back);
    // `known` still applies: another chunk at the head it names.
    let other = store::Head { index: place.head.index, digest: [7; 32] };
    assert!(store::resume(&w.host, &h.keys, SESSION, index(&w, &h), place, Some(other)).unwrap_err().rolled_back);

    // Chunk 7 sealed onto another chunk 6 than the one this device read.
    let mut sealer = e2e::ChunkSealer::new(&h.keys, krowk_client::sync::uuid(SESSION), place.next, [1; 32], place.fence);
    let forked = sealer.seal(&serde_json::to_vec(&json!({"type": "events", "events": [event(SESSION, 7, "forked")]})).unwrap(), false).unwrap();
    w.host.put_chunk(SESSION, place.next, &forked, &h.token).unwrap();
    let e = store::resume(&w.host, &h.keys, SESSION, index(&w, &h), place, None).unwrap_err();
    assert!(e.permanent && e.rolled_back, "{e:?}");
    // And the phone calls it a rollback.
    let record = ReadPlaceRecord { next: place.next, previous: place.previous.to_vec(), fence: place.fence, epoch: place.epoch, head: place.head.into() };
    assert_eq!(refused(w.phone.resume_session(SESSION.into(), record, None, None)), "session_rolled_back");
    // Read from chunk 0, the forked chunk does not follow either.
    assert!(store::attach(&w.host, &h.keys, SESSION, index(&w, &h), None).is_err());
}

/// The phone keeps `place` and opens the session from it next time: only
/// the new events, the same as a `catch_up` on the session it kept open.
#[test]
fn r_ios_1_resume_session_reads_only_the_events_after_the_place() {
    let w = World::new();
    let mut host = w.host_session(SESSION, "fix the build", &[event(SESSION, 1, "before")], &[event(SESSION, 2, "after")]);
    let opened = w.phone.open_session(SESSION.into(), None, None).unwrap();
    let place = opened.place().expect("a place");
    assert_eq!(place.head, opened.head().unwrap());

    host.write(&[event(SESSION, 3, "later"), event(SESSION, 4, "latest")]);
    host.write(&[event(SESSION, 5, "last")]);
    let resumed = w.phone.resume_session(SESSION.into(), place.clone(), opened.head(), None).unwrap();
    assert_eq!(resumed.title(), "fix the build");
    assert_eq!(runs(&resumed.events().unwrap()), ["later", "latest", "last"]);
    assert_eq!(runs(&opened.catch_up().unwrap()), runs(&resumed.events().unwrap()), "what a catch-up reads");
    assert_eq!(resumed.head(), opened.head());
    assert_eq!(resumed.place(), opened.place());
    assert!(resumed.catch_up().unwrap().is_empty());

    // A place past the log served, and one no reading stopped at.
    let ahead = ReadPlaceRecord { next: 9, previous: vec![7; 32], head: HeadRecord { index: 8, digest: vec![7; 32] }, ..place.clone() };
    assert_eq!(refused(w.phone.resume_session(SESSION.into(), ahead, None, None)), "session_rolled_back");
    let short = ReadPlaceRecord { previous: vec![7; 31], ..place.clone() };
    assert!(matches!(w.phone.resume_session(SESSION.into(), short, None, None), Err(KrowkError::InvalidInput { .. })));
    let skewed = ReadPlaceRecord { next: place.next + 1, ..place.clone() };
    assert!(matches!(w.phone.resume_session(SESSION.into(), skewed, None, None), Err(KrowkError::InvalidInput { .. })));
}

/// A chunk after the place that this build cannot open yet, a newer
/// format's, is refused for good but is no rollback: the app may need
/// updating.
#[test]
fn r_ios_1_a_newer_chunk_after_the_place_is_not_a_rollback() {
    let w = World::new();
    let h = chunked(&w, 3);
    let a = store::attach(&w.host, &h.keys, SESSION, index(&w, &h), None).unwrap();
    let place = a.place().unwrap();
    let mut sealer = e2e::ChunkSealer::new(&h.keys, krowk_client::sync::uuid(SESSION), place.next, place.previous, place.fence);
    let mut newer = sealer.seal(b"{}", false).unwrap();
    newer[0] = e2e::CHUNK_V2 + 1;
    w.host.put_chunk(SESSION, place.next, &newer, &h.token).unwrap();
    let e = store::resume(&w.host, &h.keys, SESSION, index(&w, &h), place, None).unwrap_err();
    assert!(e.permanent && !e.rolled_back && e.detail.contains("upgrade krowk"), "{e:?}");
}
