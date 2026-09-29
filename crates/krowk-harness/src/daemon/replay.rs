//! The running turn's frames a session keeps for catching a client up: only
//! those after its last logged event, since the log holds everything up to
//! it (R-LAG-1: the typing of the item under way, which the log does not
//! hold until the item ends). Bounded (R-LAG-10): past the cap it becomes a
//! snapshot — a run of an item's deltas merged into one delta of all their
//! text, the latest of each progress frame, and a subagent's oldest frames
//! let go — so a long item costs about its text, not a frame per token. A
//! subagent's frames let go are not replayed: they are in the subagent's
//! own log, which catching up does not read, so a client caught up past
//! the cap sees the subagent from where the replay still holds it.
//!
//! A merged delta remembers where each of its pieces ended and which
//! `seq` it had, so a client resuming from a `seq` inside it is sent only
//! the text after it: nothing twice.

use super::outbox::{Slot, slot_of};
use crate::protocol::{Delta, LiveEvent, StreamLine};
use std::collections::HashMap;

/// The replay kept per session, in bytes of the frames as sent.
pub const CAP: usize = 1 << 20;

struct Kept {
    seq: u64,
    line: StreamLine,
    /// For a merged delta: each piece's `seq` and where its text ends.
    marks: Vec<(u64, usize)>,
    weight: usize,
}

#[derive(Default)]
pub struct Tail {
    kept: Vec<Kept>,
    slots: HashMap<Slot, Kept>,
    bytes: usize,
    cap: usize,
}

impl Tail {
    pub fn new(cap: usize) -> Tail {
        Tail { cap, ..Tail::default() }
    }

