//! The SuperGrok login (R-PROV-4) against a stand-in authorization server:
//! the device code flow, the browser flow with PKCE and a loopback
//! redirect, the credentials file's permissions, and refresh — including two
//! krowk processes refreshing one rotating refresh token.

#[path = "common/mock.rs"]
mod mock;
#[path = "common/providers.rs"]
mod providers;

use krowk_harness::oauth::{self, DevicePrompt, Login, Store, Tokens};
use std::path::PathBuf;

fn dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("krowk-oauth-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    // krowk's home, which exists before anything is kept in it.
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn login(issuer: &str) -> Login {
    Login { issuer: issuer.into(), client_id: None, scope: "openid offline_access".into() }
}

#[cfg(unix)]
fn mode(p: &std::path::Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).unwrap().permissions().mode() & 0o777
}

#[tokio::test]
async fn r_prov_4_a_device_login_is_stored_in_a_credentials_file_created_0600() {
    let auth = providers::auth_server(3600);
    let http = krowk_harness::http::client().unwrap();
    let mut shown: Vec<DevicePrompt> = Vec::new();
    let stored = oauth::login_device(&http, &login(&auth.mock.url), &mut |p| shown.push(p.clone())).await.unwrap();
    assert_eq!(shown.len(), 1);
    assert_eq!(shown[0].user_code, "KRWK-2026");
    assert!(shown[0].verification_uri_complete.as_deref().unwrap().ends_with("/device?user_code=KRWK-2026"));
    assert_eq!((stored.access_token.as_str(), stored.refresh_token.as_deref(), stored.client_id.as_str()), ("xai-at-1", Some("xai-rt-1"), "krowk-registered"));
    assert!(stored.expires_at_ms.unwrap() > krowk_store::now_ms() + 3_000_000);
    assert!(!format!("{stored:?}").contains("xai-at-1"), "a token never prints");
    {
        let seen = auth.mock.seen.lock().unwrap();
        let polls = seen.iter().filter(|s| s.path == "/oauth2/token").count();
        assert_eq!(polls, 2, "one authorization_pending, then the token");
    }

    let d = dir("device");
    let store = Store::new(d.join("credentials.json"));
    store.save("supergrok", &stored).unwrap();
    #[cfg(unix)]
    // The directory is krowk's home, made 0700 by `krowk_api::home`.
    assert_eq!(mode(&store.path), 0o600, "the credentials file is created 0600");
    // A second login lands beside the first; neither is lost.
    store.save("grok:team", &stored).unwrap();
    assert_eq!(store.names().unwrap(), ["grok:team", "supergrok"]);
    #[cfg(unix)]
    assert_eq!(mode(&store.path), 0o600, "and stays 0600 when rewritten");
    assert!(store.remove("grok:team").unwrap() && !store.remove("grok:team").unwrap());
    // A file krowk cannot read is never written over.
    std::fs::write(&store.path, "not json").unwrap();
    assert!(store.save("supergrok", &stored).unwrap_err().message.contains("refusing to write over it"));
    assert_eq!(std::fs::read_to_string(&store.path).unwrap(), "not json");
    let _ = std::fs::remove_dir_all(&d);
}

