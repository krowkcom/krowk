//! What the stand-in remembers between calls, the clock it reads, and the
//! lookups and predicates every handler shares.

use jiff::{SignedDuration, Timestamp};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::Read;
use std::sync::{Arc, Mutex};

/// The registry's own max_upload_bytes.
pub const DEFAULT_LIMIT_BYTES: i64 = 100 << 20;

/// How long a presigned URL stays good, matching the real 15 minutes. Public
/// alongside [`Clock`] because the two together are how a test lapses a
/// signature: nothing else makes 15 minutes pass inside one.
pub const UPLOAD_URL_LIFETIME: SignedDuration = SignedDuration::from_mins(15);

/// How long an upload with no paid plan behind it survives
/// (Artifact::EPHEMERAL_LIFETIME) — keyless, or keyed into a free workspace.
pub const EPHEMERAL_LIFETIME: SignedDuration = SignedDuration::from_hours(24);

/// How long a browser login stays open, and how long a lapsed one is kept so a
/// late poll still learns the window closed rather than that nothing existed.
pub const CLI_AUTHORIZATION_LIFETIME: SignedDuration = SignedDuration::from_mins(15);
pub const CLI_AUTHORIZATION_GRACE: SignedDuration = SignedDuration::from_hours(1);

/// Seconds between polls: one rather than the real five, because on loopback a
/// developer watching the flow should not spend most of it waiting.
pub const CLI_AUTHORIZATION_INTERVAL: i64 = 1;

pub const DEFAULT_PAGE_SIZE: i64 = 50;
pub const MAX_PAGE_SIZE: i64 = 100;
/// Canon's cap on a record's metadata. Size is the only thing validated about it.
pub const MAX_METADATA_BYTES: usize = 16 << 10;

/// The shared workspace keyless uploads land in, padded to a slug's shape.
pub const ANONYMOUS_WORKSPACE: &str = "ws_anonymous000000000000000";
pub const DEFAULT_VISIBILITY: &str = "public";
pub const SHARED_VISIBILITY: &str = "shared";
/// The visibilities a client may name, in the order the refusal lists them.
pub const DECLARABLE_VISIBILITIES: [&str; 3] = [DEFAULT_VISIBILITY, "private", SHARED_VISIBILITY];
/// The region every artifact is stored in; it leads the storage key.
pub const ARTIFACT_REGION: &str = "weur";

/// A key with this in it is a free workspace; every other key is paid, which is
/// what a developer poking at --dev wants by default.
pub const FREE_PLAN_KEY_MARKER: &str = "free";

/// The injectable now — Go's `HandlerWithClock`. A 24-hour lifetime and a
/// 15-minute upload window are not going to elapse inside a test otherwise.
pub type Clock = Arc<dyn Fn() -> Timestamp + Send + Sync>;

pub struct Artifact {
    pub slug: String,
    pub state: &'static str,
    pub filename: String,
    pub content_type: String,
    pub byte_size: i64,
    pub checksum: String,
    pub region: String,
    /// Decides the storage key's shape, who a read outside the owning workspace
    /// is answered for, and whether the card renders keyless.
    pub visibility: String,
    /// The run's slug, empty for none; served as the nested run object.
    pub run: String,
    pub url: String,
    pub file_url: String,
    pub markdown: String,
    pub expires_at: Option<Timestamp>,
    pub created_at: String,
    /// Stored as sent and compacted on the way out, as `json.RawMessage` is.
    pub metadata: Option<Vec<u8>>,

    pub workspace: String,
    pub claim_hash: String,
    pub uploaded: bool,
    /// What storage measured, copied onto the published fields at finalize:
    /// a pending artifact reports nothing storage has not confirmed.
    pub stored_size: i64,
    pub stored_sum: String,
    pub stored_width: i64,
    pub stored_height: i64,
    pub width: i64,
    pub height: i64,
    pub upload_tok: String,
    pub upload_til: Timestamp,
    pub claimed: bool,
    /// A tombstone: the bytes are gone, the row stays, a read is a 410.
    pub deleted_at: Option<Timestamp>,
    /// Pinned at declare: a claim moves the artifact, not its bytes.
    pub storage_key: String,
    /// The secret behind share_url, fresh on every entry to shared.
    pub share_token: String,
    /// Orders a listing; a timestamp would tie.
    pub seq: usize,
}

pub struct Run {
    pub slug: String,
    pub status: &'static str,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub metadata: Vec<u8>,
    pub created_at: String,
    pub workspace: String,
    pub seq: usize,
}

/// A create already answered, found again by its Idempotency-Key. The hash is
/// what the key was first used for; the record is held by slug so it cannot
/// point at something gone.
pub struct Answered {
    pub request_hash: String,
    pub artifact: String,
    pub run: String,
}

