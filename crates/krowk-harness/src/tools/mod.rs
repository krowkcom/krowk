//! The tools the native loop offers (R-TOOL-1): `read`, `write`, one edit
//! tool, `bash`, `grep`, `glob`, `todo_write` (`crate::todo`), `publish`
//! (`crate::evidence`) and `subagent` (`crate::subagent`) — the last two of
//! which the loop runs itself, since they act on the session rather than
//! the file system. Which edit tool — `str_replace`,
//! `apply_patch` or `search_replace` — is the turn's toolset preset's to
//! say (`crate::toolset`). Each input is a Rust type the tool's JSON Schema
//! is derived from, so the definition the model sees and the parser that
//! reads its call cannot disagree.
//!
//! A tool never fails the turn. A bad input, a missing file, a timeout or a
//! refusal is a result with `isError`, which the model reads and corrects:
//! the result item is `toolResult {callId, output, isError}`, `output` the
//! text the model is sent back.
//!
//! Whether a call runs is the permission evaluator's to say
//! (`crate::permissions`): `describe` says what a call reads, changes or
//! runs, the evaluator judges that against the mode, the rules and the
//! grants, and `execute` runs it in the scope the verdict opened — the
//! working directory and its added directories, unless a rule, a person or
//! bypassPermissions opened more.

mod edit;
mod patch;
pub(crate) mod search;

use crate::permissions::{Access, Call};
use crate::protocol::{Grammar, PermissionMode, ToolDefinition};
use crate::toolset::{EditTool, Toolset};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub use edit::{SearchReplaceInput, StrReplaceInput, WriteInput};
pub use patch::{ApplyPatchInput, GRAMMAR as APPLY_PATCH_GRAMMAR};
pub use search::{GlobInput, GrepInput};

pub const READ: &str = "read";
pub const BASH: &str = "bash";

/// A read returns at most this many lines unless asked for fewer.
const READ_DEFAULT_LINES: usize = 2000;
/// And at most this many bytes of them, whatever the line count.
const READ_MAX_BYTES: usize = 256 << 10;
/// A line longer than this is cut, so one minified file cannot fill the
/// context on its own.
const READ_MAX_LINE: usize = 2000;
const BASH_DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);
const BASH_MAX_TIMEOUT: Duration = Duration::from_secs(600);
/// Output beyond this is cut from the middle: the start says what ran, the
/// end says how it finished.
const BASH_MAX_OUTPUT: usize = 30_000;

/// Read a file from the filesystem.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadInput {
    /// The file to read.
    pub path: String,
    /// The first line to return, counting from 1.
    #[serde(default)]
    pub offset: Option<usize>,
    /// How many lines to return (at most 2000 by default).
    #[serde(default)]
    pub limit: Option<usize>,
}

/// Run a shell command.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BashInput {
    /// The command, run by `bash -c` in the working directory.
    pub command: String,
    /// Milliseconds before the command is killed: 120000 by default, at most 600000.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

pub const WRITE: &str = "write";
pub const STR_REPLACE: &str = "str_replace";
pub const APPLY_PATCH: &str = "apply_patch";
pub const SEARCH_REPLACE: &str = "search_replace";
pub const GREP: &str = "grep";
pub const GLOB: &str = "glob";

impl EditTool {
    /// The tool's name, as the model calls it.
    pub fn name(self) -> &'static str {
        match self {
            EditTool::StrReplace => STR_REPLACE,
            EditTool::ApplyPatch => APPLY_PATCH,
            EditTool::SearchReplace => SEARCH_REPLACE,
        }
    }
}

const APPLY_PATCH_DESCRIPTION: &str = "Edit files with a patch. The patch is an envelope:\n\
*** Begin Patch\n\
[one or more hunks]\n\
*** End Patch\n\
A hunk is `*** Add File: <path>` followed by the new file's lines, each prefixed with `+`; `*** Delete File: <path>`; or `*** Update File: <path>`, optionally followed by `*** Move to: <new path>`, then its changes. A change is a run of lines prefixed with ` ` (context, unchanged), `-` (removed) or `+` (added), with about three lines of context above and below; start each run with `@@ <a line just above it, such as its function or class>` when the context alone is ambiguous, and end a run that reaches the end of the file with `*** End of File`. Paths are relative to the working directory. The patch applies whole or not at all.";

/// The definitions, in the order the model is shown them. The order is part
/// of the cached prefix, so it never varies within a toolset.
pub fn definitions(ts: &Toolset) -> Vec<ToolDefinition> {
    let function = |name: &str, description: &str, input_schema: Value| ToolDefinition { name: name.into(), description: description.into(), input_schema, grammar: None };
    let edit = match ts.preset.edit {
        EditTool::StrReplace => function(
            STR_REPLACE,
            "Edit a file by replacing text. `old_str` must match the file exactly, whitespace and indentation included, and occur exactly once — include enough surrounding lines to make it unique — unless `replace_all` is set. Read the file before editing it. Use write to create a file.",
            input_schema::<StrReplaceInput>(),
        ),
        EditTool::SearchReplace => function(
            SEARCH_REPLACE,
            "Edit a file by searching for text and replacing it. `old_string` must match the file exactly, whitespace and indentation included, and occur exactly once — include enough surrounding lines to make it unique — unless `replace_all` is set. Read the file before editing it. Use write to create a file.",
            input_schema::<SearchReplaceInput>(),
        ),
        // Freeform where the model takes grammar tools: the patch is the
        // call's whole input, with no JSON string escaping to get wrong.
        EditTool::ApplyPatch if ts.custom_tools => ToolDefinition {
            name: APPLY_PATCH.into(),
            description: APPLY_PATCH_DESCRIPTION.into(),
            input_schema: serde_json::json!({ "type": "string" }),
            grammar: Some(Grammar { syntax: "lark".into(), definition: patch::GRAMMAR.into() }),
        },
        EditTool::ApplyPatch => function(APPLY_PATCH, APPLY_PATCH_DESCRIPTION, input_schema::<ApplyPatchInput>()),
    };
    vec![
        function(
            READ,
            "Read a text file. Returns its lines numbered from 1, like `cat -n`; use `offset` and `limit` for a long file. Prefer this to running `cat` through bash.",
            input_schema::<ReadInput>(),
        ),
        function(WRITE, "Write a file, creating it or replacing it whole. To change part of an existing file, use the edit tool instead.", input_schema::<WriteInput>()),
        edit,
        function(
            BASH,
            "Run a shell command with `bash -c` in the working directory; returns its combined stdout and stderr, then the exit code. Output beyond 30000 bytes is cut from the middle.",
            input_schema::<BashInput>(),
        ),
        function(
            GREP,
            "Search file contents with a regular expression. Returns matching lines as `path:line:text`, 200 at most. Skips binary files and what .gitignore excludes. Prefer this to running grep or rg through bash.",
            input_schema::<GrepInput>(),
        ),
        function(GLOB, "Find files whose path matches a glob. Returns paths sorted, 1000 at most, skipping what .gitignore excludes.", input_schema::<GlobInput>()),
        function(crate::todo::TODO_WRITE, crate::todo::DESCRIPTION, input_schema::<crate::todo::TodoWriteInput>()),
        function(crate::evidence::PUBLISH, crate::evidence::DESCRIPTION, input_schema::<crate::evidence::PublishInput>()),
        function(crate::subagent::SUBAGENT, crate::subagent::DESCRIPTION, input_schema::<crate::subagent::SubagentInput>()),
    ]
}

/// The schema of a tool's input, as the Anthropic API wants it: an object
/// schema, without the `$schema` and `title` noise schemars adds, and
/// tidied, since every byte of it rides on every model call: a nested type
/// is written in place rather than referenced, and an optional field is its
/// type alone — `required` already says it may be left out, so `null` and
/// `default: null` beside it tell the model nothing.
pub(crate) fn input_schema<T: JsonSchema>() -> Value {
    let mut v = schemars::schema_for!(T).to_value();
    let defs = match &mut v {
        Value::Object(m) => {
            m.remove("$schema");
            m.remove("title");
            m.remove("description");
            m.remove("$defs")
        }
        _ => None,
    };
    tidy(&mut v, defs.as_ref());
    v
}

