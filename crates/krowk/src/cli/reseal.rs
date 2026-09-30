//! Starting over from a device that holds the user key (canon,
//! engineering/devices.md → Starting over): the sessions it can open under
//! the old list are sealed again under the new list's generation 1, and
//! their records signed again, so they come along.
//!
//! In this order, so a crash anywhere leaves a device that syncs plus the
//! old keys:
//! 1. The old `user-keys.json` and `device-list.json` are copied into
//!    `before-start-over/` in krowk's home, before anything is posted. They
//!    open with this device's key, which a start-over keeps.
//! 2. The new list's seq 0 is posted.
//! 3. The new generation 1 and list are kept in place of the old ones.
//! 4. Every session the old keys open, and the new ones don't, is sealed
//!    again. A session whose write the registry refuses — another
//!    workspace's, one this device may not replace, a lease another device
//!    holds — is left, not a failed run; `krowk sync init --start-over`,
//!    run again (under another workspace's key, for its sessions), goes
//!    through what is left.
//!
//! The aside is never deleted on its own: the old sessions of a workspace
//! not yet gone through need it. `krowk sync recovery discard-old` drops it,
//! when the person says so.

use super::chain::{keystore_at, Me};
use super::Ctx;
use krowk_api::sync::SyncSession;
use krowk_api::{fail, Client, Error};
use krowk_client::device_chain::Chain;
use krowk_client::e2e::{self, SessionKey};
use krowk_client::keystore::Keystore;
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

/// What a re-seal did, in the workspace its key is for.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Count {
    pub sealed: usize,
    pub already: usize,
    /// Sessions the old keys open whose write was refused: left as they
    /// were, for a later run.
    pub left: usize,
}

pub(super) fn aside_dir(home: &Path) -> PathBuf {
    home.join(ASIDE)
}

/// The old list and keys a start-over set aside, if there are any.
pub(super) fn set_aside(home: &Path) -> Result<Option<Old>, Error> {
    let dir = aside_dir(home);
    let live = keystore_at(home);
    let unreadable = |e: String| fail("keys_unreadable", e);
    let chain = live.device_list_at(&dir.join(file(&live, Part::List))).map_err(unreadable)?;
    let keys = live.user_keys_at(&dir.join(file(&live, Part::Keys))).map_err(unreadable)?;
    Ok(chain.zip(keys).map(|(chain, keys)| Old { chain, keys }))
}

enum Part {
    Keys,
    List,
}

/// The file name of one of the two files, as the live home names it.
fn file(ks: &Keystore, part: Part) -> PathBuf {
    let path = match part {
        Part::Keys => ks.user_keys_path(),
        Part::List => ks.device_list_path(),
    };
    PathBuf::from(path.file_name().expect("a file name"))
}

/// Copies the old list and user keys aside, before anything is posted: the
/// live ones stay until the new ones replace them.
pub(super) fn copy_aside(home: &Path) -> Result<(), Error> {
    let dir = aside_dir(home);
    let failed = |e: std::io::Error| fail("sync_setup_failed", format!("the old device list could not be set aside in {}: {e} — nothing was changed", dir.display()));
    std::fs::create_dir_all(&dir).map_err(failed)?;
    let live = keystore_at(home);
    for part in [Part::Keys, Part::List] {
        let name = file(&live, part);
        let from = home.join(&name);
        if from.exists() {
            let tmp = dir.join(format!("{}.tmp", name.display()));
            std::fs::copy(&from, &tmp).and_then(|_| std::fs::rename(&tmp, dir.join(&name))).map_err(failed)?;
        }
    }
    Ok(())
}

/// Seals again, under `keys` and `chain`, every session the old keys open
/// in the workspace `client`'s key is for; one page after another.
pub(super) fn run(ctx: &Ctx, client: &Client, me: &Me, old: &Old, keys: &UserKeys, chain: &Chain) -> Result<Count, Error> {
    walk(ctx, client, me, old, keys, chain, true)
}

/// How many sessions in this workspace the old keys open and the new ones
/// don't: what dropping the aside would lose.
pub(super) fn still_left(ctx: &Ctx, client: &Client, me: &Me, old: &Old, keys: &UserKeys, chain: &Chain) -> Result<usize, Error> {
    walk(ctx, client, me, old, keys, chain, false).map(|c| c.left)
}

