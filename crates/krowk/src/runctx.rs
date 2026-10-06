//! Where the work came from: the repository, commit, branch and dirty flag,
//! the agent and its session, the pull request — detected from git and the
//! environment, and overridden by whatever the caller said.

use serde::Serialize;
use std::path::Path;
use std::process::Command;

/// The process environment, as the CLI reads it.
pub type Env<'a> = &'a dyn Fn(&str) -> String;

/// The run context, keyed by the OpenTelemetry names canon adopts. Empty
/// fields are left out: an absent key and an empty one mean the same, and
/// the absent one is shorter on every card.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Metadata {
    #[serde(rename = "vcs.repository.name", skip_serializing_if = "String::is_empty")]
    pub repo_name: String,
    #[serde(rename = "vcs.repository.url.full", skip_serializing_if = "String::is_empty")]
    pub repo_url: String,
    #[serde(rename = "vcs.ref.head.revision", skip_serializing_if = "String::is_empty")]
    pub commit: String,
    #[serde(rename = "vcs.ref.head.name", skip_serializing_if = "String::is_empty")]
    pub branch: String,
    /// None outside a git checkout, and where it cannot be told without
    /// running a repository's filters — the distinction a plain bool cannot
    /// carry.
    #[serde(rename = "krowk.vcs.dirty", skip_serializing_if = "Option::is_none")]
    pub dirty: Option<bool>,
    #[serde(rename = "krowk.harness", skip_serializing_if = "String::is_empty")]
    pub harness: String,
    #[serde(rename = "gen_ai.request.model", skip_serializing_if = "String::is_empty")]
    pub model: String,
    #[serde(rename = "gen_ai.system", skip_serializing_if = "String::is_empty")]
    pub system: String,
    #[serde(rename = "krowk.client", skip_serializing_if = "String::is_empty")]
    pub client: String,
    #[serde(rename = "vcs.change.id", skip_serializing_if = "String::is_empty")]
    pub change_id: String,
    #[serde(rename = "vcs.change.title", skip_serializing_if = "String::is_empty")]
    pub change_title: String,
    #[serde(rename = "krowk.change.url", skip_serializing_if = "String::is_empty")]
    pub change_url: String,
    #[serde(rename = "krowk.session", skip_serializing_if = "String::is_empty")]
    pub session: String,
    #[serde(rename = "krowk.links", skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<Link>,
    #[serde(rename = "krowk.references", skip_serializing_if = "Vec::is_empty")]
    pub references: Vec<String>,
    /// The slug the origin remote names, kept to judge whether an overridden
    /// repository still matches the URL detected for it.
    #[serde(skip)]
    remote_slug: String,
}

/// A link the work is about: the issue, the spec, the discussion.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Link {
    pub url: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub title: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub rel: String,
}

pub const MAX_LINKS: usize = 20;
pub const MAX_LINK_URL: usize = 2048;
pub const MAX_LINK_TITLE: usize = 140;
pub const MAX_LINK_REL: usize = 64;
pub const MAX_LINKS_BYTES: usize = 8192;

/// The rels a reader recognises; any other word is recorded as given.
pub const LINK_RELS: &[&str] = &["tracks", "fixes", "spec", "discussion", "source", "supersedes"];

/// Refuses links a card could not show honestly: too many, too long, not an
/// absolute http(s) URL, carrying credentials, or text that repaints a row.
pub fn validate_links(links: &[Link]) -> Result<(), String> {
    if links.len() > MAX_LINKS {
        return Err(format!(
            "{} links is more than the {MAX_LINKS} a run records: link what the work is about, not everything it touched",
            links.len()
        ));
    }
    for (i, l) in links.iter().enumerate() {
        let at = format!("link {}", i + 1);
        validate_link_url(&at, &l.url)?;
        validate_link_line(&at, "title", &l.title, MAX_LINK_TITLE)?;
        validate_link_line(&at, "rel", &l.rel, MAX_LINK_REL)?;
    }
    let bytes = if links.is_empty() { 0 } else { serde_json::to_string(links).map_or(0, |s| s.len()) };
    if bytes > MAX_LINKS_BYTES {
        return Err(format!(
            "the links come to {bytes} bytes of metadata, past the {MAX_LINKS_BYTES} they may fill: a run's metadata budget is shared with everything krowk detects"
        ));
    }
    Ok(())
}

