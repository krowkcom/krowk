//! The Claude Code stream decoder against live recordings of `claude`
//! (`fixtures/claude/recorded`, whose README says how they were made): a
//! `user` line written to its stdin while a turn runs, which steering
//! (R-STEER-1) is built on.

use krowk_harness::claude::stream::Translator;
use krowk_harness::engine::EngineEvent;
use krowk_harness::protocol::Item;
use serde_json::Value;
use std::path::Path;

const STEER: &str = "Also: end your answer with the word PINEAPPLE.";

/// One line of a recording: written (`>>`) or printed (`<<`).
enum Line {
    Wrote(Value),
    Printed(Value),
}

fn recording(name: &str) -> Vec<Line> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claude/recorded").join(name);
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    raw.lines()
        .filter(|l| !l.is_empty())
        .map(|l| match l.split_at(3) {
            (">> ", j) => Line::Wrote(serde_json::from_str(j).unwrap()),
            ("<< ", j) => Line::Printed(serde_json::from_str(j).unwrap()),
            _ => panic!("{name}: {l}"),
        })
        .collect()
}

/// What each Claude Code turn of a recording decodes to, a `Translator` a
/// turn, as the backend keeps one: its finished items and its outcome text.
fn turns(lines: &[Line]) -> Vec<(Vec<Item>, String)> {
    let mut turns = Vec::new();
    let mut t = Translator::default();
    let mut items = Vec::new();
    for l in lines {
        let Line::Printed(msg) = l else { continue };
        for ev in t.apply(msg).unwrap() {
            if let EngineEvent::ItemCompleted { item, .. } = ev {
                items.push(item);
            }
        }
        if let Some(o) = t.outcome.take() {
            turns.push((std::mem::take(&mut items), o.text));
            t = Translator::default();
        }
    }
    assert!(items.is_empty(), "items after the last result: {items:?}");
    turns
}

fn is_steer(msg: &Value) -> bool {
    msg.pointer("/message/content/0/text").and_then(Value::as_str) == Some(STEER)
}

/// Where the steer was replayed, and the index of every printed line that
/// is a `tool_result` or a `result`.
fn landmarks(lines: &[Line]) -> (usize, usize, Vec<usize>) {
    let wrote = lines.iter().position(|l| matches!(l, Line::Wrote(m) if is_steer(m))).expect("the steer was written");
    let replayed = lines.iter().position(|l| matches!(l, Line::Printed(m) if is_steer(m))).expect("the steer was replayed");
    let Line::Printed(r) = &lines[replayed] else { unreachable!() };
    assert_eq!(r["isReplay"], true, "{r}");
    let results = lines.iter().enumerate().filter(|(_, l)| matches!(l, Line::Printed(m) if m["type"] == "result")).map(|(i, _)| i).collect();
    (wrote, replayed, results)
}

#[test]
fn r_steer_1_a_line_written_during_a_tool_call_is_replayed_after_its_result_and_read_in_the_same_turn() {
    let lines = recording("steer_mid_turn.txt");
    let (wrote, replayed, results) = landmarks(&lines);
    let tool_result = lines.iter().position(|l| matches!(l, Line::Printed(m) if m.pointer("/message/content/0/type").and_then(Value::as_str) == Some("tool_result"))).unwrap();
    assert!(wrote < tool_result && tool_result + 1 == replayed, "written at {wrote}, the call's result at {tool_result}, replayed at {replayed}");
    assert_eq!(results.len(), 1, "one turn");
    assert!(replayed < results[0]);
    let turns = turns(&lines);
    assert_eq!(turns.len(), 1);
    // The replayed steer is not a tool result, so the decoder gives no item
    // for it: logging it is the backend's (ST1).
    let (items, answer) = &turns[0];
    assert!(items.iter().any(|i| matches!(i, Item::ToolCall { .. })) && items.iter().any(|i| matches!(i, Item::ToolResult { output, .. } if output == "done")), "{items:?}");
    assert!(answer.contains("PINEAPPLE"), "the model read it: {answer}");
}

#[test]
fn r_steer_1_a_line_written_during_the_final_answer_becomes_a_turn_claude_code_begins_after_the_result() {
    let lines = recording("steer_late.txt");
    let (wrote, replayed, results) = landmarks(&lines);
    assert_eq!(results.len(), 2, "two turns");
    assert!(wrote < results[0] && results[0] < replayed && replayed < results[1], "written at {wrote}, replayed at {replayed}, results at {results:?}");
    let Line::Printed(init) = &lines[results[0] + 1] else { panic!() };
    assert_eq!((init["type"].as_str(), init["subtype"].as_str()), (Some("system"), Some("init")), "the second turn opens with init");
    for i in &results {
        let Line::Printed(r) = &lines[*i] else { unreachable!() };
        assert!(r.get("origin").is_none_or(Value::is_null), "a turn begun for a queued line has no origin: {r}");
    }
    let turns = turns(&lines);
    assert_eq!(turns.len(), 2);
    assert!(!turns[0].1.contains("PINEAPPLE"), "not read in the first turn");
    assert!(turns[1].1.contains("PINEAPPLE"), "read in the second: {}", turns[1].1);
}