#[tokio::test]
async fn r_prov_4_a_browser_login_uses_pkce_and_a_loopback_redirect() {
    let auth = providers::auth_server(3600);
    auth.state.lock().unwrap().registration = false;
    let http = krowk_harness::http::client().unwrap();
    let page = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let seen_page = page.clone();
    // The browser's thread says when it has the page: the login can end
    // before that thread stores it.
    let (browsed, has_page) = std::sync::mpsc::channel();
    let mut opened = String::new();
    let l = Login { client_id: Some("krowk-test".into()), ..login(&auth.mock.url) };
    let stored = oauth::login_pkce(&http, &l, &mut |url: &str| {
        opened = url.to_string();
        let url = url.to_string();
        let (page, browsed) = (seen_page.clone(), browsed.clone());
        std::thread::spawn(move || {
            // A probe with the wrong state first: ignored, the login waits on.
            let port = url.split("redirect_uri=http%3A%2F%2F127.0.0.1%3A").nth(1).unwrap().split("%2F").next().unwrap().to_string();
            let mut probe = std::net::TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();
            use std::io::{Read, Write};
            write!(probe, "GET /callback?code=forged&state=wrong HTTP/1.1\r\nhost: x\r\n\r\n").unwrap();
            let mut answer = String::new();
            let _ = probe.read_to_string(&mut answer);
            assert!(answer.starts_with("HTTP/1.1 400"), "{answer}");
            *page.lock().unwrap() = providers::browse(&url);
            let _ = browsed.send(());
        });
    })
    .await
    .unwrap();
    assert_eq!(stored.access_token, "xai-at-1");
    has_page.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
    assert!(page.lock().unwrap().contains("krowk is signed in"));
    for want in ["response_type=code", "client_id=krowk-test", "code_challenge_method=S256", "code_challenge=", "state=", "scope=openid+offline_access"] {
        assert!(opened.contains(want), "{want} in {opened}");
    }
    // The verifier went to the token endpoint, never in the URL.
    {
        let seen = auth.mock.seen.lock().unwrap();
        let exchange = seen.iter().find(|s| s.path == "/oauth2/token").unwrap();
        assert!(exchange.raw.contains("code_verifier=") && exchange.raw.contains("grant_type=authorization_code"));
        assert!(!opened.contains("code_verifier"));
    }
    // No client id and no registration: a login says what to pass.
    let e = oauth::login_device(&http, &login(&auth.mock.url), &mut |_| {}).await.unwrap_err();
    assert!(e.message.contains("--client-id"), "{}", e.message);
    // Plain http anywhere but loopback is refused before anything is sent.
    let e = oauth::login_device(&http, &login("http://auth.example"), &mut |_| {}).await.unwrap_err();
    assert!(e.message.contains("https"), "{}", e.message);
}

#[tokio::test]
async fn r_prov_4_an_expired_token_is_refreshed_once_across_processes_and_rotation_is_kept() {
    // Tokens that are already inside the expiry margin when issued.
    let auth = providers::auth_server(0);
    let http = krowk_harness::http::client().unwrap();
    let first = oauth::login_device(&http, &login(&auth.mock.url), &mut |_| {}).await.unwrap();
    let d = dir("refresh");
    let store = Store::new(d.join("credentials.json"));
    store.save("supergrok", &first).unwrap();
    // Two sessions (two krowk processes) holding the same login.
    let a = Tokens::open(store.clone(), "supergrok").unwrap();
    let b = Tokens::open(store.clone(), "supergrok").unwrap();
    assert_eq!(a.bearer(&http, false).await.unwrap(), "xai-at-2");
    assert_eq!(store.load("supergrok").unwrap().unwrap().refresh_token.as_deref(), Some("xai-rt-2"), "the rotated refresh token is saved");
    // B's copy is stale, and its refresh token already spent: it reads the
    // file under the lock and refreshes with the live one.
    assert_eq!(b.bearer(&http, false).await.unwrap(), "xai-at-3");
    assert_eq!(auth.state.lock().unwrap().refreshes, 2, "no refresh was made with a spent token");
    #[cfg(unix)]
    assert_eq!(mode(&store.path), 0o600);
    // A refresh the server refuses asks for a new login, and names no token.
    auth.state.lock().unwrap().refresh = "revoked".into();
    let e = a.bearer(&http, true).await.unwrap_err();
    assert_eq!(e.code, "not_authenticated");
    assert!(e.message.contains("krowk connect xai --method subscription") && !e.message.contains("xai-rt") && !e.message.contains("xai-at"), "{}", e.message);
    // An instance with no login says how to sign in.
    assert!(Tokens::open(store.clone(), "grok:team").unwrap_err().message.contains("krowk connect grok:team"));
    let _ = std::fs::remove_dir_all(&d);
}

