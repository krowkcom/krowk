//! One result rendered three ways: for a person, for an agent, and for
//! pasting into a pull request.

pub mod fix;
pub mod jq;
pub mod spinner;
pub mod workspace;

use krowk_api::{fail, Artifact, Error, Key, Page, Run, RunPage};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// The shape of a rendered result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Human,
    Json,
    Markdown,
    Url,
}

impl Format {
    pub fn name(self) -> &'static str {
        match self {
            Format::Human => "human",
            Format::Json => "json",
            Format::Markdown => "markdown",
            Format::Url => "url",
        }
    }

    fn parse(s: &str) -> Option<Format> {
        Some(match s {
            "human" => Format::Human,
            "json" => Format::Json,
            "markdown" => Format::Markdown,
            "url" => Format::Url,
            _ => return None,
        })
    }
}

/// Human on a terminal and JSON when piped, so an agent capturing stdout gets
/// structured data without asking. A --format nobody has heard of is refused
/// even when --json or --jq would have settled the format anyway.
pub fn resolve_format(flag: &str, json_flag: bool, tty: bool) -> Result<Format, Error> {
    if flag.is_empty() {
        return Ok(if json_flag || !tty { Format::Json } else { Format::Human });
    }
    match Format::parse(flag) {
        Some(_) if json_flag => Ok(Format::Json),
        Some(f) => Ok(f),
        None => Err(fail("bad_format", format!("unknown --format {flag} (expected human, json, markdown or url)"))),
    }
}

pub const EMBED_SURFACES: &str = "GitHub, Linear, Notion — renders the image";
pub const PLAIN_SURFACES: &str = "GitHub, Linear, Notion — plain link, no preview to embed";
pub const LINK_SURFACES: &str = "Slack, Basecamp — they unfurl the link themselves";
pub const PRIVATE_EMBED_SURFACES: &str =
    "GitHub, Linear, Notion — renders the image; the card behind it opens only for a signed-in workspace member";
pub const PRIVATE_PLAIN_SURFACES: &str =
    "GitHub, Linear, Notion — plain link to a card that opens only for a signed-in workspace member";
pub const PRIVATE_LINK_SURFACES: &str =
    "a place only workspace members read — nothing unfurls this link, and the card opens only after they sign in";

/// What a visibility this build has never heard of is called: the value, and
/// no promise — describing an unknown one as private would be the dangerous
/// way to be wrong.
pub fn surfaces_for_unknown(a: &Artifact) -> String {
    format!(
        "nowhere yet — this artifact's visibility is {:?}, which this krowk does not know how to describe. Upgrade before promising anybody anything about the link",
        a.visibility
    )
}

/// One upload in the two forms its destinations need.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Paste {
    pub markdown: String,
    pub url: String,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub destinations: BTreeMap<String, String>,
}

/// The artifact's krowk block — the registry's, never composed here.
pub fn block_for(a: &Artifact) -> String {
    if let Some(p) = a.paste.as_ref().filter(|p| !p.markdown.is_empty()) {
        return p.markdown.clone();
    }
    if !a.markdown.is_empty() {
        return a.markdown.clone();
    }
    card_for(a)
}

/// The bare link: a shared artifact's share_url, else the registry's link.
pub fn card_for(a: &Artifact) -> String {
    if a.shared() && !a.share_url.is_empty() {
        return a.share_url.clone();
    }
    if let Some(p) = a.paste.as_ref().filter(|p| !p.url.is_empty()) {
        return p.url.clone();
    }
    a.url.clone()
}

/// The honest label for the markdown form: an image only where one embeds.
pub fn markdown_surfaces_for(a: &Artifact) -> String {
    if !a.public() && !a.private() && !a.shared() {
        return surfaces_for_unknown(a);
    }
    if a.shared() && (a.share_url.is_empty() || !block_for(a).contains(&a.share_url)) {
        return surfaces_for_unknown(a);
    }
    let open = a.shared() || a.public();
    match (block_for(a).contains("!["), open) {
        (true, true) => EMBED_SURFACES,
        (true, false) => PRIVATE_EMBED_SURFACES,
        (false, true) => PLAIN_SURFACES,
        (false, false) => PRIVATE_PLAIN_SURFACES,
    }
    .into()
}

/// The honest label for the bare-link form.
pub fn link_surfaces_for(a: &Artifact) -> String {
    if a.shared() && a.share_url.is_empty() {
        return surfaces_for_unknown(a);
    }
    if a.public() || a.shared() {
        LINK_SURFACES.into()
    } else if a.private() {
        PRIVATE_LINK_SURFACES.into()
    } else {
        surfaces_for_unknown(a)
    }
}

/// Who may read it, said only when it is not the public default.
fn visibility_fact(a: &Artifact) -> &str {
    if a.public() { "" } else { &a.visibility }
}

/// One call left to make, spelled out well enough to run.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Breadcrumb {
    pub action: String,
    pub cmd: String,
    pub description: String,
}

