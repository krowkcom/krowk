//! The one error envelope the API answers in, so a client branches on
//! `error.code` rather than on prose — and the refusals more than one
//! endpoint makes.

use crate::encode::Json;
use crate::http::{Req, Resp};
use crate::json::{self, ParseError, Value};
use crate::store::{Artifact, DECLARABLE_VISIBILITIES, Store, rfc3339_nano};

/// `details` is present whenever it is given, empty included: the taken-down
/// 410 sends an empty one on purpose.
pub fn error(status: u16, code: &str, message: &str, details: Option<Json>) -> Resp {
    let mut e = vec![("code", Json::str(code)), ("message", Json::str(message))];
    if let Some(d) = details {
        e.push(("details", d));
    }
    let e = Json::map_of(e.into_iter().map(|(k, v)| (k.to_owned(), v)).collect());
    Resp::json(status, &Json::map([("error", e)]))
}

/// A validation failure's details: the field and what is wrong with it.
pub fn invalid(field: &str, problem: &str) -> Resp {
    let mut message = field.replace('_', " ");
    message[..1].make_ascii_uppercase();
    let details = Json::map_of(vec![(field.to_owned(), Json::Arr(vec![Json::str(problem)]))]);
    error(422, "invalid", &format!("{message} {problem}"), Some(details))
}

pub fn not_found() -> Resp {
    error(404, "not_found", "No such record.", None)
}

pub fn unauthorized() -> Resp {
    error(401, "unauthorized", "Provide a valid API key as `Authorization: Bearer krowk_sk_...`.", None)
}

pub fn parameter_missing(name: &str) -> Resp {
    error(400, "parameter_missing", &format!("Missing required parameter: {name}."), None)
}

pub fn already_finalized(slug: &str) -> Resp {
    error(409, "already_finalized", &format!("{slug} is already finalized — declare a new artifact for new bytes"), None)
}

/// The one refusal an Idempotency-Key adds: 409, since what refuses it is a
/// record that already exists rather than anything wrong with the payload.
pub fn key_reused(slug: &str) -> Resp {
    error(
        409,
        "idempotency_key_reused",
        &format!("this Idempotency-Key already created {slug} from a different request — send a new key to create something else"),
        None,
    )
}

/// The shared_needs_key / private_needs_key refusal: a keyless upload lands in
/// the anonymous workspace, which nobody is a member of.
pub fn needs_key(visibility: &str) -> Resp {
    let (code, what) =
        if visibility == "shared" { ("shared_needs_key", "shared") } else { ("private_needs_key", "private") };
    error(
        422,
        code,
        &format!(
            "A {what} artifact needs an API key — a keyless upload lands in the shared anonymous workspace, which \
             nobody is a member of. Send Authorization: Bearer <key>, or declare it public."
        ),
        None,
    )
}

/// A word this API does not take as a visibility, echoed back truncated to the
/// registry's 30 characters (ellipsis included) on a character boundary.
pub fn refuse_visibility(asked: &str, verb: &str) -> Resp {
    let chars: Vec<char> = asked.chars().collect();
    let asked = if chars.len() > 30 { chars[..27].iter().collect::<String>() + "..." } else { asked.to_owned() };
    error(
        422,
        "visibility_unavailable",
        &format!("{} is not a visibility you can {verb}. Send one of: {}.", go_quote(&asked), DECLARABLE_VISIBILITIES.join(", ")),
        None,
    )
}

/// Go's `%q`, for the characters a visibility someone typed might hold.
fn go_quote(s: &str) -> String {
    let mut out = String::from('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{7}' => out.push_str("\\a"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{b}' => out.push_str("\\v"),
            '\u{0}'..='\u{1f}' | '\u{7f}' => out.push_str(&format!("\\x{:02x}", c as u32)),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// An artifact the API will not act on any further. Takedown first: an
/// artifact can be both, and the one somebody decided is the truer answer. A
/// takedown names nothing — echoing the filename would undo half of it.
pub fn refuse_if_gone(s: &Store, a: &Artifact) -> Option<Resp> {
    if a.deleted_at.is_some() {
        return Some(error(410, "taken_down", &format!("{} was taken down", a.slug), Some(Json::Obj(vec![]))));
    }
    if s.expired(a) {
        let at = a.expires_at.map(rfc3339_nano).unwrap_or_default();
        let details = Json::map([("filename", Json::str(&a.filename)), ("created_at", Json::str(&a.created_at))]);
        return Some(error(410, "expired", &format!("{} expired at {at}", a.slug), Some(details)));
    }
    None
}

/// A body as `json.NewDecoder(io.LimitReader(body, limit)).Decode` leaves it.
pub enum Decoded {
    Value(Value),
    /// Nothing was sent: `io.EOF`.
    Empty,
    /// The body could not be read at all.
    Failed,
}

/// Decodes the body, answering the one failure every endpoint answers alike: a
/// body that is not JSON is `bad_request`, since it has no parameter to name.
pub fn decode(req: &mut Req, limit: u64) -> Result<Decoded, Resp> {
    let Ok(raw) = req.read_body(limit) else { return Ok(Decoded::Failed) };
    match json::parse_first(&raw) {
        Ok(v) => Ok(Decoded::Value(v)),
        Err(ParseError::Empty) => Ok(Decoded::Empty),
        Err(_) => Err(bad_request()),
    }
}

pub fn bad_request() -> Resp {
    error(400, "bad_request", "The request body is not valid JSON.", None)
}

/// The string fields of a small body, as the endpoints that take one decode it:
/// an unreadable body is refused, anything else yields what it can.
pub fn body_strings<const N: usize>(req: &mut Req, names: [&str; N]) -> Result<[String; N], Resp> {
    let value = match decode(req, 1 << 16)? {
        Decoded::Value(v) => v,
        _ => Value::Null,
    };
    let mut fields = value.fields();
    Ok(names.map(|n| fields.string(n)))
}
