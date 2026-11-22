//! Runs: opening one, listing a workspace's, reading one, completing one, and
//! what one run produced.

use crate::artifacts::{idempotency_key, page_limit, paginate};
use crate::auth::require_key;
use crate::encode::Json;
use crate::errors::{bad_request, invalid, key_reused, not_found, parameter_missing};
use crate::json::{self, ParseError};
use crate::http::{Req, Resp};
use crate::store::{App, Answered, Artifact, MAX_METADATA_BYTES, Run, generate_slug, rfc3339_nano, sha256_hex};
use crate::view::{serialize_artifact, serialize_run};

pub fn create(app: &App, req: &mut Req) -> Resp {
    let workspace = match require_key(req) {
        Ok(w) => w,
        Err(r) => return r,
    };
    // A body that does not parse is refused exactly as a declare's is; an empty
    // one is fine, since a run needs no metadata.
    let Ok(raw) = req.read_body(1 << 20) else { return parameter_missing("run") };
    let metadata = match json::parse_first(&raw) {
        Err(ParseError::Empty) => b"{}".to_vec(),
        Err(_) => return bad_request(),
        Ok(v) => {
            let mut top = v.fields();
            let run = top.nested("run");
            if top.mismatched {
                return parameter_missing("run");
            }
            run.raw("metadata").map_or_else(|| b"{}".to_vec(), |r| raw[r].to_vec())
        }
    };
    if metadata.len() > MAX_METADATA_BYTES {
        return invalid("metadata", &format!("must be at most {MAX_METADATA_BYTES} bytes"));
    }
    let attempt = match idempotency_key(req) {
        Ok(a) => a,
        Err(r) => return r,
    };
    let hash = sha256_hex(&metadata);

    let mut s = app.lock();
    // A run has no upload and no tombstone, so a replay is simply the run the
    // first attempt opened, however far through its lifecycle it has gone.
    if let Some(attempt) = &attempt
        && let Some((found, matches)) = s.replay("run", &workspace, attempt, &hash)
        && let Some(opened) = s.runs.get(&found.run)
    {
        return if matches { Resp::json(201, &serialize_run(opened)) } else { key_reused(&opened.slug) };
    }
    let now = rfc3339_nano(s.now());
    s.runs_open += 1;
    let run = Run {
        slug: generate_slug("run"),
        status: "open",
        started_at: now.clone(),
        finished_at: None,
        metadata,
        created_at: now,
        workspace: workspace.clone(),
        seq: s.runs_open,
    };
    let resp = Resp::json(201, &serialize_run(&run));
    if let Some(attempt) = &attempt {
        s.remember("run", &workspace, attempt, Answered { request_hash: hash, artifact: String::new(), run: run.slug.clone() });
    }
    s.runs.insert(run.slug.clone(), run);
    resp
}

/// One page of a workspace's runs, newest first.
pub fn list(app: &App, req: &Req) -> Resp {
    let workspace = match require_key(req) {
        Ok(w) => w,
        Err(r) => return r,
    };
    let limit = page_limit(req);
    let s = app.lock();
    let mut owned: Vec<&Run> = s.runs.values().filter(|r| r.workspace == workspace).collect();
    owned.sort_by_key(|x| std::cmp::Reverse(x.seq));
    let before = req.query_get("before");
    if !before.is_empty() {
        let Some(cursor) = s.find_run(&workspace, &before) else { return not_found() };
        owned.retain(|r| r.seq < cursor.seq);
    }
    let (owned, next) = paginate(owned, limit, |r| &r.slug);
    let page = owned.into_iter().map(serialize_run).collect();
    Resp::json(200, &Json::map([("runs", Json::Arr(page)), ("next", next)]))
}

pub fn show(app: &App, req: &Req, slug: &str) -> Resp {
    let workspace = match require_key(req) {
        Ok(w) => w,
        Err(r) => return r,
    };
    let s = app.lock();
    s.find_run(&workspace, slug).map_or_else(not_found, |r| Resp::json(200, &serialize_run(r)))
}

/// Idempotent: finishing a finished run keeps the moment it first finished.
pub fn finish(app: &App, req: &Req, slug: &str) -> Resp {
    let workspace = match require_key(req) {
        Ok(w) => w,
        Err(r) => return r,
    };
    let mut s = app.lock();
    if s.find_run(&workspace, slug).is_none() {
        return not_found();
    }
    let now = rfc3339_nano(s.now());
    let run = s.runs.get_mut(slug).unwrap();
    if run.status != "finished" {
        run.status = "finished";
        run.finished_at = Some(now);
    }
    Resp::json(200, &serialize_run(run))
}

/// What one run produced — a collection of the run rather than a filter, so an
/// unknown run is a 404 where a filter would answer an indistinguishable empty
/// page. The workspace is checked as well: this is the boundary between tenants,
/// and one that holds only by invariant fails silently when the invariant moves.
pub fn artifacts(app: &App, req: &Req, slug: &str) -> Resp {
    let workspace = match require_key(req) {
        Ok(w) => w,
        Err(r) => return r,
    };
    let limit = page_limit(req);
    let s = app.lock();
    if s.find_run(&workspace, slug).is_none() {
        return not_found();
    }
    let mut made: Vec<&Artifact> = s
        .artifacts
        .values()
        .filter(|a| a.run == slug && a.workspace == workspace && a.deleted_at.is_none())
        .collect();
    made.sort_by_key(|x| std::cmp::Reverse(x.seq));
    let before = req.query_get("before");
    if !before.is_empty() {
        // Looked up among the run's own, so a cursor from elsewhere is not one.
        let Some(cursor) = s.artifacts.get(&before).filter(|c| c.run == slug) else { return not_found() };
        made.retain(|a| a.seq < cursor.seq);
    }
    let (made, next) = paginate(made, limit, |a| &a.slug);
    let page = made.iter().map(|a| serialize_artifact(&s, a)).collect();
    Resp::json(200, &Json::map([("artifacts", Json::Arr(page)), ("next", next)]))
}