fn crumb(action: &str, cmd: impl Into<String>, description: impl Into<String>) -> Breadcrumb {
    Breadcrumb { action: action.into(), cmd: cmd.into(), description: description.into() }
}

/// What every JSON result is wrapped in.
#[derive(Debug, Default, Serialize)]
pub struct Envelope {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paste: Option<Paste>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub summary: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub breadcrumbs: Vec<Breadcrumb>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<BTreeMap<String, Value>>,
}

fn ok(data: impl Serialize, summary: impl Into<String>, breadcrumbs: Vec<Breadcrumb>) -> String {
    encode(&Envelope {
        ok: true,
        data: Some(serde_json::to_value(data).expect("result serializes")),
        summary: summary.into(),
        breadcrumbs,
        ..Envelope::default()
    })
}

/// What one upload command produced.
#[derive(Debug, Clone, Default, Serialize)]
pub struct UploadResult {
    pub artifacts: Vec<Artifact>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run: Option<Run>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub title: String,
}

impl UploadResult {
    pub fn bytes(&self) -> i64 {
        self.artifacts.iter().map(|a| a.byte_size).sum()
    }
}

pub fn encode(v: &impl Serialize) -> String {
    serde_json::to_string_pretty(v).expect("output serializes")
}

/// A byte count the way the terminal output says it.
pub fn human_bytes(n: i64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let (mut f, mut i) = (n as f64, 0);
    while f >= 1024.0 && i < UNITS.len() - 1 {
        f /= 1024.0;
        i += 1;
    }
    if i > 0 && f < 10.0 {
        format!("{f:.1} {}", UNITS[i])
    } else {
        format!("{f:.0} {}", UNITS[i])
    }
}

fn parse_time(iso: &str) -> Option<jiff::Timestamp> {
    iso.parse().ok()
}

/// "expires in 24h", for an agent, which does better with a number.
pub fn relative_expiry(iso: &str, now: jiff::Timestamp) -> String {
    let Some(at) = parse_time(iso) else { return String::new() };
    let secs = at.duration_since(now).as_secs_f64();
    if secs <= 0.0 {
        return "expired".into();
    }
    let hours = (secs / 3600.0).round() as i64;
    if hours < 48 {
        return format!("expires in {hours}h");
    }
    format!("expires in {}d", (secs / 86400.0).round() as i64)
}

/// The same fact said the way a person would: "expires tomorrow" counts
/// calendar days in the local zone, not elapsed hours.
fn friendly_expiry(iso: &str, now: &jiff::Zoned) -> String {
    let Some(at) = parse_time(iso) else { return String::new() };
    let at = at.to_zoned(now.time_zone().clone());
    let secs = at.timestamp().duration_since(now.timestamp()).as_secs_f64();
    if secs <= 0.0 {
        return "expired".into();
    }
    if secs < 3600.0 {
        return format!("expires in {}", plural(((secs / 60.0).round() as i64).max(1), "minute"));
    }
    let days = (at.date() - now.date()).get_days();
    match days {
        0 => format!("expires in {}", plural((secs / 3600.0).round() as i64, "hour")),
        1 => "expires tomorrow".into(),
        d => format!("expires in {}", plural(d as i64, "day")),
    }
}

fn plural(n: i64, noun: &str) -> String {
    if n == 1 { format!("1 {noun}") } else { format!("{n} {noun}s") }
}

/// The form this destination wants, from the registry's table: markdown
/// unless the table says url, and markdown when nothing was served.
fn destination_form(r: &UploadResult, destination: &str) -> String {
    for a in &r.artifacts {
        if let Some(p) = &a.paste {
            let form = p.form_for(destination);
            if !form.is_empty() {
                return form.into();
            }
        }
    }
    "markdown".into()
}

/// The result the way the named tool wants it pasted.
pub fn destination(r: &UploadResult, destination: &str) -> String {
    if destination_form(r, destination) == "url" { url_result(r) } else { markdown_result(r) }
}

/// The one line for stderr when a bare card link was printed for a card no
/// keyless reader may open. Empty when there is nothing to warn about.
pub fn unfurl_warning(r: &UploadResult, f: Format, dest: &str) -> String {
    let bare = f == Format::Url || (!dest.is_empty() && destination_form(r, dest) == "url");
    if !bare {
        return String::new();
    }
    for a in &r.artifacts {
        if a.private() {
            return format!(
                "{} card link: it does not unfurl into a preview, and it opens only for a signed-in workspace member. The markdown form still shows the image",
                a.visibility
            );
        }
        if a.public() || (a.shared() && !a.share_url.is_empty()) {
            continue;
        }
        return format!(
            "{} card link: this krowk does not know who that reaches or whether it unfurls — upgrade before pasting it anywhere",
            a.visibility
        );
    }
    String::new()
}

/// A successful upload.
pub fn upload(r: &UploadResult, f: Format, quiet: bool, colour: bool, now: &jiff::Zoned) -> String {
    match f {
        Format::Markdown => markdown_result(r),
        Format::Url => url_result(r),
        Format::Human => human_result(r, quiet, colour, now),
        Format::Json if quiet => encode(r),
        Format::Json => encode(&upload_envelope(r, Vec::new())),
    }
}