fn validate_link_line(at: &str, field: &str, value: &str, max: usize) -> Result<(), String> {
    let n = value.chars().count();
    if n > max {
        return Err(format!("{at} has a {n}-character {field}, past the {max} one line holds"));
    }
    if value.chars().any(display_hostile) {
        return Err(format!(
            "{at} has a {field} that would repaint the row it is drawn on — a line break, a control character or a bidi override: it is what a reader sees instead of the URL, on one row"
        ));
    }
    Ok(())
}

/// What a single row must not carry: controls, line and paragraph
/// separators, and the invisible characters that reorder text.
fn display_hostile(c: char) -> bool {
    c != '\u{200d}' && (c.is_control() || matches!(c, '\u{2028}' | '\u{2029}' | '\u{200b}'..='\u{200f}') || crate::termclean::reordering(c))
}

fn validate_link_url(at: &str, raw: &str) -> Result<(), String> {
    if raw.trim().is_empty() {
        return Err(format!("{at} has no url: every link needs an absolute http(s) URL"));
    }
    if raw.chars().any(|c| c == ' ' || display_hostile(c)) {
        return Err(format!(
            "{at} ({raw:?}) has a space, a control character or a bidi override in it: a URL is one unbroken word, and one that reads as it resolves"
        ));
    }
    let n = raw.chars().count();
    if n > MAX_LINK_URL {
        return Err(format!("{at} is {n} characters, past the {MAX_LINK_URL} a URL may be"));
    }
    let not_url = || format!("{at} ({raw}) is not an absolute http(s) URL — a ticket key or an internal ID belongs in references, not in links");
    let (scheme, rest) = raw.split_once("://").ok_or_else(not_url)?;
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return Err(not_url());
    }
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    if host.is_empty() {
        return Err(not_url());
    }
    if authority.contains('@') {
        return Err(format!(
            "{at} carries a username or password in the URL, and run metadata is public and permanent: link {scheme}://{host}{tail} instead"
        ));
    }
    Ok(())
}

/// What the caller said, which wins over what was detected.
#[derive(Debug, Clone, Default)]
pub struct Overrides {
    pub repo: String,
    pub commit: String,
    pub agent: String,
    pub pull_request: String,
    pub links: Vec<Link>,
    pub references: Vec<String>,
    pub session: String,
    pub title: String,
    pub client: String,
}

/// Detection with the caller's overrides applied.
pub fn resolve(env: Env, o: Overrides) -> Metadata {
    resolve_in(env, o, None)
}

/// `resolve`, with git asked about `dir` rather than the working directory:
/// a harness session's own, which a resumed session need not share.
pub fn resolve_in(env: Env, o: Overrides, dir: Option<&Path>) -> Metadata {
    let mut m = detect_in(env, dir);
    let over = |dst: &mut String, v: String| {
        if !v.is_empty() {
            *dst = v;
        }
    };
    over(&mut m.repo_name, o.repo);
    over(&mut m.commit, o.commit);
    over(&mut m.harness, o.agent);
    over(&mut m.change_url, o.pull_request);
    over(&mut m.session, o.session);
    m.links = o.links;
    m.references = o.references;
    m.change_title = o.title;
    m.client = o.client;
    m.system = detect_system(&m.harness, &m.model).into();
    m.change_id = change_id(&m.change_url);
    m.reconcile_repo();
    m
}

/// What git and the environment say, with nothing overridden.
pub fn detect(env: Env) -> Metadata {
    detect_in(env, None)
}

fn detect_in(env: Env, dir: Option<&Path>) -> Metadata {
    let remote = git(dir, &["remote", "get-url", "origin"]);
    let mut m = Metadata {
        repo_name: first(&[env("GITHUB_REPOSITORY"), slug(&remote)]),
        repo_url: repo_url(env, &remote),
        commit: first(&[env("GITHUB_SHA"), git(dir, &["rev-parse", "HEAD"])]),
        branch: branch_in(env, dir),
        dirty: dirty(dir),
        harness: detect_agent(env),
        model: first(&[env("KROWK_MODEL"), env("ANTHROPIC_MODEL")]),
        session: first(&[env("KROWK_SESSION"), env("CLAUDE_CODE_SESSION_ID"), env("CURSOR_TRACE_ID"), env("GITHUB_RUN_ID")]),
        change_url: ci_pull_request(env),
        remote_slug: slug(&remote),
        ..Metadata::default()
    };
    m.system = detect_system(&m.harness, &m.model).into();
    m.change_id = change_id(&m.change_url);
    m.reconcile_repo();
    m
}

