//! krowk over MCP stdio, for agents that cannot shell out. A client, not a
//! second implementation: every tool goes through the same registry client
//! and renderers the CLI uses, and a failure comes back as a tool result with
//! the registry's fix line rather than as a transport error.
//!
//! krowk_push is confined to a root. In the CLI a person types the path; here
//! a model picks it, and a model reads repository files, web pages and issue
//! bodies — so without the boundary, a prompt injection anywhere turns push
//! into "publish any file on this machine".

use crate::output::{self, Paste};
use crate::runctx::{self, Link, Overrides};
use krowk_api::slug::{parse_slug, KIND_ARTIFACT, KIND_RUN};
use krowk_api::{fail, Artifact, Client, Error, Run, VISIBILITY_PRIVATE};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

const PROTOCOLS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];
const MAX_LINE: usize = 4 << 20;

pub struct Server<'a> {
    pub client: Client,
    pub env: &'a dyn Fn(&str) -> String,
    pub version: String,
    /// The upload root; the working directory when empty.
    pub root: String,
    /// Why the server holds no key although one was asked for — a checkout
    /// pinned to a workspace this machine has no key for. Writes are refused
    /// rather than landing anonymously.
    pub workspace_err: Option<Error>,
}

fn instructions() -> String {
    format!(
        "krowk turns local files into permalinks, one artifact per file, grouped under a
run that carries the metadata about where they came from.

Call krowk_push with the paths you want to share. Every artifact comes back in
two paste-ready forms and you must pick by destination. For a public artifact,
which is what a push is unless it asks otherwise:

  - markdown  {}
  - url       {}

Pasting the markdown form into Slack shows raw text; pasting the bare URL into a
GitHub comment shows a plain link with no image. Neither surface renders the
other's form, so choose deliberately.

Those two lines describe a public artifact and nothing else. Every result labels
its own two forms, and a private artifact's labels say something different: the
image still embeds anywhere, because its byte URL is itself the authorization,
but the card behind it opens only for a signed-in workspace member and unfurls
nowhere. Read the labels on the result rather than these two lines — and never
tell somebody a link will preview without having read them.

If a push comes back anonymous, each artifact carries a claim_token. Spending it
— krowk_claim_artifact, or `krowk claim <slug> <token>` — is the only way to
keep that upload past its expiry, and anyone holding the token can do it. Treat
it as a secret: give it to the human, never paste it into a pull request, an
issue or a chat message. An anonymous upload belongs to no run, so pass the run
to krowk_claim_artifact if the upload should be grouped with the rest of them.

krowk_push only uploads files from the working directory and below. Anything
outside it is refused, symlinks included, and credential files are refused even
inside it — .env, .ssh, .aws, .netrc, private keys, credentials.json. An artifact
is published at a URL that needs no credential to read — `private: true` narrows
who finds the card, not who can fetch the bytes off the link — so do not try to
route around this: if a file you were asked to share sits elsewhere, say so and
let the human move it.",
        output::EMBED_SURFACES,
        output::LINK_SURFACES
    )
}

/// Names that hold credentials wherever they sit: nobody publishes these on
/// purpose, and the cost of refusing is an error message.
const SECRET_NAMES: &[&str] = &[
    ".ssh", ".aws", ".gnupg", ".kube", ".docker", ".env", ".netrc", ".npmrc", ".pypirc", ".git-credentials",
    "credentials.json", "id_rsa", "id_ed25519", "id_ecdsa",
];

fn secret_component(path: &Path) -> Option<String> {
    path.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).find(|part| {
        let lower = part.to_lowercase();
        SECRET_NAMES.contains(&lower.as_str()) || lower.starts_with(".env.")
    })
}

impl Server<'_> {
    /// The root, resolved. `/`, the home directory and anything under a
    /// credential directory are refused: `~/.ssh` sits inside a root that broad.
    fn resolve_root(&self) -> Result<PathBuf, Error> {
        let root = if self.root.is_empty() {
            std::env::current_dir().map_err(|e| fail("no_working_directory", format!("cannot determine the working directory: {e}")))?
        } else {
            PathBuf::from(&self.root)
        };
        let abs = std::path::absolute(&root).map_err(|e| fail("bad_root", format!("cannot resolve {}: {e}", root.display())))?;
        let abs = abs.canonicalize().unwrap_or(abs);
        if abs == Path::new("/") {
            return Err(fail("root_too_broad", "the upload root is / — start the server in a project directory, or pass --root"));
        }
        if let Some(home) = krowk_api::creds::home_dir()
            && abs == home.canonicalize().unwrap_or(home)
        {
            return Err(fail(
                "root_too_broad",
                "the upload root is the home directory, which holds ~/.ssh and ~/.aws — start the server in a project directory, or pass --root",
            ));
        }
        if let Some(name) = secret_component(&abs) {
            return Err(fail(
                "root_too_broad",
                format!("the upload root is inside {name}, which holds credentials — start the server in a project directory, or pass --root"),
            ));
        }
        Ok(abs)
    }
}

