//! The registry's records as this client reads and writes them. Field order
//! and emptiness rules are the wire's, so a record read back renders the way
//! the Go client rendered it: fields in this order, empty strings and absent
//! values left out.

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub const VISIBILITY_PUBLIC: &str = "public";
pub const VISIBILITY_PRIVATE: &str = "private";
pub const VISIBILITY_SHARED: &str = "shared";

fn empty(s: &str) -> bool {
    s.is_empty()
}

/// A field whose JSON null reads as its empty value, as Go decodes it. The
/// registry sends `"finished_at": null` for a run still open.
fn nullable<'de, D: Deserializer<'de>, T: Default + Deserialize<'de>>(d: D) -> Result<T, D::Error> {
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

/// A field carried verbatim, like Go's json.RawMessage: absent is None, and a
/// JSON null that arrived is kept as null rather than read as absent.
pub mod raw {
    use super::*;
    pub fn keep<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Value>, D::Error> {
        Value::deserialize(d).map(Some)
    }
}

/// Where the bytes go, signed for one specific body.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Upload {
    #[serde(default, deserialize_with = "nullable")]
    pub method: String,
    #[serde(default, deserialize_with = "nullable")]
    pub url: String,
    #[serde(default, deserialize_with = "nullable")]
    pub headers: Option<BTreeMap<String, String>>,
    #[serde(default, deserialize_with = "nullable")]
    pub expires_at: String,
}

/// The run an artifact belongs to, as the artifact reports it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ArtifactRun {
    #[serde(default, deserialize_with = "nullable")]
    pub slug: String,
    #[serde(default, deserialize_with = "raw::keep", skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    #[serde(default, deserialize_with = "nullable", skip_serializing_if = "empty")]
    pub created_at: String,
}

/// One stored file, as the registry reports it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Artifact {
    #[serde(default, deserialize_with = "nullable")]
    pub slug: String,
    #[serde(default, deserialize_with = "nullable")]
    pub state: String,
    #[serde(default, deserialize_with = "nullable")]
    pub filename: String,
    #[serde(default, deserialize_with = "nullable")]
    pub content_type: String,
    #[serde(default, deserialize_with = "nullable")]
    pub byte_size: i64,
    #[serde(default, deserialize_with = "nullable", skip_serializing_if = "empty")]
    pub checksum: String,
    #[serde(default, deserialize_with = "nullable", skip_serializing_if = "empty")]
    pub region: String,
    #[serde(default, deserialize_with = "nullable", skip_serializing_if = "Option::is_none")]
    pub run: Option<ArtifactRun>,
    /// Who may read this artifact. Empty where a registry predating the field
    /// answered, which `public()` reads as the behaviour every artifact had then.
    #[serde(default, deserialize_with = "nullable", skip_serializing_if = "empty")]
    pub visibility: String,
    /// The card page: the link to paste.
    #[serde(default, deserialize_with = "nullable")]
    pub url: String,
    /// The byte URL on the CDN. Only an image embed is built from it.
    #[serde(default, deserialize_with = "nullable", skip_serializing_if = "empty")]
    pub file_url: String,
    #[serde(default, deserialize_with = "nullable", skip_serializing_if = "empty")]
    pub markdown: String,
    #[serde(default, skip_serializing_if = "empty", deserialize_with = "nullable")]
    pub share_url: String,
    #[serde(default, deserialize_with = "nullable", skip_serializing_if = "Option::is_none")]
    pub paste: Option<Paste>,
    #[serde(default, deserialize_with = "nullable", skip_serializing_if = "empty")]
    pub expires_at: String,
    #[serde(default, deserialize_with = "nullable", skip_serializing_if = "empty")]
    pub created_at: String,
    /// The artifact's own production record. Public: the card page is keyless.
    #[serde(default, deserialize_with = "raw::keep", skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    /// Only on the create response.
    #[serde(default, deserialize_with = "nullable", skip_serializing_if = "Option::is_none")]
    pub upload: Option<Upload>,
    #[serde(default, deserialize_with = "nullable", skip_serializing_if = "empty")]
    pub next_step: String,
    /// Shown exactly once, by the call that created an anonymous artifact.
    #[serde(default, deserialize_with = "nullable", skip_serializing_if = "empty")]
    pub claim_token: String,
}


impl Artifact {
    /// Whether this is the open kind. An unset visibility — an older registry —
    /// reads as public here and nowhere else.
    pub fn public(&self) -> bool {
        self.visibility.is_empty() || self.visibility == VISIBILITY_PUBLIC
    }

    /// The one non-public visibility this build knows how to describe. Not
    /// `!public()`: a build that has not heard of a visibility must not call it
    /// workspace-only.
    pub fn private(&self) -> bool {
        self.visibility == VISIBILITY_PRIVATE
    }

    pub fn shared(&self) -> bool {
        self.visibility == VISIBILITY_SHARED
    }