fn walk(ctx: &Ctx, client: &Client, me: &Me, old: &Old, keys: &UserKeys, chain: &Chain, write: bool) -> Result<Count, Error> {
    let api = me.sign(client);
    let env = krowk_api::relay_env(&api.base_url, ctx.io.env);
    let mut count = Count::default();
    let mut before = String::new();
    loop {
        let page = api.list_sync_sessions(&before, 50)?;
        for s in &page.sessions {
            match one(&api, env, me, s, old, keys, chain, write)? {
                Step::Sealed => count.sealed += 1,
                Step::Already => count.already += 1,
                Step::Left => count.left += 1,
                Step::Foreign => {}
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
    /// Neither the old keys nor the new open it: not this device's to seal.
    Foreign,
}

/// Whether the registry refused this one write — as against the network,
/// or the registry's own failure, which end the run.
fn refused(e: &Error) -> bool {
    (400..500).contains(&e.status)
}

#[allow(clippy::too_many_arguments)]
fn one(api: &Client, env: &str, me: &Me, s: &SyncSession, old: &Old, keys: &UserKeys, chain: &Chain, write: bool) -> Result<Step, Error> {
    if open(s, keys, chain).is_ok() {
        return Ok(Step::Already);
    }
    let Ok(key) = open(s, &old.keys, &old.chain) else { return Ok(Step::Foreign) };
    if !write {
        return Ok(Step::Left);
    }
    let raw = session_id(&s.id).ok_or_else(|| fail("malformed_response", format!("the registry lists a session under {:?}, which is no session id", s.id)))?;
    let sealed = e2e::seal_session_key(&key, &raw, keys, chain.generation()).map_err(|e| fail("sync_failed", e.0))?;
    let signature = session_record::sign(&raw, &sealed, session_record::SEAL_USER, keys.newest(), &me.signing).map_err(|e| fail("sync_failed", e.0))?;
    let device = me.device.id().to_string();
    let lease = match api.acquire_lease(&s.id, &device, LEASE_TTL, env) {
        Ok(l) => l,
        Err(e) if refused(&e) => return Ok(Step::Left),
        Err(e) => return Err(e),
    };
    let put = api.put_sync_session(&s.id, &e2e::hex(&sealed), Some((&e2e::hex(&signature), &device)), None, Some(&lease.token));
    let _ = api.release_lease(&s.id, &lease.token);
    match put {
        Ok(_) => Ok(Step::Sealed),
        Err(e) if refused(&e) => Ok(Step::Left),
        Err(e) => Err(e),
    }
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
    /// device key, copies the old list and user keys aside — where they
    /// still open with that key — before anything is posted, and then keeps
    /// generation 1 of the new list in their place, which the
    /// never-go-back store would refuse over the old keys.
    #[test]
    fn d6_a_start_over_sets_the_old_keys_aside_where_they_still_open() {
        let home = std::env::temp_dir().join(format!("krowk-reseal-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let ks = Keystore::new(&home);
        let (old_chain, old) = listed(&ks, "laptop");
        ks.save_device_list(&old.entries).unwrap();
        let g2 = old.newest.next().unwrap();
        ks.save_user_keys(&UserKeys::new(g2.clone(), [g2.wrap_previous(&old.newest).unwrap()]).unwrap()).unwrap();
        let device = ks.device().unwrap().unwrap().id();

        copy_aside(&home).unwrap();
        assert!(ks.user_keys().unwrap().is_some(), "the live keys stay until the new ones replace them");
        let (new_chain, new) = listed(&ks, "laptop");
        ks.forget_user_keys().unwrap();
        ks.save_user_keys(&UserKeys::new(new.newest.clone(), Vec::new()).unwrap()).unwrap();
        ks.save_device_list(&new.entries).unwrap();

        assert_eq!(ks.device().unwrap().unwrap().id(), device, "the device key is kept");
        assert!(!aside_dir(&home).join("device.json").exists(), "no second copy of the device key");
        let back = set_aside(&home).unwrap().expect("the old list and keys, aside");
        assert_eq!(back.chain.root(), old_chain.root());
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