/// A path allowed to be pushed: resolved through its symlinks before the
/// check, inside the root, no credential name on the way, and not a file with
/// a second name — a hard link is a key outside the root with nothing to resolve.
/// A relative path resolves from `base`: the working directory when it is
/// empty, the session's for the harness's `publish`.
fn permit(root: &Path, base: &Path, path: &str) -> Result<String, Error> {
    let real = std::fs::canonicalize(base.join(path))
        .map_err(|_| fail("file_unreadable", format!("cannot read `{path}` — paths resolve from the working directory")))?;
    let Ok(rel) = real.strip_prefix(root) else {
        return Err(fail(
            "outside_root",
            format!("`{path}` is outside {} — krowk_push only uploads files from the working directory", root.display()),
        ));
    };
    if let Some(name) = secret_component(rel) {
        return Err(fail("secret_path", format!("`{path}` is a credential file (`{name}`) — refusing to publish it at a public URL")));
    }
    #[cfg(unix)]
    if let Ok(meta) = std::fs::metadata(&real) {
        use std::os::unix::fs::MetadataExt;
        if meta.is_file() && meta.nlink() > 1 {
            return Err(fail(
                "hard_linked",
                format!(
                    "`{path}` has more than one name on disk, so it may be a file from outside {} — copy it in and push the copy",
                    root.display()
                ),
            ));
        }
    }
    Ok(real.to_string_lossy().into_owned())
}

#[derive(Deserialize)]
struct Request {
    #[serde(default)]
    id: Option<Value>,
    #[serde(default)]
    method: String,
    #[serde(default)]
    params: Option<Value>,
}

fn rpc_error(code: i64, message: &str) -> Value {
    json!({ "code": code, "message": message })
}

impl Server<'_> {
    /// One JSON-RPC message per line, answered in order; notifications (no id)
    /// are dispatched and not answered.
    pub fn serve(&self, input: impl BufRead, out: &mut impl Write) -> std::io::Result<()> {
        let mut input = input;
        loop {
            // Bounded before it is buffered: a peer that never sends a
            // newline costs MAX_LINE bytes, not whatever it sends.
            let mut line = Vec::new();
            let n = std::io::Read::take(&mut input, MAX_LINE as u64 + 1).read_until(b'\n', &mut line)?;
            if n == 0 {
                break;
            }
            if line.last() == Some(&b'\n') {
                line.pop();
            }
            if line.len() > MAX_LINE {
                return Err(std::io::Error::other(format!("mcp: message longer than {MAX_LINE} bytes")));
            }
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            let response = match serde_json::from_slice::<Request>(&line) {
                Err(_) => {
                    let message = if serde_json::from_slice::<Value>(&line).is_ok() {
                        rpc_error(-32600, "the message is JSON but not a Request object")
                    } else {
                        rpc_error(-32700, "the message is not JSON")
                    };
                    Some(json!({ "jsonrpc": "2.0", "id": null, "error": message }))
                }
                Ok(req) if req.method.is_empty() => Some(json!({
                    "jsonrpc": "2.0", "id": null,
                    "error": rpc_error(-32600, "the message is JSON but not a Request object"),
                })),
                Ok(req) => {
                    let result = self.dispatch(&req);
                    req.id.filter(|id| !id.is_null()).map(|id| match result {
                        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                        Err(error) => json!({ "jsonrpc": "2.0", "id": id, "error": error }),
                    })
                }
            };
            if let Some(response) = response {
                writeln!(out, "{response}")?;
                out.flush()?;
            }
        }
        Ok(())
    }

    fn dispatch(&self, req: &Request) -> Result<Value, Value> {
        match req.method.as_str() {
            "initialize" => {
                let requested = req.params.as_ref().and_then(|p| p.get("protocolVersion")).and_then(Value::as_str).unwrap_or_default();
                let bad_version = req.params.as_ref().and_then(|p| p.get("protocolVersion")).is_some_and(|v| !v.is_string() && !v.is_null());
                if bad_version || req.params.as_ref().is_some_and(|p| !p.is_object() && !p.is_null()) {
                    return Err(rpc_error(-32602, "initialize params are not an object"));
                }
                let version = PROTOCOLS.iter().find(|v| **v == requested).unwrap_or(&PROTOCOLS[0]);
                Ok(json!({
                    "protocolVersion": version,
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "krowk", "version": self.version },
                    "instructions": instructions(),
                }))
            }
            "notifications/initialized" => Ok(Value::Null),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tool_schemas() })),
            "tools/call" => self.call(req.params.as_ref()),
            other => Err(rpc_error(-32601, &format!("unknown method {other}"))),
        }
    }

    fn call(&self, params: Option<&Value>) -> Result<Value, Value> {
        let name = params.and_then(|p| p.get("name")).and_then(Value::as_str).ok_or_else(|| rpc_error(-32602, "tools/call needs a name and arguments"))?;
        let args = params.and_then(|p| p.get("arguments")).cloned().unwrap_or(Value::Null);
        let outcome = match name {
            "krowk_push" => self.push(&args),
            "krowk_list_artifacts" => self.list_artifacts(&args),
            "krowk_get_artifact" => self.get_artifact(&args),
            "krowk_claim_artifact" => self.claim_artifact(&args),
            "krowk_get_run" => self.get_run(),
            "krowk_verify_key" => self.verify_key(),
            other => return Err(rpc_error(-32602, &format!("unknown tool {other}"))),
        };
        Ok(match outcome {
            Ok((text, structured)) => tool_result(&text, Some(structured), false),
            Err(e) => tool_result(&describe_error(&e), Some(json!(e.body)), true),
        })
    }
}