impl Metadata {
    /// The production record stamped on each artifact: the work-level facts
    /// — the change, the session, the links — live on the run.
    pub fn artifact(&self) -> Metadata {
        Metadata {
            change_id: String::new(),
            change_title: String::new(),
            change_url: String::new(),
            session: String::new(),
            links: Vec::new(),
            references: Vec::new(),
            ..self.clone()
        }
    }

    /// The record with the caller's --metadata pairs on top; theirs wins.
    pub fn with_extras(&self, extras: &[(String, String)]) -> serde_json::Value {
        let mut v = serde_json::to_value(self).expect("metadata serializes");
        if let serde_json::Value::Object(map) = &mut v {
            for (k, val) in extras {
                map.insert(k.clone(), serde_json::Value::String(val.clone()));
            }
        }
        v
    }

    /// A repository named by override is not the one the detected URL points
    /// at, so the URL goes rather than contradict it.
    fn reconcile_repo(&mut self) {
        if !self.remote_slug.is_empty() && self.repo_name != self.remote_slug {
            self.repo_url.clear();
        }
    }
}

fn first(values: &[String]) -> String {
    values.iter().find(|v| !v.is_empty()).cloned().unwrap_or_default()
}

/// One git query. A repository's config names commands git runs on its
/// own — `core.fsmonitor` on `status` above all — and the repository may be
/// one a model was handed, so none is run and no lock is taken
/// (`krowk_api::git`).
fn git_cmd(dir: Option<&Path>) -> Command {
    krowk_api::git::query(dir.unwrap_or(Path::new(".")))
}

fn git(dir: Option<&Path>, args: &[&str]) -> String {
    git_cmd(dir)
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

/// The branch: CI's word first, then git's, and none on a detached head.
pub fn branch(env: Env) -> String {
    branch_in(env, None)
}

fn branch_in(env: Env, dir: Option<&Path>) -> String {
    let head_ref = env("GITHUB_HEAD_REF");
    if !head_ref.is_empty() {
        return head_ref;
    }
    let ref_name = env("GITHUB_REF_NAME");
    if !ref_name.is_empty() && env("GITHUB_REF_TYPE") == "branch" {
        return ref_name;
    }
    let b = git(dir, &["rev-parse", "--abbrev-ref", "HEAD"]);
    if b == "HEAD" { String::new() } else { b }
}

/// The model vendor, from the model's name or, failing that, the harness.
pub fn detect_system(harness: &str, model: &str) -> &'static str {
    if model.starts_with("claude") {
        "anthropic"
    } else if model.starts_with("gpt") {
        "openai"
    } else if model.starts_with("gemini") {
        "gcp.gemini"
    } else if harness == "claude-code" {
        "anthropic"
    } else {
        ""
    }
}

/// The harness driving the upload.
pub fn detect_agent(env: Env) -> String {
    let agent = env("KROWK_AGENT");
    if !agent.is_empty() {
        agent
    } else if !env("CLAUDECODE").is_empty() || !env("CLAUDE_CODE").is_empty() {
        "claude-code".into()
    } else if !env("CURSOR_TRACE_ID").is_empty() {
        "cursor".into()
    } else if !env("GITHUB_ACTIONS").is_empty() {
        "github-actions".into()
    } else {
        String::new()
    }
}

/// The web URL of the repository. An https remote names it directly, with
/// any credentials in it dropped — CI clones as https://x-access-token:<token>@…,
/// and this lands in public metadata. An ssh remote gets one only where the
/// host's web shape is known.
pub fn repo_url(env: Env, remote: &str) -> String {
    if remote.starts_with("http://") || remote.starts_with("https://") {
        let clean = without_userinfo(remote);
        let clean = clean.trim_end_matches('/');
        return clean.strip_suffix(".git").unwrap_or(clean).to_string();
    }
    let slug = slug(remote);
    if slug.is_empty() {
        return String::new();
    }
    let mut server = env("GITHUB_SERVER_URL");
    if server.is_empty() && host(remote) == "github.com" {
        server = "https://github.com".into();
    }
    if server.is_empty() {
        return String::new();
    }
    format!("{}/{slug}", server.trim_end_matches('/'))
}

