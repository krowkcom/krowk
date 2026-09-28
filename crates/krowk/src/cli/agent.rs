//! The commands an agent runs: push and the uploads, runs and claim that
//! follow from it. Every file becomes its own artifact; a run groups them and
//! carries the metadata about where they came from.

use super::workspace::resolve_workspace;
use super::{Ctx, VERSION};
use crate::output::{self, spinner::Spinner, Listing, UploadResult};
use crate::runctx::{self, Metadata, Overrides};
use krowk_api::slug::{parse_slug, KIND_ARTIFACT, KIND_RUN};
use krowk_api::{creds, fail, Artifact, Client, Error, VISIBILITY_PRIVATE};
use serde_json::{json, Value};

/// What every claim token is minted with, so a second positional either looks
/// like one or is a mistake worth naming.
const CLAIM_TOKEN_PREFIX: &str = "krowk_claim_";

/// The metadata key a caption is recorded under, on the artifact.
const CAPTION_KEY: &str = "krowk.caption";

/// The one place a client gets built: --dev, KROWK_API_URL, KROWK_DEV for
/// where; KROWK_TOKEN, then --workspace, KROWK_WORKSPACE, the repo config,
/// the global config and the stored default for which key. A workspace that
/// resolved and holds no key is a refusal, not an anonymous fallback — but
/// KROWK_TOKEN stays the strongest word, read before any config file.
pub(crate) fn new_client(ctx: &Ctx) -> Result<Client, Error> {
    let base = krowk_api::base_url_for(ctx.f.dev, ctx.io.env);
    let token = ctx.env("KROWK_TOKEN");
    if !token.is_empty() {
        return Ok(Client::new(&base, &token));
    }
    let (ws, _) = resolve_workspace(ctx)?;
    Ok(Client::new(&base, &creds::resolve_token(ctx.io.env, &ws)?))
}

fn workspace_key_missing(e: &Error) -> bool {
    matches!(e.code().as_str(), "no_key_for_workspace" | "dangling_default")
}

/// --run as the positionals read it: the slug, or a link carrying it.
fn run_flag(ctx: &mut Ctx) -> Result<(), Error> {
    ctx.f.run = parse_slug(KIND_RUN, &ctx.f.run)?;
    Ok(())
}

/// The positional naming one record. Blank counts as absent: `"$SLUG"` with
/// the variable unset is one empty word, and the answer is the command's own
/// "pass the artifact".
fn slug_arg(kind: &str, args: &[String], missing: &str, fix: &str) -> Result<String, Error> {
    match args.first() {
        Some(a) if !a.trim().is_empty() => parse_slug(kind, a),
        _ => Err(fail(missing, fix)),
    }
}

fn now() -> jiff::Zoned {
    jiff::Zoned::now()
}

/// Everything worth remembering about where an upload came from. Flags win;
/// the rest is detected, so an agent never has to type it.
pub(crate) fn metadata_for(ctx: &Ctx) -> Metadata {
    let f = &ctx.f;
    runctx::resolve(
        ctx.io.env,
        Overrides {
            repo: f.repo.clone(),
            commit: f.commit.clone(),
            agent: f.agent.clone(),
            pull_request: f.pull_request.clone(),
            links: f.links.clone(),
            references: f.references.clone(),
            session: f.session.clone(),
            title: f.title.clone(),
            client: format!("krowk-cli/{VERSION}"),
        },
    )
}

/// Repeated --metadata key=value. The value may hold `=`; the key may not be
/// empty. Values stay strings — the registry never learns shapes.
fn parse_metadata(pairs: &[String]) -> Result<Vec<(String, String)>, Error> {
    let mut out: Vec<(String, String)> = Vec::new();
    for pair in pairs {
        match pair.split_once('=') {
            Some((k, v)) if !k.is_empty() => {
                out.retain(|(key, _)| key != k);
                out.push((k.into(), v.into()));
            }
            _ => return Err(fail("bad_flag", "--metadata takes key=value, like --metadata krowk.caption=\"before the fix\"")),
        }
    }
    Ok(out)
}

