//! The upload: handing a presigned URL out again, and `/_storage`, which
//! stands in for R2. The one thing faked is signing — the URL carries an
//! opaque token — while everything a client has to get right is checked: the
//! token, the window, the headers, the length and the digest.

use crate::auth::authenticate;
use crate::errors::{already_finalized, body_strings, not_found, parameter_missing, refuse_if_gone};
use crate::http::{Req, Resp};
use crate::image::image_size;
use crate::store::{App, authorized_to_write, base64_sum, sha256_hex};
use crate::view::{declared, fresh_upload};

/// Mints the upload of an artifact again: same slug, same key, same declared
/// size, so a lapsed signature is recoverable without a second slug. A keyless
/// caller's authority is the claim token — a presigned PUT decides what the
/// bytes are, and the slug is in whatever the link was pasted into.
///
/// Nothing here touches created_at: asking for a URL is no evidence the upload
/// is coming, and a deadline a client can push out by asking is none.
pub fn presign(app: &App, req: &mut Req, slug: &str) -> Resp {
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
    // Missing, another workspace's, and a wrong token are one answer: a wrong
    // guess learns nothing from the difference.
    if !authorized_to_write(s.artifacts.get(slug), &workspace, &claim_token) {
        return not_found();
    }
    let a = &s.artifacts[slug];
    if let Some(r) = refuse_if_gone(&s, a) {
        return r;
    }
    // A ready artifact is a permalink: a URL over its key would be permission
    // to swap what a link resolves to.
    if a.state == "ready" {
        return already_finalized(&a.slug);
    }
    let now = s.now();
    fresh_upload(s.artifacts.get_mut(slug).unwrap(), now);
    Resp::json(200, &declared(&s, &s.artifacts[slug], ""))
}

/// Object storage's PUT, enforcing what a real signature does. The artifact is
/// looked up *by* the key, so bytes can land only where it says they live.
pub fn put_object(app: &App, req: &mut Req, key: &str) -> Resp {
    // Copied under the lock, then the body is read outside it.
    let found = {
        let s = app.lock();
        s.by_storage_key(key).map(|a| {
            (a.slug.clone(), a.upload_tok.clone(), a.content_type.clone(), a.checksum.clone(), a.byte_size, a.upload_til)
        })
    };
    if found.is_none()
        && let Some(r) = crate::sync::put_chunk_object(app, req, key)
    {
        return r;
    }
    let Some((slug, token, want_type, want_sum, want_size, until)) =
        found.filter(|f| !f.1.is_empty() && req.query_get("upload_token") == f.1)
    else {
        // A ready artifact's token is spent, so a re-PUT of a permalink lands here.
        return Resp::xml(403, "SignatureDoesNotMatch");
    };
    if app.lock().now() > until {
        return Resp::xml(403, "AccessDenied");
    }
    if req.header("Content-Type").unwrap_or("") != want_type {
        return Resp::xml(403, "SignatureDoesNotMatch");
    }
    // The digest is signed as a header, so storage refuses before reading the
    // body when it is missing or altered.
    if !want_sum.is_empty() && req.header("x-amz-checksum-sha256").unwrap_or("") != base64_sum(&want_sum) {
        return Resp::xml(403, "SignatureDoesNotMatch");
    }
    let Ok(bytes) = req.read_body(want_size as u64 + 1) else {
        return Resp::xml(400, "IncompleteBody");
    };
    if bytes.len() as i64 != want_size {
        return Resp::xml(400, "IncorrectContentLength");
    }
    let sum = sha256_hex(&bytes);
    if !want_sum.is_empty() && sum != want_sum {
        return Resp::xml(400, "BadDigest");
    }

    let mut s = app.lock();
    // A finalize may have landed while the body was read.
    let Some(a) = s.artifacts.get_mut(&slug).filter(|a| a.upload_tok == token) else {
        return Resp::xml(403, "SignatureDoesNotMatch");
    };
    a.uploaded = true;
    a.stored_size = bytes.len() as i64;
    a.stored_sum = sum;
    (a.stored_width, a.stored_height) = image_size(&a.content_type, &bytes);
    s.objects.insert(key.to_owned(), bytes);
    Resp::empty(200)
}

/// Object storage's GET — the CDN, so a link handed out resolves. Keyless at
/// every visibility: the unguessable key is the whole authorization. An
/// expired artifact's bytes stop serving, as the lifecycle rule deletes them.
pub fn get_object(app: &App, key: &str) -> Resp {
    let s = app.lock();
    let mut bytes = s.objects.get(key);
    let mut content_type = "";
    if let Some(a) = s.by_storage_key(key) {
        if s.expired(a) {
            bytes = None;
        }
        content_type = &a.content_type;
    }
    let Some(bytes) = bytes else { return Resp::xml(404, "NoSuchKey") };
    // The type the object was stored with: og:image is only an image to an
    // unfurler if this says so.
    let mut resp = Resp::empty(200);
    resp.body = bytes.clone();
    if !content_type.is_empty() {
        resp.headers.push(("Content-Type", content_type.to_owned()));
    }
    resp
}
