//! What the commands that change a person's device list share (canon,
//! engineering/devices.md): reading the list and verifying it against the
//! one this device keeps, the fresh sign-in a destructive change needs, the prompts
//! only a person may answer, and turning a batch into the post that
//! carries it. The chain, its verifier and the keys are `krowk_client`'s;
//! this is the command line around them.

use super::sync::{device_name, keystore, printable};
use super::Ctx;
use krowk_api::sync::{ListPost, ListedDevice};
use krowk_api::{fail, Client, Error};
use krowk_client::device_chain::{Batch, Chain, Device, Kind, SignedEntry, Subject};
use krowk_client::e2e::{self, DeviceKey, SigningKey};
use krowk_client::recovery::RecoveryDevice;
use krowk_client::user_key::{UserKey, UserKeys};
use std::path::PathBuf;

/// What the recovery device is called on the list.
pub(super) const KIT_NAME: &str = "recovery kit";

/// The most bytes read from stdin for the kit's words: twelve of the
/// longest words and their spaces are well under it.
const WORDS_MAX: usize = 4096;

pub(super) fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// The keystore of a home other than this one: the aside a start-over
/// keeps the old list in.
pub(super) fn keystore_at(home: &std::path::Path) -> krowk_client::keystore::Keystore {
    krowk_client::keystore::Keystore::new(home)
}

pub(super) fn home(ctx: &Ctx) -> Result<PathBuf, Error> {
    krowk_api::home::dir(ctx.io.env)
}

/// Whether the debug build's test suite stands in for the person.
fn unattended(ctx: &Ctx) -> bool {
    cfg!(debug_assertions) && ctx.env("KROWK_TEST_UNATTENDED_DEVICE_APPROVAL") == "1"
}

/// Refused off a terminal: every command here makes a change to the list
/// of devices that can read a person's sessions, or shows the kit that
/// can, and an agent following instructions it read somewhere is not the
/// person. The kit's words may be piped in (`piped_words`); the prompts
/// that follow read the terminal itself. Debug builds let the test suite
/// answer.
pub(super) fn need_person(ctx: &Ctx, what: &str, piped_words: bool) -> Result<(), Error> {
    if unattended(ctx) || (ctx.io.err_tty && (ctx.io.stdin_tty || piped_words)) {
        return Ok(());
    }
    Err(fail("confirmation_required", format!("{what}, so it needs a person at a terminal — run it in one")))
}

/// The debug build's stand-in answer to the next prompt, from
/// KROWK_TEST_ANSWERS (`|`-separated, taken in order).
fn test_answer(ctx: &Ctx) -> Option<String> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    if !unattended(ctx) {
        return None;
    }
    let all = ctx.env("KROWK_TEST_ANSWERS");
    let i = NEXT.fetch_add(1, Ordering::SeqCst);
    Some(all.split('|').nth(i).unwrap_or("").to_string())
}

/// A yes or no from the person, with no default: Enter alone is not an
/// answer.
pub(super) fn ask(ctx: &mut Ctx, question: &str) -> Result<bool, Error> {
    if let Some(a) = test_answer(ctx) {
        return Ok(matches!(a.trim(), "y" | "yes"));
    }
    inquire::Confirm::new(question).prompt().map_err(|_| fail("selection_cancelled", "not answered, so nothing was done"))
}

/// A line from the person.
pub(super) fn ask_line(ctx: &mut Ctx, prompt: &str) -> Result<String, Error> {
    if let Some(a) = test_answer(ctx) {
        return Ok(a);
    }
    inquire::Text::new(prompt).prompt().map_err(|_| fail("selection_cancelled", "not answered, so nothing was done"))
}

/// Twelve words, at a prompt that doesn't echo them, or piped in — never
/// as an argument, which a shell keeps in its history and every process
/// list shows. Empty when the person just pressed Enter.
pub(super) fn ask_words(ctx: &mut Ctx, prompt: &str) -> Result<krowk_client::Zeroizing<String>, Error> {
    if let Some(a) = test_answer(ctx).filter(|_| ctx.io.stdin_tty) {
        return Ok(krowk_client::Zeroizing::new(a));
    }
    if ctx.io.stdin_tty {
        let typed = inquire::Password::new(prompt).without_confirmation().prompt().map_err(|_| fail("selection_cancelled", "no words were entered, so nothing was done"))?;
        return Ok(krowk_client::Zeroizing::new(typed));
    }
    use std::io::Read;
    // Room for the most it reads, so the words are never reallocated (and
    // an unwiped copy left behind).
    let mut raw = krowk_client::Zeroizing::new(String::with_capacity(WORDS_MAX + 1));
    std::io::stdin().take(WORDS_MAX as u64 + 1).read_to_string(&mut raw).map_err(|e| fail("bad_recovery_kit", format!("the words could not be read from stdin: {e}")))?;
    if raw.len() > WORDS_MAX {
        return Err(fail("bad_recovery_kit", "more than 4 KiB was piped in, which is no recovery kit"));
    }
    Ok(raw)
}