/// One browser login in flight. The slug collects the key and never reaches a
/// browser; the code is what a person confirms and can only approve or deny.
pub struct Authorization {
    pub slug: String,
    pub code: String,
    pub state: &'static str,
    pub created_at: Timestamp,
    /// The plaintext key, taken out by the read that delivers it.
    pub token: String,
    pub key_id: String,
    pub workspace: String,
    /// Collected already — an empty token alone could mean never minted.
    pub spent: bool,
}

pub struct Store {
    pub artifacts: HashMap<String, Artifact>,
    pub runs: HashMap<String, Run>,
    pub objects: HashMap<String, Vec<u8>>,
    pub authorizations: HashMap<String, Authorization>,
    /// Keyed by kind, caller and key — all three, as the registry digests all
    /// three — so one key covers a push's run and artifact, and one client's key
    /// cannot collide with another's.
    pub idempotent: HashMap<String, Answered>,
    /// Sync's records (`sync.rs`), each keyed by workspace and its own id so
    /// one tenant's id is never another's.
    pub sync: crate::sync::SyncStore,
    pub created: usize,
    pub runs_open: usize,
    clock: Clock,
}

/// Everything a handler reaches: the state behind its one lock, and the two
/// flags the process was started with.
pub struct App {
    pub store: Mutex<Store>,
    pub limit_bytes: i64,
    pub site: String,
}

impl App {
    pub fn new(limit_bytes: i64, site: &str, clock: Clock) -> App {
        App {
            store: Mutex::new(Store {
                artifacts: HashMap::new(),
                runs: HashMap::new(),
                objects: HashMap::new(),
                authorizations: HashMap::new(),
                idempotent: HashMap::new(),
                sync: crate::sync::SyncStore::default(),
                created: 0,
                runs_open: 0,
                clock,
            }),
            limit_bytes: if limit_bytes <= 0 { DEFAULT_LIMIT_BYTES } else { limit_bytes },
            site: site.to_owned(),
        }
    }

    pub fn lock(&self) -> std::sync::MutexGuard<'_, Store> {
        self.store.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Store {
    pub fn now(&self) -> Timestamp {
        (self.clock)()
    }

    pub fn expired(&self, a: &Artifact) -> bool {
        a.expires_at.is_some_and(|at| at < self.now())
    }

    pub fn authorization_expired(&self, a: &Authorization) -> bool {
        self.now() > a.created_at + CLI_AUTHORIZATION_LIFETIME
    }

    /// A lookup scoped to the request's workspace, so another tenant's slug
    /// reads as not existing. The write paths use it; reads use [`readable`].
    pub fn find(&self, workspace: &str, slug: &str) -> Option<&Artifact> {
        let workspace = if workspace.is_empty() { ANONYMOUS_WORKSPACE } else { workspace };
        self.artifacts.get(slug).filter(|a| a.workspace == workspace)
    }

    pub fn find_run(&self, workspace: &str, slug: &str) -> Option<&Run> {
        self.runs.get(slug).filter(|r| r.workspace == workspace)
    }

    /// The artifact whose bytes live under `key`. A scan rather than an index a
    /// visibility change would have to keep in step.
    pub fn by_storage_key(&self, key: &str) -> Option<&Artifact> {
        self.artifacts.values().find(|a| a.storage_key == key)
    }

    pub fn replay(&self, kind: &str, scope: &str, key: &str, hash: &str) -> Option<(&Answered, bool)> {
        self.idempotent.get(&format!("{kind}\n{scope}\n{key}")).map(|f| (f, f.request_hash == hash))
    }

    pub fn remember(&mut self, kind: &str, scope: &str, key: &str, entry: Answered) {
        self.idempotent.insert(format!("{kind}\n{scope}\n{key}"), entry);
    }
}

/// The read boundary, deliberately looser than the write one: a public
/// artifact reads by slug from anywhere, anything else only from its own
/// workspace — and everyone else is told it is missing, not forbidden.
pub fn readable(a: &Artifact, workspace: &str) -> bool {
    a.visibility == DEFAULT_VISIBILITY || (!workspace.is_empty() && a.workspace == workspace)
}

/// Whether the request carries a shared artifact's live share token.
pub fn share_readable(a: &Artifact, share: &str) -> bool {
    a.visibility == SHARED_VISIBILITY && !a.share_token.is_empty() && share == a.share_token
}

/// Whether the caller holds an authority over this artifact: a key's workspace,
/// or an unspent claim token issued for it. A tombstone still answers to both,
/// which makes a retried takedown a success.
pub fn authorized_to_write(a: Option<&Artifact>, workspace: &str, claim_token: &str) -> bool {
    match a {
        None => false,
        Some(a) if !workspace.is_empty() => a.workspace == workspace,
        Some(a) => !a.claim_hash.is_empty() && !a.claimed && a.claim_hash == sha256_hex(claim_token.as_bytes()),
    }
}

pub fn sha256_hex(b: &[u8]) -> String {
    hex(&Sha256::digest(b))
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn random_bytes(n: usize) -> Vec<u8> {
    // ponytail: /dev/urandom, since the stand-in only runs on macOS and Linux.
    let mut b = vec![0; n];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut b))
        .expect("a stand-in with no randomness cannot issue slugs");
    b
}

