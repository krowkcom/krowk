//! The JSON a synced session's host and viewers send each other, and keep
//! at rest, pinned byte for byte (R-CLIENT-1, R-PROTO-2; engineering/
//! harness.md → Sync): every client reads and writes these shapes through
//! `krowk_client::sync`, so a change to one shows here first. Each case is
//! read from the JSON and written back to the same bytes.

use krowk_client::protocol::{Command, LiveEvent, StreamLine};
use krowk_client::sync::store::{Checkpoint, Chunk, Head, HostedOn, Index, Mark};
use krowk_client::sync::{uuid, Answer, Batch, Candidate, Handed, Remote, Tailnet, ViewerFrame, Welcome};
use serde::de::DeserializeOwned;
use serde::Serialize;

const SESSION: &str = "01a0f731-f1c0-7000-92d7-64b247e51781";
const DIGEST: &str = "0101010101010101010101010101010101010101010101010101010101010101";

/// `json` read as a `T`, and written back as the same bytes.
fn round_trip<T: Serialize + DeserializeOwned>(json: &str) -> T {
    let t: T = serde_json::from_str(json).unwrap_or_else(|e| panic!("{json}: {e}"));
    assert_eq!(serde_json::to_string(&t).unwrap(), json);
    t
}

/// A viewer's three frames: a command under its id, a catch-up after the
/// last event it holds (none: everything), a handoff to its own device.
#[test]
fn r_client_1_each_viewer_frame_is_the_json_the_host_reads() {
    let interrupt = Command::Interrupt { session_id: SESSION.into() };
    let command = format!(r#"{{"type":"command","id":"c1","command":{{"type":"interrupt","sessionId":"{SESSION}"}}}}"#);
    let ViewerFrame::Command(Remote { id, command: c }) = round_trip(&command) else { panic!("not a command") };
    assert_eq!((id.as_str(), c), ("c1", interrupt.clone()));
    assert_eq!(serde_json::to_string(&ViewerFrame::Command(Remote { id: "c1".into(), command: interrupt })).unwrap(), command);

    assert!(matches!(round_trip(r#"{"type":"catchUp","after":null}"#), ViewerFrame::CatchUp { after: None }));
    assert!(matches!(round_trip(r#"{"type":"catchUp","after":"e9"}"#), ViewerFrame::CatchUp { after: Some(a) } if a == "e9"));
    assert!(matches!(round_trip(r#"{"type":"handoff","id":"h1","device":"d2"}"#), ViewerFrame::Handoff { id, device } if id == "h1" && device == "d2"));
}

/// The host's three answers to one viewer: an ack, a page of logged events,
/// and a handoff done, naming the device that handed it over.
#[test]
fn r_client_1_each_answer_is_the_json_the_viewer_reads() {
    assert!(matches!(round_trip(r#"{"type":"ack","id":"c1","error":null}"#), Answer::Ack { id, error: None } if id == "c1"));
    assert!(matches!(round_trip(r#"{"type":"ack","id":"c1","error":"refused"}"#), Answer::Ack { error: Some(e), .. } if e == "refused"));

    let event = format!(r#"{{"id":"e1","sessionId":"{SESSION}","timeMs":7,"type":"run.opened","turnId":"t1","run":"r1"}}"#);
    let Answer::CatchUp { events, more } = round_trip(&format!(r#"{{"type":"catchUp","events":[{event}],"more":true}}"#)) else { panic!("not a catch-up") };
    assert!(more);
    assert_eq!((events.len(), events[0].id.as_str()), (1, "e1"));

    let handoff = r#"{"type":"handoff","id":"h1","handed":{"transport":"tr","key":"k","token":"tok","fence":3,"ticket":"ti","fromDevice":"d1"},"error":null}"#;
    let Answer::Handoff { id, handed: Some(h), error: None } = round_trip(handoff) else { panic!("not a handoff done") };
    assert_eq!(id, "h1");
    assert_eq!(h, Handed { transport: "tr".into(), key: "k".into(), token: "tok".into(), fence: 3, ticket: "ti".into(), from_device: "d1".into() });
    assert!(matches!(round_trip(r#"{"type":"handoff","id":"h1","handed":null,"error":"no"}"#), Answer::Handoff { handed: None, error: Some(_), .. }));
}

/// A welcome from a host older than direct paths has no `candidates`, and
/// reads as none; one with them round-trips, the LAN told from Tailscale.
#[test]
fn r_client_1_a_welcome_without_candidates_reads() {
    let w: Welcome = serde_json::from_str(r#"{"head":null,"approvals":[]}"#).unwrap();
    assert!(w.head.is_none() && w.approvals.is_empty() && w.candidates.is_empty());

    let json = format!(r#"{{"head":{{"index":4,"digest":"{DIGEST}"}},"approvals":[],"candidates":[{{"url":"ws://100.64.0.1:51915","via":"tailscale"}},{{"url":"ws://192.168.1.2:51915","via":"lan"}}]}}"#);
    let w: Welcome = round_trip(&json);
    assert_eq!(w.head, Some(Head { index: 4, digest: [1; 32] }));
    assert_eq!(w.candidates[0], Candidate { url: "ws://100.64.0.1:51915".into(), via: "tailscale".into() });
    assert_eq!((w.candidates[0].path(), w.candidates[1].path()), ("direct over Tailscale", "direct over LAN"));
    assert!(Candidate { url: String::new(), via: "mdns".into() }.on_lan());
}

/// A batch keeps every line it can read, in order: one of a type a newer
/// host sends, and one missing its fields, are dropped, and the rest
/// still apply.
#[test]
fn r_proto_2_a_batch_drops_the_lines_it_cannot_read_and_keeps_the_rest_in_order() {
    let log = format!(r#"{{"id":"e1","sessionId":"{SESSION}","timeMs":7,"type":"run.opened","turnId":"t1","run":"r1"}}"#);
    let notice = format!(r#"{{"type":"notice","sessionId":"{SESSION}","turnId":"t1","text":"hi"}}"#);
    let json = format!(r#"{{"lines":[{log},{{"type":"from.the.future","x":1}},{{"type":"notice","sessionId":"{SESSION}"}},{notice}],"head":{{"index":0,"digest":"{DIGEST}"}}}}"#);
    let b: Batch = serde_json::from_str(&json).unwrap();
    assert_eq!(b.lines.len(), 2, "{:?}", b.lines);
    assert!(matches!(&b.lines[0], StreamLine::Log(e) if e.id == "e1"));
    assert!(matches!(&b.lines[1], StreamLine::Live(LiveEvent::Notice { text, .. }) if text == "hi"));
    assert_eq!(b.head, Some(Head { index: 0, digest: [1; 32] }));
    assert_eq!(serde_json::to_string(&b).unwrap(), format!(r#"{{"lines":[{log},{notice}],"head":{{"index":0,"digest":"{DIGEST}"}}}}"#));
}

/// An index written before hosts were kept has no `host` and reads; one
/// naming its host and tailnet node round-trips, and one without a tailnet
/// writes none.
#[test]
fn r_client_1_an_index_reads_without_a_host_and_round_trips_with_one() {
    let old = format!(r#"{{"title":"t","cwd":"/w","updatedMs":5,"head":{{"index":2,"digest":"{DIGEST}"}},"checkpoint":{{"index":1,"previous":"{DIGEST}","fence":3}}}}"#);
    let i: Index = round_trip(&old);
    assert_eq!(i.host, None);
    assert_eq!(i.checkpoint, Some(Mark { index: 1, previous: [1; 32], fence: 3 }));

    let hosted = r#"{"title":"t","cwd":"/w","updatedMs":5,"head":null,"checkpoint":null,"host":{"device":"d1","tailnet":{"name":"a","dnsName":"a.tail1.ts.net","ips":["100.64.0.1","fd7a:115c:a1e0::1"]}}}"#;
    let i: Index = round_trip(hosted);
    let tailnet = Tailnet { name: "a".into(), dns_name: "a.tail1.ts.net".into(), ips: vec!["100.64.0.1".parse().unwrap(), "fd7a:115c:a1e0::1".parse().unwrap()] };
    assert_eq!(i.host, Some(HostedOn { device: "d1".into(), tailnet: Some(tailnet) }));

    round_trip::<Index>(r#"{"title":"t","cwd":"/w","updatedMs":5,"head":null,"checkpoint":null,"host":{"device":"d1"}}"#);
    let t: Tailnet = serde_json::from_str(r#"{"name":"a","ips":[]}"#).unwrap();
    assert_eq!(t.dns_name, "", "a node without MagicDNS");
}

/// A chunk of the log, opened: the events since the chunk before, or a
/// checkpoint naming the head it follows.
#[test]
fn r_client_1_each_chunk_is_the_json_a_reader_opens() {
    let Chunk::Events { events } = round_trip(r#"{"type":"events","events":[{"id":"e1"}]}"#) else { panic!("not events") };
    assert_eq!(events[0]["id"], "e1");
    let json = format!(r#"{{"type":"checkpoint","events":[],"logOffset":9,"worktree":"abc","chunk":{{"index":8,"digest":"{DIGEST}"}}}}"#);
    let Chunk::Checkpoint(cp) = round_trip(&json) else { panic!("not a checkpoint") };
    assert_eq!(cp, Checkpoint { events: Vec::new(), log_offset: 9, worktree: Some("abc".into()), chunk: Some(Head { index: 8, digest: [1; 32] }) });
    round_trip::<Chunk>(r#"{"type":"checkpoint","events":[],"logOffset":0,"worktree":null,"chunk":null}"#);
}

/// A head's digest is 32 bytes in hex, and nothing else reads as one.
#[test]
fn r_client_1_a_head_whose_digest_is_not_32_hex_bytes_is_refused() {
    for bad in ["01", &DIGEST[..62], &format!("{DIGEST}01"), &DIGEST.replace('1', "g"), ""] {
        let e = serde_json::from_str::<Head>(&format!(r#"{{"index":0,"digest":"{bad}"}}"#)).unwrap_err();
        assert!(e.to_string().contains("32 hex bytes"), "{bad}: {e}");
    }
    assert!(serde_json::from_str::<Head>(r#"{"index":0,"digest":7}"#).is_err());
}

/// A session id's sixteen bytes, as the relay and the sealed chunks bind
/// them; anything not a UUID is all zeros.
#[test]
fn r_client_1_uuid_gives_a_session_ids_bytes_and_zeros_for_anything_else() {
    assert_eq!(uuid(SESSION), [0x01, 0xa0, 0xf7, 0x31, 0xf1, 0xc0, 0x70, 0x00, 0x92, 0xd7, 0x64, 0xb2, 0x47, 0xe5, 0x17, 0x81]);
    assert_eq!(uuid(&SESSION.replace('-', "")), uuid(SESSION));
    for bad in ["x", "", "01a0f731-f1c0-7000-92d7-64b247e5178g", "01a0f731-f1c0-7000-92d7-64b247e517"] {
        assert_eq!(uuid(bad), [0; 16], "{bad}");
    }
}
