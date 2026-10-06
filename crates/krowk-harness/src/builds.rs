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
//! `|`, `&`, `(`, a backquote or a newline, and inside `$(…)` or a
//! backquote within double quotes — past `VAR=x` assignments, the shell's
//! own words (`{`, `!`, `if`, `then`, `do`, …) and the wrappers in
//! `WRAPPERS` (`env`, `time`, `nice`, `nohup`, `timeout`, `sudo`, …) with
//! their options. Quotes are honoured and a heredoc's body skipped, so
//! `echo 'cargo test'` is not heavy. Only the command line itself is read:
//! a build started from inside a script or `bash -c "…"` is not seen.
//!
//! The slot is held until the shell exits. A build the command puts in the
//! background (`cargo build &`, `nohup make &`) keeps running after that,
//! and is no longer held to a slot: krowk does not follow what a command
//! leaves behind.
//!
//! A heavy command also gets `CARGO_BUILD_JOBS`: the machine's cores split
//! between the slots, so the builds that run at once share the cores
//! rather than each taking all of them — or the person's own value, when
//! krowk's environment sets one. It is passed explicitly, so a sandboxed
//! command, whose environment is an allowlist, gets it too.

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
    /// `CARGO_BUILD_JOBS` for a heavy command: the person's own, else the
    /// cores split between the slots.
    pub jobs: Option<String>,
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
        let jobs = match env("CARGO_BUILD_JOBS") {
            own if own.is_empty() => (cores / slots).max(1).to_string(),
            own => own,
        };
        Builds { pool: Some(crate::slots::Pool::new(crate::slots::runtime_dir(env), POOL, slots)), jobs: Some(jobs) }
    }
}

/// One word of a command line, or a break between commands.
#[derive(Debug, PartialEq)]
enum Token {
    Word(String),
    Break,
}

/// What the reading is inside of: a double-quoted string, or a command
/// substitution opened within one (`"$(…)"`, with the parentheses opened
/// in it since, or a backquote).
enum Inside {
    Quotes,
    Substitution(usize),
    Backquote,
}

/// The command line as words and breaks: quotes and escapes taken off,
/// comments and heredoc bodies dropped, and a substitution inside double
/// quotes read as the commands it runs. Close enough to the shell's own
/// reading to find each command's program; never used to run anything.
fn tokens(command: &str) -> Vec<Token> {
    let mut r = Reader { chars: command.chars().peekable(), out: Vec::new(), word: None, inside: Vec::new(), heredocs: Vec::new() };
    while let Some(c) = r.chars.next() {
        if matches!(r.inside.last(), Some(Inside::Quotes)) {
            r.quoted(c);
        } else {
            r.bare(c);
        }
    }
    r.end();
    r.out
}

/// `tokens`' reading, part way through.
struct Reader<'a> {
    chars: std::iter::Peekable<std::str::Chars<'a>>,
    out: Vec<Token>,
    word: Option<String>,
    inside: Vec<Inside>,
    /// Heredocs begun on this line: each one's delimiter, and whether its
    /// lines may be indented with tabs (`<<-`).
    heredocs: Vec<(String, bool)>,
}

impl Reader<'_> {
    fn push(&mut self, c: char) {
        self.word.get_or_insert_with(String::new).push(c);
    }

    fn end(&mut self) {
        if let Some(w) = self.word.take() {
            self.out.push(Token::Word(w));
        }
    }

    fn brk(&mut self) {
        self.end();
        self.out.push(Token::Break);
    }

    /// A substitution inside a string has ended: what follows it is the
    /// string's, never a command, so a word stands in for the string.
    fn closed(&mut self) {
        self.inside.pop();
        self.out.push(Token::Word("\"…\"".into()));
    }

    /// One character inside double quotes.
    fn quoted(&mut self, c: char) {
        match c {
            '"' => {
                self.inside.pop();
            }
            '\\' => {
                if let Some(n) = self.chars.next() {
                    self.push(n);
                }
            }
            '$' if self.chars.next_if_eq(&'(').is_some() => {
                self.brk();
                self.inside.push(Inside::Substitution(0));
            }
            '`' => {
                self.brk();
                self.inside.push(Inside::Backquote);
            }
            c => self.push(c),
        }
    }

    /// One character outside quotes.
    fn bare(&mut self, c: char) {
        match c {
            ' ' | '\t' => self.end(),
            '\n' => {
                self.brk();
                self.skip_heredocs();
            }
            ';' | '&' | '|' => self.brk(),
            '(' => {
                self.brk();
                if let Some(Inside::Substitution(open)) = self.inside.last_mut() {
                    *open += 1;
                }
            }
            ')' => {
                self.brk();
                match self.inside.last_mut() {
                    Some(Inside::Substitution(0)) => self.closed(),
                    Some(Inside::Substitution(open)) => *open -= 1,
                    _ => {}
                }
            }
            '`' => {
                self.brk();
                if matches!(self.inside.last(), Some(Inside::Backquote)) {
                    self.closed();
                }
            }
            '#' if self.word.is_none() => {
                while self.chars.next_if(|c| *c != '\n').is_some() {}
            }
            '\'' => {
                let w = self.word.get_or_insert_with(String::new);
                for c in self.chars.by_ref() {
                    if c == '\'' {
                        break;
                    }
                    w.push(c);
                }
            }
            '"' => {
                self.word.get_or_insert_with(String::new);
                self.inside.push(Inside::Quotes);
            }
            '<' if self.chars.next_if_eq(&'<').is_some() => self.heredoc(),
            '\\' => match self.chars.next() {
                Some('\n') | None => {}
                Some(n) => self.push(n),
            },
            c => self.push(c),
        }
    }

    /// After `<<`: a heredoc's delimiter, whose body — from the next line
    /// to the delimiter — is text, not commands. `<<<` is a here-string,
    /// read as words.
    fn heredoc(&mut self) {
        if self.chars.next_if_eq(&'<').is_some() {
            return;
        }
        self.end();
        let tabs = self.chars.next_if_eq(&'-').is_some();
        while self.chars.next_if(|c| *c == ' ' || *c == '\t').is_some() {}
        let mut delimiter = String::new();
        while let Some(c) = self.chars.next_if(|c| !c.is_whitespace() && !";&|()<>".contains(*c)) {
            if !matches!(c, '\'' | '"' | '\\') {
                delimiter.push(c);
            }
        }
        if !delimiter.is_empty() {
            self.heredocs.push((delimiter, tabs));
        }
    }

    /// At a line's end: the bodies of the heredocs it began.
    fn skip_heredocs(&mut self) {
        for (delimiter, tabs) in std::mem::take(&mut self.heredocs) {
            loop {
                let line: String = self.chars.by_ref().take_while(|c| *c != '\n').collect();
                let line = if tabs { line.trim_start_matches('\t') } else { line.as_str() };
                if line == delimiter || self.chars.peek().is_none() {
                    break;
                }
            }
        }
    }
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