fn tidy(v: &mut Value, defs: Option<&Value>) {
    match v {
        Value::Object(m) => {
            if let Some(Value::String(r)) = m.get("$ref")
                && let Some(def) = r.strip_prefix("#/$defs/").and_then(|name| defs?.get(name))
            {
                let mut def = def.clone();
                if let Value::Object(d) = &mut def {
                    // The field's own description says what it is for.
                    d.remove("description");
                }
                m.remove("$ref");
                if let Value::Object(d) = def {
                    for (k, x) in d {
                        m.entry(k).or_insert(x);
                    }
                }
            }
            if let Some(Value::Array(types)) = m.get("type")
                && types.len() == 2
                && types.iter().any(|t| t == "null")
            {
                let t = types.iter().find(|t| *t != "null").cloned().unwrap_or(Value::Null);
                m.insert("type".into(), t);
                if m.get("default") == Some(&Value::Null) {
                    m.remove("default");
                }
            }
            for x in m.values_mut() {
                tidy(x, defs);
            }
        }
        Value::Array(a) => a.iter_mut().for_each(|x| tidy(x, defs)),
        _ => {}
    }
}

/// Everything a tool call is run with.
#[derive(Clone, Copy)]
pub struct ToolEnv<'a> {
    pub cwd: &'a Path,
    pub permission_mode: PermissionMode,
    /// The edit tool the turn offers: the only one it runs.
    pub edit: EditTool,
    /// Where `publish` sends files, and the channel a run it opens is
    /// reported on; none when the host publishes nothing.
    pub evidence: Option<(&'a crate::evidence::Evidence, &'a crate::engine::Events)>,
}

/// Parses a call's input, or answers why it cannot.
fn parse_input<T: for<'de> Deserialize<'de>>(name: &str, input: &Value) -> Result<T, (String, bool)> {
    T::deserialize(input).map_err(|e| (format!("invalid input for {name}: {e}"), true))
}

/// Blocking file work, off the async runtime: the runtime also has to hear
/// an interrupt while it runs.
async fn blocking(f: impl FnOnce() -> (String, bool) + Send + 'static) -> (String, bool) {
    tokio::task::spawn_blocking(f).await.unwrap_or_else(|e| (format!("the tool failed: {e}"), true))
}

/// What a call of one of these tools is, in the terms permission rules are
/// written in (`crate::permissions`): the paths it reads or changes, or the
/// command it runs. A call that cannot run at all — a bad input, a tool the
/// turn does not offer, a patch that does not parse — is answered here.
pub fn describe(name: &str, input: &Value, env: &ToolEnv<'_>) -> Result<Call, (String, bool)> {
    let is_edit = [WRITE, STR_REPLACE, APPLY_PATCH, SEARCH_REPLACE].contains(&name);
    if is_edit && name != WRITE && name != env.edit.name() {
        return Err((format!("there is no tool named {name:?} in this session — edit files with {}", env.edit.name()), true));
    }
    let at = |p: &str| resolve(env.cwd, p);
    let call = |access: Access| Call { tool: crate::permissions::rules::canonical(name), access, subject: None };
    Ok(match name {
        READ => call(Access::Read(vec![at(&parse_input::<ReadInput>(name, input)?.path)])),
        WRITE => call(Access::Edit(vec![at(&parse_input::<WriteInput>(name, input)?.path)])),
        STR_REPLACE => call(Access::Edit(vec![at(&parse_input::<StrReplaceInput>(name, input)?.path)])),
        SEARCH_REPLACE => call(Access::Edit(vec![at(&parse_input::<SearchReplaceInput>(name, input)?.file_path)])),
        APPLY_PATCH => {
            let text = patch::input_text(input).map_err(|e| (format!("invalid input for apply_patch: {e}"), true))?;
            let paths = patch::paths(&text).map_err(|e| (format!("the patch did not parse, so nothing was changed: {e}"), true))?;
            call(Access::Edit(paths.iter().map(|p| at(p)).collect()))
        }
        GREP => call(Access::Read(vec![parse_input::<GrepInput>(name, input)?.path.as_deref().map_or_else(|| env.cwd.to_path_buf(), at)])),
        GLOB => call(Access::Read(vec![parse_input::<GlobInput>(name, input)?.path.as_deref().map_or_else(|| env.cwd.to_path_buf(), at)])),
        BASH => call(Access::Bash(parse_input::<BashInput>(name, input)?.command)),
        crate::evidence::PUBLISH => crate::evidence::call(env.cwd, input)?,
        other => return Err((format!("there is no tool named {other:?} — the tools are read, write, {}, bash, grep, glob, todo_write, publish and subagent", env.edit.name()), true)),
    })
}

/// Runs one call judged by the mode alone, with nobody to ask: what a
/// caller with no settings and no client gets. The native loop judges by
/// the turn's whole policy instead (`describe`, then its gate, then
/// `execute`). Returns the output and whether it is an error.
pub async fn run(name: &str, input: &Value, env: &ToolEnv<'_>) -> (String, bool) {
    let call = match describe(name, input, env) {
        Ok(c) => c,
        Err(e) => return e,
    };
    let gate = crate::permissions::Gate::modes_only(env.cwd, env.permission_mode);
    match gate.verdict(&call, None) {
        crate::permissions::Verdict::Allow(opens) => execute(name, input, env, gate.scope(opens)).await,
        crate::permissions::Verdict::Deny(m) => (m, true),
        crate::permissions::Verdict::Ask { .. } => {
            let (tx, _rx) = tokio::sync::mpsc::channel(1);
            let (_c, cancel) = tokio::sync::watch::channel(false);
            match gate.check(&call, name, input, None, &tx, &cancel).await {
                Ok(opens) => execute(name, input, env, gate.scope(opens)).await,
                Err(m) => (m, true),
            }
        }
    }
}

/// Runs one call the permission evaluator allowed, in the scope it opened:
/// the tools still hold every path to that scope, a second fence behind the
/// first.
pub async fn execute(name: &str, input: &Value, env: &ToolEnv<'_>, scope: Scope) -> (String, bool) {
    match name {
        READ => match parse_input::<ReadInput>(name, input) {
            Ok(i) => read(&i, &scope).await,
            Err(e) => e,
        },
        WRITE => match parse_input::<WriteInput>(name, input) {
            Ok(i) => blocking(move || edit::write(&i, &scope)).await,
            Err(e) => e,
        },
        STR_REPLACE => match parse_input::<StrReplaceInput>(name, input) {
            Ok(i) => blocking(move || {
                let r = edit::Replace { tool: STR_REPLACE, old_name: "old_str", path: &i.path, old: &i.old_str, new: &i.new_str, replace_all: i.replace_all.unwrap_or(false) };
                edit::replace(&r, &scope)
            })
            .await,
            Err(e) => e,
        },
        SEARCH_REPLACE => match parse_input::<SearchReplaceInput>(name, input) {
            Ok(i) => blocking(move || {
                let r = edit::Replace { tool: SEARCH_REPLACE, old_name: "old_string", path: &i.file_path, old: &i.old_string, new: &i.new_string, replace_all: i.replace_all.unwrap_or(false) };
                edit::replace(&r, &scope)
            })
            .await,
            Err(e) => e,
        },
        APPLY_PATCH => match patch::input_text(input) {
            Ok(text) => blocking(move || patch::apply(&text, &scope)).await,
            Err(e) => (format!("invalid input for apply_patch: {e}"), true),
        },
        GREP => match parse_input::<GrepInput>(name, input) {
            Ok(i) => blocking(move || search::grep(&i, &scope)).await,
            Err(e) => e,
        },
        GLOB => match parse_input::<GlobInput>(name, input) {
            Ok(i) => blocking(move || search::glob(&i, &scope)).await,
            Err(e) => e,
        },
        BASH => match parse_input::<BashInput>(name, input) {
            Ok(i) => bash(&i, env.cwd).await,
            Err(e) => e,
        },
        // krowk_push's rules keep it in the working directory and away from
        // credential files and hard links; the mode keeps it from running at
        // all where nothing may leave the session.
        crate::evidence::PUBLISH => match env.evidence {
            Some((ev, events)) => ev.publish(env.cwd, input, events).await,
            None => (crate::evidence::UNAVAILABLE.into(), true),
        },
        // The loop's own: they act on the session, not the file system.
        crate::todo::TODO_WRITE | crate::subagent::SUBAGENT => (format!("{name} is not available in this session"), true),
        other => (format!("there is no tool named {other:?} — the tools are read, write, {}, bash, grep, glob, todo_write, publish and subagent", env.edit.name()), true),
    }
}

