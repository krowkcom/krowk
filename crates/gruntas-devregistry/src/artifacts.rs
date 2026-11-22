//! Declaring an artifact, reading it back, listing a workspace's, finalizing
//! one and taking one down — plus the pagination and Idempotency-Key rules
//! every create and listing shares.

use crate::auth::{authenticate, free_plan, require_key};
use crate::encode::{Json, canonical};
use crate::errors::{
    already_finalized, bad_request, body_strings, error, invalid, key_reused, needs_key, not_found,
    parameter_missing, refuse_if_gone, refuse_visibility,
};
use crate::http::{Req, Resp};
use crate::json::{self, Member, Value};
use crate::store::{
    ANONYMOUS_WORKSPACE, ARTIFACT_REGION, App, Answered, Artifact, DECLARABLE_VISIBILITIES, DEFAULT_PAGE_SIZE,
    DEFAULT_VISIBILITY, EPHEMERAL_LIFETIME, MAX_METADATA_BYTES, MAX_PAGE_SIZE, SHARED_VISIBILITY, Store,
    UPLOAD_URL_LIFETIME, authorized_to_write, new_share_token, random_token, readable, rfc3339_nano,
    share_readable, sha256_hex,
};
use crate::view::{declared, fresh_upload, markdown, serialize_artifact, share_url, storage_key_for};

/// The declare parameters the API permits, which is what the registry digests:
/// a key it never reads must not make two requests different.
const ARTIFACT_PARAMS: [&str; 7] = ["byte_size", "checksum", "content_type", "filename", "metadata", "run", "visibility"];

/// How many rows a listing serves, clamped rather than refused. A number too
/// large to hold is still a number asked for, so it clamps like one; only
/// something that is not a number takes the default.
pub fn page_limit(req: &Req) -> usize {
    let raw = req.query_get("limit");
    let digits = raw.strip_prefix(['+', '-']).unwrap_or(&raw);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return DEFAULT_PAGE_SIZE as usize;
    }
    let n = raw.parse::<i64>().unwrap_or(if raw.starts_with('-') { i64::MIN } else { i64::MAX });
    n.clamp(1, MAX_PAGE_SIZE) as usize
}

/// One page of a newest-first listing, and the cursor for the next. `next` is
/// present whenever the page came back full — an exact multiple of the limit
/// costs one empty page rather than a count on every listing.
pub fn paginate<T>(mut rows: Vec<T>, limit: usize, slug: impl Fn(&T) -> &str) -> (Vec<T>, Json) {
    rows.truncate(limit);
    let next = if rows.len() < limit { Json::Null } else { Json::str(slug(rows.last().unwrap())) };
    (rows, next)
}

/// The Idempotency-Key a client named its attempt with. Sent empty is refused
/// rather than ignored: a retry that quietly stops deduplicating surfaces on a
/// bill, not as an error.
pub fn idempotency_key(req: &Req) -> Result<Option<String>, Resp> {
    let Some(key) = req.header("Idempotency-Key") else { return Ok(None) };
    if key.is_empty() {
        return Err(parameter_missing("Idempotency-Key"));
    }
    Ok(Some(key.to_owned()))
}

/// The artifact parameter carrying nothing the API reads — null, `{}`, or only
/// unread keys — which `params.expect` treats as the parameter being absent.
fn declared_blank(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::Obj(_) => ARTIFACT_PARAMS.iter().all(|n| v.get(n).is_none()),
        _ => false,
    }
}

/// The declared artifact reduced to the permitted parameters and canonicalized,
/// keys sorted at every level: re-serializing a body is the same request, and
/// an absent parameter differs from one sent empty.
fn declared_digest(v: &Value) -> String {
    let permitted = ARTIFACT_PARAMS
        .iter()
        .filter_map(|n| v.get(n))
        .map(|m| Member { key: m.key.clone(), value: m.value.clone(), raw: 0..0 })
        .collect();
    sha256_hex(canonical(&Value::Obj(permitted)).as_bytes())
}

