//! The Claude Code stream decoder against live recordings of `claude`
//! (`fixtures/claude/recorded`, whose README says how they were made): a
//! `user` line written to its stdin while a turn runs, which steering
//! (R-STEER-1) is built on; and the agents a turn starts with Claude
//! Code's `Agent` tool, whose lines, progress, permission requests and
//! `stop_task` the per-agent child sessions (R-SUB-7 to R-SUB-10) are
//! built on.

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

#[test]
fn r_steer_1_an_interrupt_that_cancels_what_is_queued_names_the_unread_line_and_no_turn_reads_it() {
    let lines = recording("steer_interrupt.txt");
    let steer = lines.iter().find_map(|l| match l {
        Line::Wrote(m) if is_steer(m) || m.pointer("/message/content").and_then(Value::as_str) == Some(STEER) => m["uuid"].as_str(),
        _ => None,
    });
    let steer = steer.expect("the steer was written with a uuid");
    let answer = lines.iter().find_map(|l| match l {
        Line::Printed(m) if m["type"] == "control_response" => Some(&m["response"]["response"]),
        _ => None,
    });
    let answer = answer.expect("the interrupt was answered");
    assert_eq!(answer["cancelled"], serde_json::json!([steer]), "{answer}");
    assert!(!lines.iter().any(|l| matches!(l, Line::Printed(m) if m["isReplay"] == true && m.pointer("/message/content").and_then(Value::as_str) == Some(STEER))), "never replayed");
    let turns = turns(&lines);
    assert_eq!(turns.len(), 1, "no turn follows for the cancelled line");
}

/// The printed lines of a recording, numbered as in the recording.
fn printed(lines: &[Line]) -> Vec<(usize, &Value)> {
    lines.iter().enumerate().filter_map(|(i, l)| if let Line::Printed(m) = l { Some((i, m)) } else { None }).collect()
}

fn system<'a>(lines: &'a [Line], subtype: &str) -> Vec<(usize, &'a Value)> {
    printed(lines).into_iter().filter(|(_, m)| m["type"] == "system" && m["subtype"] == subtype).collect()
}

/// The depth-1 agents of the conversation's own: `task_started` with
/// `task_type` `local_agent`.
fn agents(lines: &[Line]) -> Vec<(usize, &Value)> {
    system(lines, "task_started").into_iter().filter(|(_, m)| m["task_type"] == "local_agent").collect()
}

/// A child's own lines: those naming the `Agent` call that started it.
fn child_lines<'a>(lines: &'a [Line], call: &str) -> Vec<(usize, &'a Value)> {
    printed(lines).into_iter().filter(|(_, m)| m["parent_tool_use_id"] == call).collect()
}

/// How a task ended: its `task_updated` status and its `task_notification`
/// status, with where each was printed.
fn ended<'a>(lines: &'a [Line], task: &str) -> ((usize, &'a str), (usize, &'a str)) {
    let of = |subtype, ptr| system(lines, subtype).into_iter().find(|(_, m)| m["task_id"] == task && m.pointer(ptr).is_some()).map(|(i, m)| (i, m.pointer(ptr).and_then(Value::as_str).unwrap()));
    (of("task_updated", "/patch/status").expect("task_updated"), of("task_notification", "/status").expect("task_notification"))
}

fn initialize(lines: &[Line]) -> &Value {
    lines.iter().find_map(|l| match l {
        Line::Wrote(m) if m.pointer("/request/subtype").and_then(Value::as_str) == Some("initialize") => Some(&m["request"]),
        _ => None,
    }).expect("initialize was written")
}

