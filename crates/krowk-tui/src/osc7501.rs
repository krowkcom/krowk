//! OSC 7501, the Program Status Protocol (revision 0.4): one report of
//! what krowk, or one of its subagents, is doing, for a terminal that
//! keeps a record per program. Only ever well-formed: a terminal discards
//! a whole report over one bad byte or one field past its limit, and a
//! discarded report says nothing.

use base64::Engine as _;

/// The decoded limits on the free-text fields, in bytes.
pub const MSG_MAX: usize = 2048;
pub const TITLE_MAX: usize = 192;

/// What waits on the person while a record is blocked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// An approval.
    Permission,
    /// An answer to type.
    Question,
    /// A sign-in.
    Auth,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum State {
    #[default]
    Idle,
    Working,
    Blocked(Kind),
    Done,
    Error,
    /// Not a state: removes the record and every record beneath it, or
    /// with no id every record on the terminal.
    Clear,
}

/// One record as krowk reports it. `id` is the path's segments, empty for
/// the root record; `title` and `msg` are plain text, cleaned here.
#[derive(Clone, Copy, Debug, Default)]
pub struct Report<'a> {
    pub state: State,
    pub id: &'a [&'a str],
    pub title: &'a str,
    pub msg: &'a str,
}

/// `ESC ] 7501 ; state=…:kind=…:app=krowk:id=…:title=…:msg=… ESC \`, or
/// `None` for an id outside the grammar or the limits: such a report is
/// dropped, never sent to the root record in its place. A clear carries
/// no title or msg.
pub fn encode(r: &Report) -> Option<String> {
    let id = id(r.id)?;
    let (state, kind) = match r.state {
        State::Idle => ("idle", None),
        State::Working => ("working", None),
        State::Blocked(k) => ("blocked", Some(k)),
        State::Done => ("done", None),
        State::Error => ("error", None),
        State::Clear => ("clear", None),
    };
    let mut out = format!("\x1b]7501;state={state}");
    if let Some(k) = kind {
        out.push_str(match k {
            Kind::Permission => ":kind=permission",
            Kind::Question => ":kind=question",
            Kind::Auth => ":kind=auth",
        });
    }
    out.push_str(":app=krowk");
    if let Some(id) = id {
        out.push_str(":id=");
        out.push_str(&id);
    }
    if r.state != State::Clear {
        for (key, text, max) in [("title", r.title, TITLE_MAX), ("msg", r.msg, MSG_MAX)] {
            let text = text_field(text, max);
            if !text.is_empty() {
                out.push(':');
                out.push_str(key);
                out.push('=');
                out.push_str(&base64::engine::general_purpose::STANDARD.encode(text));
            }
        }
    }
    out.push_str("\x1b\\");
    Some(out)
}

/// The id as sent: `Some(None)` for the root, `None` when it breaks the
/// grammar (`[A-Za-z0-9_.+-]{1,32}` a segment) or the limits (8 levels,
/// 128 bytes).
fn id(segments: &[&str]) -> Option<Option<String>> {
    if segments.is_empty() {
        return Some(None);
    }
    let ok = |s: &&str| (1..=32).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'+' | b'-'));
    if segments.len() > 8 || !segments.iter().all(ok) {
        return None;
    }
    let id = segments.join("/");
    (id.len() <= 128).then_some(Some(id))
}