/// A fresh sign-in: a browser login whose approval asks for the person's
/// password (or Google sign-in) again, with its key stored and returned.
/// The stored key and an open browser session are not enough — a thief
/// holding the laptop may hold both (devices.md → Destructive actions need
/// a fresh sign-in). It also puts a person behind a key that had none, as
/// a sign-up's default key has not. Debug builds' test suite keeps the
/// key it has (KROWK_TEST_FRESH_SIGN_IN=skip); the stand-in registry takes
/// it as fresh.
pub(super) fn fresh_sign_in(ctx: &mut Ctx, why: &str) -> Result<Client, Error> {
    if cfg!(debug_assertions) && ctx.env("KROWK_TEST_FRESH_SIGN_IN") == "skip" {
        return super::sync::keyed_client(ctx, "this");
    }
    let _ = writeln!(ctx.io.stderr, "{why} — sign in again in the browser, with your password.");
    let (token, _) = super::auth::browser_login(ctx, true)?;
    Ok(Client::new(&krowk_api::base_url_for(ctx.f.dev, ctx.io.env), &token))
}

/// This machine as the list names it: its name, without what a list name
/// refuses, cut to the list's 64 bytes on a character boundary, and its
/// operating system.
pub(super) fn this_subject(ctx: &Ctx, device: &DeviceKey, signing: &SigningKey) -> Subject {
    let mut name: String = device_name(ctx).chars().filter(|c| !krowk_client::device_chain::refused_in_name(*c)).collect();
    while name.len() > 64 {
        name.pop();
    }
    if name.trim().is_empty() {
        name = "krowk device".into();
    }
    Subject { kind: Kind::Device, name, os: std::env::consts::OS.into(), device: device.public(), signing: signing.public() }
}

/// The recovery device a kit derives, as the list names it.
pub(super) fn kit_subject(kit: &RecoveryDevice) -> Subject {
    Subject { kind: Kind::Recovery, name: KIT_NAME.into(), os: String::new(), device: kit.key.public(), signing: kit.signing.public() }
}

/// Whether `kit` is the recovery device on `chain`: both its keys.
pub(super) fn is_kit(chain: &Chain, kit: &RecoveryDevice) -> bool {
    chain.recovery().is_some_and(|r| r.device == kit.key.public() && r.signing == kit.signing.public())
}

/// A client on the same registry and key, signing as `device`: a command
/// here may act as two devices (this machine and the kit).
pub(super) fn signed_as(client: &Client, device: &DeviceKey, signing: &SigningKey) -> Client {
    let key = SigningKey::from_secret(&*signing.secret_bytes()).expect("a signing key's own secret");
    Client::new(&client.base_url, &client.token).signed_by(e2e::DeviceSigner::new(device.id(), key).shared())
}

/// This device's keys.
pub(super) struct Me {
    pub device: DeviceKey,
    pub signing: SigningKey,
}

impl Me {
    pub fn load(ctx: &Ctx) -> Result<Me, Error> {
        let store = keystore(ctx)?;
        let device = store.device_key().map_err(|e| fail("sync_setup_failed", e))?;
        let signing = store.signing_key().map_err(|e| fail("sync_setup_failed", e))?;
        Ok(Me { device, signing })
    }

    pub fn sign(&self, client: &Client) -> Client {
        signed_as(client, &self.device, &self.signing)
    }
}

/// Says a key just minted by a fresh sign-in speaks for this device, as
/// the one it replaces did; every sync call after needs that. Any refusal
/// is said: a key bound to another device fails every call after.
pub(super) fn claim(client: &Client, me: &Me) -> Result<(), Error> {
    me.sign(client).claim_key_device()
}