fn without_userinfo(remote: &str) -> String {
    let (scheme, rest) = remote.split_once("://").unwrap_or((remote, ""));
    let (authority, path) = match rest.split_once('/') {
        Some((a, p)) => (a, Some(p)),
        None => (rest, None),
    };
    let authority = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    match path {
        Some(p) => format!("{scheme}://{authority}/{p}"),
        None => format!("{scheme}://{authority}"),
    }
}

/// `owner/name` out of any remote spelling.
pub fn slug(remote: &str) -> String {
    static RE: std::sync::LazyLock<regex_lite::Regex> =
        std::sync::LazyLock::new(|| regex_lite::Regex::new(r"[:/]([^/:]+/[^/]+?)(?:\.git)?/?$").unwrap());
    RE.captures(remote).map(|c| c[1].to_string()).unwrap_or_default()
}

fn host(remote: &str) -> String {
    static RE: std::sync::LazyLock<regex_lite::Regex> =
        std::sync::LazyLock::new(|| regex_lite::Regex::new(r"^(?:[a-z+]+://)?(?:[^@/]+@)?([^/:]+)[:/]").unwrap());
    RE.captures(remote).map(|c| c[1].to_string()).unwrap_or_default()
}

/// The change's number, from its URL.
pub fn change_id(url: &str) -> String {
    static RE: std::sync::LazyLock<regex_lite::Regex> =
        std::sync::LazyLock::new(|| regex_lite::Regex::new(r"/(\d+)/?$").unwrap());
    RE.captures(url).map(|c| c[1].to_string()).unwrap_or_default()
}

/// refs/pull/412/merge as the pull request's URL.
pub fn ci_pull_request(env: Env) -> String {
    static RE: std::sync::LazyLock<regex_lite::Regex> =
        std::sync::LazyLock::new(|| regex_lite::Regex::new(r"^refs/pull/(\d+)/").unwrap());
    let repo = env("GITHUB_REPOSITORY");
    match RE.captures(&env("GITHUB_REF")) {
        Some(c) if !repo.is_empty() => format!("https://github.com/{repo}/pull/{}", &c[1]),
        _ => String::new(),
    }
}

