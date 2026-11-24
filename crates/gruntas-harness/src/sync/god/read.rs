//! `read` (GA04, D3): one page of a session's transcript, about 12 KB,
//! newest first, redacted and framed as foreign text. Claude Code from its
//! registration's transcript path (the file's last 2 MB, through the Claude
//! importer's parser); gruntas-run through the session log reader; Codex
//! refused `cant_read`. The page goes back through the answer record.
//!
//! A page is whole entries (a message's text, a tool call, a tool result),
//! newest first; its `next` names the oldest entry on it, `before:<id>`, so
//! the page before it in time stays the same while the session goes on.

use super::super::auto::Config;
use crate::protocol::{Item, LogBody, PluginHarness};
use gruntas_client::host_command::{refusal, HostAnswer, TranscriptPage};
use std::io::{BufRead, Seek, SeekFrom};
use std::path::Path;

/// About how much text a page holds (D3).
pub const PAGE_BYTES: usize = 12 * 1024;

/// How much of a Claude Code transcript is read, from its end (D3).
pub const TAIL_BYTES: u64 = 2 * 1024 * 1024;

/// The most of one tool call's input or result an entry shows.
const TOOL_BYTES: usize = 2 * 1024;

/// The cursor's prefix: the page before the entry it names.
const BEFORE: &str = "before:";

/// The answer to `read { session, page }`: `HostAnswer::Page`, or refused
/// `no_session` or `cant_read`.
pub(crate) fn answer(c: &Config, session: &str, page: Option<&str>) -> HostAnswer {
    answer_in(c.socket.parent().unwrap_or(Path::new("/")), &c.sessions, session, page)
}

/// `answer` over the daemon's directory (its registrations) and the
/// session logs' directory.
fn answer_in(daemon: &Path, sessions: &Path, session: &str, page: Option<&str>) -> HostAnswer {
    let log = sessions.join(session).join(crate::log::EVENTS_FILE);
    let entries = match crate::daemon::registry::Registry::read(daemon).get(session) {
        Some(r) if r.harness == PluginHarness::Codex => return HostAnswer::refused(refusal::CANT_READ, "can't read this session yet: it is a Codex session, and gruntas reads no Codex transcript yet"),
        Some(r) => match r.transcript_path.as_deref() {
            Some(path) => claude(Path::new(path)),
            None => return HostAnswer::refused(refusal::CANT_READ, "can't read this session yet: Claude Code has not said where its transcript is"),
        },
        None if crate::log::valid_id(session) && log.is_file() => gruntas_run(&log),
        None => return HostAnswer::refused(refusal::NO_SESSION, format!("this machine has no session {session}")),
    };
    match entries {
        Ok(entries) => HostAnswer::Page(paged(session, &entries, page)),
        Err(why) => HostAnswer::refused(refusal::CANT_READ, format!("can't read this session: {why}")),
    }
}

/// One entry of a transcript: its id, the same while the session goes on,
/// and its text.
struct Entry {
    id: String,
    text: String,
}

/// A Claude Code transcript's last `TAIL_BYTES`, from the first line that
/// starts in them, parsed by the importer.
fn claude(path: &Path) -> Result<Vec<Entry>, String> {
    // Not a FIFO or a device, which would hold the command's thread.
    if !std::fs::metadata(path).is_ok_and(|m| m.is_file()) {
        return Err("its transcript is not a file".into());
    }
    let mut f = std::fs::File::open(path).map_err(|e| format!("its transcript does not open: {e}"))?;
    let len = f.metadata().map_err(|e| e.to_string())?.len();
    let mut from = 0;
    if len > TAIL_BYTES {
        f.seek(SeekFrom::Start(len - TAIL_BYTES)).map_err(|e| e.to_string())?;
        // The line the tail starts inside is left out: from the next one.
        let skipped = std::io::BufReader::new(&f).skip_until(b'\n').map_err(|e| e.to_string())?;
        from = len - TAIL_BYTES + skipped as u64;
    }
    let messages = gruntas_import::claude::messages_from(&mut f, from).map_err(|e| e.message().to_string())?;
    let mut out = Vec::new();
    for m in messages {
        for (n, p) in m.parts.iter().enumerate() {
            let data: serde_json::Value = serde_json::from_str(&p.data).unwrap_or_default();
            let text = match p.kind.as_str() {
                gruntas_import::PART_TEXT => format!("{}: {}", m.role.as_str(), data["text"].as_str().unwrap_or_default()),
                gruntas_import::PART_TOOL_CALL => call(data["name"].as_str().unwrap_or_default(), &data["input"]),
                gruntas_import::PART_TOOL_RESULT => result(&data["output"].as_str().map_or_else(|| data["output"].to_string(), str::to_string), data["is_error"].as_bool().unwrap_or_default()),
                _ => continue,
            };
            out.push(Entry { id: format!("{}.{n}", m.foreign_id), text });
        }
    }
    Ok(out)
}

