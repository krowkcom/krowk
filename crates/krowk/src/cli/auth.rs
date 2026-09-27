//! `auth`, and its short forms `login`, `logout` and `whoami`: your krowk
//! account — getting a key onto this machine, printing it, checking it,
//! and taking it off again. A model provider is not signed in to here but
//! connected, with `krowk connect`.

use super::agent::new_client;
use super::workspace::resolve_workspace;
use super::Ctx;
use crate::output::{self, Authorization, Login};
use krowk_api::creds::{self, Identity};
use krowk_api::{fail, CliAuthorization, Client, Error, AUTHORIZATION_APPROVED, AUTHORIZATION_DENIED};
use serde_json::json;
use std::time::{Duration, Instant};

/// A browser login's window and pace are the registry's to set, and bounded
/// here, so an absurd answer can neither make krowk hammer it nor leave a
/// forgotten terminal polling all afternoon.
const DEFAULT_WINDOW: Duration = Duration::from_secs(15 * 60);
const MIN_WINDOW: Duration = Duration::from_secs(60);
const MAX_WINDOW: Duration = Duration::from_secs(30 * 60);
const DEFAULT_POLL: Duration = Duration::from_secs(5);
const MIN_POLL: Duration = Duration::from_secs(1);
const MAX_POLL: Duration = Duration::from_secs(30);

/// Words that name a model provider, or an instance of one, rather than
/// anything `login` takes: somebody who types them wants `krowk connect`.
const PROVIDER_WORDS: &[&str] =
    &["anthropic", "claude", "openai", "chatgpt", "codex", "gpt", "xai", "grok", "supergrok", "openrouter", "openai-compatible"];

/// Where a model provider is connected, in the words of an error that
/// points there: the vendor and method a word means, in the one form every
/// fix line takes — `krowk connect anthropic --method subscription --name
/// work` — the name and the method kept from an instance's name.
fn connect_hint(word: &str) -> String {
    #[cfg(feature = "harness")]
    if let Some((prefix, _)) = word.split_once(':') {
        // An instance's name: its prefix is the method's — `anthropic:work`
        // an API key, `claude:work` a subscription — as a fix line says it.
        use krowk_harness::connect;
        let cmd = match connect::by_provider(&prefix.to_ascii_lowercase(), false) {
            Some(m) => connect::connect_command(&format!("{}:{}", prefix.to_ascii_lowercase(), &word[prefix.len() + 1..]), m.kind),
            None => format!("krowk connect {word}"),
        };
        return format!("to connect a model provider, run `{cmd}`");
    }
    if cfg!(feature = "harness") {
        let way = match word.to_ascii_lowercase().as_str() {
            "anthropic" | "claude" => "anthropic --method subscription",
            "openai" | "chatgpt" | "codex" | "gpt" => "openai --method subscription",
            "xai" | "grok" | "supergrok" => "xai --method subscription",
            "openrouter" => "openrouter",
            _ => return "to connect a model provider, run `krowk connect openai-compatible --name <name> --base-url <url>`".into(),
        };
        format!("to connect a model provider, run `krowk connect {way}`")
    } else {
        "model providers are connected in the full build (a release, or `--features harness`), with its connect command".into()
    }
}

pub(crate) fn login(ctx: &mut Ctx, args: &[String]) -> Result<(), Error> {
    // A key typed without its flag is caught by name — and not quoted back,
    // since it is already in a shell history.
    if let Some(first) = args.first() {
        if first.starts_with("krowk_sk_") {
            return Err(fail(
                "token_not_a_positional",
                "a key has to go behind the flag: `krowk login --token krowk_sk_...` — passed as a bare argument it is ignored",
            ));
        }
        let word = first.trim().to_ascii_lowercase();
        if PROVIDER_WORDS.contains(&word.as_str()) || PROVIDER_WORDS.iter().any(|p| word.starts_with(&format!("{p}:"))) {
            return Err(fail(
                "unexpected_argument",
                format!("`login` is your krowk account and takes no provider — {}", connect_hint(first.trim())),
            ));
        }
        return Err(fail(
            "unexpected_argument",
            format!("`krowk login` takes no arguments, and got `{}` — the key goes behind --token", args[..args.len().min(2)].join(" ")),
        ));
    }
    if ctx.f.token.is_empty() { login_in_browser(ctx) } else { login_with_token(ctx) }
}

