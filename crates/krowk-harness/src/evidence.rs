//! `publish`, krowk's evidence tool (R-EVID-1, R-TOOL-1): the files an
//! agent wants a person to see — a screenshot, a diff, a log — pushed as
//! krowk artifacts, each with a permalink, grouped under the session's
//! krowk run and tagged with the session (`krowk.session`).
//!
//! The push itself is krowk's own: the harness does not link the registry
//! client, so the CLI hands the host a `Publisher` that runs `krowk_push`'s
//! code — its root confinement, its credential-file and hard-link refusals,
//! unchanged — with the session's working directory as the root. What this
//! module owns is the tool's input, and the session's run: none until the
//! first publish opens one (with an API key; without one an upload is
//! anonymous and belongs to no run), then logged as `run.opened`, and every
//! later publish — a resumed session's included — attaches to it.
//!
//! The same call serves the native loop and every backend, which is offered
//! it through the tool bridge as `mcp__krowk__publish`.

use crate::engine::{EngineEvent, Events};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The tool's name, native and bridged alike.
pub const PUBLISH: &str = "publish";

/// What the model is told the tool does. Short: it rides on every call.
pub const DESCRIPTION: &str = "Publish files — screenshots, diffs, logs — as krowk artifacts under this session's run, and get each one's link. Files must be in the working directory; credential files are refused. Anyone with the link can read the file.";

/// Publish files as krowk artifacts.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PublishInput {
    /// Paths in the working directory, one artifact each.
    pub files: Vec<String>,
    /// One line shown with each artifact.
    #[serde(default)]
    pub caption: Option<String>,
}

/// One publish, as the host's publisher is handed it.
#[derive(Debug, Clone)]
pub struct PublishRequest {
    /// The session's working directory: nothing outside it is published.
    pub root: PathBuf,
    pub files: Vec<String>,
    pub caption: Option<String>,
    /// The krowk session, recorded as `krowk.session`.
    pub session_id: String,
    /// The session's run, once it has one.
    pub run: Option<String>,
    /// What did the work this file came out of.
    pub producer: Producer,
}

/// What did the work a published file came out of, stamped on each artifact
/// at its own moment (canon, engineering/metadata.md): the turn's model can
/// change mid-session and a subagent runs its own, so it is the publishing
/// turn's, not the session's first.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Producer {
    /// `krowk.engine`: `krowk` for krowk's own loop, else the backend that
    /// ran it — `claude-code`, `codex-app-server`.
    pub engine: String,
    /// `gen_ai.request.model`: the provider's model id.
    pub model: String,
    /// `gen_ai.system`: the provider the model is priced under.
    pub provider: String,
}

impl Producer {
    /// The engine name for a turn on this instance: a backend's kind, or
    /// `krowk` when the native loop runs it.
    pub fn new(instance: &crate::instances::Resolved, model: &str) -> Producer {
        let engine = if instance.backend.is_some() { instance.kind } else { "krowk" };
        Producer { engine: engine.into(), model: model.into(), provider: instance.provider.clone() }
    }
}

/// What a publish produced: the text the model reads — every artifact's
/// links, notes, or why nothing was published — and the run the artifacts
/// went under, when there is one.
#[derive(Debug, Clone, PartialEq)]
pub struct Published {
    pub text: String,
    pub run: Option<String>,
    /// What only the person may see — an anonymous upload's claim command,
    /// whose token is a secret: sent as a `notice`, never logged, never in
    /// `text`.
    pub for_person: Vec<String>,
}

/// Runs one publish, blocking: `Ok` with what was published, `Err` with the
/// text of a refusal or a failure, which the model reads as a tool error.
pub type Publisher = Arc<dyn Fn(&PublishRequest) -> Result<Published, String> + Send + Sync>;

/// A session's evidence: where its files go and the run they go under.
#[derive(Clone)]
pub struct Evidence {
    publisher: Publisher,
    session_id: String,
    producer: Producer,
    /// Held across a publish, so two at once open one run between them.
    run: Arc<tokio::sync::Mutex<Option<String>>>,
    /// Where a run it opens is reported instead of the caller's channel: a
    /// subagent's publish opens its parent's run, which the parent's log
    /// must hold for the parent's next turn to attach to it.
    report_to: Option<Events>,
}

impl std::fmt::Debug for Evidence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Evidence").field("session_id", &self.session_id).finish_non_exhaustive()
    }
}

impl Evidence {
    /// `run` is the session's, from its log's `run.opened`, if it has one.
    pub fn new(publisher: Publisher, session_id: &str, run: Option<String>, producer: Producer) -> Evidence {
        Evidence { publisher, session_id: session_id.into(), producer, run: Arc::new(tokio::sync::Mutex::new(run)), report_to: None }
    }

    /// The same evidence — the same session tag, the same run — for a
    /// subagent, whose run, when it opens one, is reported on its parent's
    /// `events` and so logged in the parent's log. What it publishes is the
    /// subagent's work, so the producer is its own.
    pub fn for_subagent(&self, events: Events, producer: Producer) -> Evidence {
        Evidence { report_to: Some(events), producer, ..self.clone() }
    }