fn resolve(cwd: &Path, path: &str) -> PathBuf {
    let p = Path::new(path);
    if p.is_absolute() { p.to_path_buf() } else { cwd.join(p) }
}

/// Files a deny rule keeps from being read, which a search skips rather
/// than shows: `Read(.env)` denied holds for grep over the directory too.
#[derive(Clone, Default)]
pub struct Hidden(pub Option<HideFn>);

/// Whether a path is one a deny rule keeps from being read.
pub type HideFn = std::sync::Arc<dyn Fn(&Path) -> bool + Send + Sync>;

impl Hidden {
    pub fn hides(&self, p: &Path) -> bool {
        self.0.as_ref().is_some_and(|f| f(p))
    }
}

impl std::fmt::Debug for Hidden {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_some() { "Hidden(rules)" } else { "Hidden(none)" })
    }
}

/// Where the file tools may reach: the working directory and the
/// directories the settings add to it (`additionalDirectories`), and for reading
/// the skills' directories too. A path is judged by where it really leads —
/// `..`, an absolute path, a symlinked directory or file, a dangling
/// symlink a write would follow — never by how it is spelled. The
/// permission evaluator judges a call against this scope first; the tools
/// hold the scope the verdict opened.
#[derive(Debug, Clone)]
pub struct Scope {
    pub cwd: PathBuf,
    /// More directories the file tools reach as they reach the working
    /// directory.
    pub roots: Vec<PathBuf>,
    /// Directories they may read and never change.
    pub read_roots: Vec<PathBuf>,
    /// Reaching outside all of those is allowed: by bypassPermissions, an
    /// allow rule, or a person.
    pub outside: bool,
    /// The fenced directories are open: bypassPermissions, or a person
    /// approved this call.
    pub open: bool,
    /// More directories no edit may reach unless they are open, beside
    /// `.git`, `.claude`, `.codex` and `.krowk`: krowk's own config
    /// directory, Claude Code's, a backend instance's (`CLAUDE_CONFIG_DIR`,
    /// `CODEX_HOME`), whose settings and hooks decide what runs next.
    pub protected: Vec<PathBuf>,
    /// Directories no tool reads, searches or changes without a person's
    /// say, wherever they sit: krowk's home, as named and as it leads — its
    /// keys, logins, sessions and settings.
    pub secrets: Vec<PathBuf>,
    pub hidden: Hidden,
}

/// Where one path leads, for the evaluator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reach {
    Inside,
    /// Outside the working directory and its added directories: why.
    Outside(String),
    /// Inside a fenced directory: why.
    Fenced(String),
}

/// Symlinks followed at most while resolving one path: a loop is refused,
/// not followed forever.
const MAX_LINKS: usize = 40;

/// The directories the file tools keep out of, whichever repository they
/// are in.
const FENCED: [&str; 4] = [".git", ".claude", ".codex", ".krowk"];

impl Scope {
    /// Only the working directory, nothing opened.
    pub fn within(cwd: &Path) -> Scope {
        Scope { cwd: cwd.to_path_buf(), roots: Vec::new(), read_roots: Vec::new(), outside: false, open: false, protected: Vec::new(), secrets: Vec::new(), hidden: Hidden::default() }
    }

    /// Where `p` (absolute) leads against the tools' reach, for a read or an
    /// edit, before anything is opened.
    pub fn reach(&self, p: &Path, edit: bool) -> Reach {
        let real = match real_path(p, 0) {
            Ok(r) => r,
            Err(e) => return Reach::Outside(format!("{} cannot be resolved: {e}", p.display())),
        };
        let canon = |d: &Path| d.canonicalize().unwrap_or_else(|_| d.to_path_buf());
        let root = canon(&self.cwd);
        let mut roots: Vec<PathBuf> = vec![root.clone()];
        roots.extend(self.roots.iter().map(|d| canon(d)));
        if !edit {
            roots.extend(self.read_roots.iter().map(|d| canon(d)));
        }
        let inside = roots.iter().any(|r| real.starts_with(r));
        if self.secret(p) {
            return Reach::Fenced(format!("{} is inside krowk's home, which holds its keys, logins and sessions: the tools read, search or change it only with a person's say", p.display()));
        }
        if edit && let Some(why) = self.fence(p, &real, &root) {
            return Reach::Fenced(why);
        }
        if inside {
            return Reach::Inside;
        }
        let leads = if real == p { String::new() } else { format!(" (it leads to {})", real.display()) };
        Reach::Outside(format!("{}{leads} is outside the working directory {}", p.display(), root.display()))
    }

    /// The reason `p` may not be changed without a person's say, if it may
    /// not: inside a `.git`, `.claude`, `.codex` or `.krowk` directory, as
    /// spelled or as it leads, or inside a `protected` one. git runs what
    /// its config names (`core.fsmonitor`, hooks), Claude Code what its
    /// settings, hooks and agents name, Codex what its project config, rules
    /// and hooks name, krowk what its own settings and hooks name — so a
    /// model that could write `.git/config` or `.claude/settings.json`
    /// could run any command without the bash permission. Codex keeps
    /// `.git` read-only for the same reason.
    fn fence(&self, p: &Path, real: &Path, root: &Path) -> Option<String> {
        // Compared the way the file system may: case-insensitively (macOS
        // and Windows open `.Claude` as `.claude`), and with the trailing
        // dots and spaces Windows drops (`.git.` is `.git`) — on every OS,
        // since a checkout travels between them.
        let fold = |c: &std::ffi::OsStr| c.to_string_lossy().trim_end_matches(['.', ' ']).to_ascii_lowercase();
        let inside = |q: &Path| q.components().find_map(|c| FENCED.into_iter().find(|d| fold(c.as_os_str()) == *d));
        let rel = |q: &Path| {
            let mut bases: Vec<PathBuf> = vec![self.cwd.clone(), root.to_path_buf()];
            bases.extend(self.roots.iter().cloned());
            bases.iter().find_map(|b| q.strip_prefix(b).ok().map(Path::to_path_buf)).unwrap_or_else(|| q.to_path_buf())
        };
        if let Some(dir) = inside(&rel(p)).or_else(|| inside(&rel(real))) {
            let runs = match dir {
                ".git" => "git runs what its config and hooks name, so it is left to git itself",
                ".claude" => "Claude Code runs what its settings, hooks and agents name",
                ".codex" => "Codex runs what its project config, rules and hooks name",
                _ => "krowk runs what its project config's hooks name, and its rules decide what else runs",
            };
            return Some(format!("{} is inside a {dir} directory, which the file tools change only with a person's say: {runs}", p.display()));
        }
        // Judged as spelled and as it leads, against the directory as named
        // and as it leads: a home reached through a symlink, or a link inside
        // the working directory that points into the home, is caught either way.
        let lower = |p: &Path| PathBuf::from(p.to_string_lossy().to_lowercase());
        let within = |q: &Path, d: &Path| {
            let q = lower(q);
            q.starts_with(lower(d)) || q.starts_with(lower(&d.canonicalize().unwrap_or_else(|_| d.to_path_buf())))
        };
        self.protected.iter().find(|d| within(real, d) || within(p, d)).map(|d| {
            format!("{} is inside {}, a directory whose settings decide what runs (krowk's, or a backend's own), which the file tools change only with a person's say", p.display(), d.display())
        })
    }

    /// Whether `p` is, or is inside, one of `secrets` — by where it leads
    /// (`..` taken out by its words, and every symlink followed), against
    /// the secret as named and as it leads, regardless of case: a search
    /// through `.krowk/../krowk` or a symlinked alias skips it too.
    pub fn secret(&self, p: &Path) -> bool {
        if self.secrets.is_empty() {
            return false;
        }
        let lower = |p: &Path| PathBuf::from(p.to_string_lossy().to_lowercase());
        let seen: Vec<PathBuf> = [Some(lexical(p)), real_path(p, 0).ok()].into_iter().flatten().map(|q| lower(&q)).collect();
        // The secrets as named and as they lead: `Policy::load` lists both,
        // so a search resolves only the path it is at, once.
        self.secrets.iter().any(|s| {
            let n = lower(&lexical(s));
            seen.iter().any(|q| q.starts_with(&n))
        })
    }

