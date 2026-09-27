//! One flag set serves every command, which is what lets flags follow
//! positionals the way agents write them: `krowk push a.png --title x b.png`.
//! Which flags exist, and their types, come from the catalog.

use super::catalog::{self, Flag};
use crate::runctx::Link;
use std::collections::BTreeSet;

#[derive(Debug, Default, Clone)]
pub struct Flags {
    pub run: String,
    pub before: String,
    pub limit: i64,
    pub pull_request: String,
    pub links: Vec<Link>,
    pub references: Vec<String>,
    pub metadata: Vec<String>,
    pub caption: Vec<String>,
    pub destination: String,
    pub private: bool,
    pub session: String,
    pub title: String,
    pub repo: String,
    pub commit: String,
    pub agent: String,
    pub token: String,
    pub workspace: String,
    pub global: bool,
    pub format: String,
    pub dev: bool,
    pub no_browser: bool,
    pub json: bool,
    pub quiet: bool,
    pub jq: String,
    pub help: bool,
    pub version: bool,
    pub from: String,
    pub dry_run: bool,
    pub yes: bool,
    pub no_network: bool,
    pub harness: String,
    pub worktree: String,
    pub all: bool,
    pub thinking: bool,
    pub max_usd: String,
    pub max_tokens: String,
    /// `-p`: run one prompt headless.
    #[cfg(feature = "harness")]
    pub print: bool,
    #[cfg(feature = "harness")]
    pub output_format: String,
    #[cfg(feature = "harness")]
    pub model: String,
    #[cfg(feature = "harness")]
    pub resume: String,
    /// `--resume` with no value: pick the session from a list.
    #[cfg(feature = "harness")]
    pub resume_pick: bool,
    #[cfg(feature = "harness")]
    pub permission_mode: String,
    #[cfg(feature = "harness")]
    pub toolset: String,
    #[cfg(feature = "harness")]
    pub effort: String,
    /// `-p`: run a backend in a repository nobody has trusted yet.
    #[cfg(feature = "harness")]
    pub trust: bool,
    /// `providers add`: the instance's name, its key's variable, its base
    /// URL, and how SuperGrok signs in.
    #[cfg(feature = "harness")]
    pub name: String,
    #[cfg(feature = "harness")]
    pub api_key_env: String,
    #[cfg(feature = "harness")]
    pub base_url: String,
    #[cfg(feature = "harness")]
    pub client_id: String,
    #[cfg(feature = "harness")]
    pub device: bool,
    /// `providers add claude|codex`: the binary, and the config directory
    /// (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`).
    #[cfg(feature = "harness")]
    pub binary: String,
    #[cfg(feature = "harness")]
    pub config_dir: String,
    /// `connect`: the way in, and whether it becomes the default model;
    /// `disconnect`: whether the definition goes too.
    #[cfg(feature = "harness")]
    pub method: String,
    #[cfg(feature = "harness")]
    pub default: bool,
    #[cfg(feature = "harness")]
    pub remove: bool,
    /// Which flags were typed, by canonical name — a different question from
    /// what they carry: `--jq "$UNSET"` was given and is empty.
    pub given: BTreeSet<String>,
    /// Which --link each --link-title / --link-rel has already described.
    described: BTreeSet<(String, usize)>,
}

/// Parses the whole command line into flags and positionals. A `--` ends the
/// flags: everything after it is positional.
pub fn parse(args: &[String]) -> (Flags, Vec<String>, Result<(), String>) {
    let known = catalog::catalog("").all_flags();
    let mut f = Flags::default();
    let mut positionals = Vec::new();
    let mut i = 0;
    let result = (|| {
        while i < args.len() {
            let arg = &args[i];
            i += 1;
            if arg == "--" {
                positionals.extend(args[i..].iter().cloned());
                i = args.len();
                break;
            }
            let Some(body) = arg.strip_prefix("--").or_else(|| arg.strip_prefix('-')).filter(|b| !b.is_empty()) else {
                positionals.push(arg.clone());
                continue;
            };
            if body.starts_with('-') || body.starts_with('=') {
                return Err(format!("bad flag syntax: {arg}"));
            }
            let (name, inline) = match body.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (body, None),
            };
            let Some(spec) = lookup(&known, name) else {
                return Err(format!("flag provided but not defined: -{name}"));
            };
            let value = if spec.kind == catalog::BOOL {
                inline.unwrap_or_else(|| "true".into())
            } else if let Some(v) = inline {
                v
            } else if i < args.len() {
                i += 1;
                args[i - 1].clone()
            } else {
                // Last on the line with no value: `krowk --resume`, which
                // picks the session. Only the TUI takes it; `cli::run`
                // refuses it everywhere else the way it always was.
                #[cfg(feature = "harness")]
                if spec.name == "resume" {
                    f.resume_pick = true;
                    f.given.insert(spec.name.to_string());
                    continue;
                }
                return Err(format!("flag needs an argument: -{name}"));
            };
            f.set(spec, &value)?;
            f.given.insert(spec.name.to_string());
        }
        Ok(())
    })();
    (f, positionals, result)
}