fn upload_envelope(r: &UploadResult, extra: Vec<Breadcrumb>) -> Envelope {
    let mut breadcrumbs = crumbs_for(r);
    breadcrumbs.extend(extra);
    Envelope {
        ok: true,
        data: Some(serde_json::to_value(r).expect("result serializes")),
        paste: paste_for_result(r),
        summary: summary(r),
        breadcrumbs,
        error: None,
    }
}

fn paste_for_result(r: &UploadResult) -> Option<Paste> {
    if r.artifacts.is_empty() {
        return None;
    }
    let destinations = r
        .artifacts
        .iter()
        .filter_map(|a| a.paste.as_ref())
        .find(|p| !p.destinations.is_empty())
        .map(|p| p.destinations.clone())
        .unwrap_or_default();
    Some(Paste { markdown: markdown_result(r), url: url_result(r), destinations })
}

fn url_result(r: &UploadResult) -> String {
    r.artifacts.iter().map(card_for).collect::<Vec<_>>().join("\n")
}

/// Every artifact's block, a blank line apart: CommonMark folds consecutive
/// lines into one paragraph.
fn markdown_result(r: &UploadResult) -> String {
    r.artifacts.iter().map(block_for).collect::<Vec<_>>().join("\n\n")
}

fn summary(r: &UploadResult) -> String {
    let noun = if r.artifacts.len() == 1 { "artifact" } else { "artifacts" };
    let mut s = format!("{} {noun}, {}", r.artifacts.len(), human_bytes(r.bytes()));
    let run = summary_run(r);
    if !run.is_empty() {
        s += &format!(", run {run}");
    }
    s
}

/// The run the whole result went into: the one it opened, else the one every
/// artifact agrees on.
fn summary_run(r: &UploadResult) -> String {
    if let Some(run) = &r.run {
        return run.slug.clone();
    }
    let Some(first) = r.artifacts.first() else { return String::new() };
    let shared = first.run_slug();
    if r.artifacts.iter().all(|a| a.run_slug() == shared) { shared.into() } else { String::new() }
}

/// A claim per anonymous upload — a token belongs to exactly one — then a
/// share per artifact.
fn crumbs_for(r: &UploadResult) -> Vec<Breadcrumb> {
    let mut out: Vec<Breadcrumb> = r.artifacts.iter().filter(|a| !a.claim_token.is_empty()).map(claim_crumb).collect();
    out.extend(r.artifacts.iter().map(share_crumb));
    out
}

fn share_crumb(a: &Artifact) -> Breadcrumb {
    let description = if a.private() {
        "hand this link to a workspace member — it opens the card after they sign in to the app, and reads as not \
         found to everyone else. The image embeds anywhere: its byte URL is the capability, so paste the markdown \
         form outside the workspace only when the picture itself is meant to be seen"
            .to_string()
    } else if a.shared() {
        "hand this link on — whoever holds it opens the card with no key, so share it only with who should see it".into()
    } else if !a.public() {
        format!(
            "check who this link reaches before handing it on: its visibility is {}, which this krowk does not know how to describe",
            a.visibility
        )
    } else {
        "hand this link on — it is public and needs no key to read".into()
    };
    crumb("share", card_for(a), description)
}

/// The command that keeps one anonymous upload, its own token already in it.
pub fn claim_crumb(a: &Artifact) -> Breadcrumb {
    crumb(
        "keep past expiry",
        format!("krowk claim {} {}", a.slug, a.claim_token),
        "this upload is anonymous and expires within the day; claiming it with a key moves it into that key's \
         workspace — a Pro workspace keeps it, a free one gives it another 24 hours. The token is shown once and spent once",
    )
}

fn human_result(r: &UploadResult, quiet: bool, colour: bool, now: &jiff::Zoned) -> String {
    let mut lines = Vec::new();
    if let [a] = r.artifacts.as_slice() {
        lines.push(success(colour, &format!("Uploaded {} → {}", a.filename, card_for(a))));
    } else {
        lines.push(success(colour, &format!("Uploaded {} files", r.artifacts.len())));
        let width = r.artifacts.iter().map(|a| a.filename.len()).max().unwrap_or(0);
        for a in &r.artifacts {
            lines.push(format!("  {:<width$}  {}", a.filename, card_for(a)));
        }
    }
    let mut facts = vec![human_bytes(r.bytes())];
    if let Some(first) = r.artifacts.first() {
        let v = visibility_fact(first);
        if !v.is_empty() {
            facts.push(v.into());
        }
    }
    let run = summary_run(r);
    if !run.is_empty() {
        facts.push(format!("run {run}"));
    }
    if !r.title.is_empty() {
        facts.push(r.title.clone());
    }
    if let Some(first) = r.artifacts.first() {
        let e = friendly_expiry(&first.expires_at, now);
        if !e.is_empty() {
            facts.push(e);
        }
    }
    lines.push(paint(colour, DIM, &format!("  {}", facts.join(" · "))));
    for note in &r.notes {
        lines.push(format!("  {}", warning(colour, note)));
    }
    // The claim command is the one breadcrumb worth printing to a person: the
    // token is shown exactly once, by this response.
    for a in r.artifacts.iter().filter(|a| !quiet && !a.claim_token.is_empty()) {
        lines.push(crumb_line("Keep it", &claim_crumb(a).cmd, colour));
    }
    // The block goes last, so whoever copies the final thing shown copies it.
    let block = markdown_result(r);
    if !quiet && !block.is_empty() {
        lines.extend([String::new(), paint(colour, DIM, "  Paste this:"), block]);
    }
    lines.join("\n")
}

