//! An artifact and a run as the API reports them, and the strings derived from
//! an artifact: its storage key, its markdown, its paste block.

use crate::encode::Json;
use crate::store::{
    Artifact, DEFAULT_VISIBILITY, Run, SHARED_VISIBILITY, Store, UPLOAD_URL_LIFETIME, base64_sum, random_base36,
    rfc3339_nano,
};
use crate::{encode, json};

/// A measurement never made is null, not 0 — which would read as an image
/// zero pixels wide.
fn nullable(v: i64) -> Json {
    if v <= 0 { Json::Null } else { Json::Int(v) }
}

pub fn serialize_artifact(s: &Store, a: &Artifact) -> Json {
    Json::map([
        ("slug", Json::str(&a.slug)),
        ("state", Json::str(a.state)),
        ("filename", Json::str(&a.filename)),
        ("content_type", Json::str(&a.content_type)),
        ("byte_size", Json::Int(a.byte_size)),
        // Always present, null when unmeasured or unset, so a reader branches on
        // the value rather than on whether the key arrived.
        ("width", nullable(a.width)),
        ("height", nullable(a.height)),
        ("checksum", Json::str(&a.checksum)),
        ("region", Json::str(&a.region)),
        ("visibility", Json::str(&a.visibility)),
        ("run", serialize_artifact_run(s, a)),
        ("url", Json::str(&a.url)),
        ("file_url", Json::str(&a.file_url)),
        ("share_url", Json::opt(share_url_value(a))),
        ("markdown", Json::str(&a.markdown)),
        ("paste", paste_for(a)),
        ("expires_at", Json::opt(a.expires_at.map(rfc3339_nano))),
        ("created_at", Json::str(&a.created_at)),
        ("metadata", a.metadata.as_deref().map_or(Json::Null, |m| Json::Raw(encode::compact(m)))),
    ])
}

/// The run nested in an artifact, so a reader learns what produced it without
/// a second call. Null for none, and for a run the store no longer has.
fn serialize_artifact_run(s: &Store, a: &Artifact) -> Json {
    let Some(r) = s.runs.get(&a.run).filter(|_| !a.run.is_empty()) else {
        return Json::Null;
    };
    let metadata = if r.metadata.is_empty() { "{}".to_owned() } else { encode::compact(&r.metadata) };
    Json::map([
        ("slug", Json::str(&r.slug)),
        ("metadata", Json::Raw(metadata)),
        ("created_at", Json::str(&r.created_at)),
    ])
}

/// A run is a Go struct on the wire, so its fields go out in declaration order.
pub fn serialize_run(r: &Run) -> Json {
    Json::Obj(vec![
        ("slug".into(), Json::str(&r.slug)),
        ("status".into(), Json::str(r.status)),
        ("started_at".into(), Json::str(&r.started_at)),
        ("finished_at".into(), Json::opt(r.finished_at.clone())),
        ("metadata".into(), Json::Raw(encode::compact(&r.metadata))),
        ("created_at".into(), Json::str(&r.created_at)),
    ])
}

/// What a create answers with: the artifact, where to put its bytes, and what
/// to do next. A replay and a represign answer the same shape, with the upload
/// minted again — a stored signature is usually dead by then.
pub fn declared(s: &Store, a: &Artifact, claim_token: &str) -> Json {
    let Json::Obj(mut payload) = serialize_artifact(s, a) else { unreachable!() };
    let mut headers = vec![
        ("Content-Type".to_owned(), Json::str(&a.content_type)),
        ("Content-Length".to_owned(), Json::str(a.byte_size.to_string())),
    ];
    // The real presign signs the checksum as a header, and storage refuses the
    // PUT without it.
    if !a.checksum.is_empty() {
        headers.push(("x-amz-checksum-sha256".to_owned(), Json::str(base64_sum(&a.checksum))));
    }
    payload.push((
        "upload".into(),
        Json::map([
            ("method", Json::str("PUT")),
            ("url", Json::str(format!("{}?upload_token={}", a.file_url, a.upload_tok))),
            ("headers", Json::map_of(headers)),
            ("expires_at", Json::str(rfc3339_nano(a.upload_til))),
        ]),
    ));
    // Spelled out because the caller is often an agent following instructions.
    // Whether the recovery carries a claim token follows the claim hash, not a
    // token in hand: this is served again on every represign.
    let with_token = if a.claim_hash.is_empty() { "" } else { " with claim_token" };
    payload.push((
        "next_step".into(),
        Json::str(format!(
            "PUT the file to upload.url with the headers in upload.headers, then PUT /v1/artifacts/{0}/finalization. \
             If upload.url expires first, POST /v1/artifacts/{0}/upload{with_token} for a fresh one — the slug does not change",
            a.slug
        )),
    ));
    if !claim_token.is_empty() {
        payload.push(("claim_token".into(), Json::str(claim_token)));
    }
    Json::map_of(payload)
}

/// Keeps the upload window measured from `now`: a presign, a replay and a
/// declare all hand out a fresh one.
pub fn fresh_upload(a: &mut Artifact, now: jiff::Timestamp) {
    a.upload_tok = crate::store::random_token();
    a.upload_til = now + UPLOAD_URL_LIFETIME;
}

