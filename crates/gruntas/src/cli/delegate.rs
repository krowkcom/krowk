//! What krowk-mcp's delegation tools (R-ALLOC-11) need of the command line:
//! the `Placer` its tasks are placed by (`synced::source`), and
//! `krowk machines --json` read back. A
//! task that stays on this machine runs as `krowk -p --worktree` does, in a
//! worktree of its own, and lands as a remote one does.

use super::{Io, VERSION};
use gruntas_harness::instances::{default_model_of, Asked, Registry};
use gruntas_harness::protocol::{ModelRef, RunResult, TurnStatus};
use gruntas_harness::sync::fleet::{Resolver, RunLocal};
use gruntas_harness::sync::placement::{LocalRun, Placer, PlacerSource};
use serde_json::Value;
use std::sync::{Arc, OnceLock};

/// The server's `Placer`, made on the first call that asks and kept, so
/// its placements of the last 30 seconds count: none when sync is not set up
/// here (remembered until the server restarts).
pub fn placer() -> Option<Arc<Placer>> {
    static SOURCE: OnceLock<PlacerSource> = OnceLock::new();
    SOURCE.get_or_init(|| super::synced::source(&super::flags::Flags::default(), true))()
}

/// `krowk machines --json`, run in this process, as a value; why not when
/// it fails.
pub fn machines() -> Result<Value, String> {
    let (code, out, err) = run(&["machines", "--json"]);
    if code != 0 {
        return Err(first_line(&err).unwrap_or_else(|| "krowk machines failed".into()));
    }
    serde_json::from_str(out.trim()).map_err(|e| format!("krowk machines did not print JSON: {e}"))
}

/// One invocation of the command line, with this process's environment:
/// its exit code, stdout and stderr.
fn run(args: &[&str]) -> (i32, String, String) {
    let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let env = |k: &str| std::env::var(k).unwrap_or_default();
    let code = {
        let mut io = Io { stdout: &mut out, stderr: &mut err, env: &env, tty: false, err_tty: false, stdin_tty: false };
        super::run(&args, &mut io)
    };
    (code, String::from_utf8_lossy(&out).into_owned(), String::from_utf8_lossy(&err).into_owned())
}

fn first_line(text: &str) -> Option<String> {
    text.lines().map(str::trim).find(|l| !l.is_empty()).map(|l| l.chars().take(500).collect())
}

/// The model a call names, made whole: an `<instance>/<model>`, an instance
/// alone (its default model), a bare model id (routed to an instance ready
/// here), or nothing, which `krowk -p`'s default routing picks.
pub fn resolver(registry: Registry, f: super::flags::Flags) -> Resolver {
    Box::new(move |c| {
        let asked = match (&c.instance, &c.model) {
            (Some(instance), Some(model)) => return Ok(ModelRef { instance: instance.clone(), model: model.clone() }),
            (Some(name), None) => return named(&registry, name),
            (None, Some(m)) if registry.get(m.trim()).is_ok() => return named(&registry, m.trim()),
            (None, Some(m)) => Some(registry.read_model(m)?),
            (None, None) => None,
        };
        if let Some(Asked::Exact(m)) = &asked {
            return Ok(m.clone());
        }
        let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
        super::host::own_ctx(&f, |ctx| super::prompt::route(ctx, &registry, asked.as_ref(), None, &cwd, false)).map_err(|e| e.fix())?.ok_or_else(|| "no model is ready here".to_string())
    })
}

/// The instance `name`, on its default model.
fn named(registry: &Registry, name: &str) -> Result<ModelRef, String> {
    let kind = registry.get(name)?.kind;
    default_model_of(kind).map(|m| ModelRef { instance: name.to_string(), model: m.to_string() }).ok_or_else(|| format!("name a model for {name}: {name}/<model>"))
}

/// How a task that stays on this machine runs: one `krowk -p --worktree`
/// turn on the model, in the server's directory. Its changes are in the
/// worktree it keeps; a task that changed nothing keeps none. It is trusted
/// as a person's own run in the repository is, never more.
pub fn run_local() -> RunLocal {
    Box::new(|spec, model| {
        let mut args: Vec<String> = ["-p", "--worktree", "--output-format", "json", "--model"].map(String::from).into();
        args.push(model.to_string());
        if let Some(mode) = spec.permission_mode {
            args.extend(["--permission-mode".to_string(), mode.name().to_string()]);
        }
        if let Some(usd) = spec.budget.max_usd {
            args.extend(["--max-usd".to_string(), usd.to_string()]);
        }
        if let Some(tokens) = spec.budget.max_tokens {
            args.extend(["--max-tokens".to_string(), tokens.to_string()]);
        }
        // After `--`, so a task that starts with a dash is still the prompt.
        args.push("--".into());
        args.push(spec.prompt.clone());
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let (code, out, err) = run(&args);
        let result: Option<RunResult> = out.lines().rev().find_map(|l| serde_json::from_str(l.trim()).ok());
        let Some(result) = result.filter(|r| code == 0 && r.status == TurnStatus::Completed) else {
            // Its stderr, which says what stopped it and, when it left edits
            // behind, the worktree they are kept in.
            let said: String = err.lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join(" ").chars().take(600).collect();
            return Err(if said.is_empty() { first_line(&out).unwrap_or_else(|| format!("krowk {VERSION} stopped with code {code}")) } else { said });
        };
        let worktree = match &result.worktree {
            None => None,
            Some(kept) => {
                let env = |k: &str| std::env::var(k).unwrap_or_default();
                let root = gruntas_api::home::worktrees_root(&env).ok_or("krowk found no home directory to find its worktrees under")?;
                let found = gruntas_harness::worktree::manage::find(&root, &kept.path).map_err(|e| format!("the child's worktree {} could not be found: {e:?}", kept.path))?;
                Some(found.worktree().ok_or_else(|| format!("the child's worktree {} has no known base", kept.path))?)
            }
        };
        Ok(LocalRun { summary: result.result, usage: result.usage, worktree, denied: Vec::new() })
    })
}