/// A breadcrumb for a person: the label dimmed and the command in cyan,
/// because the command is what gets selected and pasted.
pub fn crumb_line(label: &str, cmd: &str, colour: bool) -> String {
    format!("{} {}", paint(colour, DIM, &format!("  {label}:")), paint(colour, CYAN, cmd))
}

/// What scoped a page, so the next page's command is the same query.
#[derive(Debug, Clone, Default)]
pub struct Listing {
    pub run: String,
    pub limit: i64,
}

const NEXT_PAGE: &str = "this page came back full, so any older rows are behind that cursor";

fn next_page_cmd(cmd: &str, l: &Listing, next: &str) -> String {
    let mut cmd = cmd.to_string();
    if !l.run.is_empty() {
        cmd += &format!(" --run {}", l.run);
    }
    if l.limit != 0 {
        cmd += &format!(" --limit {}", l.limit);
    }
    format!("{cmd} --before {next}")
}

/// A page of artifacts.
pub fn list(p: &Page, l: &Listing, f: Format, quiet: bool, colour: bool, now: &jiff::Zoned) -> String {
    let as_result = || UploadResult { artifacts: p.artifacts.clone(), ..UploadResult::default() };
    match f {
        Format::Markdown => markdown_result(&as_result()),
        Format::Url => url_result(&as_result()),
        Format::Human => human_list(p, l, colour, now),
        Format::Json if quiet => encode(p),
        Format::Json => {
            let crumbs = if p.next.is_empty() {
                Vec::new()
            } else {
                vec![crumb("next page", next_page_cmd("krowk artifacts list", l, &p.next), NEXT_PAGE)]
            };
            ok(p, count(p.artifacts.len(), "artifact"), crumbs)
        }
    }
}

fn count(n: usize, noun: &str) -> String {
    if n == 1 { format!("1 {noun}") } else { format!("{n} {noun}s") }
}

fn human_list(p: &Page, l: &Listing, colour: bool, now: &jiff::Zoned) -> String {
    if p.artifacts.is_empty() {
        return paint(colour, DIM, "No artifacts.");
    }
    let name_w = p.artifacts.iter().map(|a| a.filename.len()).max().unwrap_or(0);
    let size_w = p.artifacts.iter().map(|a| human_bytes(a.byte_size).len()).max().unwrap_or(0);
    let mut lines = Vec::new();
    for a in &p.artifacts {
        let mut line = format!("{:<name_w$}  {:>size_w$}  {}", a.filename, human_bytes(a.byte_size), card_for(a));
        if a.state != "ready" {
            line += &paint(colour, DIM, &format!("  ({})", a.state));
        }
        let v = visibility_fact(a);
        if !v.is_empty() {
            line += &paint(colour, DIM, &format!("  {v}"));
        }
        let e = friendly_expiry(&a.expires_at, now);
        if !e.is_empty() {
            line += &paint(colour, DIM, &format!("  {e}"));
        }
        lines.push(line);
    }
    if !p.next.is_empty() {
        lines.push(crumb_line("More", &next_page_cmd("krowk artifacts list", l, &p.next), colour));
    }
    lines.join("\n")
}

/// One artifact that already exists — the upload's envelope, a reading line.
pub fn artifact(a: &Artifact, f: Format, quiet: bool, colour: bool, now: &jiff::Zoned) -> String {
    let result = UploadResult { artifacts: vec![a.clone()], ..UploadResult::default() };
    match f {
        Format::Human => human_artifact(a, colour, now),
        Format::Markdown => markdown_result(&result),
        Format::Url => card_for(a),
        Format::Json => upload(&result, f, quiet, colour, now),
    }
}

/// An artifact just claimed, plus what a claim leaves undone: a run.
pub fn claimed(a: &Artifact, f: Format, quiet: bool, colour: bool, now: &jiff::Zoned) -> String {
    if f == Format::Human {
        let mut said = human_claimed(a, colour);
        if !quiet && a.run_slug().is_empty() {
            said += &format!("\n{}", crumb_line("Group it", &attach_crumb(a).cmd, colour));
        }
        return said;
    }
    if !a.run_slug().is_empty() || quiet || f != Format::Json {
        return artifact(a, f, quiet, colour, now);
    }
    let result = UploadResult { artifacts: vec![a.clone()], ..UploadResult::default() };
    encode(&upload_envelope(&result, vec![attach_crumb(a)]))
}