/// Captions lined up with the files: one per file, or one for all of them.
fn captions_for(captions: &[String], files: usize) -> Result<Vec<String>, Error> {
    match captions.len() {
        0 => Ok(Vec::new()),
        1 if files > 1 => Ok(vec![captions[0].clone(); files]),
        n if n != files => Err(fail(
            "bad_flag",
            format!(
                "--caption was given {n} times for {files} files: pass one caption per file, in the order the files are, or a single one for all of them"
            ),
        )),
        _ => Ok(captions.to_vec()),
    }
}

/// One file's caption over the shared extras; a hand-written
/// --metadata krowk.caption still wins.
fn with_caption(extras: &[(String, String)], captions: &[String], i: usize) -> Vec<(String, String)> {
    let mut out = extras.to_vec();
    if let Some(c) = captions.get(i).filter(|_| !extras.iter().any(|(k, _)| k == CAPTION_KEY)) {
        out.push((CAPTION_KEY.into(), c.clone()));
    }
    out
}

fn run_metadata_given(ctx: &Ctx) -> Vec<&'static str> {
    let f = &ctx.f;
    [
        (!f.pull_request.is_empty(), "--pull-request"),
        (!f.links.is_empty(), "--link"),
        (!f.references.is_empty(), "--reference"),
        (!f.session.is_empty(), "--session"),
        (!f.title.is_empty(), "--title"),
    ]
    .into_iter()
    .filter_map(|(given, name)| given.then_some(name))
    .collect()
}

fn artifact_metadata_given(ctx: &Ctx) -> Vec<&'static str> {
    [(!ctx.f.metadata.is_empty(), "--metadata"), (!ctx.f.caption.is_empty(), "--caption")]
        .into_iter()
        .filter_map(|(given, name)| given.then_some(name))
        .collect()
}

/// Metadata asked for by name and not recorded is said, not dropped: an agent
/// would otherwise believe the pull request it named is on the upload.
fn anonymous_metadata_note(ctx: &Ctx) -> Option<String> {
    let given: Vec<&str> = run_metadata_given(ctx).into_iter().chain(artifact_metadata_given(ctx)).collect();
    (!given.is_empty()).then(|| {
        format!(
            "{} was not recorded: a keyless upload records no metadata — run `krowk login --token krowk_sk_...`",
            given.join(", ")
        )
    })
}

fn named_run_metadata_note(ctx: &Ctx) -> Option<String> {
    let given = run_metadata_given(ctx);
    (!given.is_empty()).then(|| {
        format!(
            "{} was not recorded: run {} already carries the metadata it was opened with — pass these to `krowk runs start`, or drop --run and let this push open its own run",
            given.join(", "),
            ctx.f.run
        )
    })
}

/// A failed batch keeps what it would otherwise lose: the links that did
/// upload, and the run this command opened.
fn with_progress(mut e: Error, done: &[Artifact], run: &str, own_run: bool) -> Error {
    if !done.is_empty() {
        e.body.insert("uploaded_before_failure".into(), json!(done.iter().map(|a| a.url.clone()).collect::<Vec<_>>()));
    }
    if own_run {
        e.body.insert("run".into(), json!(run));
        let finish = format!("the run is still open — close it with `krowk runs finish {run}`");
        let fix = match e.fix() {
            f if f.is_empty() => finish,
            f => format!("{f}; {finish}"),
        };
        e.body.insert("fix".into(), json!(fix));
    }
    e
}

/// Whether anyone is watching the wait: a terminal on stderr, and nobody
/// asking for the answer as data.
fn watching(ctx: &Ctx) -> bool {
    ctx.io.err_tty && ctx.format == output::Format::Human && !ctx.f.quiet && ctx.f.destination.is_empty()
}

fn uploading(paths: &[String], i: usize) -> String {
    let name = std::path::Path::new(&paths[i]).file_name().map_or(paths[i].clone(), |n| n.to_string_lossy().into_owned());
    if paths.len() > 1 { format!("Uploading {name} ({}/{})", i + 1, paths.len()) } else { format!("Uploading {name}") }
}