/// A gruntas-run session's log, its current branch, through the session
/// log reader.
fn gruntas_run(path: &Path) -> Result<Vec<Entry>, String> {
    let events = crate::log::read_events(path).map_err(|e| e.message().to_string())?;
    let Some(head) = events.last() else { return Ok(Vec::new()) };
    let mut out = Vec::new();
    for ev in crate::log::branch(&events, &head.id) {
        let LogBody::ItemCompleted { item, .. } = &ev.body else { continue };
        let text = match item {
            Item::UserText { text, .. } => format!("user: {text}"),
            Item::AssistantText { text } => format!("assistant: {text}"),
            Item::ToolCall { name, input, .. } => call(name, input),
            Item::ToolResult { output, is_error, .. } => result(output, *is_error),
            Item::Reasoning { .. } => continue,
        };
        out.push(Entry { id: ev.id.clone(), text });
    }
    Ok(out)
}

/// Tool input and output are redacted before they are cut, so no secret is
/// cut in two and left half-visible.
fn call(name: &str, input: &serde_json::Value) -> String {
    format!("tool call {name}: {}", clipped(&crate::redact::redacted(&input.to_string()), TOOL_BYTES))
}

fn result(output: &str, is_error: bool) -> String {
    format!("tool {}: {}", if is_error { "error" } else { "result" }, clipped(&crate::redact::redacted(output), TOOL_BYTES))
}

/// `s` cut to about `n` bytes on a character boundary, marked when it was.
fn clipped(s: &str, n: usize) -> String {
    let (head, more) = crate::redact::page(s, 0, n);
    if more.is_some() { format!("{head}… [{} bytes more]", s.len() - head.len()) } else { head.to_string() }
}