fn attach_crumb(a: &Artifact) -> Breadcrumb {
    crumb(
        "group under a run",
        format!("krowk artifacts attach {} --run <run>", a.slug),
        "a claimed upload belongs to a workspace but to no run, and a run is where the pull request, commit and \
         session are recorded — `krowk runs start` opens one, and its slug goes in place of <run>",
    )
}

fn push_crumb(why: &str) -> Breadcrumb {
    crumb("push", "krowk push <file>", format!("{why}; <file> is the path to upload"))
}

/// A verified key and the workspace every call with it lands in.
pub fn key(k: &Key, f: Format, quiet: bool, colour: bool) -> String {
    if f != Format::Human {
        if quiet {
            return encode(k);
        }
        return ok(
            k,
            format!("{} in {}", k.key_id, k.workspace),
            vec![push_crumb("the key works, so an upload with it lands in that workspace and does not expire")],
        );
    }
    let workspace = if k.workspace_name.is_empty() { k.workspace.clone() } else { format!("{} ({})", k.workspace_name, k.workspace) };
    let mut lines = vec![success(colour, &format!("Key {} is valid", k.key_id)), format!("  {:<11} {workspace}", "Workspace")];
    if !k.name.is_empty() {
        lines.push(format!("  {:<11} {}", "Name", k.name));
    }
    if !k.expires_at.is_empty() {
        lines.push(format!("  {:<11} {}", "Expires", k.expires_at));
    }
    lines.join("\n")
}

/// What `auth login` stored, and whether the registry confirmed it.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Login {
    pub path: String,
    pub confirmed: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub key_id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub workspace: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub reason: String,
    /// KROWK_TOKEN outranks the file this login just wrote.
    #[serde(rename = "shadowed_by_env", skip_serializing_if = "std::ops::Not::not")]
    pub shadowed: bool,
}

fn shadowed_crumb() -> Breadcrumb {
    crumb(
        "verify",
        "krowk whoami",
        "KROWK_TOKEN outranks the key just stored, so this reports the one that uploads will really use — unset KROWK_TOKEN to use the stored one instead",
    )
}

pub fn stored_key(l: &Login, f: Format, quiet: bool, colour: bool) -> String {
    if f != Format::Human {
        if quiet {
            return encode(l);
        }
        let (mut summary, mut next) = (
            format!("{} stored, uploads land in {}", l.key_id, l.workspace),
            push_crumb("the key is stored and accepted, so nothing else is needed before uploading"),
        );
        if !l.confirmed {
            summary = format!("token stored, unconfirmed — {}", l.reason);
            next = crumb(
                "verify",
                "krowk whoami",
                "the token was written down but the registry never confirmed it, so whether it works is still unknown",
            );
        }
        if l.shadowed {
            summary += " — but KROWK_TOKEN is set and outranks it";
            next = shadowed_crumb();
        }
        return ok(l, summary, vec![next]);
    }
    let mut lines = if l.confirmed {
        vec![success(colour, &format!("Stored key {} in {}", l.key_id, l.path)), format!("  Uploads land in {}", l.workspace)]
    } else {
        vec![
            success(colour, &format!("Stored the token in {}", l.path)),
            format!("  {}", warning(colour, &format!("Not confirmed yet — {}", l.reason))),
            crumb_line("Check it once the registry is reachable", "krowk whoami", colour),
        ]
    };
    if l.shadowed {
        lines.push(format!(
            "  {}",
            warning(colour, "KROWK_TOKEN is set and wins over this file, so uploads use that key instead — unset it to use the one just stored")
        ));
    }
    lines.join("\n")
}

/// A browser login still waiting: the code to confirm and the page to confirm
/// it on. Not an envelope — `ok` is a verdict on a finished command.
#[derive(Debug, Clone, Serialize)]
pub struct Authorization {
    pub code: String,
    pub page: String,
    pub opened: bool,
}

pub fn authorizing(a: &Authorization, f: Format, colour: bool) -> String {
    if f == Format::Json {
        return encode(&json!({ "authorizing": a })) + "\n";
    }
    let head = if a.opened { "Your browser is opening — confirm the code there" } else { "Open this page and confirm the code" };
    [
        head.to_string(),
        format!("  {}  {}", paint(colour, DIM, "Code"), paint(colour, BOLD, &a.code)),
        format!("  {}  {}", paint(colour, DIM, "Page"), a.page),
        paint(colour, DIM, "  Waiting for approval… (Ctrl-C to stop)"),
        String::new(),
    ]
    .join("\n")
}

fn human_claimed(a: &Artifact, colour: bool) -> String {
    let mut facts = vec![human_bytes(a.byte_size)];
    if !a.run_slug().is_empty() {
        facts.push(format!("run {}", a.run_slug()));
    }
    facts.push("kept for good".into());
    format!(
        "{}\n{}",
        success(colour, &format!("Claimed {} → {}", a.filename, card_for(a))),
        paint(colour, DIM, &format!("  {}", facts.join(" · ")))
    )
}

