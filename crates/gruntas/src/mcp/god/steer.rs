//! gruntas_message and gruntas_interrupt (GA05, R-GOD-6): a `message` or
//! `interrupt` host command to the machine that holds the session, carrying
//! this session's origin (an agent's, with its session id, attendance and
//! this machine's name); the target daemon checks it against its `steer`
//! setting and delivers it.

use super::super::Outcome;
use serde_json::Value;

pub(super) fn message(_args: &Value) -> Outcome {
    super::not_yet("gruntas_message", "GA05")
}

pub(super) fn interrupt(_args: &Value) -> Outcome {
    super::not_yet("gruntas_interrupt", "GA05")
}

#[cfg(test)]
mod tests {
    #[test]
    #[ignore = "GA05: the tool sends this session's agent origin and says the target's answer or refusal"]
    fn ga05_a_steer_carries_this_sessions_origin() {
        unimplemented!("GA05")
    }
}