#[test]
fn r_sub_7_r_sub_8_parallel_agents_announce_their_call_and_prompt_before_any_line_of_their_own() {
    let lines = recording("agents_parallel.txt");
    assert_eq!(initialize(&lines)["forwardSubagentText"], true);
    let agents = agents(&lines);
    assert_eq!(agents.len(), 2, "two agents");
    for (at, a) in &agents {
        for k in ["task_id", "tool_use_id", "description", "subagent_type", "prompt"] {
            assert!(a[k].as_str().is_some_and(|s| !s.is_empty()), "task_started.{k}: {a}");
        }
        assert_eq!(a["spawn_depth"], 1, "{a}");
        assert_eq!(a["is_backgrounded"], false, "in this recording both run in the foreground: {a}");
        assert!(a.get("owned_by_subagent").is_none(), "{a}");
        let call = a["tool_use_id"].as_str().unwrap();
        let own = child_lines(&lines, call);
        assert!(own.len() >= 2, "{call}: the child's lines are forwarded");
        assert!(own.iter().all(|(i, _)| i > at), "no line of a child before its task_started");
        // Finished messages only: a child's partial messages are not forwarded.
        assert!(own.iter().all(|(_, m)| m["type"] == "user" || m["type"] == "assistant"), "{call}");
        // A foreground child's first line is its prompt.
        let (_, first) = own[0];
        assert_eq!(first["type"], "user");
        assert_eq!(first.pointer("/message/content/0/text"), Some(&a["prompt"]));
        // They name the call and the agent's type, never the task id.
        for (_, m) in &own {
            assert_eq!(m["subagent_type"], a["subagent_type"]);
            assert_eq!(m["task_description"], a["description"]);
            assert!(m.get("task_id").is_none() && m.get("agent_id").is_none(), "{m}");
        }
    }
    assert!(!printed(&lines).iter().any(|(_, m)| m["type"] == "stream_event" && m["parent_tool_use_id"].is_string()), "no stream_event of a child");
    let reader = agents.iter().find(|(_, a)| a["description"] == "read notes").unwrap().1;
    let ((_, updated), (_, notified)) = ended(&lines, reader["task_id"].as_str().unwrap());
    assert_eq!((updated, notified), ("completed", "completed"));
    // The stream as today's decoder reads it: one turn.
    assert_eq!(turns(&lines).len(), 1);
}

#[test]
fn r_sub_9_task_progress_names_the_agent_its_last_tool_and_its_usage() {
    let lines = recording("agents_parallel.txt");
    let agents = agents(&lines);
    let progress = system(&lines, "task_progress");
    assert_eq!(progress.len(), 2, "one for each agent's tool call");
    for (at, p) in &progress {
        let (started, a) = agents.iter().find(|(_, a)| a["task_id"] == p["task_id"]).expect("names a started agent");
        assert!(started < at);
        assert_eq!(p["tool_use_id"], a["tool_use_id"]);
        assert!(p["last_tool_name"].as_str().is_some_and(|s| !s.is_empty()), "{p}");
        assert!(p["description"].is_string(), "{p}");
        for k in ["total_tokens", "tool_uses", "duration_ms"] {
            assert!(p["usage"][k].as_u64().is_some(), "usage.{k}: {p}");
        }
    }
    assert!(progress.iter().any(|(_, p)| p["last_tool_name"] == "Bash") && progress.iter().any(|(_, p)| p["last_tool_name"] == "Read"));
}

#[test]
fn r_sub_9_a_childs_can_use_tool_names_its_agent_and_its_own_call() {
    let lines = recording("agents_parallel.txt");
    let asks: Vec<&Value> = printed(&lines).into_iter().filter(|(_, m)| m.pointer("/request/subtype").and_then(Value::as_str) == Some("can_use_tool")).map(|(_, m)| &m["request"]).collect();
    assert_eq!(asks.len(), 1, "the sleeper's Bash; the reader's Read in the working directory is not asked about");
    let ask = asks[0];
    let sleeper = agents(&lines).into_iter().find(|(_, a)| a["description"] == "sleeper").unwrap().1;
    assert_eq!(ask["tool_name"], "Bash");
    assert_eq!(ask["agent_id"], sleeper["task_id"], "agent_id is the agent's task_id");
    // tool_use_id is the child's own call, one of its forwarded tool_use blocks.
    let call = sleeper["tool_use_id"].as_str().unwrap();
    let uses: Vec<&Value> = child_lines(&lines, call).into_iter().filter_map(|(_, m)| m.pointer("/message/content/0").filter(|b| b["type"] == "tool_use")).map(|b| &b["id"]).collect();
    assert!(uses.contains(&&ask["tool_use_id"]), "{ask}");
    assert_ne!(ask["tool_use_id"], sleeper["tool_use_id"]);
}