/// Stores a key once the registry has had the chance to reject it. A
/// rejection is fatal; every other outcome — no network, a registry that is
/// down — stores it anyway, unconfirmed, since none of those is evidence
/// about the key.
fn login_with_token(ctx: &mut Ctx) -> Result<(), Error> {
    let token = ctx.f.token.clone();
    let verified = Client::new(&krowk_api::base_url_for(ctx.f.dev, ctx.io.env), &token).verify_key();
    if let Err(e) = &verified
        && (e.status == 401 || e.status == 403)
    {
        let mut e = e.clone();
        e.body.insert(
            "fix".into(),
            json!("the registry does not accept this key — check it was pasted whole, or issue a new one in the dashboard"),
        );
        return Err(e);
    }
    let id = verified.as_ref().map(|k| Identity {
        key_id: k.key_id.clone(),
        workspace: k.workspace.clone(),
        workspace_name: k.workspace_name.clone(),
    });
    let path = creds::save_credentials(&token, id.as_ref().unwrap_or(&Identity::default())).map_err(|e| {
        fail("credentials_unwritable", format!("could not write {}: {}", creds::credentials_path().display(), e.fix()))
    })?;
    let mut result = Login { path, confirmed: verified.is_ok(), shadowed: !ctx.env("KROWK_TOKEN").is_empty(), ..Login::default() };
    match &verified {
        Ok(k) => (result.key_id, result.workspace) = (k.key_id.clone(), k.workspace.clone()),
        Err(e) if e.status != 0 => result.reason = format!("{} (HTTP {})", e.code(), e.status),
        Err(e) => result.reason = e.code(),
    }
    let rendered = output::stored_key(&result, ctx.format, ctx.f.quiet, ctx.colour);
    ctx.emit(&rendered)
}

/// Mints a key by having somebody approve the request in a browser. The slug
/// collects the key and never appears in a browser; the code is what a person
/// confirms and can only approve or deny — so the half that travels cannot be
/// turned into a key by whoever sees it. The key arrives exactly once.
fn login_in_browser(ctx: &mut Ctx) -> Result<(), Error> {
    if in_ci(ctx) && !ctx.f.no_browser {
        return Err(fail(
            "no_one_to_approve",
            "a browser login needs somebody to approve it, and this looks like CI — pass `krowk login --token krowk_sk_...`, \
             or add --no-browser if there is somebody to hand the code to",
        ));
    }
    // Keyless whatever the environment holds: the endpoint exists for a
    // machine with no key.
    let client = Client::new(&krowk_api::base_url_for(ctx.f.dev, ctx.io.env), "");
    let auth = client.start_cli_authorization().map_err(|mut e| {
        if e.status == 404 {
            e.body.insert(
                "fix".into(),
                json!("this registry does not answer browser login — check KROWK_API_URL, or issue a key in the dashboard and store it with `krowk login --token krowk_sk_...`"),
            );
        }
        e
    })?;
    let page = browsable_url(&auth.verification_url, &client)?;
    let opened = !ctx.f.no_browser && !headless(ctx) && open_browser(&page);
    // stderr: the code and the page are what a person needs during the
    // command, while stdout stays the one document a program parses.
    let notice = output::authorizing(&Authorization { code: auth.code.clone(), page, opened }, ctx.format, ctx.colour);
    let _ = write!(ctx.io.stderr, "{notice}");

    let deadline = Instant::now() + window(&auth.expires_at);
    let granted = await_authorization(&client, &auth, deadline)?;
    let id = Identity {
        key_id: granted.key_id.clone(),
        workspace: granted.workspace.clone(),
        workspace_name: granted.workspace_name.clone(),
    };
    let path = creds::save_credentials(&granted.token, &id).map_err(|e| {
        fail(
            "credentials_unwritable",
            format!(
                "could not write {}: {} — the approved key was handed over once and the registry keeps no copy, so fix the path and run `krowk login` again for a new one",
                creds::credentials_path().display(),
                e.fix()
            ),
        )
    })?;
    let confirmed = !granted.key_id.is_empty() && !granted.workspace.is_empty();
    let result = Login {
        path,
        key_id: granted.key_id,
        workspace: granted.workspace,
        confirmed,
        shadowed: !ctx.env("KROWK_TOKEN").is_empty(),
        reason: if confirmed { String::new() } else { "the registry approved it without naming the key or its workspace".into() },
    };
    let rendered = output::stored_key(&result, ctx.format, ctx.f.quiet, ctx.colour);
    ctx.emit(&rendered)
}

