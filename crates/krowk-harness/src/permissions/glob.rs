//! Globs over `/`-separated paths, as `.gitignore` and ripgrep read them:
//! what a permission rule's path pattern is matched with, and the `glob`
//! and `grep` tools' filters.

use std::path::Path;

/// A compiled glob: `*`, `?`, `**`, `[…]` and `{a,b}`, over `/`-separated
/// paths. A pattern with no `/` matches the file name at any depth, as in
/// `.gitignore` and ripgrep.
pub(crate) struct Glob {
    alternatives: Vec<Vec<char>>,
    name_only: bool,
}

impl Glob {
    pub fn new(pattern: &str) -> Result<Glob, String> {
        let pattern = pattern.trim().trim_start_matches("./");
        if pattern.is_empty() {
            return Err("the glob is empty".into());
        }
        let mut alternatives = Vec::new();
        expand(pattern, &mut alternatives)?;
        Ok(Glob { name_only: !pattern.contains('/'), alternatives: alternatives.into_iter().map(|a| a.chars().collect()).collect() })
    }

    pub fn matches(&self, path: &Path) -> bool {
        let full = path.to_string_lossy().replace('\\', "/");
        let subject: Vec<char> = if self.name_only { full.rsplit('/').next().unwrap_or_default().chars().collect() } else { full.chars().collect() };
        self.alternatives.iter().any(|p| glob_match(p, &subject))
    }
}

/// `{a,b}` expanded into alternatives, nested braces included; at most 256.
fn expand(p: &str, out: &mut Vec<String>) -> Result<(), String> {
    let Some(open) = p.find('{') else {
        out.push(p.to_string());
        return Ok(());
    };
    let (mut depth, mut close, mut commas) = (0, None, vec![]);
    for (i, c) in p[open..].char_indices().map(|(i, c)| (i + open, c)) {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(i);
                    break;
                }
            }
            ',' if depth == 1 => commas.push(i),
            _ => {}
        }
    }
    let close = close.ok_or_else(|| format!("the glob {p:?} has a `{{` with no `}}`"))?;
    let mut bounds = vec![open];
    bounds.extend(commas);
    bounds.push(close);
    for w in bounds.windows(2) {
        expand(&format!("{}{}{}", &p[..open], &p[w[0] + 1..w[1]], &p[close + 1..]), out)?;
        if out.len() > 256 {
            return Err("the glob expands to more than 256 alternatives".into());
        }
    }
    Ok(())
}

/// Matches one alternative against a path, segment by segment: `**` as a
/// whole segment spans any number of segments, including none; `*` and
/// `?` stay within one. Both levels backtrack only to the last star, so a
/// pattern costs at most the product of the two lengths — never the
/// exponential a recursive matcher pays on `*a*a*a…b`.
fn glob_match(p: &[char], s: &[char]) -> bool {
    let pats: Vec<&[char]> = p.split(|c| *c == '/').collect();
    let segs: Vec<&[char]> = s.split(|c| *c == '/').collect();
    let (mut pi, mut si) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while si < segs.len() {
        if pi < pats.len() && pats[pi] == ['*', '*'] {
            star = Some((pi, si));
            pi += 1;
            continue;
        }
        if pi < pats.len() && segment_match(pats[pi], segs[si]) {
            pi += 1;
            si += 1;
            continue;
        }
        match star {
            // The last `**` takes one more segment, and the rest is tried again.
            Some((sp, ss)) => {
                star = Some((sp, ss + 1));
                pi = sp + 1;
                si = ss + 1;
            }
            None => return false,
        }
    }
    pats[pi..].iter().all(|p| *p == ['*', '*'])
}