/// push and uploads create.
pub(crate) fn upload(ctx: &mut Ctx, files: &[String]) -> Result<(), Error> {
    if files.is_empty() {
        return Err(fail("no_file", "pass at least one path: `krowk push screenshot.png`"));
    }
    let extras = parse_metadata(&ctx.f.metadata)?;
    let captions = captions_for(&ctx.f.caption, files.len())?;
    runctx::validate_links(&ctx.f.links).map_err(|e| fail("bad_flag", e))?;
    run_flag(ctx)?;

    // Every file is measured before anything is sent, so a typo in the last
    // path fails before the first upload.
    let specs = files.iter().map(|p| krowk_api::spec::inspect(p)).collect::<Result<Vec<_>, _>>()?;
    let client = new_client(ctx)?;
    // A keyless --private push cannot be given what it asked for, and the one
    // outcome to head off is publishing the file anyway.
    if ctx.f.private && !client.authenticated() {
        return Err(krowk_api::private_needs_key());
    }

    let mut result = UploadResult { title: ctx.f.title.clone(), ..UploadResult::default() };
    let paths: Vec<String> = specs.iter().map(|s| s.path.clone()).collect();
    let spin = Spinner::start(watching(ctx), &uploading(&paths, 0));

    // The run: the one named, a fresh one carrying the detected metadata, or
    // none without a key to open one with.
    let (run, own_run) = if !client.authenticated() {
        result.notes.extend(anonymous_metadata_note(ctx));
        (ctx.f.run.clone(), false)
    } else if !ctx.f.run.is_empty() {
        result.notes.extend(named_run_metadata_note(ctx));
        (ctx.f.run.clone(), false)
    } else {
        let opened = client.create_run(&serde_json::to_value(metadata_for(ctx)).expect("metadata serializes"))?;
        let slug = opened.slug.clone();
        result.run = Some(opened);
        (slug, true)
    };

    // Each artifact is stamped with the state found at its own moment; the
    // caption is per file, laid over the shared stamp.
    let stamp = client.authenticated().then(|| metadata_for(ctx).artifact());
    for (i, mut spec) in specs.into_iter().enumerate() {
        spec.run = run.clone();
        if ctx.f.private {
            spec.visibility = VISIBILITY_PRIVATE.into();
        }
        if let Some(stamp) = &stamp {
            spec.metadata = Some(stamp.with_extras(&with_caption(&extras, &captions, i)));
        }
        spin.say(&uploading(&paths, i));
        match client.push(&spec) {
            Ok(a) => result.artifacts.push(a),
            Err(e) => return Err(with_progress(e, &result.artifacts, &run, own_run)),
        }
    }

    // A run this command opened is a run it closes; failing to is worth
    // saying, not worth failing the upload over.
    if own_run {
        match client.finish_run(&run) {
            Ok(finished) => result.run = Some(finished),
            Err(e) => result.notes.push(format!("run {run} could not be finished: {} — retry `krowk runs finish {run}`", e.code())),
        }
    }
    drop(spin);

    let warning = output::unfurl_warning(&result, ctx.format, &ctx.f.destination);
    if !warning.is_empty() {
        let _ = writeln!(ctx.io.stderr, "! {warning}");
    }
    let rendered = if ctx.f.destination.is_empty() {
        output::upload(&result, ctx.format, ctx.f.quiet, ctx.colour, &now())
    } else {
        output::destination(&result, &ctx.f.destination)
    };
    ctx.emit(&rendered)
}


pub(crate) fn uploads_list(ctx: &mut Ctx) -> Result<(), Error> {
    run_flag(ctx)?;
    let client = new_client(ctx)?;
    let page = if ctx.f.run.is_empty() {
        client.list_artifacts(&ctx.f.before, ctx.f.limit)?
    } else {
        client.list_run_artifacts(&ctx.f.run, &ctx.f.before, ctx.f.limit)?
    };
    let listing = Listing { run: ctx.f.run.clone(), limit: ctx.f.limit };
    let rendered = output::list(&page, &listing, ctx.format, ctx.f.quiet, ctx.colour, &now());
    ctx.emit(&rendered)
}