/// Polls until somebody answers or the window closes. Approved, denied and a
/// refusal about this authorization are answers; a rate limit, a 5xx or no
/// network are the moment, and the window is kept rather than an approval
/// thrown away.
fn await_authorization(client: &Client, auth: &CliAuthorization, deadline: Instant) -> Result<CliAuthorization, Error> {
    let interval = poll_interval(auth.interval);
    // The last poll that got no answer: if the window closes with one
    // outstanding, krowk could not ask, and "nobody approved it" would blame a
    // person for a question that never got out.
    let mut unanswered: Option<Error> = None;
    let mut wait = interval;
    loop {
        // Slept before the first read too: an authorization milliseconds old
        // cannot have been approved yet. Never past the deadline.
        std::thread::sleep(wait.min(deadline.saturating_duration_since(Instant::now())));
        wait = interval;
        if Instant::now() >= deadline {
            return Err(window_closed(unanswered));
        }
        match client.read_cli_authorization(&auth.slug) {
            Err(e) if worth_another_poll(&e) => {
                if let Some(asked) = krowk_api::client::retry_after_for(&e).filter(|a| *a > wait) {
                    wait = asked;
                }
                unanswered = Some(e);
            }
            Err(e) => return Err(login_fix(e)),
            Ok(granted) if granted.state == AUTHORIZATION_APPROVED => {
                if granted.token.is_empty() {
                    return Err(fail(
                        "malformed_response",
                        "the registry approved this login without handing over a key — run `krowk login` again, and report it if it repeats",
                    ));
                }
                return Ok(granted);
            }
            Ok(granted) if granted.state == AUTHORIZATION_DENIED => {
                return Err(fail("authorization_denied", "this login was denied in the browser — run `krowk login` to ask again"));
            }
            // Pending, or a state this build has no word for: still inside the window.
            Ok(_) => unanswered = None,
        }
    }
}

fn window_closed(unanswered: Option<Error>) -> Error {
    unanswered.unwrap_or_else(|| {
        fail("authorization_expired", "nobody approved this login before it lapsed — run `krowk login` to ask again")
    })
}

fn worth_another_poll(e: &Error) -> bool {
    e.code() == "network_unreachable" || e.status == 429 || e.status >= 500
}

/// Advice written for artifacts, replaced on a login: "upload it again" is
/// nonsense to someone trying to log in.
fn login_fix(mut e: Error) -> Error {
    let fix = match (e.code().as_str(), e.status) {
        ("expired", _) => "this login lapsed before it was approved — run `krowk login` to ask again",
        ("spent", _) => "this login's key was already collected, and the registry keeps no second copy — run `krowk login` for a new one",
        (_, 404) => "the registry does not know this login — it may have lapsed and been swept; run `krowk login` to ask again",
        _ => return e,
    };
    e.body.insert("fix".into(), json!(fix));
    e
}

/// The registry's expiry, clamped: a clock minutes off the registry's would
/// otherwise abandon a good login before its first poll.
fn window(expires_at: &str) -> Duration {
    match expires_at.parse::<jiff::Timestamp>() {
        Ok(at) => {
            let secs = at.duration_since(jiff::Timestamp::now()).as_secs().max(0) as u64;
            Duration::from_secs(secs).clamp(MIN_WINDOW, MAX_WINDOW)
        }
        Err(_) => DEFAULT_WINDOW,
    }
}

fn poll_interval(seconds: i64) -> Duration {
    if seconds <= 0 {
        return DEFAULT_POLL;
    }
    Duration::from_secs(seconds.min(MAX_POLL.as_secs() as i64) as u64).max(MIN_POLL)
}