    /// The path a tool was given, resolved against the working directory,
    /// or why the tool may not touch it. The path is returned as spelled
    /// (joined to the working directory), so messages name what the model
    /// asked for; the check is on where it leads.
    pub fn path(&self, path: &str) -> Result<PathBuf, String> {
        let p = resolve(&self.cwd, path);
        if self.outside {
            return Ok(p);
        }
        match self.reach(&p, false) {
            Reach::Inside | Reach::Fenced(_) => Ok(p),
            Reach::Outside(why) => Err(format!("{why}: the file tools reach only inside the working directory and the directories the settings add, unless a person allows it, an allow rule covers it, or krowk runs with `--permission-mode bypassPermissions`")),
        }
    }

    /// `path`, for a tool that changes the file: also never inside a fenced
    /// or `protected` directory unless the scope is open.
    pub fn edit_path(&self, path: &str) -> Result<PathBuf, String> {
        let p = resolve(&self.cwd, path);
        match self.reach(&p, true) {
            Reach::Inside => Ok(p),
            Reach::Fenced(_) if self.open => Ok(p),
            Reach::Outside(_) if self.outside => Ok(p),
            Reach::Fenced(why) => Err(format!("{why} — ask the person, or rerun with `--permission-mode bypassPermissions`")),
            Reach::Outside(why) => Err(format!("{why}: the file tools reach only inside the working directory and the directories the settings add, unless a person allows it, an allow rule covers it, or krowk runs with `--permission-mode bypassPermissions`")),
        }
    }
}

/// `.` and `..` taken out of a path by its words alone.
fn lexical(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir if out.pop() => {}
            c => out.push(c),
        }
    }
    out
}

/// Where a path leads once every symlink in it is followed, for a path
/// that need not exist yet: the nearest part that exists is canonicalized,
/// and the rest, which cannot hold a symlink, is appended. A dangling
/// symlink is followed to where it points, since a write through it would
/// create that.
pub(crate) fn real_path(p: &Path, links: usize) -> Result<PathBuf, String> {
    use std::path::Component;
    if links > MAX_LINKS {
        return Err("too many levels of symbolic links".into());
    }
    if let Ok(c) = p.canonicalize() {
        return Ok(c);
    }
    match std::fs::symlink_metadata(p) {
        Ok(m) if m.file_type().is_symlink() => {
            let target = std::fs::read_link(p).map_err(|e| e.to_string())?;
            let target = if target.is_absolute() { target } else { p.parent().unwrap_or(Path::new("/")).join(target) };
            real_path(&target, links + 1)
        }
        Ok(_) => Err("it exists but cannot be resolved".into()),
        Err(_) => {
            let parent = p.parent().ok_or("no parent directory")?;
            let base = real_path(parent, links)?;
            match p.components().next_back() {
                Some(Component::ParentDir) => Ok(base.parent().map(Path::to_path_buf).unwrap_or(base)),
                Some(Component::CurDir) | None => Ok(base),
                Some(c) => Ok(base.join(c.as_os_str())),
            }
        }
    }
}

/// Replaces a file's content as one step: written to a temporary file
/// beside it, given the old file's permissions, then renamed over it, so a
/// crash or a full disk never leaves half a file. A symlink is written
/// through, as a plain write would be: the link stays a link.
pub(crate) fn write_atomic(path: &Path, content: &[u8]) -> std::io::Result<()> {
    let tmp = stage(path, content)?;
    commit(&tmp, path)
}