    /// One `publish` call: the output the model reads and whether it is an
    /// error. A run it opened is reported on `events`, for the log.
    pub async fn publish(&self, cwd: &Path, input: &Value, events: &Events) -> (String, bool) {
        let input = match PublishInput::deserialize(input) {
            Ok(i) => i,
            Err(e) => return (format!("invalid input for publish: {e}"), true),
        };
        if input.files.iter().all(|f| f.trim().is_empty()) {
            return ("publish needs at least one path in `files`".into(), true);
        }
        let mut run = self.run.lock().await;
        let req = PublishRequest { root: cwd.to_path_buf(), files: input.files, caption: input.caption.filter(|c| !c.trim().is_empty()), session_id: self.session_id.clone(), run: run.clone(), producer: self.producer.clone() };
        let publisher = self.publisher.clone();
        // The upload is blocking network work, off the runtime, which must
        // stay free to hear an interrupt.
        match tokio::task::spawn_blocking(move || publisher(&req)).await {
            Ok(Ok(p)) => {
                for text in p.for_person {
                    let _ = events.send(EngineEvent::Notice { text }).await;
                }
                if let Some(opened) = p.run.filter(|r| run.as_ref() != Some(r)) {
                    *run = Some(opened.clone());
                    let _ = self.report_to.as_ref().unwrap_or(events).send(EngineEvent::RunOpened { run: opened }).await;
                }
                (p.text, false)
            }
            Ok(Err(why)) => (why, true),
            Err(e) => (format!("publish failed: {e}"), true),
        }
    }
}

/// A publish, as the permission evaluator judges it (`crate::permissions`):
/// `Publish` of every file it names. An artifact is at a URL that needs no
/// credential to read, so it is held to what changing files is — run under
/// `acceptEdits` and `bypassPermissions`, asked about under `default`,
/// refused in plan — and a `Read` deny rule denies it too: uploading a file
/// is reading it out. One rule for the native tool and the bridged one,
/// whoever asks.
pub fn call(cwd: &Path, input: &serde_json::Value) -> Result<crate::permissions::Call, (String, bool)> {
    let i = PublishInput::deserialize(input).map_err(|e| (format!("invalid input for publish: {e}"), true))?;
    let at = |f: &String| {
        let p = Path::new(f.trim());
        if p.is_absolute() { p.to_path_buf() } else { cwd.join(p) }
    };
    Ok(crate::permissions::Call { tool: "Publish".into(), access: crate::permissions::Access::Publish(i.files.iter().filter(|f| !f.trim().is_empty()).map(at).collect()), subject: None })
}

/// A host without a publisher still offers the tool — its definition is
/// part of the cached prefix and does not come and go — and says why it
/// cannot run.
pub const UNAVAILABLE: &str = "publish is not available in this session: krowk was started without a way to reach its registry.";

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn r_evid_1_the_first_publish_opens_the_run_and_the_next_attaches_to_it() {
        let asked: Arc<Mutex<Vec<PublishRequest>>> = Arc::default();
        let seen = asked.clone();
        let publisher: Publisher = Arc::new(move |r: &PublishRequest| {
            seen.lock().unwrap().push(r.clone());
            Ok(Published { text: format!("published {}", r.files.join(", ")), run: Some(r.run.clone().unwrap_or_else(|| "run_1".into())), for_person: Vec::new() })
        });
        let producer = Producer { engine: "krowk".into(), model: "claude-sonnet-4-6".into(), provider: "anthropic".into() };
        let ev = Evidence::new(publisher, "s-1", None, producer.clone());
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        rt.block_on(async {
            let (out, err) = ev.publish(Path::new("/repo"), &serde_json::json!({"files": ["shot.png"], "caption": "the fix"}), &tx).await;
            assert_eq!((out.as_str(), err), ("published shot.png", false));
            assert_eq!(rx.try_recv().unwrap(), EngineEvent::RunOpened { run: "run_1".into() });
            ev.publish(Path::new("/repo"), &serde_json::json!({"files": ["diff.txt"]}), &tx).await;
            assert!(rx.try_recv().is_err(), "the run is logged once");
            let (out, err) = ev.publish(Path::new("/repo"), &serde_json::json!({"paths": ["x"]}), &tx).await;
            assert!(err && out.starts_with("invalid input for publish"), "{out}");
        });
        let asked = asked.lock().unwrap();
        assert_eq!((asked[0].run.as_deref(), asked[1].run.as_deref()), (None, Some("run_1")));
        assert_eq!((asked[0].session_id.as_str(), asked[0].caption.as_deref(), asked[0].root.as_path()), ("s-1", Some("the fix"), Path::new("/repo")));
        assert_eq!(asked[0].producer, producer, "R-EVID-2: each publish carries what did the work");
    }
}
