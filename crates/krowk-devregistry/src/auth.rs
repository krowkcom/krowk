//! Keys, the key self-check, and the browser login: `/v1/key`, the CLI
//! authorizations, and the `/_approve` pages that stand in for the real
//! approval screen.

use crate::encode::Json;
use crate::errors::{error, unauthorized};
use crate::http::{Req, Resp};
use crate::page::{escape, page};
use crate::store::{
    App, Authorization, CLI_AUTHORIZATION_GRACE, CLI_AUTHORIZATION_INTERVAL, CLI_AUTHORIZATION_LIFETIME,
    FREE_PLAN_KEY_MARKER, generate_code, person_for, generate_slug, random_token, rfc3339, sha256_hex, workspace_for,
};
use std::sync::Arc;

const PENDING: &str = "pending";
const APPROVED: &str = "approved";
const DENIED: &str = "denied";

fn bearer(header: &str) -> Option<String> {
    let b = header.as_bytes();
    if b.len() <= 7 || !b[..7].eq_ignore_ascii_case(b"bearer ") {
        return None;
    }
    let token = String::from_utf8_lossy(&b[7..]).trim().to_owned();
    (!token.is_empty()).then_some(token)
}

/// The workspace a request acts in, empty for a keyless one. A malformed
/// Authorization header is a 401 rather than a fall back to anonymous, which
/// would hand an ephemeral artifact to a client that asked for an owned one.
pub fn authenticate(req: &Req) -> Result<String, Resp> {
    match req.header("Authorization") {
        None | Some("") => Ok(String::new()),
        Some(h) => bearer(h).map(|t| workspace_for(&t)).ok_or_else(unauthorized),
    }
}

/// [`authenticate`] for the endpoints that cannot work without a key.
pub fn require_key(req: &Req) -> Result<String, Resp> {
    req.header("Authorization").and_then(bearer).map(|t| workspace_for(&t)).ok_or_else(unauthorized)
}

/// The request's bearer token, if it sent one that parses.
pub fn token(req: &Req) -> Option<String> {
    req.header("Authorization").and_then(bearer)
}

/// Whether the request's key belongs to a free workspace.
pub fn free_plan(req: &Req) -> bool {
    req.header("Authorization").and_then(bearer).is_some_and(|t| t.contains(FREE_PLAN_KEY_MARKER))
}

/// The key self-check. Any bearer token resolves to the workspace the
/// artifact endpoints derive, so `auth verify` and a push agree.
pub fn show_key(req: &Req) -> Resp {
    let Some(token) = req.header("Authorization").and_then(bearer) else {
        return unauthorized();
    };
    Resp::json(
        200,
        &Json::map([
            // Derived, never the token itself: a key ID ends up in logs.
            ("key_id", Json::str(format!("key_{}", &sha256_hex(token.as_bytes())[..8]))),
            ("name", Json::str("local")),
            ("workspace", Json::str(workspace_for(&token))),
            ("workspace_name", Json::str("Local workspace")),
            // The person the key speaks for, which a pairing binds (devices.md
            // → Adding a device). The registry's KeySerializer has no such
            // field yet; this is the stand-in's guess at it.
            ("user_id", Json::str(person_for(&token))),
            // Their email, which the new machine names them by; the stand-in
            // has none, so it makes one of the person's id. Also a guess at a
            // field KeySerializer lacks.
            ("email", Json::str(format!("{}@example.test", &person_for(&token)[..8]))),
        ]),
    )
}

/// Opens a browser login and answers both halves. No Idempotency-Key: a lost
/// response means nobody saw the code, so the login charges nothing and lapses.
pub fn create_cli_authorization(app: &App, req: &Req, site: &str) -> Resp {
    let fresh = matches!(req.query_get("fresh").as_str(), "true" | "1");
    let mut s = app.lock();
    // Swept here, the only moment the set grows, and only past a grace period
    // so a late poll still hears `410 expired` rather than `404`.
    let at = s.now();
    s.authorizations.retain(|_, a| at <= a.created_at + CLI_AUTHORIZATION_LIFETIME + CLI_AUTHORIZATION_GRACE);
    let code = loop {
        let code = generate_code();
        if !s.authorizations.values().any(|a| a.code == code) {
            break code;
        }
    };
    let auth = Authorization {
        slug: generate_slug("aut"),
        code,
        state: PENDING,
        created_at: at,
        token: String::new(),
        key_id: String::new(),
        workspace: String::new(),
        spent: false,
        fresh,
    };
    let body = Json::map([
        ("slug", Json::str(&auth.slug)),
        ("state", Json::str(auth.state)),
        ("code", Json::str(&auth.code)),
        // Only the code travels in the URL; the slug collects the key. The
        // alphabet needs no escaping.
        ("verification_url", Json::str(format!("{site}/_approve/cli/authorizations/new?code={}", auth.code))),
        ("interval", Json::Int(CLI_AUTHORIZATION_INTERVAL)),
        ("expires_at", Json::str(rfc3339(auth.created_at + CLI_AUTHORIZATION_LIFETIME))),
    ]);
    s.authorizations.insert(auth.slug.clone(), auth);
    Resp::json(201, &body)
}