    /// The run this artifact belongs to, or "" for none.
    pub fn run_slug(&self) -> &str {
        self.run.as_ref().map_or("", |r| r.slug.as_str())
    }
}

/// An artifact in the forms its destinations need, computed by the registry.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Paste {
    #[serde(default, deserialize_with = "nullable")]
    pub markdown: String,
    #[serde(default, deserialize_with = "nullable")]
    pub url: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty", deserialize_with = "nullable")]
    pub destinations: BTreeMap<String, String>,
}


impl Paste {
    /// The form this destination wants, "" when nothing was served to answer
    /// with; an unnamed destination gets `_default`.
    pub fn form_for(&self, destination: &str) -> &str {
        if self.destinations.is_empty() {
            return "";
        }
        self.destinations
            .get(&destination.to_lowercase())
            .or_else(|| self.destinations.get("_default"))
            .map_or("", String::as_str)
    }
}

/// The artifacts one agent run produced, and the facts about the work.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Run {
    #[serde(default, deserialize_with = "nullable")]
    pub slug: String,
    #[serde(default, deserialize_with = "nullable")]
    pub status: String,
    #[serde(default, deserialize_with = "nullable", skip_serializing_if = "empty")]
    pub started_at: String,
    #[serde(default, deserialize_with = "nullable", skip_serializing_if = "empty")]
    pub finished_at: String,
    #[serde(default, deserialize_with = "raw::keep", skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

/// The key a request is made with, as the registry reports it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Key {
    #[serde(default, deserialize_with = "nullable", skip_serializing_if = "empty")]
    pub key_id: String,
    #[serde(default, deserialize_with = "nullable", skip_serializing_if = "empty")]
    pub name: String,
    #[serde(default, deserialize_with = "nullable", skip_serializing_if = "empty")]
    pub workspace: String,
    #[serde(default, deserialize_with = "nullable", skip_serializing_if = "empty")]
    pub workspace_name: String,
    #[serde(default, deserialize_with = "nullable", skip_serializing_if = "empty")]
    pub expires_at: String,
    #[serde(default, deserialize_with = "nullable", skip_serializing_if = "empty")]
    pub last_used_at: String,
    #[serde(default, deserialize_with = "nullable", skip_serializing_if = "empty")]
    pub created_at: String,
    /// The HTTP status the read answered with. Transport detail, never rendered.
    #[serde(skip)]
    pub status: u16,
}

pub const AUTHORIZATION_PENDING: &str = "pending";
pub const AUTHORIZATION_APPROVED: &str = "approved";
pub const AUTHORIZATION_DENIED: &str = "denied";

/// One browser login in progress. The slug collects the key and never appears
/// in a browser; the code is what a person reads and can only approve or deny.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct CliAuthorization {
    #[serde(default, deserialize_with = "nullable")]
    pub slug: String,
    #[serde(default, deserialize_with = "nullable")]
    pub state: String,
    #[serde(default, deserialize_with = "nullable", skip_serializing_if = "empty")]
    pub code: String,
    #[serde(default, deserialize_with = "nullable", skip_serializing_if = "empty")]
    pub verification_url: String,
    #[serde(default, deserialize_with = "nullable", skip_serializing_if = "is_zero")]
    pub interval: i64,
    #[serde(default, deserialize_with = "nullable", skip_serializing_if = "empty")]
    pub expires_at: String,
    #[serde(default, deserialize_with = "nullable", skip_serializing_if = "empty")]
    pub token: String,
    #[serde(default, deserialize_with = "nullable", skip_serializing_if = "empty")]
    pub key_id: String,
    #[serde(default, deserialize_with = "nullable", skip_serializing_if = "empty")]
    pub workspace: String,
    #[serde(default, deserialize_with = "nullable", skip_serializing_if = "empty")]
    pub workspace_name: String,
    /// Whether the approval asks for the person's credential again.
    #[serde(default, deserialize_with = "nullable", skip_serializing_if = "std::ops::Not::not")]
    pub fresh: bool,
}

fn is_zero(n: &i64) -> bool {
    *n == 0
}

/// One page of a workspace's artifacts, newest first.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Page {
    #[serde(default, deserialize_with = "nullable")]
    pub artifacts: Vec<Artifact>,
    #[serde(default, deserialize_with = "nullable", skip_serializing_if = "empty")]
    pub next: String,
}

/// One page of a workspace's runs, newest first.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RunPage {
    #[serde(default, deserialize_with = "nullable")]
    pub runs: Vec<Run>,
    #[serde(default, deserialize_with = "nullable", skip_serializing_if = "empty")]
    pub next: String,
}


/// The descriptor at the API root.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Service {
    #[serde(default, deserialize_with = "nullable")]
    pub service: String,
    #[serde(default, deserialize_with = "nullable")]
    pub versions: Vec<String>,
}