pub fn random_token() -> String {
    hex(&random_bytes(32))
}

/// Canon's slug length: a type prefix plus exactly this many characters,
/// which readers validate.
pub const SLUG_RANDOM_LENGTH: usize = 24;

/// Lowercase base36, because a slug becomes a DNS label and labels ignore case.
pub fn random_base36() -> String {
    const ALPHABET: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    random_bytes(SLUG_RANDOM_LENGTH).iter().map(|v| ALPHABET[*v as usize % ALPHABET.len()] as char).collect()
}

pub fn generate_slug(prefix: &str) -> String {
    format!("{prefix}_{}", random_base36())
}

pub fn new_share_token() -> String {
    format!("krowk_share_{}", random_base36())
}

/// Leaves out 0/O and 1/I, the pairs that get confused read off a screen or
/// aloud. 32 divides 256, so one pick per byte is unbiased.
pub const CODE_ALPHABET: &str = "23456789ABCDEFGHJKLMNPQRSTUVWXYZ";

/// Two groups of four: quick to compare, and 32^8 is far beyond what a quarter
/// hour leaves room to guess.
pub fn generate_code() -> String {
    let a = CODE_ALPHABET.as_bytes();
    let c: String = random_bytes(8).iter().map(|v| a[*v as usize % a.len()] as char).collect();
    format!("{}-{}", &c[..4], &c[4..])
}

/// A workspace derived from the token, so two keys never see each other's
/// artifacts. Hex is a subset of base36, so this is a slug of canon's shape.
pub fn workspace_for(token: &str) -> String {
    format!("ws_{}", &sha256_hex(token.as_bytes())[..SLUG_RANDOM_LENGTH])
}

/// Checksums travel as hex in the API and as base64 in S3's header.
pub fn base64_sum(hex_sum: &str) -> String {
    let Some(raw) = (0..hex_sum.len())
        .step_by(2)
        .map(|i| hex_sum.get(i..i + 2).and_then(|h| u8::from_str_radix(h, 16).ok()))
        .collect::<Option<Vec<u8>>>()
        .filter(|_| hex_sum.len().is_multiple_of(2))
    else {
        return String::new();
    };
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in raw.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, b)| n | (*b as u32) << (16 - 8 * i));
        for i in 0..4 {
            out.push(if i <= chunk.len() { T[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
        }
    }
    out
}

/// Go's RFC3339Nano: the fraction trimmed of trailing zeros, gone when zero.
pub fn rfc3339_nano(ts: Timestamp) -> String {
    let s = ts.strftime("%Y-%m-%dT%H:%M:%S").to_string();
    let frac = format!("{:09}", ts.subsec_nanosecond());
    let frac = frac.trim_end_matches('0');
    if frac.is_empty() { format!("{s}Z") } else { format!("{s}.{frac}Z") }
}

pub fn rfc3339(ts: Timestamp) -> String {
    ts.strftime("%Y-%m-%dT%H:%M:%SZ").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_print_the_way_go_prints_them() {
        let ts: Timestamp = "2026-09-24T10:00:00.120Z".parse().unwrap();
        assert_eq!(rfc3339_nano(ts), "2026-09-24T10:00:00.12Z");
        assert_eq!(rfc3339_nano("2026-09-24T10:00:00Z".parse().unwrap()), "2026-09-24T10:00:00Z");
        assert_eq!(rfc3339(ts), "2026-09-24T10:00:00Z");
    }

    /// Two groups of four from the alphabet that leaves out 0/O and 1/I.
    #[test]
    fn approval_codes_avoid_confusable_characters() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..200 {
            let code = generate_code();
            let (a, b) = code.split_once('-').unwrap();
            assert!(a.len() == 4 && b.len() == 4 && (a.to_owned() + b).chars().all(|c| CODE_ALPHABET.contains(c)), "{code}");
            seen.insert(code);
        }
        assert!(seen.len() >= 190, "200 codes produced only {} distinct ones", seen.len());
        assert!(!CODE_ALPHABET.contains(['0', '1', 'O', 'I']));
    }

    #[test]
    fn checksums_convert_to_base64() {
        assert_eq!(base64_sum("00ff10"), "AP8Q");
        assert_eq!(base64_sum("00ff"), "AP8=");
        assert_eq!(base64_sum("zz"), "");
    }
}
