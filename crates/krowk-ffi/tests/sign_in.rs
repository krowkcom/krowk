//! R-CLIENT-1, R-FFI-3: the phone signs in as the terminal does — it opens
//! a browser login named after itself, the browser only approves, and the
//! phone collects the key — and keeps the key in the app's storage, not a
//! file. Against the stand-in registry, approved and denied by the raw
//! calls the approval page's buttons make.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use krowk_ffi::{KeyStorage, KrowkError, SignIn};

#[derive(Default)]
struct Memory(Mutex<HashMap<String, Vec<u8>>>);

impl KeyStorage for Memory {
    fn read(&self, slot: String) -> Result<Option<Vec<u8>>, KrowkError> {
        Ok(self.0.lock().unwrap().get(&slot).cloned())
    }
    fn write(&self, slot: String, bytes: Vec<u8>) -> Result<(), KrowkError> {
        self.0.lock().unwrap().insert(slot, bytes);
        Ok(())
    }
    fn remove(&self, slot: String) -> Result<(), KrowkError> {
        self.0.lock().unwrap().remove(&slot);
        Ok(())
    }
}

fn registry() -> krowk_devregistry::Running {
    krowk_devregistry::start(TcpListener::bind("127.0.0.1:0").unwrap(), Default::default()).unwrap()
}

fn api(r: &krowk_devregistry::Running) -> String {
    format!("{}/v1", r.url())
}

/// One request, as the approval page's browser makes it; the whole response.
fn call(addr: SocketAddr, method: &str, path: &str) -> String {
    let mut s = TcpStream::connect(addr).unwrap();
    write!(s, "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    out
}

fn refused(r: Result<krowk_ffi::SignedIn, KrowkError>) -> String {
    match r {
        Err(KrowkError::Refused { code, .. }) => code,
        other => panic!("not a refusal: {other:?}"),
    }
}

#[test]
fn r_client_1_r_ffi_3_an_approved_sign_in_keeps_its_key_in_the_apps_storage() {
    let registry = registry();
    let s = SignIn::start(api(&registry), "Test iPhone".into()).unwrap();
    assert_eq!(s.code().len(), 9, "{}", s.code());
    assert!(s.verification_url().starts_with(&registry.url()), "{}", s.verification_url());
    assert!(s.verification_url().contains(&s.code()), "the page names the code: {}", s.verification_url());
    assert!(!s.expires_at().is_empty());

    // The approval page asks about the phone by name.
    let page = call(registry.addr(), "GET", &format!("/_approve/cli/authorizations/new?code={}", s.code()));
    assert!(page.contains("A terminal on Test iPhone asked to sign in"), "{page}");
    let approved = call(registry.addr(), "POST", &format!("/_approve/cli/authorizations/{}/approval", s.code()));
    assert!(approved.starts_with("HTTP/1.1 200"), "{approved}");

    let storage = Arc::new(Memory::default());
    let who = s.wait(storage.clone()).unwrap();
    assert!(who.key_id.starts_with("key_"), "{who:?}");
    assert!(!who.workspace.is_empty(), "{who:?}");

    let held = storage.read("credentials.json".into()).unwrap().expect("the key is stored");
    let creds: serde_json::Value = serde_json::from_slice(&held).unwrap();
    assert_eq!(creds["version"], 1);
    assert!(creds["token"].as_str().unwrap().starts_with("krowk_sk_"), "{creds}");
    assert_eq!(creds["key_id"], who.key_id.as_str());
    assert_eq!(creds["workspace"], who.workspace.as_str());
    assert_eq!(storage.0.lock().unwrap().len(), 1, "only the key is written; the device keys wait for Account::open");

    // The key was handed over once: a second wait finds it collected.
    assert_eq!(refused(s.wait(Arc::new(Memory::default()))), "spent");

    // The account opens from it, signing as the device it makes.
    let account = krowk_ffi::Account::open(api(&registry), storage).unwrap();
    assert_eq!(account.workspace(), who.workspace);
}

#[test]
fn r_client_1_a_denied_sign_in_is_refused_and_stores_nothing() {
    let registry = registry();
    let s = SignIn::start(api(&registry), "Test iPhone".into()).unwrap();
    let denied = call(registry.addr(), "POST", &format!("/_approve/cli/authorizations/{}/denial", s.code()));
    assert!(denied.starts_with("HTTP/1.1 200"), "{denied}");
    let storage = Arc::new(Memory::default());
    assert_eq!(refused(s.wait(storage.clone())), "authorization_denied");
    assert!(storage.0.lock().unwrap().is_empty());
}

#[test]
fn r_client_1_cancel_from_another_thread_ends_the_wait_within_a_second() {
    let registry = registry();
    let s = SignIn::start(api(&registry), "Test iPhone".into()).unwrap();
    let waiting = {
        let s = Arc::clone(&s);
        std::thread::spawn(move || {
            let r = s.wait(Arc::new(Memory::default()));
            (r, Instant::now())
        })
    };
    // Most likely in the poll's first sleep by now (the stand-in's pace is
    // a second); a cancel before `wait` began would end it all the same.
    std::thread::sleep(Duration::from_millis(300));
    let cancelled = Instant::now();
    s.cancel();
    let (r, ended) = waiting.join().unwrap();
    assert_eq!(refused(r), "cancelled");
    assert!(ended.duration_since(cancelled) < Duration::from_secs(1), "{:?}", ended.duration_since(cancelled));
    // A cancelled sign-in stays cancelled.
    assert_eq!(refused(s.wait(Arc::new(Memory::default()))), "cancelled");
}

#[test]
fn r_client_1_no_registry_is_a_network_error() {
    // A port nothing listens on.
    let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    match SignIn::start(format!("http://127.0.0.1:{port}/v1"), "Test iPhone".into()) {
        Err(KrowkError::Network { .. }) => {}
        Err(e) => panic!("{e:?}"),
        Ok(_) => panic!("started"),
    }
}

/// A registry that answers the one sign-in request with `body`.
fn answering(body: &'static str) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") && s.read(&mut byte).unwrap() == 1 {
            head.push(byte[0]);
        }
        let _ = write!(
            s,
            "HTTP/1.1 201 Created\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
    });
    addr
}

#[test]
fn r_client_1_a_page_that_is_not_http_s_is_never_handed_out() {
    let addr = answering(
        r#"{"slug":"aut_x","state":"pending","code":"ABCD-EFGH","verification_url":"javascript:alert(1)","interval":1,"expires_at":"2099-01-01T00:00:00Z"}"#,
    );
    match SignIn::start(format!("http://{addr}/v1"), "Test iPhone".into()) {
        Err(KrowkError::Refused { code, .. }) => assert_eq!(code, "unsafe_url"),
        Err(e) => panic!("{e:?}"),
        Ok(_) => panic!("started"),
    }
}