/// Programs that run a command given after their own options: each one's
/// name, its options that take a value, and the operands that come before
/// the command (`timeout`'s duration).
const WRAPPERS: &[(&str, &[&str], usize)] = &[
    ("env", &["-u", "-C", "-S"], 0),
    ("time", &["-f", "-o"], 0),
    ("nice", &["-n"], 0),
    ("nohup", &[], 0),
    ("timeout", &["-s", "-k"], 1),
    ("exec", &["-a"], 0),
    ("command", &[], 0),
    ("sudo", &["-u", "-g", "-C", "-h", "-p", "-D", "-r", "-t", "-T", "-U"], 0),
    ("stdbuf", &["-i", "-o", "-e"], 0),
];

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
        if let Some((_, valued, operands)) = WRAPPERS.iter().find(|(name, _, _)| *name == p) {
            while let Some(o) = words.get(i).filter(|o| o.starts_with('-')) {
                i += 1;
                match o.as_str() {
                    "--" => break,
                    // `command -v cargo` asks where cargo is; it runs nothing.
                    "-v" | "-V" if p == "command" => return false,
                    o if valued.contains(&o) => i += 1,
                    _ => {}
                }
            }
            i += operands;
            continue;
        }
        return HEAVY.contains(&p);
    }
    false
}

/// Whether `command` runs a build or test program in command position, and
/// so waits for a build slot.
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
            "nohup cargo build &",
            "cargo test &",
            "out=\"$(cargo test 2>&1)\"; echo \"$out\"",
            "echo \"$(make)\"",
            "echo \"built: `make -s`\"",
            "echo \"$(cd x && (npm ci))\"",
            "timeout 600 cargo test",
            "timeout -k 5 -s INT 10m make check",
            "exec cargo run",
            "command cargo build",
            "sudo -u build make install",
            "sudo -- make install",
            "stdbuf -oL -e L pytest",
            "stdbuf -o L cargo test",
            "cat > n.md <<'EOF'\nmake sure\nEOF\ncargo test",
            "cat <<<\"x\" && go test",
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
            "cat > n.md <<'EOF'\nmake sure\nEOF",
            "git commit -F - <<EOF\ngo live\nEOF",
            "cat <<-  \"END\"\n\tcargo test\n\tEND\nls",
            "echo \"$(date) make\"",
            "echo \"$(date)make\"",
            "echo \"`date`cargo\"",
            "timeout 600 sleep 5",
            "sudo -u build ls",
            "command -v cargo",
            "nohup ./server &",
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
        assert_eq!(b.jobs, Some((cores / slots).max(1).to_string()));
        let b = Builds::resolve(&BuildsConfig { slots: Some(cores * 2) }, &none);
        assert_eq!((b.pool.unwrap().size(), b.jobs.as_deref()), (cores * 2, Some("1")), "never fewer than one job");
        let set = |k: &str| if k == "CARGO_BUILD_JOBS" { "3".to_string() } else { String::new() };
        assert_eq!(Builds::resolve(&BuildsConfig::default(), &set).jobs.as_deref(), Some("3"), "the person's own setting, passed on as it is");
        assert_eq!(Builds::default().pool, None, "a default host takes no slots");
    }

    #[test]
    fn a_long_wait_is_noted_and_a_short_one_is_not() {
        assert_eq!(waited_note(std::time::Duration::from_millis(900)), None);
        assert_eq!(waited_note(std::time::Duration::from_millis(3300)).as_deref(), Some("(waited 3.3 s for a build slot)"));
        assert_eq!(waiting_line(2), "waiting for a build slot (2 in use)\n");
    }
}