/// `publish`, the harness's evidence tool (R-EVID-1): krowk_push for a krowk
/// session, run with the session's working directory as the root, so its
/// confinement, its credential-file and hard-link refusals are krowk_push's
/// own, unchanged. With a key, the session's first publish opens its run —
/// the session recorded on it — and every artifact is tagged with the
/// session (`krowk.session`) and attached to that run; without one the
/// upload is anonymous and belongs to no run, and the result says where to
/// look.
#[cfg(feature = "harness")]
impl Server<'_> {
    pub fn publish(&self, req: &krowk_harness::evidence::PublishRequest) -> Result<krowk_harness::evidence::Published, String> {
        self.publish_session(req).map_err(|e| describe_error(&e))
    }

    fn publish_session(&self, req: &krowk_harness::evidence::PublishRequest) -> Result<krowk_harness::evidence::Published, Error> {
        let keyed = self.authenticated() && self.workspace_err.is_none();
        let mut run = req.run.clone();
        if run.is_none() && keyed {
            // A publish that would be refused opens no run.
            let root = self.resolve_root()?;
            for f in &req.files {
                permit(&root, &req.root, f)?;
            }
            let meta = runctx::resolve_in(
                self.env,
                Overrides { session: req.session_id.clone(), agent: "krowk".into(), client: format!("krowk/{}", self.version), ..Overrides::default() },
                Some(&req.root),
            );
            run = Some(self.client.create_run(&serde_json::to_value(&meta).expect("metadata serializes"))?.slug);
        }
        let mut metadata = BTreeMap::new();
        if keyed {
            metadata.insert("krowk.session", req.session_id.clone());
            // Pushed by krowk's engine, through its MCP server's code.
            metadata.insert("krowk.client", format!("krowk/{}", self.version));
            if let Some(c) = &req.caption {
                metadata.insert("krowk.caption", c.clone());
            }
        }
        let args = json!({ "files": req.files, "run": run.clone().unwrap_or_default(), "metadata": metadata });
        let (_, pushed) = self.push_from(&req.root, &args)?;
        // A keyless upload's claim token is a secret the person spends:
        // what the model reads is logged, sent to the provider on every
        // later call and printed by stream-json, so it never carries one.
        // The claim command goes to the person alone.
        let mut artifacts: Vec<Artifact> = serde_json::from_value(pushed["artifacts"].clone()).unwrap_or_default();
        let opened: Option<Run> = pushed.get("run").and_then(|r| serde_json::from_value(r.clone()).ok());
        let notes: Vec<String> = pushed.get("notes").and_then(|n| serde_json::from_value(n.clone()).ok()).unwrap_or_default();
        let for_person: Vec<String> = artifacts
            .iter()
            .filter(|a| !a.claim_token.is_empty())
            .map(|a| format!("{} is anonymous and expires within the day — keep it with `{}` (the token is a secret, shown once: do not paste it anywhere public)", a.filename, output::claim_crumb(a).cmd))
            .collect();
        for a in &mut artifacts {
            a.claim_token.clear();
        }
        let (mut text, _) = render_push(&artifacts, opened.as_ref(), &notes);
        if !for_person.is_empty() {
            text += "\n\nThe command that keeps this anonymous upload past its expiry carries a secret, so it was shown to the person rather than to you.";
        }
        match &run {
            Some(r) => text += &format!("\n\nGrouped under run {r}, this krowk session's run."),
            None => text += "\n\nNo API key was found, so this upload is anonymous and belongs to no run — `krowk doctor` shows where krowk looks for one.",
        }
        Ok(krowk_harness::evidence::Published { text, run, for_person })
    }
}