/// The approval page arrives in a response body and is about to go to the
/// desktop's URL handler, which reaches far past an HTTP client — so only
/// http(s), and plain http only when the API itself is.
fn browsable_url(raw: &str, registry: &Client) -> Result<String, Error> {
    let malformed = |what: &str| {
        fail(
            "malformed_response",
            format!("the registry {what} — check KROWK_API_URL points at the API host, not the website"),
        )
    };
    if raw.is_empty() {
        return Err(malformed("opened a browser login without naming a page to approve it on"));
    }
    let Some((scheme, rest)) = raw.split_once("://").or_else(|| raw.split_once(':')) else {
        return Err(malformed("named a page for this login with no scheme"));
    };
    match scheme.to_ascii_lowercase().as_str() {
        "https" => {}
        "http" if registry.insecure() => {}
        "http" => {
            return Err(fail(
                "refused_verification_url",
                "the registry named an http page for this login while the API itself is https — krowk will not open it; check KROWK_API_URL",
            ))
        }
        "" => return Err(malformed("named a page for this login with no scheme")),
        other => {
            return Err(fail(
                "refused_verification_url",
                format!("the registry named a {other}: page for this login, and krowk only opens http and https"),
            ))
        }
    }
    if rest.split(['/', '?', '#']).next().unwrap_or_default().rsplit('@').next().unwrap_or_default().is_empty() {
        return Err(malformed("named a page for this login with no host"));
    }
    Ok(raw.to_string())
}

/// Nowhere to open a browser: over SSH, on CI, or on a unix session with no
/// display server.
pub(super) fn headless(ctx: &Ctx) -> bool {
    if ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"].iter().any(|k| !ctx.env(k).is_empty()) || in_ci(ctx) {
        return true;
    }
    if cfg!(any(target_os = "macos", windows)) {
        return false;
    }
    ctx.env("DISPLAY").is_empty() && ctx.env("WAYLAND_DISPLAY").is_empty()
}

fn in_ci(ctx: &Ctx) -> bool {
    krowk_api::truthy(&ctx.env("CI")) || !ctx.env("GITHUB_ACTIONS").is_empty()
}

/// Hands the page to the desktop, started rather than waited on — `xdg-open`
/// may exec a browser in the foreground. The URL is one argument, no shell.
pub(super) fn open_browser(target: &str) -> bool {
    let mut cmd = if cfg!(target_os = "macos") {
        std::process::Command::new("open")
    } else if cfg!(windows) {
        let mut c = std::process::Command::new("rundll32");
        c.arg("url.dll,FileProtocolHandler");
        c
    } else {
        std::process::Command::new("xdg-open")
    };
    match cmd.arg(target).spawn() {
        Ok(mut child) => {
            std::thread::spawn(move || child.wait());
            true
        }
        Err(_) => false,
    }
}

/// The token a command here would send: the resolved workspace's, not
/// whatever logged in last.
pub(crate) fn token(ctx: &mut Ctx) -> Result<(), Error> {
    let env_token = ctx.env("KROWK_TOKEN");
    let token = if env_token.is_empty() {
        let (ws, _) = resolve_workspace(ctx)?;
        creds::resolve_token(ctx.io.env, &ws)?
    } else {
        env_token
    };
    if token.is_empty() {
        return Err(fail("not_authenticated", "run `krowk login --token krowk_sk_...`, or upload anonymously"));
    }
    let _ = writeln!(ctx.io.stdout, "{token}");
    Ok(())
}