/// Two krowk processes whose token expired at once: their refreshes contend
/// for the store's lock, the loser waits (on its runtime, never blocking it)
/// and then uses the winner's token — exactly one refresh, so neither spends
/// a refresh token the other already rotated away. A third stores an API
/// key in the same file meanwhile (R-CRED-1): its write waits for the lock
/// too, and neither the rotated token nor the key is lost. Each racer opens
/// the file and its lock on its own descriptor, as separate processes do.
#[test]
fn r_prov_4_r_cred_1_two_processes_refreshing_at_once_make_exactly_one_refresh_beside_a_key_being_stored() {
    let auth = providers::auth_server(3600);
    auth.state.lock().unwrap().refresh_delay_ms = 400;
    let d = dir("race");
    let store = Store::new(d.join("credentials.json"));
    let rt = || tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let http = krowk_harness::http::client().unwrap();
    let mut first = rt().block_on(oauth::login_device(&http, &login(&auth.mock.url), &mut |_| {})).unwrap();
    first.expires_at_ms = Some(0);
    store.save("supergrok", &first).unwrap();
    let start = std::sync::Arc::new(std::sync::Barrier::new(2));
    let racers: Vec<_> = (0..2)
        .map(|_| {
            let (store, start) = (store.clone(), start.clone());
            std::thread::spawn(move || {
                let tokens = Tokens::open(store, "supergrok").unwrap();
                let http = krowk_harness::http::client().unwrap();
                start.wait();
                rt().block_on(tokens.bearer(&http, false)).unwrap()
            })
        })
        .collect();
    let keyed = {
        let store = store.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(100));
            store.save_key("anthropic", &krowk_harness::keys::KeyRef::Literal("sk-stored".into())).unwrap();
        })
    };
    let got: Vec<String> = racers.into_iter().map(|t| t.join().unwrap()).collect();
    keyed.join().unwrap();
    assert_eq!(got, ["xai-at-2", "xai-at-2"], "the loser used the winner's token");
    assert_eq!(auth.state.lock().unwrap().refreshes, 1, "exactly one refresh");
    assert_eq!(store.load("supergrok").unwrap().unwrap().refresh_token.as_deref(), Some("xai-rt-2"));
    assert_eq!(store.keys().unwrap().get("anthropic"), Some(&krowk_harness::keys::KeyRef::Literal("sk-stored".into())), "the key stored meanwhile is kept");
    let _ = std::fs::remove_dir_all(&d);
}

#[tokio::test]
async fn r_prov_4_metadata_naming_another_issuer_is_refused() {
    let auth = providers::auth_server(3600);
    auth.state.lock().unwrap().issuer_override = Some("https://evil.example".into());
    let http = krowk_harness::http::client().unwrap();
    let e = oauth::discover(&http, &auth.mock.url).await.unwrap_err();
    assert!(e.message.contains("names the issuer \"https://evil.example\""), "{}", e.message);
    // The configured issuer with a trailing slash, or metadata naming it
    // with one, is the same issuer.
    auth.state.lock().unwrap().issuer_override = Some(format!("{}/", auth.mock.url));
    assert!(oauth::discover(&http, &format!("{}/", auth.mock.url)).await.is_ok());
    assert!(auth.mock.seen.lock().unwrap().iter().all(|s| s.path.starts_with("/.well-known/")), "nothing but metadata was asked of it");
}

