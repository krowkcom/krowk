//! gruntas_read (GA04, R-GOD-4): a `read` host command to the machine that
//! holds the session, its page back through the answer record, with the
//! cursor for the page before.

use super::super::Outcome;
use serde_json::Value;

pub(super) fn read(_args: &Value) -> Outcome {
    super::not_yet("gruntas_read", "GA04")
}

#[cfg(test)]
mod tests {
    #[test]
    #[ignore = "GA04: a page round-trips through the dev registry's answer record to the tool"]
    fn ga04_a_page_comes_back_through_the_answer_record() {
        unimplemented!("GA04")
    }
}
