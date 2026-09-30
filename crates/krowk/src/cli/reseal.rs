//! Starting over from a device that holds the user key (canon,
//! engineering/devices.md → Starting over): the sessions it can open under
//! the old list are sealed again under the new list's generation 1, and
//! their records signed again, so they come along.
//!
//! The old list and user keys are set aside in `before-start-over/` in
//! krowk's home, with a copy of this device's key to open them, before the
//! new ones are kept. The loop is resumable: a session already sealed
//! under the new list is skipped, and running `krowk sync init
//! --start-over` again, while the aside is there, finishes what is left.
//! The aside goes once every session this device could open has been
//! sealed again.

use super::chain::{keystore_at, Me};
use super::Ctx;
use krowk_api::sync::SyncSession;
use krowk_api::{fail, Client, Error};
use krowk_client::device_chain::Chain;
use krowk_client::e2e::{self, SessionKey};
use krowk_client::session_record::{self, Signer};
use krowk_client::user_key::UserKeys;
use std::path::{Path, PathBuf};

/// Where the old list and keys wait while their sessions are sealed again.
pub(super) const ASIDE: &str = "before-start-over";

/// How long a lease taken to write one session lasts: long enough for one
/// PUT, and it is let go straight after.
const LEASE_TTL: u64 = 60;

/// The old list and the user keys that open its sessions.
pub(super) struct Old {
    pub chain: Chain,
    pub keys: UserKeys,
}

/// What a re-seal did.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Count {
    pub sealed: usize,
    pub already: usize,
    /// Sessions this device could not open, or whose lease another device
    /// holds: left as they were.
    pub left: usize,
}

pub(super) fn aside_dir(home: &Path) -> PathBuf {
    home.join(ASIDE)
}

/// The old list and keys set aside by an earlier start-over, if one did
/// not finish.
pub(super) fn set_aside(home: &Path) -> Result<Option<Old>, Error> {
    let store = keystore_at(&aside_dir(home));
    let (Some(chain), Some(keys)) = (store.device_list().map_err(|e| fail("keys_unreadable", e))?, store.user_keys().map_err(|e| fail("keys_unreadable", e))?) else {
        return Ok(None);
    };
    Ok(Some(Old { chain, keys }))
}

/// Moves the old list and user keys aside, with a copy of this device's
/// key to open them, so the new ones can be kept in their place.
pub(super) fn move_aside(home: &Path) -> Result<(), Error> {
    let dir = aside_dir(home);
    let failed = |e: std::io::Error| fail("sync_setup_failed", format!("the old device list could not be set aside in {}: {e}", dir.display()));
    std::fs::create_dir_all(&dir).map_err(failed)?;
    let live = keystore_at(home);
    for (from, to) in [(live.user_keys_path(), "user-keys.json"), (live.device_list_path(), "device-list.json")] {
        if from.exists() {
            std::fs::rename(&from, dir.join(to)).map_err(failed)?;
        }
    }
    std::fs::copy(live.device_path(), dir.join("device.json")).map_err(failed)?;
    Ok(())
}

/// Seals again, under `keys` and `chain`, every session the old list's
/// keys open; one page after another, until the registry has no more.
pub(super) fn run(ctx: &Ctx, client: &Client, me: &Me, old: &Old, keys: &UserKeys, chain: &Chain) -> Result<Count, Error> {
    let api = me.sign(client);
    let env = krowk_api::relay_env(&api.base_url, ctx.io.env);
    let mut count = Count::default();
    let mut before = String::new();
    loop {
        let page = api.list_sync_sessions(&before, 50)?;
        for s in &page.sessions {
            match one(&api, env, me, s, old, keys, chain)? {
                Step::Sealed => count.sealed += 1,
                Step::Already => count.already += 1,
                Step::Left => count.left += 1,
            }
        }
        if page.next.is_empty() {
            return Ok(count);
        }
        before = page.next;
    }
}

enum Step {
    Sealed,
    Already,
    Left,
}