/// An interrupt lands while a SuperGrok call is still getting its token.
/// Waiting on another krowk's refresh is given up at once. A refresh this
/// session already sent is not: xAI rotates the refresh token as it answers,
/// so the answer is taken and saved, and the call stops after it — the login
/// survives the interrupt, and the next call uses the new token.
#[test]
fn r_prov_4_an_interrupt_while_getting_a_token_stops_the_call_and_keeps_the_login() {
    use krowk_harness::chat::{ChatClient, Credential};
    use krowk_harness::instances::{InstanceKind, InstancesConfig, Registry};
    use krowk_harness::native::{ModelClient, ModelRequest};
    use std::time::{Duration, Instant};

    let auth = providers::auth_server(3600);
    let d = dir("interrupt");
    let store = Store::new(d.join("credentials.json"));
    let rt = || tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let http = krowk_harness::http::client().unwrap();
    let expire = |store: &Store| {
        let mut s = store.load("supergrok").unwrap().unwrap();
        s.expires_at_ms = Some(0);
        store.save("supergrok", &s).unwrap();
    };
    let first = rt().block_on(oauth::login_device(&http, &login(&auth.mock.url), &mut |_| {})).unwrap();
    store.save("supergrok", &first).unwrap();
    expire(&store);
    auth.state.lock().unwrap().refresh_delay_ms = 3000;

    let cfg = InstancesConfig {
        instances: [("supergrok".to_string(), InstanceKind::XaiOauth { base_url: Some("http://127.0.0.1:9".into()), issuer: Some(auth.mock.url.clone()), client_id: None, scope: None, effort: None })].into(),
        ..InstancesConfig::default()
    };
    let instance = Registry::resolve(&cfg, &|_| String::new()).get("supergrok").unwrap().clone();
    // Runs one call, interrupted 300 ms in; how long it took to return.
    let interrupted_call = |label: &str| -> Duration {
        let client = ChatClient::new(instance.clone(), Credential::OAuth(std::sync::Arc::new(Tokens::open(store.clone(), "supergrok").unwrap())), "test").unwrap();
        let req = ModelRequest { model: "grok-4.7".into(), system: "s".into(), ..ModelRequest::default() };
        let (tx, _rx) = tokio::sync::mpsc::channel(64);
        let (cancel_tx, cancel) = tokio::sync::watch::channel(false);
        let flip = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            let _ = cancel_tx.send(true);
            cancel_tx
        });
        let started = Instant::now();
        let resp = rt().block_on(client.stream(&req, &tx, cancel)).unwrap();
        let took = started.elapsed();
        drop(flip.join());
        assert!(resp.interrupted, "{label}: the call reports the interrupt");
        took
    };

    // Another krowk is refreshing, and holds the store's lock for 3 s: the
    // wait for it is given up at once.
    let other = {
        let store = store.clone();
        std::thread::spawn(move || {
            let tokens = Tokens::open(store, "supergrok").unwrap();
            let http = krowk_harness::http::client().unwrap();
            rt().block_on(tokens.bearer(&http, false)).unwrap()
        })
    };
    std::thread::sleep(std::time::Duration::from_millis(100));
    let took = interrupted_call("waiting on the lock");
    assert!(took < Duration::from_millis(1500), "returned {took:?} in, not after the other krowk's refresh");
    assert_eq!(other.join().unwrap(), "xai-at-2");

    // This session's own refresh is under way when the interrupt lands: it
    // finishes, is saved, and the call stops before it is made.
    expire(&store);
    let took = interrupted_call("refreshing");
    assert!(took >= Duration::from_millis(2500), "the refresh ran to its end: {took:?}");
    let saved = store.load("supergrok").unwrap().unwrap();
    assert_eq!((saved.access_token.as_str(), saved.refresh_token.as_deref()), ("xai-at-3", Some("xai-rt-3")), "the rotated token was kept");
    assert_eq!(auth.state.lock().unwrap().refreshes, 2);
    // The login survives: a later call needs no refresh, and a forced one
    // spends the live refresh token without invalid_grant.
    auth.state.lock().unwrap().refresh_delay_ms = 0;
    let later = Tokens::open(store.clone(), "supergrok").unwrap();
    assert_eq!(rt().block_on(later.bearer(&http, false)).unwrap(), "xai-at-3");
    assert_eq!(rt().block_on(later.bearer(&http, true)).unwrap(), "xai-at-4");
    let _ = std::fs::remove_dir_all(&d);
}