/// One segment against one pattern segment: `*`, `?`, `[…]` and literals.
fn segment_match(p: &[char], s: &[char]) -> bool {
    let (mut pi, mut si) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while si < s.len() {
        let step = match p.get(pi) {
            Some('*') => {
                star = Some((pi, si));
                pi += 1;
                continue;
            }
            Some('?') => Some(1),
            Some('[') => match class(&p[pi..]) {
                Some((hit, len)) => hit(s[si]).then_some(len),
                None => (s[si] == '[').then_some(1),
            },
            Some(c) => (*c == s[si]).then_some(1),
            None => None,
        };
        match (step, star) {
            (Some(len), _) => {
                pi += len;
                si += 1;
            }
            (None, Some((sp, ss))) => {
                star = Some((sp, ss + 1));
                pi = sp + 1;
                si = ss + 1;
            }
            (None, None) => return false,
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

type ClassFn = Box<dyn Fn(char) -> bool>;

/// A `[…]` class at the front of `p`: its test and its length. None when
/// the bracket never closes, which makes it a literal `[`.
fn class(p: &[char]) -> Option<(ClassFn, usize)> {
    let mut i = 1;
    let negated = matches!(p.get(i), Some('!' | '^'));
    if negated {
        i += 1;
    }
    let mut ranges = Vec::new();
    let first = i;
    while let Some(&c) = p.get(i) {
        if c == ']' && i > first {
            let hit = move |x: char| ranges.iter().any(|&(a, b)| a <= x && x <= b) != negated;
            return Some((Box::new(hit), i + 1));
        }
        if p.get(i + 1) == Some(&'-') && p.get(i + 2).is_some_and(|e| *e != ']') {
            ranges.push((c, p[i + 2]));
            i += 3;
        } else {
            ranges.push((c, c));
            i += 1;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn m(pattern: &str, path: &str) -> bool {
        Glob::new(pattern).unwrap().matches(Path::new(path))
    }

    #[test]
    fn globs_match_like_gitignore_and_ripgrep() {
        assert!(m("*.rs", "main.rs") && m("*.rs", "src/deep/lib.rs"), "no slash: the name at any depth");
        assert!(!m("*.rs", "main.rsx"));
        assert!(m("src/*.rs", "src/lib.rs") && !m("src/*.rs", "src/a/lib.rs"), "* stays in its segment");
        assert!(m("src/**/*.rs", "src/lib.rs") && m("src/**/*.rs", "src/a/b/lib.rs"), "** spans zero or more");
        assert!(m("**/test_*.py", "test_a.py") && m("**/test_*.py", "x/test_a.py"));
        assert!(m("docs/**", "docs/a/b.md") && !m("docs/**", "src/docs.md"));
        assert!(m("*.{ts,tsx}", "a/b.tsx") && m("*.{ts,tsx}", "b.ts") && !m("*.{ts,tsx}", "b.js"));
        assert!(m("file?.txt", "file1.txt") && !m("file?.txt", "file12.txt"));
        assert!(m("[ab]*.md", "b.md") && !m("[!ab]*.md", "a.md") && m("[a-c]x", "cx"));
        assert!(Glob::new("{a,b").is_err() && Glob::new(" ").is_err());
        assert!(m("a/**/b/**/c.txt", "a/x/b/y/z/c.txt") && !m("a/**/b/**/c.txt", "a/x/c.txt"));
        assert!(m("*a*b", "xxaxxb") && !m("*a*b", "xxaxxc") && m("**", "any/depth/at/all"));
    }

    #[test]
    fn a_pathological_glob_costs_linear_time_not_exponential() {
        // `*a` ten times then `*b`, against a long name of `a`s with no b: a
        // recursive matcher takes seconds per file on this.
        let pattern = format!("{}*b", "*a".repeat(10));
        let deep = format!("{}/**/{}", "**/".repeat(10).trim_end_matches('/'), pattern);
        let name = "a".repeat(200);
        let path = format!("{}/{name}", vec!["d"; 50].join("/"));
        let started = Instant::now();
        for _ in 0..100 {
            assert!(!m(&pattern, &name));
            assert!(!m(&deep, &path));
        }
        assert!(started.elapsed() < Duration::from_secs(1), "{:?}", started.elapsed());
    }
}