fn lookup<'a>(known: &'a [Flag], name: &str) -> Option<&'a Flag> {
    known.iter().find(|f| f.name == name || f.aliases.contains(&name))
}

/// An integer the way Go's flag package reads one: an optional sign, then
/// decimal, `0x` hex, `0o` or a leading `0` octal, or `0b` binary, with `_`
/// between digits.
fn parse_int(v: &str) -> Option<i64> {
    let (negative, digits) = match v.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, v.strip_prefix('+').unwrap_or(v)),
    };
    let lower = digits.to_ascii_lowercase();
    let (radix, body) = if let Some(b) = lower.strip_prefix("0x") {
        (16, b)
    } else if let Some(b) = lower.strip_prefix("0o") {
        (8, b)
    } else if let Some(b) = lower.strip_prefix("0b") {
        (2, b)
    } else if lower.len() > 1 && lower.starts_with('0') {
        (8, &lower[1..])
    } else {
        (10, lower.as_str())
    };
    // Go's rule: `_` separates digits or follows a base prefix, never doubles
    // and never ends the number.
    let prefixed = body.len() != lower.len();
    if body.is_empty() || body.ends_with('_') || body.contains("__") || (!prefixed && body.starts_with('_')) {
        return None;
    }
    let n = i64::from_str_radix(&body.replace('_', ""), radix).ok()?;
    Some(if negative { -n } else { n })
}

fn parse_bool(name: &str, v: &str) -> Result<bool, String> {
    match v {
        "1" | "t" | "T" | "true" | "TRUE" | "True" => Ok(true),
        "0" | "f" | "F" | "false" | "FALSE" | "False" => Ok(false),
        _ => Err(format!("invalid boolean value {v:?} for -{name}: parse error")),
    }
}

impl Flags {
    fn set(&mut self, spec: &Flag, v: &str) -> Result<(), String> {
        let name = spec.name;
        let text = |dst: &mut String| *dst = v.to_string();
        match name {
            "run" => text(&mut self.run),
            "before" => text(&mut self.before),
            "limit" => {
                self.limit = parse_int(v).ok_or_else(|| format!("invalid value {v:?} for flag -limit: parse error"))?;
            }
            "pull-request" => text(&mut self.pull_request),
            "link" => self.links.push(Link { url: v.into(), ..Link::default() }),
            "link-title" | "link-rel" => self.describe_link(name, v)?,
            "reference" => self.references.push(v.into()),
            "metadata" => self.metadata.push(v.into()),
            "caption" => self.caption.push(v.into()),
            "destination" => text(&mut self.destination),
            "session" => text(&mut self.session),
            "title" => text(&mut self.title),
            "repo" => text(&mut self.repo),
            "commit" => text(&mut self.commit),
            "agent" => text(&mut self.agent),
            "token" => text(&mut self.token),
            "workspace" => text(&mut self.workspace),
            "format" => text(&mut self.format),
            "jq" => text(&mut self.jq),
            "from" => text(&mut self.from),
            "harness" => text(&mut self.harness),
            "worktree" => text(&mut self.worktree),
            "max-usd" => text(&mut self.max_usd),
            "max-tokens" => text(&mut self.max_tokens),
            #[cfg(feature = "harness")]
            "output-format" => text(&mut self.output_format),
            #[cfg(feature = "harness")]
            "model" => text(&mut self.model),
            #[cfg(feature = "harness")]
            "resume" => text(&mut self.resume),
            #[cfg(feature = "harness")]
            "permission-mode" => text(&mut self.permission_mode),
            #[cfg(feature = "harness")]
            "toolset" => text(&mut self.toolset),
            #[cfg(feature = "harness")]
            "effort" => text(&mut self.effort),
            #[cfg(feature = "harness")]
            "name" => text(&mut self.name),
            #[cfg(feature = "harness")]
            "api-key-env" => text(&mut self.api_key_env),
            #[cfg(feature = "harness")]
            "base-url" => text(&mut self.base_url),
            #[cfg(feature = "harness")]
            "client-id" => text(&mut self.client_id),
            #[cfg(feature = "harness")]
            "binary" => text(&mut self.binary),
            #[cfg(feature = "harness")]
            "config-dir" => text(&mut self.config_dir),
            #[cfg(feature = "harness")]
            "method" => text(&mut self.method),
            _ => {
                let b = parse_bool(name, v)?;
                *match name {
                    "private" => &mut self.private,
                    "global" => &mut self.global,
                    "dev" => &mut self.dev,
                    "no-browser" => &mut self.no_browser,
                    "json" => &mut self.json,
                    "quiet" => &mut self.quiet,
                    "help" => &mut self.help,
                    "version" => &mut self.version,
                    "dry-run" => &mut self.dry_run,
                    "yes" => &mut self.yes,
                    "no-network" => &mut self.no_network,
                    "all" => &mut self.all,
                    "thinking" => &mut self.thinking,
                    #[cfg(feature = "harness")]
                    "print" => &mut self.print,
                    #[cfg(feature = "harness")]
                    "device" => &mut self.device,
                    #[cfg(feature = "harness")]
                    "trust" => &mut self.trust,
                    #[cfg(feature = "harness")]
                    "default" => &mut self.default,
                    #[cfg(feature = "harness")]
                    "remove" => &mut self.remove,
                    other => unreachable!("catalog flag {other} has no field"),
                } = b;
            }
        }
        Ok(())
    }