#[test]
fn r_sub_10_stop_task_in_print_mode_stops_one_agent_and_its_command_and_the_turn_goes_on() {
    let lines = recording("agents_parallel.txt");
    let sleeper = agents(&lines).into_iter().find(|(_, a)| a["description"] == "sleeper").unwrap().1;
    let task = sleeper["task_id"].as_str().unwrap();
    let (sent, id) = lines.iter().enumerate().find_map(|(i, l)| match l {
        Line::Wrote(m) if m.pointer("/request/subtype").and_then(Value::as_str) == Some("stop_task") => Some((i, m["request_id"].as_str().unwrap())),
        _ => None,
    }).expect("stop_task was written");
    let Line::Wrote(stop) = &lines[sent] else { unreachable!() };
    assert_eq!(stop["request"]["task_id"], task);
    let (answered, answer) = printed(&lines).into_iter().find(|(_, m)| m["type"] == "control_response" && m["response"]["request_id"] == id).expect("answered");
    assert_eq!(answer["response"]["subtype"], "success", "{answer}");
    let ((u, updated), (n, notified)) = ended(&lines, task);
    assert_eq!((updated, notified), ("killed", "stopped"));
    assert!(sent < u && u < n && n < answered, "the task ends, then the answer comes");
    // The Bash call the agent was running is a task of its own, stopped too.
    let bash = system(&lines, "task_started").into_iter().find(|(_, m)| m["task_type"] == "local_bash").unwrap().1;
    assert_eq!(bash["owned_by_subagent"], true);
    assert!(bash.get("spawn_depth").is_none(), "{bash}");
    let bash_ended = system(&lines, "task_notification").into_iter().find(|(_, m)| m["task_id"] == bash["task_id"]).expect("the command's task_notification").1;
    assert_eq!(bash_ended["status"], "stopped");
    // The parent's Agent call gets an error result, and its turn goes on to one result.
    let call = sleeper["tool_use_id"].as_str().unwrap();
    assert!(printed(&lines).iter().any(|(i, m)| *i > answered && m["parent_tool_use_id"].is_null() && m.pointer("/message/content/0/tool_use_id").and_then(Value::as_str) == Some(call) && m.pointer("/message/content/0/is_error") == Some(&Value::Bool(true))));
    let results: Vec<_> = printed(&lines).into_iter().filter(|(_, m)| m["type"] == "result").collect();
    assert_eq!(results.len(), 1);
    assert!(results[0].0 > answered && results[0].1["is_error"] == false);
}

#[test]
fn r_sub_7_a_background_agents_lines_arrive_between_turns_and_its_end_begins_one() {
    let lines = recording("agents_background.txt");
    assert_eq!(initialize(&lines)["forwardSubagentText"], true);
    let agents = agents(&lines);
    assert_eq!(agents.len(), 1);
    let (at, a) = agents[0];
    assert_eq!(a["is_backgrounded"], true);
    assert!(a["prompt"].as_str().is_some_and(|s| !s.is_empty()));
    let results: Vec<_> = printed(&lines).into_iter().filter(|(_, m)| m["type"] == "result").collect();
    assert_eq!(results.len(), 2, "the prompt's turn, then the one Claude Code begins");
    assert!(results[0].1.get("origin").is_none());
    assert_eq!(results[1].1.pointer("/origin/kind").and_then(Value::as_str), Some("task-notification"));
    let second_init = system(&lines, "init").into_iter().map(|(i, _)| i).find(|i| *i > results[0].0).expect("a turn Claude Code began");
    let own = child_lines(&lines, a["tool_use_id"].as_str().unwrap());
    assert!(!own.is_empty());
    assert!(own.iter().all(|(i, _)| *i > at && *i > results[0].0 && *i < second_init), "every line of the agent between the turns");
    // Unlike a foreground agent's, its prompt is not forwarded as a line:
    // task_started.prompt is the only place it is.
    assert!(!own.iter().any(|(_, m)| m["type"] == "user" && m.pointer("/message/content/0/text") == Some(&a["prompt"])));
    assert_eq!(own[0].1["type"], "assistant");
    let task = a["task_id"].as_str().unwrap();
    let ((_, updated), (n, notified)) = ended(&lines, task);
    assert_eq!((updated, notified), ("completed", "completed"));
    assert!(n < second_init, "ended before the turn that answers it");
    // Its own can_use_tool comes between turns too, with its identity.
    let ask = printed(&lines).into_iter().find(|(_, m)| m.pointer("/request/subtype").and_then(Value::as_str) == Some("can_use_tool")).unwrap();
    assert!(ask.0 > results[0].0 && ask.0 < second_init);
    assert_eq!(ask.1["request"]["agent_id"], task);
    assert!(system(&lines, "task_progress").iter().all(|(i, p)| *i > results[0].0 && p["task_id"] == task));
    assert_eq!(turns(&lines).len(), 2);
}
