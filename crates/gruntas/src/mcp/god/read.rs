//! gruntas_read (GA04, R-GOD-4): a `read` host command to the machine that
//! holds the session, its page back through the answer record, with the
//! cursor for the page before.

use super::super::{arguments, Outcome};
use gruntas_api::fail;
use gruntas_client::host_command::{HostAnswer, HostCommand};
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Deserialize, Default)]
#[serde(default)]
struct Args {
    session: String,
    page: Option<String>,
}

pub(super) fn read(args: &Value) -> Outcome {
    let a: Args = arguments(args, "gruntas_read takes a session, and optionally the page cursor a page before gave")?;
    let session = a.session.trim();
    if session.is_empty() {
        return Err(fail("bad_arguments", "gruntas_read needs a session: its id, as gruntas_status lists it"));
    }
    let (machine, answer) = crate::cli::ask_holder(session, |session| HostCommand::Read { session, page: a.page })?;
    told(&machine, answer)
}

/// The machine `machine`'s answer to a `read`, as the tool says it.
fn told(machine: &str, answer: HostAnswer) -> Outcome {
    let p = match answer {
        HostAnswer::Page(p) => p,
        HostAnswer::Refused { ref code, .. } => return Err(fail(code, answer.refusal_line(machine).unwrap_or_default())),
        _ => return Err(fail("bad_answer", format!("{machine} answered the read as another command"))),
    };
    let more = match &p.next {
        Some(next) => format!("\n\nOlder: call gruntas_read with session {} and page {next}.", p.session),
        None => "\n\nThat is the oldest this machine reads.".into(),
    };
    Ok((format!("{}{more}", p.text), json!({ "session": p.session, "machine": machine, "text": p.text, "next": p.next })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gruntas_api::sync::ListPost;
    use gruntas_api::Client;
    use gruntas_client::device_chain::{Chain, Change, Kind, SignedEntry, Subject};
    use gruntas_client::e2e::{hex, DeviceId, DeviceKey, DeviceSigner, SigningKey};
    use gruntas_client::host_command::{self, TranscriptPage};
    use gruntas_client::user_key::UserKeys;
    use gruntas_harness::sync::announce::Announcer;
    use gruntas_harness::sync::god;
    use std::sync::Arc;
    use std::time::Duration;

    const SESSION: &str = "01a0ec7b-0000-7000-8000-0000000000a1";

    fn subject(name: &str, device: &DeviceKey, signing: &SigningKey) -> Subject {
        Subject { kind: Kind::Device, name: name.into(), os: "linux".into(), device: device.public(), signing: signing.public() }
    }

    fn client(url: &str, key: &str, device: &DeviceKey, signing: &SigningKey) -> Client {
        let signing = SigningKey::from_secret(&*signing.secret_bytes()).unwrap();
        Client::new(url, key).signed_by(DeviceSigner::new(device.id(), signing).shared())
    }

    /// The stand-in, and a host and a phone on the person's list, each's
    /// announcer.
    fn person() -> (gruntas_devregistry::Running, Announcer, Announcer) {
        let registry = gruntas_devregistry::start(std::net::TcpListener::bind("127.0.0.1:0").unwrap(), gruntas_devregistry::Config::default()).unwrap();
        let url = format!("{}/v1", registry.url());
        let (hd, hs, pd, ps) = (DeviceKey::generate(), SigningKey::generate(), DeviceKey::generate(), SigningKey::generate());
        let (host, phone) = (client(&url, "krowk_sk_read#host", &hd, &hs), client(&url, "krowk_sk_read#phone", &pd, &ps));
        let entries = |b: &[SignedEntry]| b.iter().map(|e| (hex(&e.bytes), hex(&e.signatures_bytes()))).collect();
        let (chain, start) = Chain::start(subject("host", &hd, &hs), &hs, None, 1_790_000_000).unwrap();
        host.init_device_list(&ListPost { entries: entries(&start.entries), links: Vec::new(), wraps: vec![(hd.id().to_string(), hex(&start.newest.wrap_to(&hd.public()).unwrap()))], start_over: false }).unwrap();
        let (chain, added) = chain.batch(&start.newest, vec![Change::Add(subject("phone", &pd, &ps))], hd.id(), &hs, 1_790_000_001).unwrap();
        host.append_device_list(&ListPost { entries: entries(&added.entries), links: Vec::new(), wraps: vec![(pd.id().to_string(), hex(&added.newest.wrap_to(&pd.public()).unwrap()))], start_over: false }).unwrap();
        phone.claim_key_device().unwrap();
        let keys = || UserKeys::new(added.newest.clone(), []).unwrap();
        let (h, p) = (Announcer { api: Arc::new(host), keys: keys(), chain: chain.clone(), device: hd.id() }, Announcer { api: Arc::new(phone), keys: keys(), chain, device: pd.id() });
        (registry, h, p)
    }

    /// The host's side: the first command delivered answered with a page
    /// too big for an answer, through the answer record.
    fn serve(host: Announcer) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            for _ in 0..200 {
                if let Some(c) = host.api.put_presence(b"announcement", 30).unwrap().commands.into_iter().next() {
                    let from = DeviceId::parse(&c.from).unwrap();
                    let Ok(HostCommand::Read { session, page: None }) = host_command::open_command(&c.sealed, &c.id, &from, &host.device, &host.keys, &host.chain).unwrap() else { panic!("not a first read") };
                    let answer = HostAnswer::Page(TranscriptPage { session, text: "<transcript from \"session\">\nassistant: done\n".repeat(200), next: Some("before:a7.0".into()) });
                    let (sealed, record) = host_command::seal_answer_or_record(&answer, &c.id, &host.device, &from, &host.keys, host.chain.generation()).unwrap();
                    host.api.put_answer_record(&c.id, &record.expect("too big for an answer")).unwrap();
                    host.api.answer_host_command(&c.id, &sealed).unwrap();
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        })
    }

    #[test]
    fn ga04_a_page_comes_back_through_the_answer_record() {
        let (_registry, host, phone) = person();
        let to = host.device;
        let served = serve(host);
        let answer = god::ask(&phone, &to, "host", &HostCommand::Read { session: SESSION.into(), page: None }, Duration::from_secs(10)).unwrap();
        served.join().unwrap();
        let (text, data) = told("host", answer).unwrap();
        assert!(data["text"].as_str().unwrap().len() > host_command::MAX_ANSWER);
        assert_eq!((data["session"].as_str(), data["machine"].as_str(), data["next"].as_str()), (Some(SESSION), Some("host"), Some("before:a7.0")));
        assert!(text.ends_with("page before:a7.0."), "{text}");
        let refused = told("host", HostAnswer::refused("cant_read", "can't read this session yet: it is a Codex session")).unwrap_err();
        assert_eq!((refused.code().as_str(), refused.fix().as_str()), ("cant_read", "host: can't read this session yet: it is a Codex session"));
    }
}
