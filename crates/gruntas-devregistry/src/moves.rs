//! What moves an artifact after it is declared: a claim into a workspace, a
//! run it belongs to, and who may read it.

use crate::auth::{free_plan, require_key};
use crate::errors::{body_strings, error, needs_key, not_found, parameter_missing, refuse_if_gone, refuse_visibility};
use crate::http::{Req, Resp};
use crate::store::{App, DECLARABLE_VISIBILITIES, EPHEMERAL_LIFETIME, SHARED_VISIBILITY, Store, new_share_token, sha256_hex};
use crate::view::{markdown, serialize_artifact, share_url, storage_key_for};

/// Moves an anonymous artifact into the key's workspace: a paid plan lifts the
/// expiry, a free one restamps a fresh 24 hours — a move, not a rescue.
///
/// Stricter than the registry on purpose: a retry must carry the artifact's own
/// token, so a garbage one cannot ride the retry-after-success affordance.
pub fn claim(app: &App, req: &mut Req, slug: &str) -> Resp {
    let workspace = match require_key(req) {
        Ok(w) => w,
        Err(r) => return r,
    };
    let [claim_token] = match body_strings(req, ["claim_token"]) {
        Ok(v) => v,
        Err(r) => return r,
    };
    if claim_token.is_empty() {
        return parameter_missing("claim_token");
    }
    let mut s = app.lock();
    let Some(a) = s.artifacts.get(slug) else { return not_found() };
    let matches = !a.claim_hash.is_empty() && a.claim_hash == sha256_hex(claim_token.as_bytes());
    // A retry after a successful claim is the same success — ahead of the gate
    // below, tombstone included, as the registry's unscoped find_by answers it.
    if matches && a.claimed && a.workspace == workspace {
        return Resp::json(200, &serialize_artifact(&s, a));
    }
    // Every other miss is one answer, a tombstone among them: the registry
    // reaches this through a `live` scope, so a taken-down artifact is not found.
    if !matches || a.claimed || a.deleted_at.is_some() {
        return not_found();
    }
    if let Some(r) = refuse_if_gone(&s, a) {
        return r;
    }
    let expires = free_plan(req).then(|| s.now() + EPHEMERAL_LIFETIME);
    let a = s.artifacts.get_mut(slug).unwrap();
    a.workspace = workspace;
    a.expires_at = expires;
    a.claimed = true;
    Resp::json(200, &serialize_artifact(&s, &s.artifacts[slug]))
}

/// Puts an artifact under a run after the fact — how an upload that was
/// anonymous at create time ever gets one. A plain 401 without a key, as the
/// registry answers here. A finished run still accepts one: the CI run that
/// closed before anyone claimed its upload is the case this exists for.
pub fn attach_run(app: &App, req: &mut Req, slug: &str) -> Resp {
    let workspace = match require_key(req) {
        Ok(w) => w,
        Err(r) => return r,
    };
    let [run] = match body_strings(req, ["run"]) {
        Ok(v) => v,
        Err(r) => return r,
    };
    if run.is_empty() {
        return parameter_missing("run");
    }
    let mut s = app.lock();
    let Some(a) = s.find(&workspace, slug) else { return not_found() };
    if let Some(r) = refuse_if_gone(&s, a) {
        return r;
    }
    if s.find_run(&workspace, &run).is_none() {
        return not_found();
    }
    // Set, not appended to: one run per artifact, so a retry is a success.
    s.artifacts.get_mut(slug).unwrap().run = run;
    Resp::json(200, &serialize_artifact(&s, &s.artifacts[slug]))
}

/// Changes who may read an artifact and moves its bytes to a key drawn for
/// where they are going. A key is required: changing visibility withdraws a
/// URL. Naming the visibility it already has is a success and no re-key.
///
/// One answer is deliberately not the registry's: a tombstone told to keep its
/// visibility is a 410 here, where the registry's early return answers 200 with
/// a payload naming bytes the takedown deleted.
pub fn update_visibility(app: &App, req: &mut Req, slug: &str, site: &str) -> Resp {
    let [asked] = match body_strings(req, ["visibility"]) {
        Ok(v) => v,
        Err(r) => return r,
    };
    // A keyless shared is told what fixes it before the 401 would answer. Only
    // a missing header — a malformed one is a credential bug.
    if req.header("Authorization").is_none_or(str::is_empty) && asked == SHARED_VISIBILITY {
        return needs_key(SHARED_VISIBILITY);
    }
    let workspace = match require_key(req) {
        Ok(w) => w,
        Err(r) => return r,
    };
    let mut s = app.lock();
    // The scope answers before the body, as the registry evaluates the receiver
    // first — and a complaint about the body would confirm the slug resolved.
    let Some(a) = s.artifacts.get(slug).filter(|a| a.workspace == workspace) else { return not_found() };
    if asked.trim().is_empty() {
        return parameter_missing("visibility");
    }
    if !DECLARABLE_VISIBILITIES.contains(&asked.as_str()) {
        return refuse_visibility(&asked, "set");
    }
    if let Some(r) = refuse_if_gone(&s, a) {
        return r;
    }
    if a.visibility != asked {
        // A pending artifact has no confirmed bytes to copy to a new key.
        if a.state != "ready" {
            return error(422, "immovable", &format!("{} has no confirmed bytes to move", a.slug), None);
        }
        rekey(&mut s, slug, &asked, site);
    }
    Resp::json(200, &serialize_artifact(&s, &s.artifacts[slug]))
}

/// What a visibility change does: the bytes move to a key drawn for the
/// visibility they arrive at, and the key they left is emptied — that is what
/// revocation is. A private key is fresh every time; a public one is derived,
/// so a round trip puts the bytes back at the URL privatizing took away.
fn rekey(s: &mut Store, slug: &str, to: &str, site: &str) {
    let a = s.artifacts.get_mut(slug).unwrap();
    let key = storage_key_for(to, &a.region, &a.workspace, &a.slug, &a.filename);
    let old = std::mem::replace(&mut a.storage_key, key.clone());
    // Entering shared mints a fresh token, revoking the last share_url; leaving
    // clears it.
    a.share_token = if to == SHARED_VISIBILITY { new_share_token() } else { String::new() };
    let card_link = if a.share_token.is_empty() { a.url.clone() } else { share_url(&a.url, &a.share_token) };
    a.visibility = to.to_owned();
    a.file_url = format!("{site}/_storage/{key}");
    a.markdown = markdown(&a.filename, &a.content_type, &a.file_url, &card_link);
    if let Some(bytes) = s.objects.remove(&old) {
        s.objects.insert(key, bytes);
    }
}