/// The page of `entries` (oldest first) before the cursor `page` (all of
/// them without one), newest first: whole entries up to `PAGE_BYTES`, each
/// redacted before it is cut, and at least one, cut when it is longer. A
/// cursor naming no entry here (aged out of the tail) has nothing before it.
fn paged(session: &str, entries: &[Entry], page: Option<&str>) -> TranscriptPage {
    let end = match page {
        None => entries.len(),
        Some(p) => p.strip_prefix(BEFORE).and_then(|id| entries.iter().position(|e| e.id == id)).unwrap_or(0),
    };
    let (mut text, mut oldest) = (String::new(), end);
    for (i, e) in entries[..end].iter().enumerate().rev() {
        // Control characters other than line breaks and tabs go: each would
        // encode to six bytes and could take a page past what a record holds.
        let clean = crate::redact::redacted(&e.text).replace(|c: char| c.is_control() && c != '\n' && c != '\t', " ");
        if !text.is_empty() && text.len() + clean.len() + 1 > PAGE_BYTES {
            break;
        }
        text.push_str(&clipped(&clean, PAGE_BYTES));
        text.push('\n');
        oldest = i;
    }
    if end == 0 {
        text = if page.is_none() { "(nothing in this session's transcript yet)".into() } else { "(nothing older this machine can read)".into() };
    }
    let next = (oldest > 0).then(|| format!("{BEFORE}{}", entries[oldest].id));
    TranscriptPage { session: session.to_string(), text: crate::redact::framed(&text, &format!("session {session}")), next }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Activity, Registration};
    use serde_json::json;

    const SESSION: &str = "01a0ec7b-0000-7000-8000-0000000000a1";

    fn scratch(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("gruntas-god-read-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Built in two halves: a whole one in the source trips push scanning.
    fn secret() -> String {
        ["ghp_abcdefghij", "klmnopqrstuvwxyz0123456789"].concat()
    }

    fn registered(daemon: &Path, harness: PluginHarness, transcript: &Path) {
        let mut r = crate::daemon::registry::Registry::default();
        let reg = Registration { session_id: SESSION.into(), harness, harness_pid: None, config_home: None, cwd: None, repository: None, transcript_path: Some(transcript.display().to_string()), attendance: None };
        r.apply(&Activity::Register(reg));
        r.save(daemon).unwrap();
    }

    /// A Claude transcript of `n` turns, each a prompt and an answer of
    /// about 1 KB; the last answer holds a secret.
    fn claude_transcript(path: &Path, n: usize) {
        let mut lines = String::new();
        for i in 0..n {
            let said = if i + 1 == n { format!("answer {i} token {}", secret()) } else { format!("answer {i} {}", "x".repeat(1000)) };
            lines += &json!({ "type": "user", "uuid": format!("u{i}"), "sessionId": SESSION, "message": { "role": "user", "content": format!("prompt {i}") } }).to_string();
            lines += "\n";
            lines += &json!({ "type": "assistant", "uuid": format!("a{i}"), "sessionId": SESSION, "message": { "role": "assistant", "model": "claude", "content": [{ "type": "text", "text": said }] } }).to_string();
            lines += "\n";
        }
        std::fs::write(path, lines).unwrap();
    }

    fn page_of(a: HostAnswer) -> TranscriptPage {
        let HostAnswer::Page(p) = a else { panic!("{a:?}") };
        p
    }

    /// Every page from the newest, by its `next`.
    fn all_pages(d: &Path) -> Vec<TranscriptPage> {
        let mut pages = vec![page_of(answer_in(d, d, SESSION, None))];
        while let Some(cursor) = pages.last().and_then(|p| p.next.clone()) {
            pages.push(page_of(answer_in(d, d, SESSION, Some(&cursor))));
        }
        pages
    }

    #[test]
    fn ga04_a_claude_transcript_pages_newest_first_redacted() {
        let d = scratch("claude");
        let transcript = d.join("t.jsonl");
        claude_transcript(&transcript, 30);
        registered(&d, PluginHarness::Claude, &transcript);
        let pages = all_pages(&d);
        let first = &pages[0].text;
        assert!(first.starts_with("<transcript from") && first.len() < PAGE_BYTES + 1024, "{first}");
        assert!(!first.contains(&secret()) && first.contains("[redacted: github token]"), "{first}");
        assert!(first.find("answer 29").unwrap() < first.find("prompt 29").unwrap(), "newest first");
        assert!(pages.len() > 2 && pages.last().unwrap().text.contains("prompt 0"), "the last page reaches the start");
        let prompts: Vec<&str> = pages.iter().flat_map(|p| p.text.lines().filter(|l| l.starts_with("user: prompt"))).collect();
        let want: Vec<String> = (0..30).rev().map(|i| format!("user: prompt {i}")).collect();
        assert_eq!(prompts, want, "every entry once, newest first across pages");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn ga04_a_gruntas_run_log_reads_as_pages() {
        let d = scratch("run");
        let (mut log, root) = crate::log::SessionLog::create(&d, &d, "test").unwrap();
        let items = [Item::UserText { text: "fix the build".into(), images: Vec::new(), origin: None }, Item::AssistantText { text: format!("done, key {}", secret()) }];
        for (i, item) in items.into_iter().enumerate() {
            log.append(LogBody::ItemCompleted { turn_id: "t".into(), item_id: format!("i{i}"), item }).unwrap();
        }
        drop(log);
        let p = page_of(answer_in(&d, &d, &root.session_id, None));
        assert!(p.text.find("assistant: done").unwrap() < p.text.find("user: fix the build").unwrap(), "{}", p.text);
        assert!(!p.text.contains(&secret()) && p.next.is_none(), "{}", p.text);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn ga04_a_codex_session_cant_be_read_yet() {
        let d = scratch("codex");
        registered(&d, PluginHarness::Codex, &d.join("rollout.jsonl"));
        let HostAnswer::Refused { code, message } = answer_in(&d, &d, SESSION, None) else { panic!() };
        assert_eq!(code, refusal::CANT_READ);
        assert!(message.contains("can't read this session yet"), "{message}");
        let none = answer_in(&d, &d, "01a0ec7b-0000-7000-8000-0000000000ff", None);
        assert!(matches!(none, HostAnswer::Refused { ref code, .. } if code == refusal::NO_SESSION), "{none:?}");
        let _ = std::fs::remove_dir_all(&d);
    }
}