/// What must hold before this device seals anything new — a session, a
/// checkpoint, a chunk under a new key: the list kept here extended from
/// the registry and verified, and any newer user key taken up, so it seals
/// under the generation the list leaves current and never one a removed
/// device holds. A device with no list kept, or that cannot reach the
/// registry, seals nothing new.
pub(super) fn before_sealing(ctx: &Ctx, client: &Client, me: &Me) -> Result<u32, Error> {
    let v = verified(ctx, client, me)?;
    let held = held_keys(ctx)?.map(|k| k.newest().generation());
    sealing_generation(held, v.chain.generation())
}

/// The generation to seal under, or the refusal: the device must hold the
/// very generation the verified list names.
pub(super) fn sealing_generation(held: Option<u32>, named: u32) -> Result<u32, Error> {
    match held {
        Some(g) if g == named => Ok(g),
        Some(g) if g < named => Err(fail("user_key_behind", format!("your device list names user key generation {named}, and this device holds only {g} — it seals nothing new until it has taken up the newer key; run `krowk sync status`"))),
        Some(g) => Err(fail("user_key_refused", format!("this device holds user key generation {g}, newer than your device list names ({named}) — it seals nothing; run `krowk sync status`"))),
        None => Err(fail("user_key_missing", "this device holds no user key, so it seals nothing — run `krowk sync status`, or pair it again with `krowk sync join`")),
    }
}

/// The list's entries as the verifier takes them.
pub(super) fn decode(list: &krowk_api::sync::DeviceList) -> Result<Vec<SignedEntry>, Error> {
    let bad = |seq: u64| fail("malformed_response", format!("the registry served entry {seq} of your device list in a shape krowk does not read"));
    list.entries
        .iter()
        .map(|e| {
            let bytes = e2e::unhex(&e.entry).ok_or_else(|| bad(e.seq))?;
            let sigs = e2e::unhex(&e.signatures).ok_or_else(|| bad(e.seq))?;
            SignedEntry::from_parts(bytes, &sigs).map_err(|_| bad(e.seq))
        })
        .collect()
}

/// When entry `seq` says it was made, as its signer's clock read it.
pub(super) fn entry_time(entries: &[SignedEntry], seq: u64) -> Option<u64> {
    let e = entries.get(seq as usize)?;
    krowk_client::device_chain::Entry::decode(&e.bytes).ok().map(|e| e.time)
}

/// The device list as the registry has it now, verified from the one kept
/// here, and its entries.
pub(super) struct Verified {
    pub chain: Chain,
    pub entries: Vec<SignedEntry>,
}

/// The registry's list, read and checked against the one kept here
/// (`Keystore::save_device_list`, which refuses a list that is shorter,
/// forked, or doesn't reach the kept head, and keeps it): a list with
/// another first entry was started over, and this device stops rather than
/// adopt a chain it cannot tell the person made. A newer user key
/// generation is taken up (`adopt_user_key`).
pub(super) fn verified(ctx: &Ctx, client: &Client, me: &Me) -> Result<Verified, Error> {
    let store = keystore(ctx)?;
    let kept = store.device_list().map_err(|e| fail("keys_unreadable", e))?.ok_or_else(not_set_up)?;
    let entries = decode(&client.device_list()?)?;
    if entries.first().map(SignedEntry::hash) != Some(kept.root()) {
        return Err(reset(entry_time(&entries, 0)));
    }
    let chain = store.save_device_list(&entries).map_err(|e| fail("device_list_refused", format!("{e} — this device will not trust it. Nothing was changed; if it persists, the registry is serving a list your devices did not make")))?;
    if !chain.devices().iter().any(|d| d.id() == me.device.id()) {
        return Err(fail("device_removed", "this device is no longer on your device list — it was removed, and syncs nothing. Pair it again with `krowk sync join` from a device on the list"));
    }
    adopt_user_key(ctx, client, me, &chain)?;
    Ok(Verified { chain, entries })
}

/// The list was started over, at `at` when there is a new list to say it
/// by: this device is on none it made, and stops.
fn reset(at: Option<u64>) -> Error {
    let when = at.map(|t| format!(" at {}", utc(t))).unwrap_or_default();
    fail("device_list_reset", format!("your device list was started over{when}, and this device is not on the new one. It has stopped syncing. If you started over, pair it again with `krowk sync join`; if you did not, someone with your password did, so change it"))
}

pub(super) fn not_set_up() -> Error {
    fail("not_set_up", "sync is not set up on this machine — `krowk sync init` sets it up, `krowk sync join` adds this machine from one of your devices, and `krowk sync recover` gets back in with the recovery kit")
}