pub(crate) fn uploads_show(ctx: &mut Ctx, args: &[String]) -> Result<(), Error> {
    let slug = slug_arg(KIND_ARTIFACT, args, "no_artifact", "pass the artifact: `krowk uploads show art_...`")?;
    let artifact = new_client(ctx)?.show_artifact(&slug)?;
    let rendered = output::artifact(&artifact, ctx.format, ctx.f.quiet, ctx.colour, &now());
    ctx.emit(&rendered)
}

pub(crate) fn uploads_attach(ctx: &mut Ctx, args: &[String]) -> Result<(), Error> {
    let slug = slug_arg(KIND_ARTIFACT, args, "no_artifact", "pass the artifact: `krowk uploads attach art_... --run run_...`")?;
    if ctx.f.run.trim().is_empty() {
        return Err(fail("no_run", format!("pass the run to attach it to: `krowk uploads attach {slug} --run run_...`")));
    }
    run_flag(ctx)?;
    let artifact = new_client(ctx)?.attach_run(&slug, &ctx.f.run)?;
    let rendered = output::artifact(&artifact, ctx.format, ctx.f.quiet, ctx.colour, &now());
    ctx.emit(&rendered)
}

/// Takes an upload down: immediate and unrecoverable, with no prompt — this
/// is reached for when a secret was published by accident. A key speaks for
/// its workspace; a claim token for the one anonymous upload it came with.
pub(crate) fn uploads_delete(ctx: &mut Ctx, args: &[String]) -> Result<(), Error> {
    let slug = slug_arg(KIND_ARTIFACT, args, "no_artifact", "pass the artifact: `krowk uploads delete art_...`")?;
    let token = args.get(1).map(|t| t.trim().to_string()).unwrap_or_default();
    // A second word that is not a token would withhold the key and turn an
    // authorised takedown into an unauthorised one; it is not quoted back,
    // since it may be the second of two pasted links, signature and all.
    if args.len() > 1 && !token.starts_with(CLAIM_TOKEN_PREFIX) {
        return Err(fail(
            "bad_claim_token",
            format!(
                "the second argument is not a claim token — takedown takes one artifact and, after it, only the `{CLAIM_TOKEN_PREFIX}...` token that upload came back with"
            ),
        ));
    }
    let client = match new_client(ctx) {
        Ok(c) => c,
        // A claim token is its own authority, so a pinned workspace with no key
        // must not stop a token-authorised takedown.
        Err(e) if !token.is_empty() && workspace_key_missing(&e) => Client::new(&krowk_api::base_url_for(ctx.f.dev, ctx.io.env), ""),
        Err(e) => return Err(e),
    };
    if token.is_empty() && !client.authenticated() {
        return Err(fail(
            "missing_claim",
            format!(
                "taking down an anonymous upload needs the claim token it came back with: `krowk uploads delete {slug} krowk_claim_...` — with an API key, the key is authority enough"
            ),
        ));
    }
    if let Err(mut e) = client.take_down_artifact(&slug, &token) {
        // The registry answers one "no such record" for a missing slug, another
        // workspace's, and a wrong token; which to check depends on the authority sent.
        if e.code() == "not_found" {
            let fix = if token.is_empty() {
                "this workspace holds no such upload — check the slug, and that the key matches the workspace it was uploaded to; an upload that is still anonymous is taken down with its claim token instead"
            } else {
                "no anonymous upload answers to that slug and token — check both were copied whole, and note that claiming one spends the token, after which the key that claimed it is what takes it down"
            };
            e.body.insert("fix".into(), json!(fix));
        }
        return Err(e);
    }
    let rendered = output::removed(&slug, ctx.format, ctx.f.quiet, ctx.colour);
    ctx.emit(&rendered)
}

