//! One failure shape for everything: the registry's, object storage's, and
//! this client's own, flattened into the same map so every reader downstream
//! reads them the same way.

use serde_json::Value;
use std::collections::BTreeMap;

/// A failure as the wire carries it: an HTTP status (0 when nothing answered)
/// and the flattened body. The body is ordered by key because the Go client
/// kept it in a map, and every rendering of it is sorted.
#[derive(Debug, Clone, PartialEq)]
pub struct Error {
    pub status: u16,
    pub body: BTreeMap<String, Value>,
}

impl Error {
    /// The stable error identifier, e.g. checksum_mismatch; `http_<status>` when
    /// the body named none.
    pub fn code(&self) -> String {
        match self.body.get("error") {
            Some(Value::String(s)) if !s.is_empty() => s.clone(),
            _ => format!("http_{}", self.status),
        }
    }

    /// The human- and agent-readable remedy, when there is one.
    pub fn fix(&self) -> String {
        match self.body.get("fix") {
            Some(Value::String(s)) => s.clone(),
            _ => String::new(),
        }
    }

    /// An explicit verdict wins; absent one, 429 and 5xx are worth another
    /// attempt and nothing else is.
    pub fn retryable(&self) -> bool {
        match self.body.get("retryable") {
            Some(Value::Bool(b)) => *b,
            _ => self.status == 429 || self.status >= 500,
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.code())
    }
}

impl std::error::Error for Error {}

/// A client-side failure in the same shape as a server-side one.
pub fn fail(code: &str, fix: impl Into<String>) -> Error {
    let mut body = BTreeMap::new();
    body.insert("error".into(), Value::String(code.into()));
    body.insert("fix".into(), Value::String(fix.into()));
    body.insert("retryable".into(), Value::Bool(false));
    Error { status: 0, body }
}

/// The refusal a private upload with no key gets, raised before anything is
/// sent: a keyless upload lands in the one shared anonymous workspace, so there
/// is nothing for it to be private to. Shared by the CLI and the MCP server so
/// both refuse in the same words.
pub fn private_needs_key() -> Error {
    fail(
        "private_needs_key",
        "a private upload needs an API key: a keyless upload lands in the shared anonymous \
         workspace, which nobody is a member of, so there is nothing for it to be private to — \
         run `krowk login`, or push it public",
    )
}