/// The user keys this device holds (`Keystore::user_keys`).
pub(super) fn held_keys(ctx: &Ctx) -> Result<Option<UserKeys>, Error> {
    keystore(ctx)?.user_keys().map_err(|e| fail("keys_unreadable", e))
}

/// Every generation's wrap of the one before, as the registry holds them.
pub(super) fn links_of(wraps: &krowk_api::sync::UserKeyWraps) -> Vec<Vec<u8>> {
    wraps.generations.iter().filter(|g| g.generation > 1).filter_map(|g| e2e::unhex(&g.wrapped_previous)).collect()
}

/// Generation `generation` as the registry wrapped it to the asking device.
fn wrap_for(wraps: &krowk_api::sync::UserKeyWraps, generation: u32) -> Result<Vec<u8>, Error> {
    wraps.wraps.iter().find(|w| w.generation == generation).and_then(|w| e2e::unhex(&w.wrapped_key)).ok_or_else(|| fail("user_key_missing", format!("the registry holds no user key generation {generation} wrapped to this device — ask again later, or pair it again with `krowk sync join`")))
}

fn key_refused(e: krowk_client::e2e::Error) -> Error {
    fail("user_key_refused", format!("{} — the registry's wrap is not one your devices made, and this device keeps the key it holds", e.0))
}

/// Keeps `keys`, held to the ids the verified chain names for every
/// generation (`UserKeys::verified_by`).
pub(super) fn save_keys(ctx: &Ctx, keys: UserKeys, chain: &Chain) -> Result<UserKeys, Error> {
    let keys = keys.verified_by(chain).map_err(key_refused)?;
    keystore(ctx)?.save_user_keys(&keys).map_err(|e| fail("sync_setup_failed", e))?;
    Ok(keys)
}

/// The user keys the chain leaves current, held here. A newer generation
/// than the one this device holds is adopted only as the id the verified
/// chain names, and only when it opens back down to the one held (HPKE
/// Base mode does not say who wrapped it; `UserKeys::adopt`).
pub(super) fn adopt_user_key(ctx: &Ctx, client: &Client, me: &Me, chain: &Chain) -> Result<UserKeys, Error> {
    let held = held_keys(ctx)?;
    let generation = chain.generation();
    match held.as_ref().map(|k| k.newest().generation()) {
        Some(g) if g == generation => return held.expect("held").verified_by(chain).map_err(key_refused),
        Some(g) if g > generation => return Err(fail("user_key_refused", format!("this device holds user key generation {g}, newer than your device list names — it will not go back"))),
        _ => {}
    }
    let wraps = me.sign(client).user_key()?;
    let blob = wrap_for(&wraps, generation)?;
    let keys = match &held {
        Some(k) => UserKeys::adopt(k.newest(), &blob, generation, chain.key_id(), &me.device, links_of(&wraps)),
        None => UserKey::unwrap(&blob, generation, chain.key_id(), &me.device).and_then(|k| UserKeys::new(k, links_of(&wraps))),
    }
    .map_err(key_refused)?;
    save_keys(ctx, keys, chain)
}

/// The user key the chain leaves current, out of its wrap to `kit`.
pub(super) fn open_for_kit(wraps: &krowk_api::sync::UserKeyWraps, chain: &Chain, kit: &RecoveryDevice) -> Result<UserKey, Error> {
    UserKey::unwrap(&wrap_for(wraps, chain.generation())?, chain.generation(), chain.key_id(), &kit.key).map_err(key_refused)
}

/// A batch as the registry takes it.
pub(super) fn post_of(batch: &Batch) -> ListPost {
    ListPost {
        entries: batch.entries.iter().map(|e| (e2e::hex(&e.bytes), e2e::hex(&e.signatures_bytes()))).collect(),
        links: batch.links.iter().map(|l| e2e::hex(l)).collect(),
        wraps: batch.wraps.iter().map(|(d, w)| (d.to_string(), e2e::hex(w))).collect(),
    }
}