/// Records the artifact and hands back where to put the bytes.
pub fn create(app: &App, req: &mut Req, site: &str) -> Resp {
    let mut workspace = match authenticate(req) {
        Ok(w) => w,
        Err(r) => return r,
    };
    let Ok(raw) = req.read_body(1 << 20) else { return parameter_missing("artifact") };
    let body = match json::parse_first(&raw) {
        Ok(v) => v,
        Err(e) if e.unreadable() => return bad_request(),
        Err(_) => return parameter_missing("artifact"),
    };
    let top = body.fields();
    let Some(range) = top.raw("artifact").filter(|_| !top.mismatched) else { return parameter_missing("artifact") };
    let artifact_raw = &raw[range];
    let decl = json::parse(artifact_raw).unwrap_or(Value::Null);
    let mut f = decl.fields();
    let (filename, content_type, byte_size) = (f.string("filename"), f.string("content_type"), f.int("byte_size"));
    let (checksum, run, visibility) = (f.string("checksum"), f.string("run"), f.string("visibility"));
    let metadata = f.raw("metadata").map(|r| artifact_raw[r].to_vec());
    if f.mismatched || declared_blank(&decl) {
        return parameter_missing("artifact");
    }

    let limit = app.limit_bytes;
    if filename.is_empty() {
        return invalid("filename", "can't be blank");
    } else if content_type.is_empty() {
        return invalid("content_type", "can't be blank");
    } else if byte_size <= 0 {
        return invalid("byte_size", "must be greater than 0");
    } else if byte_size > limit {
        return invalid("byte_size", &format!("must be at most {limit} bytes"));
    } else if metadata.as_ref().is_some_and(|m| m.len() > MAX_METADATA_BYTES) {
        return invalid("metadata", &format!("must be at most {MAX_METADATA_BYTES} bytes"));
    }

    let anonymous = workspace.is_empty();
    if anonymous {
        workspace = ANONYMOUS_WORKSPACE.to_owned();
    }
    // Blankness on the trimmed value, membership on the value as sent: the
    // registry reads it through `.presence`, so " private" is spelled wrong.
    // Refused rather than downgraded to public, which is what the client asked
    // not to get.
    let visibility = if visibility.trim().is_empty() { DEFAULT_VISIBILITY.to_owned() } else { visibility };
    if !DECLARABLE_VISIBILITIES.contains(&visibility.as_str()) {
        return refuse_visibility(&visibility, "declare");
    }
    if anonymous && visibility != DEFAULT_VISIBILITY {
        return needs_key(&visibility);
    }
    // Refused rather than ignored: a 201 for an upload not under the run asked
    // for looks like success and is not.
    if !run.is_empty() && anonymous {
        return error(422, "run_needs_key", "Attaching an artifact to a run needs an API key — a keyless upload has no workspace.", None);
    }

    let attempt = match idempotency_key(req) {
        Ok(a) => a,
        Err(r) => return r,
    };
    // A keyless caller has nothing to prove anything with, so its key is scoped
    // by the address it came from — which makes the key its credential.
    let scope = if anonymous { format!("ip:{}", req.remote) } else { workspace.clone() };
    let hash = declared_digest(&decl);

    let mut s = app.lock();
    if let Some(attempt) = &attempt
        && let Some(resp) = replay_declare(&mut s, &scope, attempt, &hash)
    {
        return resp;
    }
    if !run.is_empty() && s.runs.get(&run).is_none_or(|r| r.workspace != workspace) {
        return not_found();
    }

    let slug = crate::store::generate_slug("art");
    let key = storage_key_for(&visibility, ARTIFACT_REGION, &workspace, &slug, &filename);
    let file_url = format!("{site}/_storage/{key}");
    let url = format!("{site}/a/{slug}");
    let now = s.now();
    // A shared artifact's link is its share_url, minted fresh on every entry.
    let share_token = if visibility == SHARED_VISIBILITY { new_share_token() } else { String::new() };
    let card_link = if share_token.is_empty() { url.clone() } else { share_url(&url, &share_token) };

    s.created += 1;
    let mut a = Artifact {
        markdown: markdown(&filename, &content_type, &file_url, &card_link),
        slug: slug.clone(),
        state: "pending",
        filename,
        content_type,
        byte_size,
        checksum: checksum.to_lowercase(),
        region: ARTIFACT_REGION.to_owned(),
        visibility,
        run,
        url,
        file_url,
        expires_at: None,
        created_at: rfc3339_nano(now),
        metadata,
        workspace,
        claim_hash: String::new(),
        uploaded: false,
        stored_size: 0,
        stored_sum: String::new(),
        stored_width: 0,
        stored_height: 0,
        width: 0,
        height: 0,
        upload_tok: random_token(),
        upload_til: now + UPLOAD_URL_LIFETIME,
        claimed: false,
        deleted_at: None,
        storage_key: key,
        share_token,
        seq: s.created,
    };
    let mut claim_token = String::new();
    if anonymous {
        a.expires_at = Some(now + EPHEMERAL_LIFETIME);
        claim_token = format!("krowk_claim_{}", random_token());
        a.claim_hash = sha256_hex(claim_token.as_bytes());
    } else if free_plan(req) {
        // Expires like an anonymous upload, with no token: it has an owner, and
        // what lifts the expiry is an upgrade.
        a.expires_at = Some(now + EPHEMERAL_LIFETIME);
    }
    s.artifacts.insert(slug.clone(), a);
    if let Some(attempt) = &attempt {
        s.remember("artifact", &scope, attempt, Answered { request_hash: hash, artifact: slug.clone(), run: String::new() });
    }
    Resp::json(201, &declared(&s, &s.artifacts[&slug], &claim_token))
}