fn one(api: &Client, env: &str, me: &Me, s: &SyncSession, old: &Old, keys: &UserKeys, chain: &Chain) -> Result<Step, Error> {
    if open(s, keys, chain).is_ok() {
        return Ok(Step::Already);
    }
    let Ok(key) = open(s, &old.keys, &old.chain) else { return Ok(Step::Left) };
    let raw = session_id(&s.id).ok_or_else(|| fail("malformed_response", format!("the registry lists a session under {:?}, which is no session id", s.id)))?;
    let sealed = e2e::seal_session_key(&key, &raw, keys, chain.generation()).map_err(|e| fail("sync_failed", e.0))?;
    let signature = session_record::sign(&raw, &sealed, session_record::SEAL_USER, keys.newest(), &me.signing).map_err(|e| fail("sync_failed", e.0))?;
    let device = me.device.id().to_string();
    let lease = match api.acquire_lease(&s.id, &device, LEASE_TTL, env) {
        Ok(l) => l,
        Err(e) if e.code() == "lease_held" => return Ok(Step::Left),
        Err(e) => return Err(e),
    };
    let put = api.put_sync_session(&s.id, &e2e::hex(&sealed), Some((&e2e::hex(&signature), &device)), None, Some(&lease.token));
    let _ = api.release_lease(&s.id, &lease.token);
    put.map(|_| Step::Sealed)
}

/// A session's key, out of its record: signed by a device `chain` has
/// held, and opened with `keys`.
fn open(s: &SyncSession, keys: &UserKeys, chain: &Chain) -> Result<SessionKey, String> {
    let raw = session_id(&s.id).ok_or("no session id")?;
    let wrapped = e2e::unhex(&s.wrapped_key).ok_or("the wrapped key is not hex")?;
    let seal = if s.seal.is_empty() { session_record::SEAL_USER } else { s.seal.as_str() };
    let signer = e2e::DeviceId::parse(&s.signer).ok_or("no signer")?;
    let signature = e2e::unhex(&s.record_signature).ok_or("no signature")?;
    session_record::verify(&raw, &wrapped, seal, signer, &signature, chain, Signer::EverHeld).map_err(|e| e.0)?;
    e2e::unwrap_session_key(&wrapped, &raw, keys).map_err(|e| e.0)
}

/// A session's id, a UUID, as its 16 bytes.
fn session_id(id: &str) -> Option<[u8; 16]> {
    e2e::unhex(&id.replace('-', "")).and_then(|b| b.try_into().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    use krowk_client::device_chain::{Kind, Subject};
    use krowk_client::keystore::Keystore;

    fn listed(ks: &Keystore, name: &str) -> (Chain, krowk_client::device_chain::Batch) {
        let (device, signing) = (ks.device_key().unwrap(), ks.signing_key().unwrap());
        let me = Subject { kind: Kind::Device, name: name.into(), os: "linux".into(), device: device.public(), signing: signing.public() };
        Chain::start(me, &signing, None, 1_790_000_000).unwrap()
    }

    /// C1 of #204's review: a start-over from a set-up device keeps its
    /// device key, sets the old list and user keys aside where they still
    /// open, and then keeps generation 1 of the new list in their place —
    /// which the never-go-back store would refuse over the old keys.
    #[test]
    fn d6_a_start_over_sets_the_old_keys_aside_where_they_still_open() {
        let home = std::env::temp_dir().join(format!("krowk-reseal-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let ks = Keystore::new(&home);
        let (old_chain, old) = listed(&ks, "laptop");
        ks.save_device_list(&old.entries).unwrap();
        let g2 = old.newest.next().unwrap();
        let held = UserKeys::new(g2.clone(), [g2.wrap_previous(&old.newest).unwrap()]).unwrap();
        ks.save_user_keys(&held).unwrap();
        let device = ks.device().unwrap().unwrap().id();

        move_aside(&home).unwrap();
        let (new_chain, new) = listed(&ks, "laptop");
        let fresh = UserKeys::new(new.newest.clone(), Vec::new()).unwrap();
        ks.save_user_keys(&fresh).unwrap();
        ks.save_device_list(&new.entries).unwrap();

        assert_eq!(ks.device().unwrap().unwrap().id(), device, "the device key is kept");
        let back = set_aside(&home).unwrap().expect("the old list and keys, aside");
        assert_eq!(back.chain.root(), old_chain.root());
        assert_eq!(back.keys.newest().generation(), 2);
        assert_eq!(back.keys.open(1).unwrap(), old.newest, "the old keys still open every old generation");
        assert_eq!(ks.user_keys().unwrap().unwrap().newest().generation(), 1);
        assert_eq!(ks.device_list().unwrap().unwrap().root(), new_chain.root());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_session_id_reads_as_its_16_bytes() {
        let raw = session_id("0190f3a8-7c1e-7a9b-8c2d-3e4f5a6b7c8d").unwrap();
        assert_eq!(e2e::hex(&raw), "0190f3a87c1e7a9b8c2d3e4f5a6b7c8d");
        assert!(session_id("not-a-uuid").is_none());
    }
}