/// Whether the work tree differs from HEAD. `git status` reads a file's
/// content when its stat data cannot settle it — a racily clean entry, a
/// touched one — and reading it runs the `filter.<driver>.clean` (or
/// `.process`) command `.gitattributes` maps it to: a command any
/// repository can name. No work-tree comparison git offers avoids that
/// read (measured: `status`, `diff-files`, `diff-index HEAD` and
/// `ls-files -m` all run a clean filter on a racy entry), so where any
/// filter is configured — the repository's, the person's, git-lfs's — the
/// answer is unknown rather than a command run. Submodules, whose own
/// config is another repository's, are not looked into.
fn dirty(dir: Option<&Path>) -> Option<bool> {
    let filters = git_cmd(dir).args(["config", "--get-regexp", r"^filter\."]).output().ok()?;
    // 1: no filter is configured; 0: some are; anything else, no answer.
    match filters.status.code() {
        Some(1) => {}
        _ => return None,
    }
    let out = git_cmd(dir).args(["status", "--porcelain", "--ignore-submodules=all"]).output().ok().filter(|o| o.status.success())?;
    Some(!String::from_utf8_lossy(&out.stdout).trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> String {
        let pairs: Vec<(String, String)> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |k| pairs.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone()).unwrap_or_default()
    }

    #[test]
    fn repo_url_names_the_web_page_and_never_the_credentials() {
        let none = env(&[]);
        for (remote, want) in [
            ("https://github.com/acme/storefront.git", "https://github.com/acme/storefront"),
            ("https://git.acme.dev/acme/storefront/", "https://git.acme.dev/acme/storefront"),
            ("git@github.com:acme/storefront.git", "https://github.com/acme/storefront"),
            ("git@gitlab.com:acme/storefront.git", ""),
            ("", ""),
            ("https://x-access-token:ghs_secret@github.com/acme/storefront.git", "https://github.com/acme/storefront"),
            ("https://user@git.acme.dev/acme/storefront", "https://git.acme.dev/acme/storefront"),
            ("https://a:b@c@github.com", "https://github.com"),
        ] {
            assert_eq!(repo_url(&none, remote), want, "{remote}");
        }
        let ghes = env(&[("GITHUB_SERVER_URL", "https://github.acme.dev/")]);
        assert_eq!(repo_url(&ghes, "git@github.acme.dev:acme/storefront.git"), "https://github.acme.dev/acme/storefront");
    }

    #[test]
    fn change_ids_and_ci_pull_requests_come_from_their_urls() {
        assert_eq!(change_id("https://github.com/acme/storefront/pull/412"), "412");
        assert_eq!(change_id("https://gitlab.com/acme/storefront/-/merge_requests/7/"), "7");
        assert_eq!(change_id("https://github.com/acme/storefront"), "");
        let ci = env(&[("GITHUB_REF", "refs/pull/42/merge"), ("GITHUB_REPOSITORY", "o/r")]);
        assert_eq!(ci_pull_request(&ci), "https://github.com/o/r/pull/42");
    }

    /// A repository a model was handed can name a command in its config;
    /// detection runs none of it, and reads the directory it is pointed at.
    #[test]
    fn detection_runs_nothing_from_the_repositorys_config_and_reads_the_directory_named() {
        let d = std::env::temp_dir().join(format!("krowk-runctx-fsmonitor-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let git = |args: &[&str]| Command::new("git").args(args).current_dir(&d).status().is_ok_and(|s| s.success());
        if !git(&["init", "-q"]) {
            eprintln!("git is not installed: skipping");
            return;
        }
        std::fs::write(d.join("a.txt"), "x\n").unwrap();
        let marker = d.join("fsmonitor-ran");
        let config = std::fs::read_to_string(d.join(".git/config")).unwrap();
        std::fs::write(d.join(".git/config"), format!("{config}[core]\n\tfsmonitor = \"touch '{}'; false\"\n", marker.display())).unwrap();
        // A filter in the person's own config (git-lfs's) makes it unknown
        // anywhere; the machine running this may have one.
        let own_filters = Command::new("git").args(["config", "--global", "--get-regexp", r"^filter\."]).output().is_ok_and(|o| o.status.success())
            || Command::new("git").args(["config", "--system", "--get-regexp", r"^filter\."]).output().is_ok_and(|o| o.status.success());
        let m = detect_in(&env(&[]), Some(&d));
        assert_eq!(m.dirty, if own_filters { None } else { Some(true) }, "read from the directory named, not the process's");
        assert!(!marker.exists(), "the repository's fsmonitor ran");

        // A clean (or process, or smudge) filter .gitattributes maps a file
        // to: git runs it to read a racily clean or touched file, so with
        // one configured dirtiness is unknown and nothing runs.
        let g = |args: &[&str]| Command::new("git").args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "core.fsmonitor=false"]).args(args).current_dir(&d).status().is_ok_and(|s| s.success());
        std::fs::write(d.join(".gitattributes"), "*.txt filter=evil\n").unwrap();
        assert!(g(&["add", "-A"]) && g(&["commit", "-qm", "x"]));
        for (key, cmd) in [("clean", "cat"), ("process", "false"), ("smudge", "cat")] {
            let mut config = std::fs::read_to_string(d.join(".git/config")).unwrap();
            config = config.split("[filter \"evil\"]").next().unwrap().to_string();
            std::fs::write(d.join(".git/config"), format!("{config}[filter \"evil\"]\n\t{key} = \"touch '{}'; {cmd}\"\n", marker.display())).unwrap();
            // Rewritten in the same second the index was: racily clean.
            std::fs::write(d.join("a.txt"), "x\n").unwrap();
            assert_eq!(detect_in(&env(&[]), Some(&d)).dirty, None, "{key}: unknown, not run");
            assert!(!marker.exists(), "the repository's {key} filter ran");
            // A touched file: stat-dirty, same content.
            std::thread::sleep(std::time::Duration::from_millis(1100));
            std::fs::write(d.join("a.txt"), "x\n").unwrap();
            assert_eq!(detect_in(&env(&[]), Some(&d)).dirty, None);
            assert!(!marker.exists(), "the repository's {key} filter ran on a touched file");
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn links_are_refused_for_what_a_card_could_not_show() {
        let link = |url: &str| Link { url: url.into(), ..Link::default() };
        assert!(validate_links(&[link("https://example.com/x")]).is_ok());
        for bad in ["ftp://x.example", "https://a b.example", "JIRA-1", "https://", "https://u:p@example.com/x"] {
            assert!(validate_links(&[link(bad)]).is_err(), "{bad}");
        }
        assert!(validate_links(&[Link { url: "https://e.x".into(), title: "a\nb".into(), rel: String::new() }]).is_err());
        assert!(validate_links(&vec![link("https://e.x"); 21]).is_err());
    }
}
