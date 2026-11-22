//! Server-sent events, parsed incrementally from whatever chunks the
//! connection hands over — a chunk boundary can fall anywhere, inside a line
//! or inside a UTF-8 sequence, so bytes are buffered until a line is whole.
//! The WHATWG rules, as far as a provider stream uses them: `event:` and
//! `data:` fields, multi-line data joined by `\n`, comments, and any of
//! `\n`, `\r\n` or `\r` ending a line.

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SseEvent {
    /// The `event:` field; `message` when the event named none.
    pub event: String,
    pub data: String,
}

#[derive(Debug, Default)]
pub struct SseParser {
    buf: Vec<u8>,
    event: String,
    data: String,
    has_data: bool,
    /// The last byte seen was a `\r`, so a `\n` opening the next chunk
    /// belongs to it.
    after_cr: bool,
}

impl SseParser {
    /// Feeds bytes in; returns every event they completed.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        let mut out = Vec::new();
        for &b in chunk {
            if self.after_cr {
                self.after_cr = false;
                if b == b'\n' {
                    continue;
                }
            }
            match b {
                b'\n' | b'\r' => {
                    self.after_cr = b == b'\r';
                    let line = std::mem::take(&mut self.buf);
                    if let Some(ev) = self.line(&String::from_utf8_lossy(&line)) {
                        out.push(ev);
                    }
                }
                _ => self.buf.push(b),
            }
        }
        out
    }

    /// The event a stream ended on without the blank line that would have
    /// dispatched it, if any.
    pub fn finish(&mut self) -> Option<SseEvent> {
        if !self.buf.is_empty() {
            let line = std::mem::take(&mut self.buf);
            if let Some(ev) = self.line(&String::from_utf8_lossy(&line)) {
                return Some(ev);
            }
        }
        self.dispatch()
    }

    fn line(&mut self, line: &str) -> Option<SseEvent> {
        if line.is_empty() {
            return self.dispatch();
        }
        if line.starts_with(':') {
            return None;
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line, ""),
        };
        match field {
            "event" => self.event = value.to_string(),
            "data" => {
                if self.has_data {
                    self.data.push('\n');
                }
                self.data.push_str(value);
                self.has_data = true;
            }
            _ => {}
        }
        None
    }

    fn dispatch(&mut self) -> Option<SseEvent> {
        let event = std::mem::take(&mut self.event);
        if !self.has_data {
            return None;
        }
        self.has_data = false;
        Some(SseEvent { event: if event.is_empty() { "message".into() } else { event }, data: std::mem::take(&mut self.data) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_survive_any_chunking_and_every_line_ending() {
        let stream = "event: ping\r\ndata: {}\r\n\r\n: a comment\nevent: a\ndata: one\ndata: two\n\nevent: b\rdata:x\r\rdata: ą\n\n";
        let whole = SseParser::default().push(stream.as_bytes());
        let want = vec![
            SseEvent { event: "ping".into(), data: "{}".into() },
            SseEvent { event: "a".into(), data: "one\ntwo".into() },
            SseEvent { event: "b".into(), data: "x".into() },
            SseEvent { event: "message".into(), data: "ą".into() },
        ];
        assert_eq!(whole, want);
        // A byte at a time: through the middle of \r\n and of ą.
        let mut p = SseParser::default();
        let bytewise: Vec<SseEvent> = stream.as_bytes().iter().flat_map(|b| p.push(&[*b])).collect();
        assert_eq!(bytewise, want);
        let mut p = SseParser::default();
        assert!(p.push(b"event: last\ndata: tail").is_empty());
        assert_eq!(p.finish(), Some(SseEvent { event: "last".into(), data: "tail".into() }));
    }
}