fn human_artifact(a: &Artifact, colour: bool, now: &jiff::Zoned) -> String {
    let mut head = format!("{}  {}", a.filename, human_bytes(a.byte_size));
    if !a.state.is_empty() {
        head += &paint(colour, DIM, &format!("  {}", a.state));
    }
    let mut lines = vec![head, format!("  {}", card_for(a))];
    let mut facts = Vec::new();
    let v = visibility_fact(a);
    if !v.is_empty() {
        facts.push(v.to_string());
    }
    if !a.run_slug().is_empty() {
        facts.push(format!("run {}", a.run_slug()));
    }
    let e = friendly_expiry(&a.expires_at, now);
    if !e.is_empty() {
        facts.push(e);
    }
    if !facts.is_empty() {
        lines.push(paint(colour, DIM, &format!("  {}", facts.join(" · "))));
    }
    lines.join("\n")
}

/// A completed takedown: the slug and the fact, since no artifact is left.
pub fn removed(slug: &str, f: Format, quiet: bool, colour: bool) -> String {
    if f != Format::Human {
        let data = json!({ "slug": slug, "taken_down": true });
        return if quiet { encode(&data) } else { ok(data, format!("{slug} taken down"), Vec::new()) };
    }
    format!(
        "{}\n{}",
        success(colour, &format!("Took {slug} down")),
        paint(colour, DIM, "  The bytes are gone for good, and the link now says so.")
    )
}

pub const STATUS_FINISHED: &str = "finished";

/// A run, for `runs start` and `runs finish`: what just happened to it.
pub fn run(r: &Run, f: Format, quiet: bool, colour: bool) -> String {
    match f {
        Format::Human | Format::Markdown => success(
            colour,
            &match r.status.as_str() {
                STATUS_FINISHED => format!("Finished run {}", r.slug),
                "open" => format!("Started run {}", r.slug),
                s => format!("Run {} is {s}", r.slug),
            },
        ),
        _ if quiet => encode(r),
        _ => ok(r, format!("run {} is {}", r.slug, r.status), run_crumbs(r, true)),
    }
}

/// The calls a run leaves: feed it or read it while open, read it once closed.
fn run_crumbs(r: &Run, with_push: bool) -> Vec<Breadcrumb> {
    let artifacts = crumb(
        "what it made",
        format!("krowk artifacts list --run {}", r.slug),
        "a run holds the metadata; its artifacts and their links are listed separately",
    );
    if r.status == STATUS_FINISHED {
        return vec![artifacts];
    }
    let first = if with_push {
        crumb(
            "attach uploads",
            format!("krowk push <file> --run {}", r.slug),
            "every push naming this run is grouped under it and inherits the metadata recorded on it; <file> is the path to upload",
        )
    } else {
        artifacts
    };
    vec![first, crumb("close", format!("krowk runs finish {}", r.slug), "marks the run finished; a run stays open until something says so")]
}

/// A page of runs.
pub fn run_list(p: &RunPage, l: &Listing, f: Format, quiet: bool, colour: bool) -> String {
    let l = Listing { run: String::new(), ..l.clone() };
    if f != Format::Human {
        if quiet {
            return encode(p);
        }
        let crumbs = if p.next.is_empty() {
            Vec::new()
        } else {
            vec![crumb("next page", next_page_cmd("krowk runs list", &l, &p.next), NEXT_PAGE)]
        };
        return ok(p, count(p.runs.len(), "run"), crumbs);
    }
    if p.runs.is_empty() {
        return paint(colour, DIM, "No runs.");
    }
    let slug_w = p.runs.iter().map(|r| r.slug.len()).max().unwrap_or(0);
    let status_w = p.runs.iter().map(|r| r.status.len()).max().unwrap_or(0);
    let mut lines: Vec<String> = p
        .runs
        .iter()
        .map(|r| {
            let mut line = format!("{:<slug_w$}  {:<status_w$}", r.slug, r.status);
            let label = run_label(r);
            if !label.is_empty() {
                line += &format!("  {label}");
            }
            line.trim_end().to_string()
        })
        .collect();
    if !p.next.is_empty() {
        lines.push(crumb_line("More", &next_page_cmd("krowk runs list", &l, &p.next), colour));
    }
    lines.join("\n")
}

/// One run and everything recorded on it.
pub fn run_detail(r: &Run, f: Format, quiet: bool, colour: bool) -> String {
    if f != Format::Human {
        if quiet {
            return encode(r);
        }
        return ok(r, format!("run {} is {}", r.slug, r.status), run_crumbs(r, false));
    }
    let mut lines = vec![format!("{}  {}", r.slug, paint(colour, DIM, &r.status))];
    if !r.started_at.is_empty() {
        lines.push(format!("  {:<13} {}", "Started", r.started_at));
    }
    if !r.finished_at.is_empty() {
        lines.push(format!("  {:<13} {}", "Finished", r.finished_at));
    }
    // Metadata is whatever the caller recorded, printed as it arrived; sorted
    // so the same run always prints the same way.
    let fields = run_fields(r);
    let mut keys: Vec<&String> = fields.keys().collect();
    keys.sort();
    for k in keys {
        lines.push(format!("  {k:<13} {}", metadata_value(&fields[k])));
    }
    lines.join("\n")
}

