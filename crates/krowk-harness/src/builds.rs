//! Build slots: the bash tool runs a command that starts a build or a test
//! run — `cargo test`, `npm install`, `make` — only once it holds one of
//! `builds.slots` slots shared by every krowk on the machine
//! (`crate::slots`, the `build-slots` pool), and holds it for the
//! command's life. Agents mostly wait on their model, so a machine hosts
//! many; what runs it out of memory is several of them building at once.
//! Any other command runs straight away.
//!
//! A command is heavy when one of its commands' programs is in `HEAVY`:
//! the word in command position — the start, or after `&&`, `||`, `;`,
//! `|`, `&`, `(`, a backquote or a newline — past `VAR=x` assignments,
//! the shell's own words (`{`, `!`, `if`, `then`, `do`, …) and the
//! wrappers `env`, `time` and `nice` with their options. Quotes are
//! honoured, so `echo 'cargo test'` is not heavy; a build started from
//! inside a script or `bash -c "…"` is not seen.
//!
//! A heavy command also gets `CARGO_BUILD_JOBS` of the machine's cores
//! split between the slots, unless krowk's own environment sets it, so
//! the builds that do run at once share the cores rather than each taking
//! all of them.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The programs whose commands take a build slot.
pub const HEAVY: &[&str] = &[
    "cargo", "rustc", "make", "cmake", "ninja", "npm", "pnpm", "yarn", "bun", "npx", "go", "pytest", "tox", "gradle", "gradlew", "mvn", "rake", "rspec", "bundle", "mix", "dotnet", "swift", "xcodebuild",
];

/// The pool's name under the runtime directory.
pub const POOL: &str = "build-slots";

/// `builds` in config.json.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BuildsConfig {
    /// Build and test commands that run at once across every krowk on the
    /// machine; a quarter of the cores (at least one) when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slots: Option<usize>,
}

impl BuildsConfig {
    pub fn is_empty(&self) -> bool {
        self.slots.is_none()
    }
}

/// The slots a turn's heavy commands take, resolved once with the host's
/// instances. The default takes none: heavy commands run as any other.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Builds {
    /// The slots; none, and nothing waits.
    pub pool: Option<crate::slots::Pool>,
    /// `CARGO_BUILD_JOBS` for a heavy command; none when krowk's
    /// environment sets it already.
    pub jobs: Option<usize>,
}

/// The machine's cores, as the OS says this process may use them.
pub fn cores() -> usize {
    std::thread::available_parallelism().map_or(1, |n| n.get())
}

impl Builds {
    /// The config's slots, in the runtime directory `env` names.
    pub fn resolve(cfg: &BuildsConfig, env: &dyn Fn(&str) -> String) -> Builds {
        let cores = cores();
        let slots = cfg.slots.unwrap_or(cores / 4).max(1);
        let jobs = env("CARGO_BUILD_JOBS").is_empty().then_some((cores / slots).max(1));
        Builds { pool: Some(crate::slots::Pool::new(crate::slots::runtime_dir(env), POOL, slots)), jobs }
    }
}

/// One word of a command line, or a break between commands.
#[derive(Debug, PartialEq)]
enum Token {
    Word(String),
    Break,
}

/// The command line as words and breaks: quotes and escapes taken off,
/// comments dropped. Close enough to the shell's own reading to find each
/// command's program; never used to run anything.
fn tokens(command: &str) -> Vec<Token> {
    let mut out = Vec::new();
    let mut word: Option<String> = None;
    let mut chars = command.chars().peekable();
    let end = |word: &mut Option<String>, out: &mut Vec<Token>| {
        if let Some(w) = word.take() {
            out.push(Token::Word(w));
        }
    };
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' => end(&mut word, &mut out),
            '\n' | ';' | '&' | '|' | '(' | ')' | '`' => {
                end(&mut word, &mut out);
                out.push(Token::Break);
            }
            '#' if word.is_none() => {
                while chars.peek().is_some_and(|c| *c != '\n') {
                    chars.next();
                }
            }
            '\'' => {
                let w = word.get_or_insert_with(String::new);
                for c in chars.by_ref() {
                    if c == '\'' {
                        break;
                    }
                    w.push(c);
                }
            }
            '"' => {
                let w = word.get_or_insert_with(String::new);
                while let Some(c) = chars.next() {
                    match c {
                        '"' => break,
                        '\\' => {
                            if let Some(n) = chars.next() {
                                w.push(n);
                            }
                        }
                        c => w.push(c),
                    }
                }
            }
            '\\' => match chars.next() {
                Some('\n') | None => {}
                Some(n) => word.get_or_insert_with(String::new).push(n),
            },
            c => word.get_or_insert_with(String::new).push(c),
        }
    }
    end(&mut word, &mut out);
    out
}

/// The shell's own words that may stand before a command's program.
const KEYWORDS: &[&str] = &["{", "}", "!", "if", "then", "else", "elif", "do", "while", "until"];