pub fn share_url(card_url: &str, token: &str) -> String {
    format!("{card_url}?share={token}")
}

/// Null unless shared; otherwise the card page carrying the token.
pub fn share_url_value(a: &Artifact) -> Option<String> {
    (a.visibility == SHARED_VISIBILITY && !a.share_token.is_empty()).then(|| share_url(&a.url, &a.share_token))
}

/// Which paste form each tool wants. Here and only here, because a tool
/// proving out is a registry deploy, never a client release.
const DESTINATION_CLASSES: [(&str, &str); 8] = [
    ("github", "markdown"),
    ("gitlab", "markdown"),
    ("linear", "markdown"),
    ("notion", "markdown"),
    ("slack", "url"),
    ("basecamp", "url"),
    ("asana", "url"),
    ("_default", "markdown"),
];

fn paste_for(a: &Artifact) -> Json {
    let destinations = DESTINATION_CLASSES.iter().map(|(k, v)| (k.to_string(), Json::str(*v))).collect();
    Json::map([
        ("markdown", Json::str(paste_block(a))),
        ("url", Json::str(share_url_value(a).unwrap_or_else(|| a.url.clone()))),
        ("destinations", Json::map_of(destinations)),
    ])
}

/// The krowk block: an image embeds its bytes and clicks through to the card;
/// anything else is the same line with the caption bolded. An unclaimed
/// artifact says when it goes.
fn paste_block(a: &Artifact) -> String {
    let caption = escape_label(&paste_caption(a));
    let image = a.content_type.starts_with("image/");
    let link = share_url_value(a).unwrap_or_else(|| a.url.clone());
    let label = if image { caption.clone() } else { format!("**{caption}**") };
    let mut parts = vec![label, format!("[View preview ↗]({link})")];
    if let Some(at) = a.expires_at {
        parts.push(format!("expires {}", at.strftime("%b %-d")));
    }
    let line = parts.join(" · ");
    if image { format!("[![{caption}]({})]({link})\n{line}", a.file_url) } else { line }
}

/// The caption recorded at push time, else the filename.
fn paste_caption(a: &Artifact) -> String {
    a.metadata
        .as_deref()
        .and_then(json::parse)
        .and_then(|m| match m.get("krowk.caption").map(|c| &c.value) {
            Some(json::Value::Str(c)) if !c.is_empty() => Some(c.clone()),
            _ => None,
        })
        .unwrap_or_else(|| a.filename.clone())
}

/// Ready to paste into a pull request: an image embeds and links through to
/// the card; anything else links to the card.
pub fn markdown(filename: &str, content_type: &str, file_url: &str, card_url: &str) -> String {
    let label = escape_label(filename);
    if content_type.starts_with("image/") {
        format!("[![{label}]({file_url})]({card_url})")
    } else {
        format!("[{label}]({card_url})")
    }
}

/// Escapes what would end or nest a CommonMark link label, and folds newlines,
/// which link text cannot span.
fn escape_label(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' | '[' | ']' => {
                out.push('\\');
                out.push(c);
            }
            '\n' | '\r' => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

/// The real registry's: keys are attacker-influenced, so "../../other" must not
/// land on another artifact's key.
fn safe_filename(filename: &str) -> String {
    let p = filename.replace('\\', "/");
    let trimmed = p.trim_end_matches('/');
    let base = match (p.is_empty(), trimmed.is_empty()) {
        (true, _) => ".",
        (false, true) => "/",
        _ => trimmed.rsplit('/').next().unwrap_or(trimmed),
    };
    let cleaned: String =
        base.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '-' }).collect();
    let cleaned = cleaned.trim_matches(|c| c == '-' || c == '.');
    if cleaned.is_empty() { "file".to_owned() } else { cleaned.to_owned() }
}

/// Where the bytes live, and the shape of the key is the whole of what makes a
/// non-public artifact private: a public key names the workspace and the
/// artifact, every other one replaces both with a single fresh secret.
pub fn storage_key_for(visibility: &str, region: &str, workspace: &str, slug: &str, filename: &str) -> String {
    if visibility == DEFAULT_VISIBILITY {
        format!("{region}/{workspace}/{slug}/{}", safe_filename(filename))
    } else {
        format!("{region}/{}/{}", random_base36(), safe_filename(filename))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filenames_are_made_safe_for_a_key() {
        assert_eq!(safe_filename("../../other"), "other");
        assert_eq!(safe_filename(r"C:\x\shot 1.png"), "shot-1.png");
        assert_eq!(safe_filename(".."), "file");
        assert_eq!(safe_filename(""), "file");
        assert_eq!(safe_filename("é.txt"), "txt");
    }

    #[test]
    fn labels_escape_what_would_break_a_link() {
        assert_eq!(markdown("frame[0].png", "image/png", "f", "c"), r"[![frame\[0\].png](f)](c)");
        assert_eq!(markdown("a\nb", "text/plain", "f", "c"), "[a b](c)");
    }
}