/// Takes the key that resolves here off this machine: `--workspace`'s, the
/// repository's, the default. The key itself keeps working until it is
/// revoked in the dashboard, which is said, since "logged out" could be
/// read as "revoked".
pub(crate) fn logout(ctx: &mut Ctx, args: &[String]) -> Result<(), Error> {
    // An argument is refused before anything is removed: `krowk logout
    // anthropic` meant a provider, and the krowk key it would otherwise
    // take cannot be shown again.
    if let Some(first) = args.first() {
        let word = first.trim().to_ascii_lowercase();
        let fix = if PROVIDER_WORDS.contains(&word.as_str()) || PROVIDER_WORDS.iter().any(|p| word.starts_with(&format!("{p}:"))) {
            if cfg!(feature = "harness") {
                format!("`logout` is your krowk account and takes no provider — to disconnect a model provider, run `krowk disconnect {}`", first.trim())
            } else {
                "`logout` is your krowk account and takes no provider — model providers are disconnected in the full build (a release, or `--features harness`), with its disconnect command".into()
            }
        } else {
            format!("`krowk logout` takes no arguments, and got `{}` — nothing was removed", args[..args.len().min(2)].join(" "))
        };
        return Err(fail("unexpected_argument", fix));
    }
    let (ws, _) = resolve_workspace(ctx)?;
    let gone = creds::forget_credentials(&ws).map_err(|e| {
        fail("credentials_unwritable", format!("could not write {}: {}", creds::credentials_path().display(), e.fix()))
    })?;
    let path = creds::credentials_path().display().to_string();
    let shadowed = !ctx.env("KROWK_TOKEN").is_empty();
    let left: Vec<String> = creds::stored_workspaces().into_iter().map(|k| k.name).collect();
    if ctx.format != crate::output::Format::Human {
        let data = json!({
            "removed": gone.is_some(),
            "workspace": gone.as_ref().map(|(n, _)| n.clone()).unwrap_or(ws),
            "key_id": gone.as_ref().map(|(_, id)| id.key_id.clone()).filter(|k| !k.is_empty()),
            "path": path,
            "stored": left,
            "shadowed_by_env": shadowed,
        });
        let rendered = if ctx.f.quiet { output::encode(&data) } else { output::encode(&json!({ "ok": true, "data": data, "summary": if gone.is_some() { "logged out" } else { "no key was stored" } })) };
        return ctx.emit(&rendered);
    }
    let mut lines = match &gone {
        Some((name, id)) => {
            let key = if id.key_id.is_empty() { "its key".to_string() } else { format!("key {}", id.key_id) };
            vec![format!("logged out of {name}: {key} is removed from {path}"), "  the key itself still works until it is revoked in the dashboard".to_string()]
        }
        None => vec!["no key is stored for the workspace that resolves here — nothing to log out of".to_string()],
    };
    if !left.is_empty() && gone.is_some() {
        lines.push(format!("  still stored: {} — `krowk workspaces use <name>` makes one the default", left.join(", ")));
    }
    if shadowed {
        lines.push("  ! KROWK_TOKEN is set and still wins — unset it too".into());
    }
    ctx.emit(&lines.join("\n"))
}

/// What the stored key can actually do. The registry just vouched for it, so a
/// login that ran offline and filed it under "default" is set straight here.
pub(crate) fn verify(ctx: &mut Ctx) -> Result<(), Error> {
    let client = new_client(ctx)?;
    if !client.authenticated() {
        return Err(fail("not_authenticated", "no key to verify — run `krowk login --token krowk_sk_...`, or upload anonymously"));
    }
    let key = client.verify_key()?;
    if ctx.env("KROWK_TOKEN").is_empty() {
        let _ = creds::adopt_identity(
            &client.token,
            &Identity { key_id: key.key_id.clone(), workspace: key.workspace.clone(), workspace_name: key.workspace_name.clone() },
        );
    }
    let rendered = output::key(&key, ctx.format, ctx.f.quiet, ctx.colour);
    ctx.emit(&rendered)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_approval_page_is_http_s_only_and_plain_http_only_beside_a_plain_api() {
        let prod = Client::new("https://api.krowk.com/v1", "");
        let local = Client::new("http://127.0.0.1:8787/v1", "");
        assert!(browsable_url("https://app.krowk.com/cli/ABCD", &prod).is_ok());
        assert_eq!(browsable_url("http://app.krowk.com/x", &prod).unwrap_err().code(), "refused_verification_url");
        assert!(browsable_url("http://127.0.0.1:8787/x", &local).is_ok());
        assert_eq!(browsable_url("file:///etc/passwd", &prod).unwrap_err().code(), "refused_verification_url");
        assert_eq!(browsable_url("https:///nohost", &prod).unwrap_err().code(), "malformed_response");
        assert_eq!(browsable_url("", &prod).unwrap_err().code(), "malformed_response");
    }

    #[test]
    fn the_pace_and_window_are_bounded() {
        assert_eq!(poll_interval(0), DEFAULT_POLL);
        assert_eq!(poll_interval(1_000_000), MAX_POLL);
        assert_eq!(window("not a time"), DEFAULT_WINDOW);
        assert_eq!(window("2000-01-01T00:00:00Z"), MIN_WINDOW);
    }
}