/// A program's name without its directory or a Windows extension.
fn program(word: &str) -> &str {
    let base = word.rsplit(['/', '\\']).next().unwrap_or(word);
    [".exe", ".cmd", ".bat"].iter().find_map(|x| base.strip_suffix(x)).unwrap_or(base)
}

/// `NAME=value`, a variable set for the command.
fn assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        let mut c = name.chars();
        c.next().is_some_and(|f| f.is_ascii_alphabetic() || f == '_') && c.all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// Whether one command — the words between two breaks — runs a heavy
/// program.
fn heavy_words(words: &[String]) -> bool {
    let mut i = 0;
    while let Some(w) = words.get(i) {
        i += 1;
        if assignment(w) || KEYWORDS.contains(&w.as_str()) {
            continue;
        }
        let p = program(w);
        if matches!(p, "env" | "time" | "nice") {
            // The wrapper's options, and the one that takes a value.
            while let Some(o) = words.get(i).filter(|o| o.starts_with('-')) {
                i += 1;
                if matches!((p, o.as_str()), ("env", "-u" | "-C" | "-S") | ("nice", "-n")) {
                    i += 1;
                }
            }
            continue;
        }
        return HEAVY.contains(&p);
    }
    false
}

/// Whether `command` runs a build or test program anywhere in command
/// position, and so waits for a build slot.
pub fn heavy(command: &str) -> bool {
    let mut words = Vec::new();
    for t in tokens(command).into_iter().chain([Token::Break]) {
        match t {
            Token::Word(w) => words.push(w),
            Token::Break if heavy_words(&words) => return true,
            Token::Break => words.clear(),
        }
    }
    false
}

/// What the result says about the wait, when it was long enough to notice.
pub fn waited_note(waited: std::time::Duration) -> Option<String> {
    (waited > std::time::Duration::from_secs(1)).then(|| format!("(waited {:.1} s for a build slot)", waited.as_secs_f64()))
}

/// What the live output shows while a heavy command waits.
pub fn waiting_line(in_use: usize) -> String {
    format!("waiting for a build slot ({in_use} in use)\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_and_test_commands_are_heavy_and_others_are_not() {
        for c in [
            "cargo test",
            "FOO=1 cargo build",
            "cd x && npm i",
            "time make -j8",
            "ls; pytest -q",
            "true || go test ./...",
            "git status | npx prettier --check .",
            "(cd web && pnpm install)",
            "env -u HOME RUSTFLAGS=-Dwarnings nice -n 5 cargo clippy",
            "nice -10 make",
            "time -p ./gradlew build",
            "/usr/bin/make all",
            "sleep 1 & cargo build",
            "echo start\ncargo test",
            "if true; then mvn package; fi",
            "echo $(cargo metadata --format-version 1)",
            "\"cargo\" test",
            "CC=clang  \\\n  cargo build",
            "cargo.exe build",
        ] {
            assert!(heavy(c), "{c:?} is heavy");
        }
        for c in [
            "ls",
            "git status",
            "echo cargo",
            "cat Cargo.toml",
            "",
            "echo 'cd x && cargo test'",
            "git commit -m \"run cargo test\"",
            "# cargo test",
            "ls # && cargo test",
            "grep -r make .",
            "cargo-watch --help",
            "rg npm package.json",
            "MAKE=make ls",
        ] {
            assert!(!heavy(c), "{c:?} is not heavy");
        }
    }

    #[test]
    fn slots_default_to_a_quarter_of_the_cores_and_split_them_for_cargo() {
        let none = |_: &str| String::new();
        let cores = cores();
        let b = Builds::resolve(&BuildsConfig::default(), &none);
        let slots = (cores / 4).max(1);
        assert_eq!(b.pool.as_ref().unwrap().size(), slots);
        assert_eq!(b.jobs, Some((cores / slots).max(1)));
        let b = Builds::resolve(&BuildsConfig { slots: Some(cores * 2) }, &none);
        assert_eq!((b.pool.unwrap().size(), b.jobs), (cores * 2, Some(1)), "never fewer than one job");
        let set = |k: &str| if k == "CARGO_BUILD_JOBS" { "3".to_string() } else { String::new() };
        assert_eq!(Builds::resolve(&BuildsConfig::default(), &set).jobs, None, "the person's own setting is left alone");
        assert_eq!(Builds::default().pool, None, "a default host takes no slots");
    }

    #[test]
    fn a_long_wait_is_noted_and_a_short_one_is_not() {
        assert_eq!(waited_note(std::time::Duration::from_millis(900)), None);
        assert_eq!(waited_note(std::time::Duration::from_millis(3300)).as_deref(), Some("(waited 3.3 s for a build slot)"));
        assert_eq!(waiting_line(2), "waiting for a build slot (2 in use)\n");
    }
}