/// One recorded value for a person: scalars as themselves, a list of scalars
/// on one line, anything deeper as the JSON it arrived as.
fn metadata_value(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Array(items) if !items.is_empty() && items.iter().all(|i| matches!(i, Value::String(_) | Value::Bool(_) | Value::Number(_))) => {
            items.iter().map(metadata_value).collect::<Vec<_>>().join("; ")
        }
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

fn run_fields(r: &Run) -> serde_json::Map<String, Value> {
    match &r.metadata {
        Some(Value::Object(m)) => m.clone(),
        _ => serde_json::Map::new(),
    }
}

/// The most identifying thing a run knows about itself, on one row.
fn run_label(r: &Run) -> String {
    let fields = run_fields(r);
    let text = |k: &str| fields.get(k).and_then(Value::as_str).map(crate::termclean::cell).unwrap_or_default();
    let first = |keys: &[&str]| keys.iter().map(|k| text(k)).find(|v| !v.is_empty()).unwrap_or_default();
    let title = first(&["vcs.change.title", "title"]);
    let change = first(&["krowk.change.url", "pull_request"]);
    let repo = first(&["vcs.repository.name", "repo"]);
    let branch = first(&["vcs.ref.head.name", "branch"]);
    let label = if !title.is_empty() {
        title
    } else if !change.is_empty() {
        change
    } else if !repo.is_empty() && !branch.is_empty() {
        format!("{repo}@{branch}")
    } else if !repo.is_empty() {
        repo
    } else {
        first(&["krowk.harness", "agent"])
    };
    clip_label(&label)
}

const MAX_LABEL_RUNES: usize = 72;

fn clip_label(s: &str) -> String {
    if s.chars().count() <= MAX_LABEL_RUNES {
        return s.into();
    }
    s.chars().take(MAX_LABEL_RUNES).collect::<String>() + "…"
}

/// A failure. JSON hands back the body as it is; a person gets the fix as a
/// headline, what else it says under it, each command it names on a line of
/// its own, and the code last, for a search or a bug report.
pub fn error(err: &Error, f: Format, quiet: bool, colour: bool) -> String {
    let mut body = err.body.clone();
    if err.status != 0 {
        body.insert("status".into(), json!(err.status));
    }
    if f == Format::Json {
        return if quiet { encode(&body) } else { encode(&Envelope { ok: false, error: Some(body), ..Envelope::default() }) };
    }
    let code = body.get("error").and_then(Value::as_str).unwrap_or_default().to_string();
    let said = fix::fix_lines(body.get("fix").and_then(Value::as_str).unwrap_or_default());
    // A fix that opens on its command has no sentence before it: the code,
    // said as words, stands in.
    let head = said.first().map(|l| l.say.clone()).filter(|s| !s.is_empty()).unwrap_or_else(|| fix::sentence(&code.replace('_', " ")));
    let mut lines = vec![format!("{} {}", paint(colour, RED, "✗"), paint(colour, BOLD, &head))];
    for (k, v) in &body {
        if matches!(k.as_str(), "error" | "fix" | "retryable" | "status") {
            continue;
        }
        if let (Value::Object(fields), "details") = (v, k.as_str()) {
            let mut names: Vec<&String> = fields.keys().collect();
            names.sort();
            for name in names {
                lines.push(paint(colour, DIM, &format!("  {}: {}", fix::capitalised(name), join_values(&fields[name]))));
            }
            continue;
        }
        lines.push(paint(colour, DIM, &format!("  {}: {}", fix::capitalised(k), plain_value(v))));
    }
    for (i, line) in said.iter().enumerate() {
        if i > 0 && !line.say.is_empty() {
            lines.push(format!("  {}", line.say));
        }
        if !line.cmd.is_empty() {
            lines.push(crumb_line("Try", &line.cmd, colour));
        }
        if !line.then.is_empty() {
            lines.push(paint(colour, DIM, &format!("  {}", line.then)));
        }
    }
    if body.get("retryable") == Some(&Value::Bool(true)) {
        lines.push(paint(colour, DIM, "  This may pass if retried."));
    }
    let status = if err.status != 0 { format!(" · HTTP {}", err.status) } else { String::new() };
    lines.push(paint(colour, DIM, &format!("  Code: {code}{status}")));
    lines.join("\n")
}

fn plain_value(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

fn join_values(v: &Value) -> String {
    match v {
        Value::Array(items) => items.iter().map(plain_value).collect::<Vec<_>>().join("; "),
        other => plain_value(other),
    }
}

pub(crate) const BOLD: &str = "1";
pub(crate) const DIM: &str = "2";
pub(crate) const GREEN: &str = "32";
pub(crate) const RED: &str = "31";
pub(crate) const YELLOW: &str = "33";
const CYAN: &str = "36";

pub fn paint(colour: bool, code: &str, s: &str) -> String {
    if colour { format!("\x1b[{code}m{s}\x1b[0m") } else { s.to_string() }
}

/// Whether a stream gets colour: a terminal, unless NO_COLOR is set
/// (no-color.org) or the terminal says it cannot show any.
pub fn colour_for(tty: bool, env: &dyn Fn(&str) -> String) -> bool {
    tty && env("NO_COLOR").is_empty() && env("TERM") != "dumb"
}

/// Something that worked: a green tick, then what happened.
pub fn success(colour: bool, said: &str) -> String {
    format!("{} {said}", paint(colour, GREEN, "✓"))
}

/// Something worth knowing that stopped nothing: a yellow bang, then the
/// sentence, capitalised.
pub fn warning(colour: bool, said: &str) -> String {
    format!("{} {}", paint(colour, YELLOW, "!"), fix::capitalised(said))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(iso: &str) -> jiff::Zoned {
        iso.parse::<jiff::Timestamp>().unwrap().to_zoned(jiff::tz::TimeZone::UTC)
    }

    #[test]
    fn bytes_and_expiries_read_the_way_a_person_says_them() {
        assert_eq!((human_bytes(5), human_bytes(2048), human_bytes(20 << 20)), ("5 B".into(), "2.0 KB".into(), "20 MB".into()));
        let now = at("2026-09-24T23:00:00Z");
        assert_eq!(friendly_expiry("2026-09-25T22:00:00Z", &now), "expires tomorrow");
        assert_eq!(friendly_expiry("2026-09-24T23:30:00Z", &now), "expires in 30 minutes");
        assert_eq!(friendly_expiry("2026-09-24T20:00:00Z", &now), "expired");
        assert_eq!(friendly_expiry("2026-09-28T23:00:00Z", &now), "expires in 4 days");
        assert_eq!(relative_expiry("2026-09-25T23:00:00Z", now.timestamp()), "expires in 24h");
    }

    #[test]
    fn colour_is_for_a_terminal_that_wants_it() {
        let env = |pairs: &'static [(&'static str, &'static str)]| move |k: &str| pairs.iter().find(|(n, _)| *n == k).map_or(String::new(), |(_, v)| v.to_string());
        assert!(colour_for(true, &env(&[("TERM", "xterm")])));
        assert!(!colour_for(false, &env(&[])));
        assert!(!colour_for(true, &env(&[("NO_COLOR", "1")])));
        assert!(!colour_for(true, &env(&[("TERM", "dumb")])));
    }

    #[test]
    fn format_resolution_refuses_a_misspelling_even_under_json() {
        assert_eq!(resolve_format("", false, true).unwrap(), Format::Human);
        assert_eq!(resolve_format("", false, false).unwrap(), Format::Json);
        assert_eq!(resolve_format("markdown", true, true).unwrap(), Format::Json);
        assert_eq!(resolve_format("yaml", true, true).unwrap_err().code(), "bad_format");
    }

    #[test]
    fn a_keyless_upload_carries_a_claim_crumb_per_artifact_and_quiet_drops_them() {
        let a = |slug: &str| Artifact { slug: slug.into(), claim_token: format!("krowk_claim_{slug}"), url: format!("https://k/a/{slug}"), ..Artifact::default() };
        let r = UploadResult { artifacts: vec![a("art_1"), a("art_2")], ..UploadResult::default() };
        let env: Value = serde_json::from_str(&upload(&r, Format::Json, false, false, &at("2026-01-01T00:00:00Z"))).unwrap();
        let actions: Vec<&str> = env["breadcrumbs"].as_array().unwrap().iter().map(|c| c["action"].as_str().unwrap()).collect();
        assert_eq!(actions, ["keep past expiry", "keep past expiry", "share", "share"]);
        let human = upload(&r, Format::Human, true, false, &at("2026-01-01T00:00:00Z"));
        assert!(!human.contains("krowk claim") && !human.contains("paste this"));
    }

    #[test]
    fn a_failure_reads_as_its_fix_with_the_command_on_its_own_line() {
        let e = fail("not_authenticated", "no key to verify — run `krowk login --token krowk_sk_...`, or upload anonymously");
        assert_eq!(
            error(&e, Format::Human, false, false),
            "✗ No key to verify.\n  Try: krowk login --token krowk_sk_...\n  Or upload anonymously.\n  Code: not_authenticated"
        );
        let json: Value = serde_json::from_str(&error(&e, Format::Json, false, false)).unwrap();
        // No sentence before the command: the code stands in, and a status
        // is said even with no fix at all.
        let lead = fail("not_authenticated", "run `krowk login`, or upload anonymously");
        assert!(error(&lead, Format::Human, false, false).starts_with("✗ Not authenticated.\n  Try: krowk login\n  Or upload anonymously."));
        let bare = Error { status: 502, ..fail("internal_server_error", "") };
        assert_eq!(error(&bare, Format::Human, false, false), "✗ Internal server error.\n  Code: internal_server_error · HTTP 502");
        assert_eq!((json["ok"].as_bool(), json["error"]["error"].as_str()), (Some(false), Some("not_authenticated")));
    }
}