/// The first half of `write_atomic`: the temporary file, written and
/// flushed, ready to rename. A patch stages every file before it renames any.
pub(crate) fn stage(path: &Path, content: &[u8]) -> std::io::Result<PathBuf> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let target = target_of(path);
    // A rename would replace a read-only file as readily as any other; the
    // mode says it is not to be written, so it is not.
    if std::fs::metadata(&target).is_ok_and(|m| m.permissions().readonly()) {
        return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "the file is read-only"));
    }
    let dir = target.parent().unwrap_or(Path::new("."));
    let name = target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = dir.join(format!(".{name}.krowk-{}-{}.tmp", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
    let written = (|| {
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        f.write_all(content)?;
        if let Ok(m) = std::fs::metadata(&target) {
            f.set_permissions(m.permissions())?;
        }
        f.sync_data()
    })();
    match written {
        Ok(()) => Ok(tmp),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Renames a staged file over its target.
pub(crate) fn commit(tmp: &Path, path: &Path) -> std::io::Result<()> {
    std::fs::rename(tmp, target_of(path)).inspect_err(|_| {
        let _ = std::fs::remove_file(tmp);
    })
}

/// The file a write lands in: through a symlink, when the path is one.
fn target_of(path: &Path) -> PathBuf {
    if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
        return path.canonicalize().or_else(|_| real_path(path, 0).map_err(std::io::Error::other)).unwrap_or_else(|_| path.to_path_buf());
    }
    path.to_path_buf()
}

/// Read scans at most this much of a file: a line count past it is not worth
/// the wait, and nothing past it is shown.
const READ_MAX_SCAN: u64 = 64 << 20;

/// Off the async runtime: a read is blocking file I/O, and the runtime also
/// has to hear an interrupt while it runs.
async fn read(i: &ReadInput, scope: &Scope) -> (String, bool) {
    let path = match scope.path(&i.path) {
        Ok(p) => p,
        Err(e) => return (e, true),
    };
    // Both come from the model: clamped, so no value overflows the arithmetic.
    let (start, limit) = (i.offset.unwrap_or(1).clamp(1, usize::MAX / 2), i.limit.unwrap_or(READ_DEFAULT_LINES).clamp(1, usize::MAX / 2));
    tokio::task::spawn_blocking(move || read_file(&path, start, limit))
        .await
        .unwrap_or_else(|e| (format!("read failed: {e}"), true))
}

/// Opens only a regular file, and never blocks opening it: a FIFO, a device
/// or `/dev/stdin` put where a file was expected is refused, not waited on
/// or read forever. The check is repeated on the open handle, so a path
/// swapped between the two cannot slip one through.
fn open_regular(path: &Path) -> Result<(std::fs::File, u64), String> {
    let not_regular = || format!("{} is not a regular file (a directory, a device or a pipe), which read does not open", path.display());
    match std::fs::metadata(path) {
        Ok(m) if m.is_file() => {}
        Ok(_) => return Err(not_regular()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(format!("{} does not exist", path.display())),
        Err(e) => return Err(format!("{} could not be read: {e}", path.display())),
    }
    let mut o = std::fs::OpenOptions::new();
    o.read(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(&mut o, libc::O_NONBLOCK);
    let f = o.open(path).map_err(|e| format!("{} could not be read: {e}", path.display()))?;
    let meta = f.metadata().map_err(|e| format!("{} could not be read: {e}", path.display()))?;
    if !meta.is_file() {
        return Err(not_regular());
    }
    Ok((f, meta.len()))
}

fn read_file(path: &Path, start: usize, limit: usize) -> (String, bool) {
    use std::io::{BufRead, Read};
    let (file, size) = match open_regular(path) {
        Ok(f) => f,
        Err(e) => return (e, true),
    };
    let mut r = std::io::BufReader::with_capacity(64 << 10, file.take(READ_MAX_SCAN));
    let (mut out, mut total, mut last, mut full) = (String::new(), 0usize, start - 1, false);
    let mut line = Vec::new();
    loop {
        // One line, keeping at most what a shown line can use: a minified
        // file's one line costs its first few kilobytes of memory, not all of it.
        line.clear();
        let mut consumed = 0usize;
        loop {
            let buf = match r.fill_buf() {
                Ok(b) => b,
                Err(e) => return (format!("{} could not be read: {e}", path.display()), true),
            };
            if buf.is_empty() {
                break;
            }
            let (chunk, done) = match buf.iter().position(|b| *b == b'\n') {
                Some(i) => (&buf[..=i], true),
                None => (buf, false),
            };
            let keep = (READ_MAX_LINE * 4).saturating_sub(line.len()).min(chunk.len());
            line.extend_from_slice(&chunk[..keep]);
            let n = chunk.len();
            consumed += n;
            r.consume(n);
            if done {
                break;
            }
        }
        if consumed == 0 {
            break;
        }
        if line.contains(&0) {
            return (format!("{} is a binary file, which read does not show", path.display()), true);
        }
        total += 1;
        if total < start || total >= start.saturating_add(limit) || full {
            continue;
        }
        while matches!(line.last(), Some(b'\n' | b'\r')) {
            line.pop();
        }
        let text = String::from_utf8_lossy(&line);
        let text = if text.chars().count() > READ_MAX_LINE { text.chars().take(READ_MAX_LINE).collect::<String>() + "…" } else { text.into_owned() };
        let row = format!("{total:>6}\t{text}\n");
        if out.len() + row.len() > READ_MAX_BYTES {
            full = true;
            continue;
        }
        out += &row;
        last = total;
    }
    let scanned_all = size <= READ_MAX_SCAN;
    let count = if scanned_all { format!("{total} lines") } else { format!("more than {total} lines (read scans the first {} MB)", READ_MAX_SCAN >> 20) };
    if total == 0 {
        return (format!("{} is empty", path.display()), false);
    }
    if start > total {
        return (format!("{} has {count}, so there is nothing from line {start}", path.display()), true);
    }
    if last < total || !scanned_all {
        out += &format!("\n({} has {count}; this is {start}–{last}. Read on with offset {}.)\n", path.display(), last + 1);
    }
    (out, false)
}

/// How long the output is still read once the shell has exited: long enough
/// for what it wrote last, short enough that a process it left in the
/// background — which holds the pipes open — does not hold the call.
const BASH_DRAIN_AFTER_EXIT: Duration = Duration::from_millis(250);

/// A bounded capture of a stream: the first half of the budget kept whole,
/// the last half as a ring, and a count of what fell in between — so a
/// command that prints gigabytes costs 30 KB.
struct Capture {
    head: Vec<u8>,
    tail: std::collections::VecDeque<u8>,
    dropped: u64,
    half: usize,
}

impl Capture {
    fn new(max: usize) -> Capture {
        Capture { head: Vec::new(), tail: std::collections::VecDeque::new(), dropped: 0, half: max / 2 }
    }

    fn push(&mut self, mut b: &[u8]) {
        let room = self.half - self.head.len();
        if room > 0 {
            let n = room.min(b.len());
            self.head.extend_from_slice(&b[..n]);
            b = &b[n..];
        }
        self.tail.extend(b);
        while self.tail.len() > self.half {
            let over = self.tail.len() - self.half;
            self.tail.drain(..over);
            self.dropped += over as u64;
        }
    }

    fn render(&self) -> String {
        let (a, b) = self.tail.as_slices();
        let tail: Vec<u8> = [a, b].concat();
        if self.dropped == 0 {
            return String::from_utf8_lossy(&[self.head.as_slice(), &tail].concat()).into_owned();
        }
        format!("{}\n… {} bytes cut …\n{}", String::from_utf8_lossy(&self.head), self.dropped, String::from_utf8_lossy(&tail))
    }
}

/// Kills the command's whole process group when dropped armed: on a timeout,
/// and when the turn is interrupted and the call's future is dropped — so
/// what the command started dies with it, not just the shell.
struct GroupKill(Option<u32>);

impl Drop for GroupKill {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.0 {
            // SAFETY: kill(2) on the group this call created; a group that
            // already exited is ESRCH, which is fine.
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
    }
}

async fn bash(i: &BashInput, cwd: &Path) -> (String, bool) {
    use tokio::io::AsyncReadExt;
    let timeout = i.timeout_ms.map_or(BASH_DEFAULT_TIMEOUT, Duration::from_millis).min(BASH_MAX_TIMEOUT);
    let mut cmd = tokio::process::Command::new("bash");
    cmd.arg("-c").arg(&i.command).current_dir(cwd);
    cmd.stdin(std::process::Stdio::null());
    // One pipe for both streams, as a terminal would have it: the model
    // reads what the command printed in the order it printed it, which two
    // pipes merged by whichever wakes first cannot promise.
    #[cfg(unix)]
    let merged = match std::io::pipe().and_then(|(r, w)| Ok((r, w.try_clone()?, w))) {
        Ok((r, w1, w2)) => {
            cmd.stdout(w1).stderr(w2);
            r
        }
        Err(e) => return (format!("bash could not be started: {e}"), true),
    };
    #[cfg(not(unix))]
    cmd.stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    cmd.kill_on_drop(true);
    // Its own process group, so a timeout or an interrupt kills what the
    // command started too, not just the shell.
    #[cfg(unix)]
    cmd.process_group(0);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return (format!("bash could not be started: {e}"), true),
    };
    let mut group = GroupKill(child.id());
    // The command's copies of the write end go with it, so the pipe closes
    // when the shell and what it started are done.
    drop(cmd);
    #[cfg(unix)]
    let (mut so, mut se): (_, Option<tokio::process::ChildStderr>) = {
        let std_out = std::process::ChildStdout::from(std::os::fd::OwnedFd::from(merged));
        match tokio::process::ChildStdout::from_std(std_out) {
            Ok(so) => (so, None),
            Err(e) => return (format!("bash's output could not be read: {e}"), true),
        }
    };
    #[cfg(not(unix))]
    let (mut so, mut se) = (child.stdout.take().expect("piped"), child.stderr.take());
    let mut cap = Capture::new(BASH_MAX_OUTPUT);
    let run = async {
        let (mut b1, mut b2) = ([0u8; 8192], [0u8; 8192]);
        let (mut so_open, mut se_open) = (true, se.is_some());
        let mut status = None;
        let mut held_open = false;
        // Fixed once, when the shell exits: a background process that keeps
        // writing must not push the window out chunk by chunk.
        let mut drain_until: Option<tokio::time::Instant> = None;
        loop {
            if !so_open && !se_open && status.is_some() {
                break;
            }
            let drained = async {
                match drain_until {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                r = so.read(&mut b1), if so_open => match r {
                    Ok(n) if n > 0 => cap.push(&b1[..n]),
                    _ => so_open = false,
                },
                r = async {
                    match se.as_mut() {
                        Some(se) => se.read(&mut b2).await,
                        None => std::future::pending().await,
                    }
                }, if se_open => match r {
                    Ok(n) if n > 0 => cap.push(&b2[..n]),
                    _ => se_open = false,
                },
                s = child.wait(), if status.is_none() => {
                    status = Some(s);
                    drain_until = Some(tokio::time::Instant::now() + BASH_DRAIN_AFTER_EXIT);
                }
                _ = drained => {
                    held_open = true;
                    break;
                }
            }
        }
        (status.and_then(|s| s.ok()).and_then(|s| s.code()), held_open)
    };
    match tokio::time::timeout(timeout, run).await {
        Ok((code, held_open)) => {
            // Finished: what it left in the background is its business.
            group.0 = None;
            let text = cap.render();
            let mut tail = match code {
                Some(c) => format!("exit code {c}"),
                None => "killed by a signal".into(),
            };
            if held_open {
                tail += " (a process it started in the background still holds its output; krowk stopped reading when the shell exited)";
            }
            let body = if text.is_empty() { tail.clone() } else { format!("{}\n{tail}", text.trim_end_matches('\n')) };
            (body, code != Some(0))
        }
        Err(_) => {
            drop(group);
            (format!("the command timed out after {} ms and was killed", timeout.as_millis()), true)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    pub(super) fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("krowk-harness-tools-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn toolset(name: &str, custom_tools: bool) -> Toolset {
        Toolset { preset: crate::toolset::by_name(name).unwrap(), custom_tools }
    }

    #[test]
    fn r_tool_1_the_core_tools_are_derived_object_schemas_in_a_fixed_order() {
        for (preset, edit) in [("claude", STR_REPLACE), ("gpt", APPLY_PATCH), ("grok", SEARCH_REPLACE)] {
            let defs = definitions(&toolset(preset, false));
            assert_eq!(defs.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(), [READ, WRITE, edit, BASH, GREP, GLOB, crate::todo::TODO_WRITE, crate::evidence::PUBLISH, crate::subagent::SUBAGENT], "{preset}");
            assert!(defs.iter().all(|d| d.input_schema["type"] == "object" && d.grammar.is_none()), "function tools everywhere without custom tools");
            assert_eq!(definitions(&toolset(preset, false)), defs, "deterministic: the definitions are part of the cached prefix");
        }
        let defs = definitions(&toolset("claude", false));
        assert_eq!(defs[0].input_schema["required"], json!(["path"]));
        assert_eq!(defs[2].input_schema["required"], json!(["path", "old_str", "new_str"]));
        assert!(defs[3].input_schema["properties"]["timeout_ms"].is_object());
        assert_eq!(definitions(&toolset("grok", false))[2].input_schema["required"], json!(["file_path", "old_string", "new_string"]));
        assert_eq!(definitions(&toolset("gpt", false))[2].input_schema["required"], json!(["input"]));
        assert_eq!(defs[6].input_schema["required"], json!(["todos"]), "todo_write takes the whole list");
        assert_eq!(defs[7].input_schema["required"], json!(["files"]), "publish takes the files krowk_push takes");
        assert_eq!(defs[8].input_schema["required"], json!(["description", "prompt"]));
    }

    #[test]
    fn r_tool_2_apply_patch_is_a_grammar_tool_where_custom_tools_are_taken() {
        let defs = definitions(&toolset("gpt", true));
        let patch = &defs[2];
        assert_eq!((patch.name.as_str(), &patch.input_schema), (APPLY_PATCH, &json!({"type": "string"})));
        let g = patch.grammar.as_ref().expect("freeform");
        assert_eq!(g.syntax, "lark");
        assert!(g.definition.starts_with("start: begin_patch hunk+ end_patch"));
        // Only apply_patch has a freeform form; the other presets are the
        // same either way.
        assert_eq!(definitions(&toolset("claude", true)), definitions(&toolset("claude", false)));
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn r_tool_1_file_tools_stay_inside_the_working_directory_unless_bypassed() {
        use std::os::unix::fs::symlink;
        let base = dir("scope");
        let (cwd, outside) = (base.join("cwd"), base.join("outside"));
        std::fs::create_dir_all(cwd.join("sub")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), "secret\n").unwrap();
        std::fs::write(cwd.join("a.txt"), "a\n").unwrap();
        symlink(outside.join("secret.txt"), cwd.join("file-link")).unwrap();
        symlink(&outside, cwd.join("dir-link")).unwrap();
        symlink(outside.join("not-yet.txt"), cwd.join("dangling")).unwrap();
        symlink(cwd.join("a.txt"), cwd.join("inside-link")).unwrap();
        let env = ToolEnv { cwd: &cwd, permission_mode: PermissionMode::AcceptEdits, edit: EditTool::ApplyPatch, evidence: None };
        let refused = |r: (String, bool)| r.1 && r.0.contains("outside the working directory") && r.0.contains("bypassPermissions");
        let abs = outside.join("new.txt").display().to_string();
        for path in ["../outside/new.txt", abs.as_str(), "dir-link/new.txt", "dangling", "file-link", "sub/../../outside/x", "new/../../x"] {
            assert!(refused(run(WRITE, &json!({"path": path, "content": "x"}), &env).await), "write {path}");
        }
        assert!(!outside.join("new.txt").exists() && !outside.join("not-yet.txt").exists() && !base.join("x").exists());
        assert!(refused(run(READ, &json!({"path": "file-link"}), &env).await), "a symlinked file is judged by its target");
        assert!(refused(run(READ, &json!({"path": "../outside/secret.txt"}), &env).await));
        let edit = ToolEnv { edit: EditTool::StrReplace, ..env };
        assert!(refused(run(STR_REPLACE, &json!({"path": "file-link", "old_str": "secret", "new_str": "x"}), &edit).await));
        let grok = ToolEnv { edit: EditTool::SearchReplace, ..env };
        assert!(refused(run(SEARCH_REPLACE, &json!({"file_path": "dir-link/secret.txt", "old_string": "secret", "new_string": "x"}), &grok).await));
        assert!(refused(run(GREP, &json!({"pattern": "secret", "path": ".."}), &env).await));
        assert!(refused(run(GREP, &json!({"pattern": "secret", "path": "dir-link"}), &env).await));
        assert!(refused(run(GLOB, &json!({"pattern": "*", "path": "/"}), &env).await));
        // A patch whose Add, Update or Move target leads out changes nothing.
        for patch in [
            "*** Begin Patch\n*** Add File: ../outside/p.txt\n+x\n*** End Patch",
            "*** Begin Patch\n*** Update File: a.txt\n*** Move to: dir-link/moved.txt\n-a\n+b\n*** End Patch",
            "*** Begin Patch\n*** Delete File: file-link/../../outside/secret.txt\n*** End Patch",
        ] {
            let r = run(APPLY_PATCH, &json!({ "input": patch }), &env).await;
            assert!(refused(r.clone()), "{patch}: {r:?}");
        }
        assert_eq!(std::fs::read_to_string(outside.join("secret.txt")).unwrap(), "secret\n");
        assert_eq!(std::fs::read_to_string(cwd.join("a.txt")).unwrap(), "a\n");
        // Inside is fine however it is spelled, and a symlink inside that
        // leads inside is written through, staying a link.
        assert!(!run(WRITE, &json!({"path": "sub/../b.txt", "content": "b"}), &env).await.1);
        let abs_inside = cwd.join("c.txt").display().to_string();
        assert!(!run(WRITE, &json!({"path": abs_inside, "content": "c"}), &env).await.1);
        let (out, err) = run(WRITE, &json!({"path": "inside-link", "content": "through\n"}), &env).await;
        assert!(!err, "{out}");
        assert!(std::fs::symlink_metadata(cwd.join("inside-link")).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(cwd.join("a.txt")).unwrap(), "through\n");
        // Bypassed, the working directory is no fence.
        let bypass = ToolEnv { permission_mode: PermissionMode::BypassPermissions, ..env };
        assert!(!run(WRITE, &json!({"path": "../outside/new.txt", "content": "x"}), &bypass).await.1);
        assert_eq!(run(READ, &json!({"path": "file-link"}), &bypass).await.0, "     1\tsecret\n");
        let _ = std::fs::remove_dir_all(base);
    }

    #[cfg(unix)]
    #[test]
    fn writes_are_atomic_and_keep_the_files_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let d = dir("atomic");
        let f = d.join("run.sh");
        std::fs::write(&f, "old").unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o750)).unwrap();
        write_atomic(&f, b"new").unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "new");
        assert_eq!(std::fs::metadata(&f).unwrap().permissions().mode() & 0o777, 0o750);
        let left: Vec<_> = std::fs::read_dir(&d).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(left, ["run.sh"], "no temporary file is left behind");
        let _ = std::fs::remove_dir_all(d);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn nothing_inside_git_is_changed_and_git_runs_nothing_from_its_config() {
        let d = dir("git-guard");
        let git = |args: &[&str]| std::process::Command::new("git").args(args).current_dir(&d).status().is_ok_and(|s| s.success());
        if !git(&["init", "-q"]) {
            eprintln!("git is not installed: skipping");
            return;
        }
        std::fs::write(d.join("a.txt"), "needle\n").unwrap();
        // A hostile config: git would run this on ls-files and check-ignore.
        let marker = d.join("fsmonitor-ran");
        let config = std::fs::read_to_string(d.join(".git/config")).unwrap();
        std::fs::write(d.join(".git/config"), format!("{config}[core]\n\tfsmonitor = \"touch '{}'; false\"\n", marker.display())).unwrap();
        let env = ToolEnv { cwd: &d, permission_mode: PermissionMode::AcceptEdits, edit: EditTool::ApplyPatch, evidence: None };
        assert_eq!(run(GREP, &json!({"pattern": "needle"}), &env).await, ("a.txt:1:needle\n".into(), false));
        assert_eq!(run(GLOB, &json!({"pattern": "*.txt"}), &env).await, ("a.txt\n".into(), false));
        assert!(!marker.exists(), "the repository's fsmonitor ran");

        // And the model cannot write one: nothing under .git is changed.
        let refused = |r: (String, bool)| r.1 && r.0.contains("inside a .git directory");
        let before = std::fs::read_to_string(d.join(".git/config")).unwrap();
        assert!(refused(run(WRITE, &json!({"path": ".git/config", "content": "[core]\n"}), &env).await));
        assert!(refused(run(WRITE, &json!({"path": ".git/hooks/pre-commit", "content": "#!/bin/sh\n"}), &env).await));
        assert!(refused(run(WRITE, &json!({"path": "sub/../.git/config", "content": "x"}), &env).await));
        let patch = "*** Begin Patch\n*** Update File: a.txt\n*** Move to: .git/moved\n-needle\n+x\n*** End Patch";
        assert!(refused(run(APPLY_PATCH, &json!({ "input": patch }), &env).await));
        let edit = ToolEnv { edit: EditTool::StrReplace, ..env };
        assert!(refused(run(STR_REPLACE, &json!({"path": ".git/config", "old_str": "[core]", "new_str": "[x]"}), &edit).await));
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(d.join(".git"), d.join("gitlink")).unwrap();
            assert!(refused(run(WRITE, &json!({"path": "gitlink/config", "content": "x"}), &env).await), "judged by where it leads");
        }
        assert_eq!(std::fs::read_to_string(d.join(".git/config")).unwrap(), before);
        assert!(!d.join(".git/hooks/pre-commit").exists() && !d.join(".git/moved").exists());
        assert_eq!(std::fs::read_to_string(d.join("a.txt")).unwrap(), "needle\n");
        // Reading it is fine, and bypassed the fence is gone.
        assert!(!run(READ, &json!({"path": ".git/config"}), &env).await.1);
        let bypass = ToolEnv { permission_mode: PermissionMode::BypassPermissions, ..env };
        assert!(!run(WRITE, &json!({"path": ".git/info/note", "content": "x"}), &bypass).await.1);
        // .claude is fenced the same way: Claude Code runs its hooks.
        let claude = |r: (String, bool)| r.1 && r.0.contains("inside a .claude directory");
        assert!(claude(run(WRITE, &json!({"path": ".claude/settings.json", "content": "{}"}), &env).await));
        assert!(claude(run(WRITE, &json!({"path": "sub/.claude/agents/x.md", "content": "x"}), &env).await), "any component");
        // However a case-insensitive or a Windows file system would open it.
        for p in [".Claude/settings.json", ".CLAUDE/hooks/h.sh", ".claude./settings.json", ".claude /agents/a.md", "sub/.ClAuDe/settings.local.json"] {
            assert!(claude(run(WRITE, &json!({"path": p, "content": "{}"}), &env).await), "{p}");
        }
        for p in [".GIT/config", ".Git/hooks/pre-commit", ".git./config", ".git /hooks/x"] {
            assert!(refused(run(WRITE, &json!({"path": p, "content": "x"}), &env).await), "{p}");
        }
        assert!(!run(WRITE, &json!({"path": ".claudette/notes.md", "content": "x"}), &env).await.1, "a name that only starts like it is fine");
        assert!(!d.join(".claude").exists());
        assert!(!run(WRITE, &json!({"path": ".claude/settings.json", "content": "{}"}), &bypass).await.1);
        let _ = std::fs::remove_dir_all(d);
    }

    /// Codex runs what `.codex` holds (project config, rules, hooks), and
    /// what its home holds for every session: both are kept like `.git`,
    /// in any case a case-insensitive file system would open.
    #[tokio::test(flavor = "current_thread")]
    async fn r_back_3_codex_config_and_the_instances_home_are_kept_like_git() {
        let d = dir("codex-guard");
        let env = ToolEnv { cwd: &d, permission_mode: PermissionMode::AcceptEdits, edit: EditTool::ApplyPatch, evidence: None };
        let fenced = |r: (String, bool), dir: &str| r.1 && r.0.contains(&format!("inside a {dir} directory"));
        assert!(fenced(run(WRITE, &json!({"path": ".codex/config.toml", "content": "x"}), &env).await, ".codex"));
        assert!(fenced(run(WRITE, &json!({"path": "sub/.Codex/rules/x.rules", "content": "x"}), &env).await, ".codex"), "any component, any case");
        assert!(fenced(run(WRITE, &json!({"path": ".GIT/config", "content": "x"}), &env).await, ".git"));
        assert!(fenced(run(WRITE, &json!({"path": ".claude/settings.json", "content": "{}"}), &env).await, ".claude"));
        assert!(!d.join(".codex").exists() && !d.join("sub").exists());
        let patch = "*** Begin Patch\n*** Add File: .codex/hooks.json\n+{}\n*** End Patch";
        assert!(run(APPLY_PATCH, &json!({ "input": patch }), &env).await.1, "a patch is fenced too");
        // The instance's own home, protected by name.
        let home = d.join("codex-home");
        std::fs::create_dir_all(&home).unwrap();
        let scope = Scope { protected: vec![home.clone()], ..Scope::within(&d) };
        assert!(scope.edit_path("codex-home/config.toml").unwrap_err().contains("settings decide what runs"));
        assert!(scope.edit_path(&home.join("sub/x").display().to_string()).is_err());
        assert!(scope.edit_path("codex-homework.txt").is_ok(), "a sibling that shares the prefix is not inside it");
        // Named through a link, and spelled as the link: both are the home.
        #[cfg(unix)]
        {
            let alias = d.join("home-alias");
            std::os::unix::fs::symlink(&home, &alias).unwrap();
            let via_alias = Scope { protected: vec![alias.clone()], ..Scope::within(&d) };
            assert!(via_alias.edit_path("codex-home/config.toml").is_err(), "the home as it leads");
            assert!(via_alias.edit_path("home-alias/config.toml").is_err(), "the home as spelled");
        }
        assert!(Scope { open: true, outside: true, ..scope }.edit_path("codex-home/config.toml").is_ok());
        let bypass = ToolEnv { permission_mode: PermissionMode::BypassPermissions, ..env };
        assert!(!run(WRITE, &json!({"path": ".codex/config.toml", "content": "x"}), &bypass).await.1);
        let _ = std::fs::remove_dir_all(d);
    }

    /// Ticket 10: a repository's agent definitions pick a subagent's model,
    /// prompt and tools, so the model cannot write one below bypass.
    #[tokio::test(flavor = "current_thread")]
    async fn r_sub_5_krowk_agent_definitions_are_kept_like_git() {
        let d = dir("krowk-guard");
        let env = ToolEnv { cwd: &d, permission_mode: PermissionMode::AcceptEdits, edit: EditTool::StrReplace, evidence: None };
        for path in [".krowk/agents/evil.md", ".KROWK/agents/evil.md", "sub/.krowk./agents/x.md", ".claude/agents/evil.md"] {
            let (out, refused) = run(WRITE, &json!({"path": path, "content": "---\nname: evil\n---\n"}), &env).await;
            assert!(refused && out.contains("inside a ."), "{path}: {out}");
        }
        assert!(!d.join(".krowk").exists() && !d.join(".KROWK").exists());
        let bypass = ToolEnv { permission_mode: PermissionMode::BypassPermissions, ..env };
        assert!(!run(WRITE, &json!({"path": ".krowk/agents/ok.md", "content": "x"}), &bypass).await.1);
        let _ = std::fs::remove_dir_all(d);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn a_read_only_file_is_not_replaced() {
        use std::os::unix::fs::PermissionsExt;
        let d = dir("read-only");
        let f = d.join("locked.txt");
        std::fs::write(&f, "keep\n").unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o444)).unwrap();
        let env = ToolEnv { cwd: &d, permission_mode: PermissionMode::BypassPermissions, edit: EditTool::StrReplace, evidence: None };
        for (tool, input) in [
            (WRITE, json!({"path": "locked.txt", "content": "gone"})),
            (STR_REPLACE, json!({"path": "locked.txt", "old_str": "keep", "new_str": "gone"})),
        ] {
            let (out, err) = run(tool, &input, &env).await;
            assert!(err && out.contains("read-only"), "{tool}: {out}");
        }
        let patch = ToolEnv { edit: EditTool::ApplyPatch, ..env };
        let (out, err) = run(APPLY_PATCH, &json!("*** Begin Patch\n*** Add File: new.txt\n+n\n*** Update File: locked.txt\n-keep\n+gone\n*** End Patch"), &patch).await;
        assert!(err && out.contains("read-only") && out.contains("nothing was changed"), "{out}");
        assert!(!d.join("new.txt").exists(), "the staged Add was not renamed into place");
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "keep\n");
        assert_eq!(std::fs::metadata(&f).unwrap().permissions().mode() & 0o777, 0o444);
        let left: Vec<_> = std::fs::read_dir(&d).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(left, ["locked.txt"], "no temporary file is left behind");
        let _ = std::fs::remove_dir_all(d);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn only_the_turns_edit_tool_runs() {
        let d = dir("edit-gate");
        std::fs::write(d.join("a.txt"), "x\n").unwrap();
        let env = ToolEnv { cwd: &d, permission_mode: PermissionMode::BypassPermissions, edit: EditTool::ApplyPatch, evidence: None };
        let (out, err) = run(STR_REPLACE, &json!({"path": "a.txt", "old_str": "x", "new_str": "y"}), &env).await;
        assert!(err && out.contains("edit files with apply_patch"), "{out}");
        assert_eq!(std::fs::read_to_string(d.join("a.txt")).unwrap(), "x\n");
        assert!(run("frobnicate", &json!({}), &env).await.0.contains("read, write, apply_patch, bash, grep, glob, todo_write, publish and subagent"));
        let (out, err) = run(crate::evidence::PUBLISH, &json!({"files": ["a.txt"]}), &env).await;
        assert!(err && out.contains("publish is not available"), "a host with no publisher says so: {out}");
        for mode in [PermissionMode::Default, PermissionMode::Plan] {
            let (out, err) = run(crate::evidence::PUBLISH, &json!({"files": ["a.txt"]}), &ToolEnv { permission_mode: mode, ..env }).await;
            let why = if mode == PermissionMode::Plan { "plan mode" } else { "--permission-mode acceptEdits" };
            assert!(err && out.contains(why), "{mode:?}: publish is held to what an edit is: {out}");
        }
        let _ = std::fs::remove_dir_all(d);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn read_numbers_lines_pages_and_refuses_what_it_cannot_show() {
        let d = dir("read");
        std::fs::write(d.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        std::fs::write(d.join("bin"), [0u8, 1, 2]).unwrap();
        let env = ToolEnv { cwd: &d, permission_mode: PermissionMode::Default, edit: EditTool::StrReplace, evidence: None };
        let (out, err) = run(READ, &json!({"path": "a.txt"}), &env).await;
        assert!(!err);
        assert_eq!(out, "     1\tone\n     2\ttwo\n     3\tthree\n");
        let (out, _) = run(READ, &json!({"path": "a.txt", "offset": 2, "limit": 1}), &env).await;
        assert!(out.starts_with("     2\ttwo\n") && out.contains("offset 3"), "{out}");
        assert!(run(READ, &json!({"path": "missing"}), &env).await.1);
        assert!(run(READ, &json!({"path": "bin"}), &env).await.1);
        assert!(run(READ, &json!({"file": "a.txt"}), &env).await.0.contains("invalid input"));
        // Offsets and limits the model makes up cannot overflow.
        let max = usize::MAX as u64;
        assert_eq!(run(READ, &json!({"path": "a.txt", "offset": 2, "limit": max}), &env).await, ("     2\ttwo\n     3\tthree\n".into(), false));
        let (out, err) = run(READ, &json!({"path": "a.txt", "offset": max, "limit": max}), &env).await;
        assert!(err && out.contains("nothing from line"), "{out}");
        let _ = std::fs::remove_dir_all(d);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn read_refuses_what_is_not_a_regular_file_without_blocking_or_reading_it_whole() {
        let d = dir("read-special");
        let fifo = d.join("fifo");
        let made = std::process::Command::new("mkfifo").arg(&fifo).status().unwrap();
        assert!(made.success());
        // Bypassed, so the devices outside the working directory are reached
        // at all: what is refused here is what they are, not where.
        let env = ToolEnv { cwd: &d, permission_mode: PermissionMode::BypassPermissions, edit: EditTool::StrReplace, evidence: None };
        for path in [fifo.display().to_string(), "/dev/zero".into(), "/dev/stdin".into(), d.display().to_string()] {
            let r = tokio::time::timeout(Duration::from_secs(2), run(READ, &json!({ "path": path }), &env)).await.expect("read never blocks");
            assert!(r.1 && r.0.contains("not a regular file"), "{path}: {r:?}");
        }
        // A long line costs its shown part, and the page stays capped.
        std::fs::write(d.join("wide"), "x".repeat(5 << 20) + "\nsecond\n").unwrap();
        let (out, err) = run(READ, &json!({"path": "wide"}), &env).await;
        assert!(!err && out.len() < 10_000 && out.contains("     2\tsecond"), "{}", out.len());
        let _ = std::fs::remove_dir_all(d);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn bash_runs_only_when_permissions_are_bypassed_and_is_bounded() {
        let d = dir("bash");
        let refused = run(BASH, &json!({"command": "echo hi"}), &ToolEnv { cwd: &d, permission_mode: PermissionMode::Default, edit: EditTool::StrReplace, evidence: None }).await;
        assert!(refused.1 && refused.0.contains("bypassPermissions"), "{refused:?}");
        let env = ToolEnv { cwd: &d, permission_mode: PermissionMode::BypassPermissions, edit: EditTool::StrReplace, evidence: None };
        assert_eq!(run(BASH, &json!({"command": "echo hi; echo oops >&2"}), &env).await, ("hi\noops\nexit code 0".into(), false));
        // One pipe: stdout and stderr arrive in the order they were written.
        let interleaved = "for i in 1 2 3 4 5 6 7 8; do echo out$i; echo err$i >&2; done";
        let want = (1..=8).map(|i| format!("out{i}\nerr{i}\n")).collect::<String>() + "exit code 0";
        for _ in 0..20 {
            assert_eq!(run(BASH, &json!({ "command": interleaved }), &env).await, (want.clone(), false));
        }
        assert_eq!(run(BASH, &json!({"command": "exit 3"}), &env).await, ("exit code 3".into(), true));
        let started = std::time::Instant::now();
        let (out, err) = run(BASH, &json!({"command": "sleep 5 & sleep 5", "timeout_ms": 200}), &env).await;
        assert!(err && out.contains("timed out"), "{out}");
        assert!(started.elapsed() < Duration::from_secs(3), "the whole group was killed");
        let (out, _) = run(BASH, &json!({"command": "head -c 40000 /dev/zero | tr '\\0' x"}), &env).await;
        assert!(out.contains("bytes cut") && out.len() < 31_000);
        // Output is bounded however much there is, not buffered whole: 200 MB
        // of it. (`yes | head`, not `timeout 1 yes`: macOS has no timeout.)
        let (out, _) = run(BASH, &json!({"command": "yes | head -c 200000000"}), &env).await;
        assert!(out.contains("bytes cut") && out.len() < 31_000, "{}", out.len());
        // A process left in the background holds the pipes; the call still
        // ends when the shell does.
        let started = std::time::Instant::now();
        let (out, err) = run(BASH, &json!({"command": "sleep 5 & echo started"}), &env).await;
        assert!(!err && out.starts_with("started\nexit code 0"), "{out}");
        assert!(started.elapsed() < Duration::from_secs(2));
        // Even one that keeps writing: the window after exit is fixed, not
        // restarted by each chunk.
        let started = std::time::Instant::now();
        let (out, err) = run(BASH, &json!({"command": "(while :; do echo x; sleep 0.1; done) & echo ok", "timeout_ms": 10000}), &env).await;
        // The loop may print before `echo ok` does: order is the scheduler's.
        assert!(!err && out.lines().any(|l| l == "ok"), "{out}");
        assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
        let _ = std::fs::remove_dir_all(d);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn an_interrupted_bash_call_kills_what_the_command_started() {
        let d = dir("bash-cancel");
        let env = ToolEnv { cwd: &d, permission_mode: PermissionMode::BypassPermissions, edit: EditTool::StrReplace, evidence: None };
        let input = json!({"command": "sleep 30 & echo $! > grandchild; wait"});
        let call = run(BASH, &input, &env);
        // Dropped mid-run, as the loop drops a call when the turn is interrupted.
        assert!(tokio::time::timeout(Duration::from_millis(500), call).await.is_err());
        let pid: i32 = std::fs::read_to_string(d.join("grandchild")).unwrap().trim().parse().unwrap();
        let mut alive = true;
        for _ in 0..40 {
            // SAFETY: signal 0 only asks whether the process exists.
            alive = unsafe { libc::kill(pid, 0) } == 0 && !std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|s| s.contains(") Z "));
            if !alive {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(!alive, "the backgrounded sleep {pid} outlived the interrupted call");
        let _ = std::fs::remove_dir_all(d);
    }
}