fn tool_result(text: &str, structured: Option<Value>, is_error: bool) -> Value {
    let mut result = json!({ "content": [{ "type": "text", "text": text }], "isError": is_error });
    if let Some(s) = structured {
        result["structuredContent"] = s;
    }
    result
}

/// A failure for an agent to read: the code, the status, the fix, and
/// whether a retry could help.
fn describe_error(e: &Error) -> String {
    let mut first = format!("krowk failed: {}", e.code());
    if e.status != 0 {
        first += &format!(" (HTTP {})", e.status);
    }
    let mut lines = vec![first];
    if !e.fix().is_empty() {
        lines.push(format!("fix: {}", e.fix()));
    }
    if e.retryable() {
        lines.push("this one is worth retrying".into());
    }
    lines.join("\n")
}

fn workspace_reason(e: &Error) -> String {
    if e.fix().is_empty() { e.code() } else { format!("{} — {}", e.code(), e.fix()) }
}

type Outcome = Result<(String, Value), Error>;

/// Arguments in the shape a tool's schema names, and nothing else: an
/// unexpected field is a misunderstanding, not something to drop silently.
fn arguments<T: for<'de> Deserialize<'de> + Default>(args: &Value, shape: &str) -> Result<T, Error> {
    if args.is_null() {
        return Ok(T::default());
    }
    serde_json::from_value(args.clone()).map_err(|e| {
        let msg = e.to_string();
        let why = if msg.contains("unknown field") {
            format!("{} — this tool takes only the arguments in its schema", msg.split(" at line").next().unwrap_or(&msg))
        } else {
            shape.to_string()
        };
        fail("bad_arguments", why)
    })
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct PushArgs {
    files: Vec<String>,
    run: String,
    title: String,
    pull_request: String,
    links: Vec<LinkArg>,
    references: Vec<String>,
    session: String,
    repo: String,
    commit: String,
    agent: String,
    metadata: BTreeMap<String, String>,
    private: bool,
}

/// Strict where the top level is lenient, as Go's checkLinkFields is: a
/// misspelled `title` here would land an unlabelled link in public metadata.
#[derive(Deserialize, Default, Clone)]
#[serde(deny_unknown_fields, default)]
struct LinkArg {
    url: String,
    title: String,
    rel: String,
}

impl Server<'_> {
    fn authenticated(&self) -> bool {
        self.client.authenticated()
    }

    fn push(&self, args: &Value) -> Outcome {
        self.push_from(Path::new(""), args)
    }

    /// krowk_push, with relative paths resolved from `base`.
    fn push_from(&self, base: &Path, args: &Value) -> Outcome {
        let a: PushArgs = arguments(
            args,
            "the arguments are not the shape this tool takes: `files` is an array of paths, `links` an array of {url, title, rel} objects — see the tool's schema",
        )?;
        if a.files.is_empty() {
            return Err(fail("no_file", "pass at least one path in `files`"));
        }
        let links: Vec<Link> = a.links.iter().map(|l| Link { url: l.url.clone(), title: l.title.clone(), rel: l.rel.clone() }).collect();
        runctx::validate_links(&links).map_err(|e| fail("bad_arguments", format!("`links`: {e}")))?;
        if let Some(e) = &self.workspace_err {
            return Err(e.clone());
        }
        if a.private && !self.authenticated() {
            return Err(krowk_api::private_needs_key());
        }
        let named_run = parse_slug(KIND_RUN, &a.run)?;
        let root = self.resolve_root()?;
        let files = a.files.iter().map(|p| permit(&root, base, p)).collect::<Result<Vec<_>, _>>()?;
        let specs = files.iter().map(|p| krowk_api::spec::inspect(p)).collect::<Result<Vec<_>, _>>()?;

        let resolved = runctx::resolve_in(
            self.env,
            Overrides {
                repo: a.repo.clone(),
                commit: a.commit.clone(),
                agent: a.agent.clone(),
                pull_request: a.pull_request.clone(),
                links,
                references: a.references.clone(),
                session: a.session.clone(),
                title: a.title.clone(),
                client: format!("krowk-mcp/{}", self.version),
            },
            (!base.as_os_str().is_empty()).then_some(base),
        );
        let (mut run, mut run_slug, mut own_run): (Option<Run>, String, bool) = (None, named_run.clone(), false);
        if run_slug.is_empty() && self.authenticated() {
            let created = self.client.create_run(&serde_json::to_value(&resolved).expect("metadata serializes"))?;
            run_slug = created.slug.clone();
            run = Some(created);
            own_run = true;
        }
        let work: Vec<&str> = [
            (!a.pull_request.is_empty(), "`pull_request`"),
            (!a.links.is_empty(), "`links`"),
            (!a.references.is_empty(), "`references`"),
            (!a.session.is_empty(), "`session`"),
            (!a.title.is_empty(), "`title`"),
        ]
        .into_iter()
        .filter_map(|(g, n)| g.then_some(n))
        .collect();
        let mut notes = Vec::new();
        if !self.authenticated() {
            let mut all = work.clone();
            if !a.metadata.is_empty() {
                all.push("`metadata`");
            }
            if !all.is_empty() {
                notes.push(format!(
                    "{} were not recorded: a keyless upload records no metadata, and opening a run needs an API key",
                    all.join(", ")
                ));
            }
        } else if !named_run.is_empty() && !work.is_empty() {
            notes.push(format!("{} were not recorded: run {named_run} already carries the metadata it was opened with", work.join(", ")));
        }

        let extras: Vec<(String, String)> = a.metadata.into_iter().collect();
        let stamp = self.authenticated().then(|| resolved.artifact().with_extras(&extras));
        let mut artifacts: Vec<Artifact> = Vec::new();
        for mut spec in specs {
            spec.run = run_slug.clone();
            if a.private {
                spec.visibility = VISIBILITY_PRIVATE.into();
            }
            spec.metadata = stamp.clone();
            match self.client.push(&spec) {
                Ok(artifact) => artifacts.push(artifact),
                Err(mut e) => {
                    if !artifacts.is_empty() {
                        e.body.insert("uploaded_before_failure".into(), json!(artifacts.iter().map(|x| x.url.clone()).collect::<Vec<_>>()));
                    }
                    if own_run {
                        e.body.insert("run".into(), json!(run_slug));
                        let finish = format!("the run is still open — close it with `krowk runs finish {run_slug}`");
                        let fix = if e.fix().is_empty() { finish } else { format!("{}; {finish}", e.fix()) };
                        e.body.insert("fix".into(), json!(fix));
                    }
                    return Err(e);
                }
            }
        }
        if own_run {
            match self.client.finish_run(&run_slug) {
                Ok(finished) => run = Some(finished),
                Err(_) => notes.push(format!("run {run_slug} could not be finished — retry `krowk runs finish {run_slug}`")),
            }
        }
        Ok(render_push(&artifacts, run.as_ref(), &notes))
    }

    fn list_artifacts(&self, args: &Value) -> Outcome {
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct Args {
            limit: i64,
            before: String,
        }
        let a: Args = arguments(args, "`limit` must be a number and `before` a slug")?;
        if let Some(e) = &self.workspace_err {
            return Err(e.clone());
        }
        let page = self.client.list_artifacts(a.before.trim(), a.limit)?;
        let mut lines = Vec::new();
        if page.artifacts.is_empty() {
            lines.push("No artifacts in this workspace yet.".to_string());
        }
        for x in &page.artifacts {
            let mut line = format!("{}  {}  {}  {}", x.slug, x.filename, output::human_bytes(x.byte_size), x.url);
            if x.state != "ready" {
                line += &format!("  ({})", x.state);
            }
            lines.push(line);
        }
        if !page.next.is_empty() {
            lines.extend([String::new(), format!("More: pass before={} for the next page.", page.next)]);
        }
        Ok((lines.join("\n"), serde_json::to_value(&page).expect("page serializes")))
    }

    fn get_artifact(&self, args: &Value) -> Outcome {
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct Args {
            slug: String,
        }
        let a: Args = arguments(args, "`slug` must be a string")?;
        if a.slug.trim().is_empty() {
            return Err(fail("missing_slug", "pass the artifact slug, e.g. art_..."));
        }
        let artifact = self.client.show_artifact(&parse_slug(KIND_ARTIFACT, &a.slug)?)?;
        Ok(render(&artifact))
    }

    fn claim_artifact(&self, args: &Value) -> Outcome {
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct Args {
            slug: String,
            claim_token: String,
            run: String,
        }
        let a: Args = arguments(args, "`slug`, `claim_token` and `run` must be strings")?;
        if a.slug.trim().is_empty() {
            return Err(fail("no_artifact", "pass the artifact slug, e.g. art_..."));
        }
        if a.claim_token.trim().is_empty() {
            return Err(fail("missing_claim", "pass the claim_token the anonymous push returned"));
        }
        if let Some(e) = &self.workspace_err {
            return Err(e.clone());
        }
        let slug = parse_slug(KIND_ARTIFACT, &a.slug)?;
        let run = parse_slug(KIND_RUN, &a.run)?;
        let mut artifact = self.client.claim_artifact(&slug, a.claim_token.trim())?;
        if !run.is_empty() {
            artifact = self.client.attach_run(&artifact.slug, &run).map_err(|mut e| {
                e.body.insert("claimed".into(), json!(artifact.slug));
                let retry = "the upload is claimed and kept, only the run is not attached — call krowk_claim_artifact again with the same slug and claim_token, and a run this workspace holds; claiming twice with the same key is the same success";
                let fix = if e.fix().is_empty() { retry.to_string() } else { format!("{}; {retry}", e.fix()) };
                e.body.insert("fix".into(), json!(fix));
                e
            })?;
        }
        let (mut text, structured) = render(&artifact);
        if !run.is_empty() {
            text += &format!("\n\nGrouped under run {run}.");
        } else if artifact.run_slug().is_empty() {
            text += "\n\nIt belongs to no run. A run is where the pull request, commit and session are recorded — call \
                     krowk_claim_artifact again with the same slug and claim_token and a `run` this workspace holds to group \
                     it under one; claiming twice with the same key is the same success.";
        }
        Ok((text, structured))
    }

    fn get_run(&self) -> Outcome {
        let m = runctx::resolve(self.env, Overrides { client: format!("krowk-mcp/{}", self.version), ..Overrides::default() });
        let mut lines = vec!["Run context detected for this working directory:".to_string()];
        for (label, value) in [("repo", &m.repo_name), ("commit", &m.commit), ("branch", &m.branch), ("agent", &m.harness), ("pull request", &m.change_url)] {
            if !value.is_empty() {
                lines.push(format!("  {label:<12} {value}"));
            }
        }
        if lines.len() == 1 {
            lines.push("  nothing detected — not a git checkout, or no remote".into());
        }
        lines.extend([String::new(), "Every push attaches this automatically; pass repo, commit or agent to override.".into()]);
        Ok((lines.join("\n"), serde_json::to_value(&m).expect("metadata serializes")))
    }

    fn verify_key(&self) -> Outcome {
        if let Some(e) = &self.workspace_err {
            return Ok((
                format!(
                    "This server has no API key, and it was meant to have one: {}\n\nUploads, claims and listings are refused rather \
                     than landing in the anonymous workspace, because this checkout's config names a workspace of its own. Looking up \
                     an artifact still works. Fix the key or the config and restart the server.",
                    workspace_reason(e)
                ),
                json!({ "authenticated": false, "workspace_error": workspace_reason(e) }),
            ));
        }
        if !self.authenticated() {
            return Ok((
                "No API key is configured, so pushes will be anonymous: they expire within a day and come back with a claim token.\n\n\
                 Set KROWK_TOKEN, or run `krowk login --token krowk_sk_...`."
                    .into(),
                json!({ "authenticated": false }),
            ));
        }
        let key = self.client.verify_key()?;
        let mut lines = vec![format!("Key {} is valid.", key.key_id), format!("  workspace  {}", key.workspace)];
        if !key.name.is_empty() {
            lines.push(format!("  name       {}", key.name));
        }
        if !key.expires_at.is_empty() {
            lines.push(format!("  expires    {}", key.expires_at));
        }
        Ok((lines.join("\n"), serde_json::to_value(&key).expect("key serializes")))
    }
}

