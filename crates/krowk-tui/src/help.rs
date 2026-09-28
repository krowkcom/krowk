//! The help menu: what the TUI can do, one entry a line, a title and a
//! short description each, filtered by what is typed in the prompt.

/// What enter on an entry does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Nothing to run: the entry only tells.
    Tell,
    Model,
    Mode,
    Settings,
    Connect,
    Disconnect,
    Todos,
    Agents,
    Details,
    Copy,
    Interrupt,
    Quit,
}

pub struct Entry {
    pub title: &'static str,
    pub description: &'static str,
    pub keys: &'static str,
    pub action: Action,
}

const fn e(title: &'static str, description: &'static str, keys: &'static str, action: Action) -> Entry {
    Entry { title, description, keys, action }
}

pub const ENTRIES: &[Entry] = &[
    e("Send", "Send the prompt", "enter", Action::Tell),
    e("New line", "Start a new line without sending", "alt-enter · ctrl-j", Action::Tell),
    e("Steer", "Type while a turn runs to redirect it", "type + enter", Action::Tell),
    e("Interrupt", "Stop the running turn", "esc · ctrl-c", Action::Interrupt),
    e("History", "Bring back an earlier prompt", "↑ ↓", Action::Tell),
    e("Commands", "Type / for commands and skills", "/", Action::Tell),
    e("Model", "Switch model or instance", "/model", Action::Model),
    e("Mode", "Switch permission mode", "/mode", Action::Mode),
    e("Settings", "The default permission mode, saved", "/settings · /config", Action::Settings),
    e("Connect", "Connect a provider, or renew a login", "/connect", Action::Connect),
    e("Disconnect", "Sign an instance out", "/disconnect", Action::Disconnect),
    e("Todos", "The task list for this session", "ctrl-t", Action::Todos),
    e("Subagents", "See, expand or stop subagents", "ctrl-g", Action::Agents),
    e("Session", "Tokens, limits and the log file", "ctrl-o", Action::Details),
    e("Copy answer", "Copy the last answer", "ctrl-y", Action::Copy),
    e("Editing", "Jump to line ends, delete words", "ctrl-a/e · ctrl-u/k/w", Action::Tell),
    e("Quit", "Leave krowk", "ctrl-d · /exit", Action::Quit),
];

/// The entries `query` finds, those whose title starts with it first, then
/// those it is anywhere in, title or description. Case is ignored; an
/// empty query finds them all.
pub fn filter(query: &str) -> Vec<&'static Entry> {
    let q = query.trim().trim_start_matches(['/', '?']).to_lowercase();
    let title = |e: &Entry| e.title.to_lowercase();
    let mut found: Vec<&Entry> = ENTRIES.iter().filter(|e| title(e).starts_with(&q)).collect();
    found.extend(ENTRIES.iter().filter(|e| !title(e).starts_with(&q) && (title(e).contains(&q) || e.description.to_lowercase().contains(&q) || e.keys.contains(&q))));
    found
}

/// What `/` offers: krowk's own commands, then the skills.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slash {
    pub name: String,
    pub description: String,
    pub skill: bool,
}

/// Names krowk's commands answer to too, unlisted: typed whole, the `/`
/// menu puts the command first, so enter runs it — never a skill whose
/// name only looks like it.
pub const ALIASES: &[(&str, &str)] = &[("quit", "exit"), ("permission-mode", "mode"), ("config", "settings")];

/// krowk's own commands, as `/` lists them.
pub const COMMANDS: &[(&str, &str)] = &[
    ("model", "Switch model or instance"),
    ("mode", "Switch permission mode — default, acceptEdits, plan, bypassPermissions, unhinged"),
    ("settings", "Settings saved to config.json — the default permission mode"),
    ("connect", "Connect a provider — a subscription or an API key"),
    ("disconnect", "Sign an instance out"),
    ("help", "Keys and what they do"),
    ("exit", "Leave krowk"),
];

/// The commands and skills `typed` (the prompt, `/` and all) finds,
/// fuzzily, as Grok Build's menu does: ranked by how well the name
/// matches, then those only the description matches. An empty query finds
/// them all, commands first.
pub fn slash(typed: &str, skills: &[(String, String)]) -> Vec<Slash> {
    use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
    use nucleo_matcher::{Config, Matcher, Utf32Str};
    let all: Vec<Slash> = COMMANDS
        .iter()
        .map(|(n, d)| Slash { name: n.to_string(), description: d.to_string(), skill: false })
        .chain(skills.iter().map(|(n, d)| Slash { name: n.clone(), description: d.clone(), skill: true }))
        .collect();
    let q = typed.trim_start_matches('/');
    if q.is_empty() {
        return all;
    }
    let pattern = Pattern::parse(q, CaseMatching::Ignore, Normalization::Smart);
    let mut matcher = Matcher::new(Config::DEFAULT);
    let mut buf = Vec::new();
    let mut score = |s: &str| pattern.score(Utf32Str::new(s, &mut buf), &mut matcher);
    // The name typed whole, or an alias of it, outranks every other hit —
    // `/mode` is not `/model` — and name hits every description hit; ties
    // keep the list's order.
    let whole = |s: &Slash| s.name.eq_ignore_ascii_case(q) || (!s.skill && ALIASES.iter().any(|(a, c)| a.eq_ignore_ascii_case(q) && *c == s.name));
    let mut ranked: Vec<(u32, usize)> = all
        .iter()
        .enumerate()
        .filter_map(|(i, s)| {
            let hit = score(&s.name).map(|n| n + (1 << 20)).or_else(|| score(&s.description));
            if whole(s) { Some((hit.unwrap_or(0) + (1 << 21), i)) } else { hit.map(|h| (h, i)) }
        })
        .collect();
    ranked.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    ranked.into_iter().map(|(_, i)| all[i].clone()).collect()
}
