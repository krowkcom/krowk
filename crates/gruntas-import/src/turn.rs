//! Where one turn ends and the next begins: a user prompt starts a turn;
//! tool results, meta lines, attachments and interrupts do not.

use gruntas_store::Role;

#[derive(Debug, Clone, Default)]
pub struct TurnCandidate {
    pub role: Option<Role>,
    pub meta: bool,
    pub attachment: bool,
    pub interrupt: bool,
    pub part_types: Vec<String>,
}

pub fn starts_turn(c: &TurnCandidate) -> bool {
    c.role == Some(Role::User) && !c.meta && !c.attachment && !c.interrupt && c.part_types.iter().any(|t| t != crate::PART_TOOL_RESULT)
}

/// Half-open [start, end) index spans over the candidates. The first always
/// starts a turn, whatever it is, so nothing lands outside one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnSpan {
    pub start: usize,
    pub end: usize,
}

pub fn split_turns(candidates: &[TurnCandidate]) -> Vec<TurnSpan> {
    let mut spans: Vec<TurnSpan> = Vec::new();
    for (i, c) in candidates.iter().enumerate() {
        if i != 0 && !starts_turn(c) {
            continue;
        }
        if let Some(last) = spans.last_mut() {
            last.end = i;
        }
        spans.push(TurnSpan { start: i, end: candidates.len() });
    }
    spans
}

/// Names each message's turn, so the store links the message to its turn
/// row: message i is candidate i, and belongs to the span holding it.
pub fn link_turns(messages: &mut [gruntas_store::Message], spans: &[TurnSpan]) {
    debug_assert_eq!(spans.last().map_or(0, |s| s.end), messages.len(), "one turn candidate per message");
    for (seq, span) in spans.iter().enumerate() {
        for m in messages.iter_mut().take(span.end).skip(span.start) {
            m.turn_seq = Some(seq as i64);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompts_start_turns_and_tool_results_do_not() {
        let user = |types: &[&str]| TurnCandidate { role: Some(Role::User), part_types: types.iter().map(|t| t.to_string()).collect(), ..Default::default() };
        let asst = TurnCandidate { role: Some(Role::Assistant), ..Default::default() };
        let spans = split_turns(&[user(&["text"]), asst.clone(), user(&["tool_result"]), asst.clone(), user(&["text"]), asst]);
        assert_eq!(spans, vec![TurnSpan { start: 0, end: 4 }, TurnSpan { start: 4, end: 6 }]);
        assert!(!starts_turn(&TurnCandidate { meta: true, ..user(&["text"]) }));
    }
}