/// The CLI's poll, and the only way the key is handed over — once. Spent is
/// checked before expiry, because a key collected and then lapsed is still on
/// somebody's disk; expiry before state, so a late approval reads as lapsed.
///
/// The token is taken out under the lock, and put back if the response could
/// not be written. Best-effort, as it is in Go and in the real registry: a
/// write handed to the kernel counts as delivered.
pub fn show_cli_authorization(app: &Arc<App>, slug: &str) -> Resp {
    let mut s = app.lock();
    let expired = match s.authorizations.get(slug) {
        None => return error(404, "not_found", "No such authorization.", None),
        Some(a) if a.spent => {
            return error(410, "spent", "This authorization's key has already been collected, and no copy was kept.", None);
        }
        Some(a) => s.authorization_expired(a),
    };
    if expired {
        return error(410, "expired", "This authorization has expired.", None);
    }
    let a = s.authorizations.get_mut(slug).unwrap();
    let mut body = vec![
        ("slug", Json::str(&a.slug)),
        ("state", Json::str(a.state)),
        ("expires_at", Json::str(rfc3339(a.created_at + CLI_AUTHORIZATION_LIFETIME))),
    ];
    let delivering = a.state == APPROVED;
    if delivering {
        body.push(("token", Json::str(&a.token)));
        body.push(("key_id", Json::str(&a.key_id)));
        body.push(("workspace", Json::str(&a.workspace)));
        body.push(("workspace_name", Json::str("Local workspace")));
    }
    let mut resp = Resp::json(200, &Json::map_of(body.into_iter().map(|(k, v)| (k.to_owned(), v)).collect()));
    if delivering {
        let token = std::mem::take(&mut a.token);
        a.spent = true;
        let (app, slug) = (Arc::clone(app), slug.to_owned());
        resp.undo = Some(Box::new(move || {
            if let Some(a) = app.lock().authorizations.get_mut(&slug) {
                a.token = token;
                a.spent = false;
            }
        }));
    }
    resp
}

fn pending_code(app: &App, code: &str) -> Option<String> {
    let s = app.lock();
    let a = s.authorizations.values().find(|a| !code.is_empty() && a.code == code)?;
    (a.state == PENDING && !s.authorization_expired(a)).then(|| a.code.clone())
}

/// Where a person confirms the code. A code matching no waiting login is
/// refused outright rather than shown with buttons that would 404 once pressed.
pub fn cli_authorization_page(app: &App, req: &Req) -> Resp {
    let Some(code) = pending_code(app, &req.query_get("code")) else {
        return Resp::html(404, page("No such login", &[], "<p>No login is waiting on that code.</p>"));
    };
    let form = |action: &str, label: &str| {
        format!(
            "<form method=\"post\" action=\"/_approve/cli/authorizations/{code}/{action}\"><button type=\"submit\">{label}</button></form>"
        )
    };
    let body = format!(
        "<p>A terminal asked to sign in. Approve it only if this code is the one it printed.</p>\n<p><strong>{}</strong></p>\n{}\n{}",
        escape(&code),
        form("approval", "Approve"),
        form("denial", "Deny"),
    );
    Resp::html(200, page("Approve this login", &[], &body))
}

/// What the two buttons post to. Approving mints the key, derived from its own
/// token exactly as `/v1/key` derives one — and nothing here carries it: it
/// goes to whoever holds the slug.
pub fn decide_cli_authorization(app: &App, code: &str, approve: bool) -> Resp {
    let mut s = app.lock();
    let Some(slug) = s.authorizations.values().find(|a| !code.is_empty() && a.code == code).map(|a| a.slug.clone())
    else {
        return error(404, "not_found", "No such authorization.", None);
    };
    if s.authorization_expired(&s.authorizations[&slug]) {
        return error(410, "expired", "This authorization has expired.", None);
    }
    let now = s.now();
    let store = &mut *s;
    let a = store.authorizations.get_mut(&slug).unwrap();
    if a.state != PENDING {
        return error(409, "already_decided", "This authorization was already answered.", None);
    }
    let decision = if approve {
        a.state = APPROVED;
        a.token = format!("krowk_sk_{}", &random_token()[..32]);
        a.key_id = format!("key_{}", &sha256_hex(a.token.as_bytes())[..8]);
        a.workspace = workspace_for(&a.token);
        if a.fresh {
            store.sync.stamps.insert(sha256_hex(a.token.as_bytes()), now);
        }
        APPROVED
    } else {
        a.state = DENIED;
        DENIED
    };
    Resp::html(200, page(&format!("Login {decision}"), &[], &format!("<p>Login {decision}. Back to your terminal.</p>")))
}