/// After a post: the user keys kept when it rotated or added this device
/// — the batch's newest generation, with `older`, the wraps of every
/// generation before the batch, and the batch's own — then the list with
/// the batch's entries on the end, last, since it is what every other sync
/// command trusts.
pub(super) fn keep(ctx: &Ctx, before: &[SignedEntry], chain: &Chain, batch: &Batch, me: &Me, older: Vec<Vec<u8>>) -> Result<(), Error> {
    if batch.wraps.iter().any(|(d, _)| *d == me.device.id()) {
        let keys = UserKeys::new(batch.newest.clone(), older.into_iter().chain(batch.links.iter().cloned())).map_err(|e| fail("sync_setup_failed", e.0))?;
        save_keys(ctx, keys, chain)?;
    }
    let entries: Vec<SignedEntry> = before.iter().chain(&batch.entries).cloned().collect();
    keystore(ctx)?.save_device_list(&entries).map_err(|e| fail("sync_setup_failed", e)).map(|_| ())
}

/// The wraps a device's held keys carry, to pass on to `keep`.
pub(super) fn older_of(keys: &UserKeys) -> Vec<Vec<u8>> {
    keys.wraps().map(|(_, w)| w.to_vec()).collect()
}

/// The devices revoked on the dashboard that the chain has not removed
/// yet: they are refused already, but a new user key wrapped to the list
/// would still reach them, so the next rotation must take them off.
pub(super) fn revoked_on_dashboard<'a>(chain: &'a Chain, listed: &[ListedDevice]) -> Vec<&'a Device> {
    chain.devices().iter().filter(|d| d.kind == Kind::Device && listed.iter().any(|l| l.id.eq_ignore_ascii_case(&d.id().to_string()) && !l.revoked_at.is_empty() && l.removed_seq.is_none())).collect()
}

/// The dashboard's revocations, or none with that said: they only annotate
/// what the chain already says.
pub(super) fn listed_devices(ctx: &mut Ctx, client: &Client) -> Vec<ListedDevice> {
    client.listed_devices().unwrap_or_else(|e| {
        let _ = writeln!(ctx.io.stderr, "(The devices revoked on the dashboard could not be read: {}. Remove any you revoked there.)", e.code());
        Vec::new()
    })
}

/// How a device reads in a prompt: its name, its system, and when it was
/// added, by the entry's own time.
pub(super) fn describe(d: &Device, entries: &[SignedEntry]) -> String {
    let day = entry_time(entries, d.added).map(|t| utc(t)[..10].to_string());
    match d.kind {
        Kind::Recovery => format!("{KIT_NAME} (made {})", day.unwrap_or_else(|| "?".into())),
        Kind::Device => {
            let about: Vec<String> = [(!d.os.is_empty()).then(|| printable(&d.os)), day.map(|d| format!("added {d}"))].into_iter().flatten().collect();
            match about.is_empty() {
                true => format!("'{}'", printable(&d.name)),
                false => format!("'{}' ({})", printable(&d.name), about.join(", ")),
            }
        }
    }
}

/// Unix seconds as `YYYY-MM-DD HH:MM UTC`, for what a person reads.
pub(super) fn utc(t: u64) -> String {
    jiff::Timestamp::from_second(t as i64).map(|ts| ts.strftime("%Y-%m-%d %H:%M UTC").to_string()).unwrap_or_else(|_| t.to_string())
}

/// What a command says when it is done: the sentence for a person, the
/// envelope for a program.
pub(super) fn say(ctx: &mut Ctx, data: serde_json::Value, summary: String) -> Result<(), Error> {
    if ctx.format == crate::output::Format::Human {
        let _ = writeln!(ctx.io.stdout, "{summary}");
        return Ok(());
    }
    super::sessions::emit_data(ctx, data, summary)
}

/// Writes the kit's words to `path` — 0600 on Unix — refusing a file that
/// is already there rather than write over it. Never a copy of the words
/// built here that their wiping would miss.
pub(super) fn save_kit(path: &str, words: &str) -> Result<(), Error> {
    use std::io::Write as _;
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    let unwritable = |e: std::io::Error| fail("kit_unwritable", format!("the recovery kit could not be written to {path}: {e} — nothing was changed; name a file that does not exist yet"));
    let mut f = o.open(path).map_err(unwritable)?;
    f.write_all(words.as_bytes()).and_then(|()| f.write_all(b"\n")).and_then(|()| f.sync_all()).map_err(unwritable)
}