/// A retried declare, answered with what the first attempt made. The state is
/// asked about first: a key outlives its artifact's lifecycle, and a replay must
/// not mint a PUT over a taken-down or ready key. The claim token is not
/// re-issued — the row keeps only its digest.
fn replay_declare(s: &mut Store, scope: &str, attempt: &str, hash: &str) -> Option<Resp> {
    let (found, matches) = s.replay("artifact", scope, attempt, hash)?;
    let slug = found.artifact.clone();
    let a = s.artifacts.get(&slug)?;
    if !matches {
        return Some(key_reused(&a.slug));
    }
    if let Some(r) = refuse_if_gone(s, a) {
        return Some(r);
    }
    if a.state == "ready" {
        return Some(already_finalized(&a.slug));
    }
    let now = s.now();
    fresh_upload(s.artifacts.get_mut(&slug).unwrap(), now);
    Some(Resp::json(201, &declared(s, &s.artifacts[&slug], "")))
}

/// One page of a workspace's artifacts, newest first, "older than this one"
/// rather than an offset. Needs a key: keyless requests all share one
/// workspace. Tombstones answer for their own slug and nowhere else.
pub fn list(app: &App, req: &Req) -> Resp {
    let workspace = match require_key(req) {
        Ok(w) => w,
        Err(r) => return r,
    };
    let limit = page_limit(req);
    let s = app.lock();
    let mut owned: Vec<&Artifact> =
        s.artifacts.values().filter(|a| a.workspace == workspace && a.deleted_at.is_none()).collect();
    owned.sort_by_key(|x| std::cmp::Reverse(x.seq));
    let before = req.query_get("before");
    if !before.is_empty() {
        let Some(cursor) = s.find(&workspace, &before) else { return not_found() };
        owned.retain(|a| a.seq < cursor.seq);
    }
    let (owned, next) = paginate(owned, limit, |a| &a.slug);
    let page = owned.iter().map(|a| serialize_artifact(&s, a)).collect();
    Resp::json(200, &Json::map([("artifacts", Json::Arr(page)), ("next", next)]))
}

pub fn show(app: &App, req: &Req, slug: &str) -> Resp {
    let workspace = match authenticate(req) {
        Ok(w) => w,
        Err(r) => return r,
    };
    let s = app.lock();
    let share = req.query_get("share");
    let Some(a) = s.artifacts.get(slug).filter(|a| readable(a, &workspace) || share_readable(a, &share)) else {
        return not_found();
    };
    refuse_if_gone(&s, a).unwrap_or_else(|| Resp::json(200, &serialize_artifact(&s, a)))
}

/// The takedown: the bytes leave at once and a tombstone stays, so a read
/// afterwards is a 410. A keyless caller's authority is the claim token — a
/// slug travels in whatever the link was pasted into. Idempotent, and 204:
/// there is nothing left to serialize.
pub fn destroy(app: &App, req: &mut Req, slug: &str) -> Resp {
    let workspace = match authenticate(req) {
        Ok(w) => w,
        Err(r) => return r,
    };
    let [claim_token] = match body_strings(req, ["claim_token"]) {
        Ok(v) => v,
        Err(r) => return r,
    };
    if workspace.is_empty() && claim_token.is_empty() {
        return parameter_missing("claim_token");
    }
    let mut s = app.lock();
    if !authorized_to_write(s.artifacts.get(slug), &workspace, &claim_token) {
        return not_found();
    }
    let now = s.now();
    let a = s.artifacts.get_mut(slug).unwrap();
    if a.deleted_at.is_none() {
        let key = a.storage_key.clone();
        a.deleted_at = Some(now);
        // Spends any outstanding upload URL, so bytes cannot land under a
        // tombstone and resurrect the link.
        a.upload_tok.clear();
        s.objects.remove(&key);
    }
    Resp::empty(204)
}

/// Verifies what landed and marks the artifact ready. Idempotent, because
/// agents retry — but gone is checked first, since a taken-down artifact is
/// normally a ready one.
pub fn finalize(app: &App, req: &Req, slug: &str) -> Resp {
    let workspace = match authenticate(req) {
        Ok(w) => w,
        Err(r) => return r,
    };
    let mut s = app.lock();
    let Some(a) = s.find(&workspace, slug) else { return not_found() };
    if let Some(r) = refuse_if_gone(&s, a) {
        return r;
    }
    if a.state != "ready" {
        // 409: well formed, just early — uploading and retrying is the fix.
        if !a.uploaded {
            return error(409, "upload_missing", &format!("nothing uploaded for {} yet", a.slug), None);
        }
        if a.stored_size == 0 {
            return error(422, "empty_upload", &format!("what was uploaded for {} is empty", a.slug), None);
        }
        if !a.checksum.is_empty() && a.stored_sum != a.checksum {
            let message = format!("{} was declared as {} but storage holds {}", a.slug, a.checksum, a.stored_sum);
            return error(422, "checksum_mismatch", &message, None);
        }
        // The size becomes what storage holds, and the upload token is spent:
        // a ready artifact's bytes are immutable.
        let slug = a.slug.clone();
        let a = s.artifacts.get_mut(&slug).unwrap();
        a.state = "ready";
        a.byte_size = a.stored_size;
        (a.width, a.height) = (a.stored_width, a.stored_height);
        if a.checksum.is_empty() {
            a.checksum = a.stored_sum.clone();
        }
        a.upload_tok.clear();
    }
    Resp::json(200, &serialize_artifact(&s, &s.artifacts[slug]))
}