fn paste_of(a: &Artifact) -> Paste {
    Paste {
        markdown: output::block_for(a),
        url: output::card_for(a),
        destinations: a.paste.as_ref().map(|p| p.destinations.clone()).unwrap_or_default(),
    }
}

/// One artifact for an agent: the record, its relative expiry (a number
/// reads better than "tomorrow" here), both paste forms labelled honestly,
/// and the claim command when there is one.
fn artifact_lines(a: &Artifact, paste: &Paste) -> Vec<String> {
    let mut lines = vec![format!("Artifact {} — {}, {}", a.slug, a.filename, output::human_bytes(a.byte_size))];
    let expiry = output::relative_expiry(&a.expires_at, jiff::Timestamp::now());
    if !expiry.is_empty() {
        lines.push(expiry);
    }
    lines.extend([
        String::new(),
        format!("Paste into {}:", output::markdown_surfaces_for(a)),
        paste.markdown.clone(),
        String::new(),
        format!("Paste into {}:", output::link_surfaces_for(a)),
        paste.url.clone(),
    ]);
    if !a.claim_token.is_empty() {
        lines.extend([
            String::new(),
            "This upload is anonymous and nobody owns it yet. The command below adopts it —".into(),
            "the token is a secret, so hand it to the human and do not paste it anywhere public:".into(),
            output::claim_crumb(a).cmd,
        ]);
    }
    lines
}