/// The first line, without a control or an invisible formatting character
/// (the spec refuses the first, and the second can make one name read as
/// another), cut at a character boundary to `max` bytes.
fn text_field(text: &str, max: usize) -> String {
    let line = crate::card::clean(text.lines().next().unwrap_or_default());
    let line = line.trim();
    line[..line.floor_char_boundary(max)].trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(b64: &str) -> String {
        String::from_utf8(base64::engine::general_purpose::STANDARD.decode(b64).unwrap()).unwrap()
    }

    /// The decoded value of `key` in a report.
    fn field(report: &str, key: &str) -> Option<String> {
        let body = report.strip_prefix("\x1b]7501;")?.strip_suffix("\x1b\\")?;
        body.split(':').find_map(|p| p.strip_prefix(key)?.strip_prefix('=')).map(decode)
    }

    fn root(state: State, msg: &str) -> String {
        encode(&Report { state, msg, ..Default::default() }).unwrap()
    }

    #[test]
    fn r_pres_1_the_specs_examples_encode_to_its_bytes_with_krowk_as_the_app() {
        // The spec's Terraform and brew messages, decoded and encoded again.
        let terraform = decode("QXBwbHkgMyB0byBhZGQsIDEgdG8gY2hhbmdlLCAwIHRvIGRlc3Ryb3k/");
        assert_eq!(terraform, "Apply 3 to add, 1 to change, 0 to destroy?");
        assert_eq!(
            root(State::Blocked(Kind::Permission), &terraform),
            "\x1b]7501;state=blocked:kind=permission:app=krowk:msg=QXBwbHkgMyB0byBhZGQsIDEgdG8gY2hhbmdlLCAwIHRvIGRlc3Ryb3k/\x1b\\"
        );
        let installing = decode("SW5zdGFsbGluZyB1cGRhdGVz");
        assert_eq!(root(State::Working, &installing), "\x1b]7501;state=working:app=krowk:msg=SW5zdGFsbGluZyB1cGRhdGVz\x1b\\");
        let password = decode("UGFzc3dvcmQgcmVxdWlyZWQgdG8gaW5zdGFsbCB1cGRhdGVz");
        assert_eq!(
            root(State::Blocked(Kind::Auth), &password),
            "\x1b]7501;state=blocked:kind=auth:app=krowk:msg=UGFzc3dvcmQgcmVxdWlyZWQgdG8gaW5zdGFsbCB1cGRhdGVz\x1b\\"
        );
        let upgraded = decode("VXBncmFkZWQgMTIgcGFja2FnZXM=");
        assert_eq!(root(State::Done, &upgraded), "\x1b]7501;state=done:app=krowk:msg=VXBncmFkZWQgMTIgcGFja2FnZXM=\x1b\\", "padding kept");
    }

    #[test]
    fn r_pres_1_each_state_and_kind_has_its_own_bytes() {
        assert_eq!(root(State::Idle, ""), "\x1b]7501;state=idle:app=krowk\x1b\\", "an empty msg is left out");
        assert_eq!(root(State::Working, ""), "\x1b]7501;state=working:app=krowk\x1b\\");
        assert_eq!(root(State::Blocked(Kind::Permission), ""), "\x1b]7501;state=blocked:kind=permission:app=krowk\x1b\\");
        assert_eq!(root(State::Blocked(Kind::Question), ""), "\x1b]7501;state=blocked:kind=question:app=krowk\x1b\\");
        assert_eq!(root(State::Blocked(Kind::Auth), ""), "\x1b]7501;state=blocked:kind=auth:app=krowk\x1b\\");
        assert_eq!(root(State::Done, ""), "\x1b]7501;state=done:app=krowk\x1b\\");
        assert_eq!(root(State::Error, "boom"), "\x1b]7501;state=error:app=krowk:msg=Ym9vbQ==\x1b\\");
        assert_eq!(root(State::Clear, "ignored"), "\x1b]7501;state=clear:app=krowk\x1b\\", "a clear with no id clears every record");
        let child = |state| encode(&Report { state, id: &["0f3a9c1d2e4b", "a1b2c3d4e5f6"], title: "US East", msg: "" }).unwrap();
        assert_eq!(child(State::Working), "\x1b]7501;state=working:app=krowk:id=0f3a9c1d2e4b/a1b2c3d4e5f6:title=VVMgRWFzdA==\x1b\\");
        assert_eq!(child(State::Clear), "\x1b]7501;state=clear:app=krowk:id=0f3a9c1d2e4b/a1b2c3d4e5f6\x1b\\");
    }

    #[test]
    fn r_pres_1_no_control_or_reordering_character_reaches_the_decoded_text() {
        for evil in ["evil\x1b]0;x\x07 tail", "a\tb\x7fc\u{85}d\u{9b}e", "txt.\u{202E}exe", "\u{200B}\u{FEFF}hidden\u{2066}"] {
            let r = encode(&Report { state: State::Working, title: evil, msg: evil, ..Default::default() }).unwrap();
            for key in ["title", "msg"] {
                let text = field(&r, key).unwrap();
                assert!(!text.chars().any(|c| c.is_control() || crate::card::is_bidi(c)), "{key} of {evil:?} decoded to {text:?}");
            }
        }
        let r = encode(&Report { state: State::Working, title: "first\nsecond", msg: "fix the test\r\nand more\n", ..Default::default() }).unwrap();
        assert_eq!(field(&r, "title").as_deref(), Some("first"));
        assert_eq!(field(&r, "msg").as_deref(), Some("fix the test"), "a multi-line text keeps its first line");
        assert_eq!(root(State::Working, "\u{202E}\x1b\n"), "\x1b]7501;state=working:app=krowk\x1b\\", "a text empty once cleaned is left out");
    }

    #[test]
    fn r_pres_1_long_texts_are_cut_at_a_character_boundary_and_the_report_stays_small() {
        let msg = "é".repeat(5 * 1024) + "x"; // 10 KB and a byte
        let title = "ü".repeat(150); // 300 bytes
        let id = ["abcdefghijklmnopqrstuvwxyz012345", "abcdefghijklmnopqrstuvwxyz012345", "abcdefghijklmnopqrstuvwxyz012345", "0123456789ab"];
        let r = encode(&Report { state: State::Blocked(Kind::Permission), id: &id, title: &title, msg: &msg }).unwrap();
        let (m, t) = (field(&r, "msg").unwrap(), field(&r, "title").unwrap());
        assert!(m.len() <= MSG_MAX && m.len() > MSG_MAX - 4 && m.chars().all(|c| c == 'é'), "{} bytes", m.len());
        assert_eq!(t, "ü".repeat(96), "192 bytes");
        assert!(r.len() < 4096, "{} bytes", r.len());
        assert!(r.split_once("msg=").unwrap().1.len() - 2 <= 2732, "the encoded msg within its limit");
    }

    #[test]
    fn r_pres_1_an_id_outside_the_grammar_or_its_limits_is_never_sent() {
        let with = |id: &[&str]| encode(&Report { state: State::Working, id, ..Default::default() });
        let long = "a".repeat(33);
        assert!(with(&[&long]).is_none(), "a 33-byte segment");
        assert!(with(&["a"; 9]).is_none(), "a ninth level");
        assert!(with(&["a", "", "b"]).is_none(), "an empty segment");
        assert!(with(&[""]).is_none(), "an empty id that is not the root");
        assert!(with(&["a:b"]).is_none(), "a `:` would end the pair");
        assert!(with(&["a/b"]).is_none(), "a `/` inside a segment");
        assert!(with(&["é"]).is_none());
        let wide = "a".repeat(32);
        assert!(with(&[&wide, &wide, &wide, &wide]).is_none(), "131 bytes");
        assert!(with(&[&wide, &wide, &wide, "a".repeat(29).as_str()]).is_some(), "128 bytes");
        assert!(with(&["a"; 8]).is_some(), "eight levels");
        assert!(with(&["A-z_0.9+"]).is_some());
    }
}