pub(crate) fn runs_start(ctx: &mut Ctx) -> Result<(), Error> {
    runctx::validate_links(&ctx.f.links).map_err(|e| fail("bad_flag", e))?;
    let client = new_client(ctx)?;
    let extras = parse_metadata(&ctx.f.metadata)?;
    let run = client.create_run(&metadata_for(ctx).with_extras(&extras))?;
    let rendered = output::run(&run, ctx.format, ctx.f.quiet, ctx.colour);
    ctx.emit(&rendered)
}

pub(crate) fn runs_list(ctx: &mut Ctx) -> Result<(), Error> {
    let page = new_client(ctx)?.list_runs(&ctx.f.before, ctx.f.limit)?;
    let rendered = output::run_list(&page, &Listing { limit: ctx.f.limit, ..Listing::default() }, ctx.format, ctx.f.quiet, ctx.colour);
    ctx.emit(&rendered)
}

pub(crate) fn runs_show(ctx: &mut Ctx, args: &[String]) -> Result<(), Error> {
    let slug = slug_arg(KIND_RUN, args, "no_run", "pass the run: `krowk runs show run_...`")?;
    let run = new_client(ctx)?.show_run(&slug)?;
    let rendered = output::run_detail(&run, ctx.format, ctx.f.quiet, ctx.colour);
    ctx.emit(&rendered)
}

pub(crate) fn runs_finish(ctx: &mut Ctx, args: &[String]) -> Result<(), Error> {
    let slug = slug_arg(KIND_RUN, args, "no_run", "pass the run: `krowk runs finish run_...`")?;
    let run = new_client(ctx)?.finish_run(&slug)?;
    let rendered = output::run(&run, ctx.format, ctx.f.quiet, ctx.colour);
    ctx.emit(&rendered)
}

/// Spends the token an anonymous upload came back with, and — with --run —
/// attaches it after, since the attach resolves in the key's workspace.
pub(crate) fn claim(ctx: &mut Ctx, args: &[String]) -> Result<(), Error> {
    if args.len() < 2 {
        return Err(fail("missing_claim", "pass both the artifact and its token: `krowk claim art_... krowk_claim_...`"));
    }
    let slug = slug_arg(KIND_ARTIFACT, args, "no_artifact", "pass both the artifact and its token: `krowk claim art_... krowk_claim_...`")?;
    run_flag(ctx)?;
    let client = new_client(ctx)?;
    let mut artifact = client.claim_artifact(&slug, args[1].trim())?;
    if !ctx.f.run.is_empty() {
        // The claim is spent, so a failure here must not read as one the caller
        // can undo by running the whole thing again.
        artifact = client.attach_run(&artifact.slug, &ctx.f.run).map_err(|mut e| {
            e.body.insert("claimed".into(), json!(artifact.slug));
            let retry = format!(
                "the upload is claimed and kept, only the run is not attached — retry `krowk uploads attach {} --run <run>` with a run this workspace holds",
                artifact.slug
            );
            let fix = match e.fix() {
                f if f.is_empty() => retry,
                f => format!("{f}; {retry}"),
            };
            e.body.insert("fix".into(), Value::String(fix));
            e
        })?;
    }
    let rendered = output::claimed(&artifact, ctx.format, ctx.f.quiet, ctx.colour, &now());
    ctx.emit(&rendered)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captions_line_up_with_files_or_spread_over_them() {
        let one = vec!["c".to_string()];
        assert_eq!(captions_for(&one, 3).unwrap().len(), 3);
        assert!(captions_for(&["a".into(), "b".into()], 3).is_err());
        assert!(captions_for(&[], 2).unwrap().is_empty());
    }

    #[test]
    fn metadata_pairs_split_on_the_first_equals_and_a_spelled_caption_wins() {
        let pairs = parse_metadata(&["a=b=c".into(), "a=d".into()]).unwrap();
        assert_eq!(pairs, vec![("a".to_string(), "d".to_string())]);
        assert!(parse_metadata(&["=x".into()]).is_err());
        let spelled = vec![(CAPTION_KEY.to_string(), "mine".to_string())];
        assert_eq!(with_caption(&spelled, &["flag".into()], 0), spelled);
    }
}