fn render(a: &Artifact) -> (String, Value) {
    let paste = paste_of(a);
    (artifact_lines(a, &paste).join("\n"), json!({ "artifact": a, "paste": paste }))
}

fn render_push(artifacts: &[Artifact], run: Option<&Run>, notes: &[String]) -> (String, Value) {
    let mut lines = Vec::new();
    let mut pastes = Vec::new();
    for (i, a) in artifacts.iter().enumerate() {
        if i > 0 {
            lines.push(String::new());
        }
        let paste = paste_of(a);
        lines.extend(artifact_lines(a, &paste));
        pastes.push(paste);
    }
    if let Some(run) = run {
        lines.extend([String::new(), format!("Grouped under run {} ({}).", run.slug, run.status)]);
    }
    for note in notes {
        lines.extend([String::new(), format!("Note: {note}")]);
    }
    let mut structured = json!({ "artifacts": artifacts, "pastes": pastes });
    if let Some(run) = run {
        structured["run"] = json!(run);
    }
    if !notes.is_empty() {
        structured["notes"] = json!(notes);
    }
    (lines.join("\n"), structured)
}

fn tool_schemas() -> Value {
    let slug = "Artifact slug, e.g. art_9f3c2e1a7b04d6c8e5f1a2b3 — or any link carrying it, like https://krowk.com/a/art_9f3c2e1a7b04d6c8e5f1a2b3.";
    json!([
        {
            "name": "krowk_push",
            "description": "Upload one or more local files; every file becomes its own artifact with a permalink, grouped under a run when an API key is configured. Returns both paste-ready forms per artifact: the markdown image embed for GitHub, Linear and Notion, and the bare URL for Slack and Basecamp. Repo, commit, branch and agent are detected automatically. Uploads are public unless `private` says otherwise, and each artifact reports its own `visibility`.",
            "inputSchema": {
                "type": "object",
                "required": ["files"],
                "properties": {
                    "files": { "type": "array", "items": { "type": "string" }, "minItems": 1,
                        "description": "Paths to upload, resolved from the working directory. Must be inside it — paths outside are refused, symlinks included, and credential files are refused even inside it, because an artifact is readable by anyone with the link." },
                    "run": { "type": "string", "description": "Attach to an existing run instead of opening one. Its slug, or any link carrying it." },
                    "private": { "type": "boolean", "description": "Upload where only this workspace can read it. The image still embeds anywhere — a private artifact's byte URL is itself the authorization — but the card page it links to opens only for a signed-in workspace member, reads as not found to everyone else, and unfurls nowhere. Needs an API key: a keyless upload has no workspace to be private to and is refused rather than published." },
                    "title": { "type": "string", "description": "Title for the run this push opens, e.g. the pull request's." },
                    "pull_request": { "type": "string", "description": "URL of the pull request this work belongs to." },
                    "links": {
                        "type": "array", "maxItems": runctx::MAX_LINKS,
                        "items": {
                            "type": "object", "required": ["url"],
                            "properties": {
                                "url": { "type": "string", "format": "uri", "maxLength": runctx::MAX_LINK_URL, "description": "Absolute http(s) URL. Anything else is refused, not trimmed." },
                                "title": { "type": "string", "maxLength": runctx::MAX_LINK_TITLE, "description": "One line naming the link, shown to a reader instead of the URL, e.g. the issue's own title." },
                                "rel": { "type": "string", "maxLength": runctx::MAX_LINK_REL, "description": format!("What this link is. Pick from {} where one fits: `fixes` for the issue this work closes, `tracks` for the ticket it is filed under, `spec` for what it implements, `discussion` for the thread about it, `source` for what it was derived from, `supersedes` for the run it replaces. Any other word is accepted and stored as given.", runctx::LINK_RELS.join(", ")) }
                            },
                            "additionalProperties": false
                        },
                        "description": "Links this work is about, recorded on the run as `krowk.links` — the issue being fixed, the spec, the discussion. Prefer this over `references` for anything that is a URL."
                    },
                    "references": { "type": "array", "items": { "type": "string" }, "description": "Related identifiers that are not URLs, e.g. a ticket key. A URL belongs in `links`." },
                    "session": { "type": "string", "description": "Override the detected agent session. Recorded on the run." },
                    "repo": { "type": "string", "description": "Override the detected repository." },
                    "commit": { "type": "string", "description": "Override the detected commit." },
                    "agent": { "type": "string", "description": "Override the detected agent name." },
                    "metadata": { "type": "object", "additionalProperties": { "type": "string" }, "description": "Extra key/value metadata recorded on each artifact, e.g. krowk.caption or url.full. Your value wins over a detected one. Metadata is public: anyone with the link can read it." }
                },
                "additionalProperties": false
            }
        },
        {
            "name": "krowk_list_artifacts",
            "description": "List the workspace's artifacts, newest first. Needs an API key: keyless uploads share the anonymous workspace, so there is nothing of one's own to list.",
            "inputSchema": { "type": "object", "properties": {
                "limit": { "type": "integer", "description": "Artifacts per page (1–100, default 50)." },
                "before": { "type": "string", "description": "Start after this artifact slug — the `next` of the last page." }
            }, "additionalProperties": false }
        },
        {
            "name": "krowk_get_artifact",
            "description": "Look up an artifact already uploaded, by its slug. Returns the same paste-ready forms as a push.",
            "inputSchema": { "type": "object", "required": ["slug"], "properties": { "slug": { "type": "string", "description": slug } }, "additionalProperties": false }
        },
        {
            "name": "krowk_claim_artifact",
            "description": "Spend a claim token to move an anonymous artifact into the key's workspace. A Pro or Business workspace keeps it; a free one restamps a fresh 24-hour expiry. Pass `run` to also group it under a run — an anonymous upload could not name one, so this is the only way it gets one. Needs an API key.",
            "inputSchema": { "type": "object", "required": ["slug", "claim_token"], "properties": {
                "slug": { "type": "string", "description": slug },
                "claim_token": { "type": "string", "description": "The claim token the anonymous push returned, e.g. krowk_claim_..." },
                "run": { "type": "string", "description": "Run to attach the claimed upload to, e.g. run_..., or any link carrying it. Attached after the claim, so it must be a run in this key's workspace." }
            }, "additionalProperties": false }
        },
        {
            "name": "krowk_get_run",
            "description": "Report the repository, commit, branch and agent that will be attached to the next push. Useful for checking detection before uploading.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        },
        {
            "name": "krowk_verify_key",
            "description": "Check whether an API key is configured, and which workspace uploads made with it land in. Without a key, pushes still work but are anonymous and expire in 24h. Call it when a push is refused: if this checkout pinned a workspace the server could not reach, the reason is here.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        }
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_names_are_caught_wherever_they_sit() {
        for p in [".env", "a/.env.local", "x/.ssh/id", "credentials.json", ".aws/config", "keys/ID_RSA"] {
            assert!(secret_component(Path::new(p)).is_some(), "{p}");
        }
        assert!(secret_component(Path::new("src/env.rs")).is_none());
    }

    fn serve(input: &[u8]) -> (std::io::Result<()>, String) {
        let env = |_: &str| String::new();
        let server = Server { client: Client::new("http://127.0.0.1:9", ""), env: &env, version: "t".into(), root: String::new(), workspace_err: None };
        let mut out = Vec::new();
        let res = server.serve(input, &mut out);
        (res, String::from_utf8(out).unwrap())
    }

    #[test]
    fn an_unterminated_message_is_refused_at_the_cap_and_a_numeric_version_is_invalid() {
        let (res, _) = serve(&vec![b'x'; MAX_LINE + 10]);
        assert!(res.is_err());
        let (res, out) = serve(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":5}}\n{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"ping\"}");
        assert!(res.is_ok());
        let lines: Vec<Value> = out.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(lines[0]["error"]["code"], -32602);
        assert_eq!(lines[1]["result"], json!({}), "a last line without its newline is still answered");
    }

    #[test]
    fn push_ignores_extra_arguments_but_refuses_extra_link_fields() {
        assert!(arguments::<PushArgs>(&json!({"files": ["a.txt"], "bogus": 1}), "").is_ok());
        let err = arguments::<PushArgs>(&json!({"files": ["a.txt"], "links": [{"url": "https://x.example", "label": "Issue"}]}), "").err().unwrap();
        assert_eq!(err.code(), "bad_arguments");
        assert!(err.fix().contains("unknown field `label`"), "{}", err.fix());
    }
}