/// Shows the kit's words on stderr, numbered, four to a row, so they are
/// copied in order. Never stdout, `--json` or a log.
pub(super) fn show_kit(ctx: &mut Ctx, words: &str) {
    let stderr = &mut *ctx.io.stderr;
    let _ = writeln!(stderr, "Your recovery kit: the only way back in if you lose every device.");
    for (i, w) in words.split(' ').enumerate() {
        let _ = write!(stderr, "{}{:>2} {w}", if i % 4 == 0 { "  " } else { "" }, i + 1);
        let _ = match i % 4 {
            3 => writeln!(stderr),
            _ => write!(stderr, "{:pad$}", "", pad = 10usize.saturating_sub(w.len())),
        };
    }
    let _ = writeln!(stderr, "Write it down or save it (--save FILE). krowk can't show it again.");
    let _ = stderr.flush();
}

#[cfg(test)]
mod tests {
    use super::*;
    use krowk_client::device_chain::Change;

    fn subject(kind: Kind, name: &str) -> (Subject, SigningKey, DeviceKey) {
        let (d, s) = (DeviceKey::generate(), SigningKey::generate());
        (Subject { kind, name: name.into(), os: "linux".into(), device: d.public(), signing: s.public() }, s, d)
    }

    /// D7: a device revoked on the dashboard and not yet removed is swept
    /// into the next rotation; one the chain already removed, or never
    /// revoked, is not.
    #[test]
    fn d7_the_dashboard_revoked_devices_still_on_the_list_are_found() {
        let (laptop, lk, _) = subject(Kind::Device, "laptop");
        let (kit, kk, _) = subject(Kind::Recovery, KIT_NAME);
        let (chain, batch) = Chain::start(laptop.clone(), &lk, Some((kit, &kk)), 1_000).unwrap();
        let (old, _, _) = subject(Kind::Device, "old");
        let (chain, _) = chain.batch(&batch.newest, vec![Change::Add(old.clone())], laptop.id(), &lk, 1_001).unwrap();
        let row = |s: &Subject, revoked: &str, removed: Option<u64>| ListedDevice { id: s.id().to_string(), kind: "device".into(), name: s.name.clone(), revoked_at: revoked.into(), removed_seq: removed, ..Default::default() };
        let found: Vec<_> = revoked_on_dashboard(&chain, &[row(&laptop, "", None), row(&old, "2026-09-30T00:00:00Z", None)]).into_iter().map(|d| d.name.clone()).collect();
        assert_eq!(found, ["old"]);
        assert!(revoked_on_dashboard(&chain, &[row(&old, "2026-09-30T00:00:00Z", Some(1))]).is_empty());
    }

    /// D6: a batch's post carries its entries, links and wraps as hex, and
    /// the wraps name exactly the devices the batch wrapped to.
    #[test]
    fn d6_a_post_carries_the_batch_whole() {
        let (laptop, lk, _) = subject(Kind::Device, "laptop");
        let (kit, kk, _) = subject(Kind::Recovery, KIT_NAME);
        let (_, batch) = Chain::start(laptop.clone(), &lk, Some((kit.clone(), &kk)), 1_000).unwrap();
        let post = post_of(&batch);
        assert_eq!(post.entries.len(), 1);
        assert!(post.links.is_empty());
        let mut to: Vec<_> = post.wraps.iter().map(|(d, _)| d.clone()).collect();
        to.sort();
        let mut want = vec![laptop.id().to_string(), kit.id().to_string()];
        want.sort();
        assert_eq!(to, want, "seq 0 wraps generation 1 to the first device and the kit");
    }

    /// D6: a device seals only under the generation its verified list
    /// names — never an older one a removed device may hold.
    #[test]
    fn d6_a_device_seals_only_under_the_generation_the_list_names() {
        assert_eq!(sealing_generation(Some(3), 3).unwrap(), 3);
        assert_eq!(sealing_generation(Some(2), 3).unwrap_err().code(), "user_key_behind");
        assert_eq!(sealing_generation(Some(4), 3).unwrap_err().code(), "user_key_refused");
        assert_eq!(sealing_generation(None, 1).unwrap_err().code(), "user_key_missing");
    }

    /// D6: a skipped kit leaves seq 0 with no recovery device, which is
    /// what the reminder is about.
    #[test]
    fn d6_a_skipped_kit_leaves_no_recovery_device() {
        let (laptop, lk, _) = subject(Kind::Device, "laptop");
        let (chain, batch) = Chain::start(laptop.clone(), &lk, None, 1_000).unwrap();
        assert!(chain.recovery().is_none());
        assert_eq!(batch.wraps.len(), 1);
    }
}