    pub fn clear(&mut self) {
        self.kept.clear();
        self.slots.clear();
        self.bytes = 0;
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// Keeps `line`, sent as `seq` and `weight` bytes, of the session
    /// `root`'s stream.
    pub fn push(&mut self, root: &str, seq: u64, line: StreamLine, weight: usize) {
        self.bytes += weight;
        let kept = Kept { seq, line, marks: Vec::new(), weight };
        match slot_of(&kept.line) {
            Some(s) => {
                if let Some(old) = self.slots.insert(s, kept) {
                    self.bytes -= old.weight;
                }
            }
            None => self.kept.push(kept),
        }
        if self.bytes > self.cap {
            self.compact(root);
        }
    }

    fn compact(&mut self, root: &str) {
        let mut out: Vec<Kept> = Vec::with_capacity(self.kept.len() / 4 + 1);
        for k in self.kept.drain(..) {
            if let Some(last) = out.last_mut()
                && let Some((text, more)) = mergeable(&mut last.line, &k.line)
            {
                if last.marks.is_empty() {
                    last.marks.push((last.seq, text.len()));
                }
                text.push_str(more);
                let end = text.len();
                last.marks.push((k.seq, end));
                last.seq = k.seq;
                // The text, a mark a piece, and the frame around them.
                last.weight = end + last.marks.len() * 16 + 160;
                continue;
            }
            out.push(k);
        }
        self.kept = out;
        self.bytes = self.kept.iter().chain(self.slots.values()).map(|k| k.weight).sum();
        // Still over: a subagent's frames, oldest first (see above).
        while self.bytes > self.cap {
            let Some(i) = self.kept.iter().position(|k| k.line.session_id() != root) else {
                break;
            };
            self.bytes -= self.kept.remove(i).weight;
        }
    }

    /// The frames after `after` (a `seq`; 0 for all), in order.
    pub fn after(&self, after: u64) -> Vec<(u64, StreamLine)> {
        let mut all: Vec<&Kept> = self.kept.iter().chain(self.slots.values()).filter(|k| k.seq > after).collect();
        all.sort_by_key(|k| k.seq);
        all.into_iter()
            .map(|k| {
                // Resuming inside a merged delta: only the text after the
                // piece the client has.
                let cut = k.marks.iter().rev().find(|(s, _)| *s <= after).map(|(_, end)| *end);
                match (cut, &k.line) {
                    (Some(at), StreamLine::Live(LiveEvent::ItemDelta { session_id, turn_id, item_id, delta })) => {
                        let delta = match delta {
                            Delta::Text { text } => Delta::Text { text: text[at..].to_string() },
                            Delta::ToolInput { partial_json } => Delta::ToolInput { partial_json: partial_json[at..].to_string() },
                        };
                        (k.seq, StreamLine::Live(LiveEvent::ItemDelta { session_id: session_id.clone(), turn_id: turn_id.clone(), item_id: item_id.clone(), delta }))
                    }
                    _ => (k.seq, k.line.clone()),
                }
            })
            .collect()
    }
}

/// When `next` continues `last` — a delta of the same item, of the same
/// kind — the text to grow and what to grow it by.
fn mergeable<'a>(last: &'a mut StreamLine, next: &'a StreamLine) -> Option<(&'a mut String, &'a str)> {
    let (StreamLine::Live(LiveEvent::ItemDelta { item_id: a, session_id: sa, delta: da, .. }), StreamLine::Live(LiveEvent::ItemDelta { item_id: b, session_id: sb, delta: db, .. })) = (last, next)
    else {
        return None;
    };
    if a != b || sa != sb {
        return None;
    }
    match (da, db) {
        (Delta::Text { text }, Delta::Text { text: more }) => Some((text, more)),
        (Delta::ToolInput { partial_json }, Delta::ToolInput { partial_json: more }) => Some((partial_json, more)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn delta(session: &str, item: &str, text: &str) -> StreamLine {
        StreamLine::Live(LiveEvent::ItemDelta { session_id: session.into(), turn_id: "t".into(), item_id: item.into(), delta: Delta::Text { text: text.into() } })
    }

    fn text(lines: &[(u64, StreamLine)]) -> String {
        lines
            .iter()
            .filter_map(|(_, l)| match l {
                StreamLine::Live(LiveEvent::ItemDelta { delta: Delta::Text { text }, .. }) => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    /// R-LAG-10: a long item past the cap is a snapshot of its text, and a
    /// client resuming from any `seq` inside it gets exactly what it lacks.
    #[test]
    fn r_lag_10_the_replay_is_bounded_and_resumes_inside_a_snapshot() {
        let mut t = Tail::new(64 * 1024);
        let mut all = String::new();
        for seq in 1..=20_000u64 {
            let piece = format!("w{seq} ");
            all.push_str(&piece);
            t.push("s", seq, delta("s", "i", &piece), piece.len() + 160);
            if seq % 7 == 0 {
                t.push("s", seq, StreamLine::Live(LiveEvent::Cost { session_id: "s".into(), turn_id: "t".into(), cost_usd: None, turn_cost_usd: None, generated_tokens: seq as i64 }), 120);
            }
        }
        // About the text and a mark a piece, not a frame per piece.
        assert!(t.bytes() < 20_000 * 16 + all.len() + 64 * 1024, "{} bytes", t.bytes());
        assert_eq!(text(&t.after(0)), all);
        for from in [1u64, 5_000, 12_345, 19_999] {
            let prefix: String = (1..=from).map(|s| format!("w{s} ")).collect();
            assert_eq!(text(&t.after(from)), all[prefix.len()..], "resumed after {from}");
        }
        // One progress frame, the latest.
        let costs = t.after(0).into_iter().filter(|(_, l)| matches!(l, StreamLine::Live(LiveEvent::Cost { .. }))).count();
        assert_eq!(costs, 1);
    }

    #[test]
    fn r_lag_10_a_subagents_oldest_frames_go_first() {
        let mut t = Tail::new(4096);
        for seq in 1..=200u64 {
            // Distinct items: nothing merges.
            t.push("s", seq, delta("child", &format!("i{seq}"), "x"), 100);
        }
        t.push("s", 201, delta("s", "own", "mine"), 100);
        assert!(t.bytes() <= 4096);
        let kept = t.after(0);
        assert_eq!(kept.last().unwrap().0, 201, "the session's own frame is kept");
        assert!(kept.len() < 200);
    }
}