    /// --link-title and --link-rel describe the --link before them. One before
    /// any --link, or a second for the same link, is a mistake worth naming:
    /// silently dropping or overwriting a label loses what the caller typed.
    fn describe_link(&mut self, name: &str, v: &str) -> Result<(), String> {
        let Some(at) = self.links.len().checked_sub(1) else {
            return Err(format!(
                "invalid value {v:?} for flag -{name}: --{name} describes the --link before it, and none was given yet: write --link <url> --{name} {v:?}"
            ));
        };
        if !self.described.insert((name.to_string(), at)) {
            return Err(format!(
                "invalid value {v:?} for flag -{name}: --{name} was given twice for the same --link ({}): each link takes one, after the --link it belongs to",
                self.links[at].url
            ));
        }
        let link = &mut self.links[at];
        if name == "link-title" {
            link.title = v.into();
        } else {
            link.rel = v.into();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(line: &str) -> (Flags, Vec<String>, Result<(), String>) {
        parse(&line.split_whitespace().map(String::from).collect::<Vec<_>>())
    }

    #[test]
    fn flags_may_follow_positionals_in_either_spelling() {
        let (f, pos, ok) = run("uploads create a.png --title=x -run r b.png --private --limit 3");
        assert!(ok.is_ok());
        assert_eq!(pos, ["uploads", "create", "a.png", "b.png"]);
        assert_eq!((f.title.as_str(), f.run.as_str(), f.private, f.limit), ("x", "r", true, 3));
        assert!(f.given.contains("title") && !f.given.contains("jq"));
    }

    #[test]
    fn a_link_takes_the_title_and_rel_that_follow_it() {
        let (f, _, ok) = run("push a --link https://a --link-rel fixes --link https://b --link-title B");
        assert!(ok.is_ok());
        assert_eq!((f.links[0].rel.as_str(), f.links[1].title.as_str()), ("fixes", "B"));
        assert!(run("push a --link-title orphan").2.is_err());
        assert!(run("push a --link https://a --link-rel x --link-rel y").2.is_err());
    }

    #[test]
    fn limits_read_the_way_go_reads_an_int() {
        for (v, want) in [("10", Some(10)), ("-3", Some(-3)), ("0x10", Some(16)), ("010", Some(8)), ("0o17", Some(15)), ("0b101", Some(5)), ("0x_1_0", Some(16)), ("1_0", Some(10)), ("1__0", None), ("10_", None), ("_1", None), ("", None), ("x", None), ("0x", None)] {
            assert_eq!(parse_int(v), want, "{v}");
        }
    }

    #[test]
    fn mistakes_are_named() {
        assert_eq!(run("push --nope").2.unwrap_err(), "flag provided but not defined: -nope");
        assert_eq!(run("push --run").2.unwrap_err(), "flag needs an argument: -run");
        assert!(run("sessions --limit x").2.unwrap_err().contains("-limit"));
        assert!(run("push --private=maybe").2.is_err());
        let (_, pos, ok) = run("push -- --not-a-flag");
        assert!(ok.is_ok());
        assert_eq!(pos, ["push", "--not-a-flag"]);
        let (f, _, _) = run("-h -v");
        assert!(f.help && f.version);
    }
}
