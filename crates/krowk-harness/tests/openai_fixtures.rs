//! The OpenAI Responses client, offline, against recorded SSE streams
//! (R-PROV-1, R-PROV-3, R-LOG-3).
//!
//! The fixtures are the Responses API's documented event stream —
//! `response.created` … `response.completed`, with a reasoning item whose
//! summary streams and whose `encrypted_content` arrives whole, a message,
//! and a freeform `custom_tool_call` for apply_patch whose input streams in
//! two pieces. They were written out from the API reference, not captured
//! from a live key; the PR lists the live check that would capture one.

#[path = "common/mock.rs"]
mod mock;
#[path = "common/providers.rs"]
mod providers;

use krowk_harness::engine::{EngineEvent, HistoryItem};
use krowk_harness::http::Decode;
use krowk_harness::native::ModelRequest;
use krowk_harness::openai::request_body;
use krowk_harness::openai::stream::Decoder;
use krowk_harness::protocol::{Effort, ImageRef, Item, LogBody, LogEvent, WireApi};
use krowk_harness::sse::SseParser;
use krowk_harness::tools;
use krowk_harness::toolset::{by_name, Toolset};
use serde_json::{json, Value};

const PATCH: &str = "*** Begin Patch\n*** Update File: README.md\n@@ # krowk\n \n-Permalinks for agent output.\n+Permalinks for everything agents make.\n*** End Patch\n";

fn decode(name: &str, chunk: usize) -> (Decoder, Vec<EngineEvent>) {
    let raw = providers::fixture(name);
    let mut p = SseParser::default();
    let mut d = Decoder::new("openai");
    let mut out = Vec::new();
    for c in raw.as_bytes().chunks(chunk) {
        for ev in p.push(c) {
            out.extend(d.apply(&ev).unwrap());
        }
    }
    (d, out)
}

fn history(items: &[(Item, Option<usize>)]) -> Vec<HistoryItem> {
    items.iter().map(|(item, response)| HistoryItem { item: item.clone(), response: *response }).collect()
}

fn gpt_tools() -> Vec<krowk_harness::protocol::ToolDefinition> {
    tools::definitions(&Toolset { preset: by_name("gpt").unwrap(), custom_tools: true })
}

#[test]
fn r_prov_1_a_responses_stream_decodes_into_items_whatever_the_chunking() {
    for chunk in [1, 7, 64, 1 << 20] {
        let (d, events) = decode("openai/turn1_apply_patch.sse", chunk);
        assert!(d.done());
        assert_eq!(d.response_id.as_deref(), Some("resp_01TurnOnePatch"));
        assert_eq!(d.model, "gpt-5.4-2026-03-05");
        assert_eq!(d.stop_reason.as_deref(), Some("tool_use"));
        // Input counts its cached part and output its reasoning: split out.
        assert_eq!((d.usage.input_tokens, d.usage.cache_read_tokens, d.usage.output_tokens, d.usage.reasoning_tokens), (1320, 0, 148, 64));
        let items: Vec<&Item> = d.items.iter().map(|(_, i)| i).collect();
        assert_eq!(items.len(), 3);
        let Item::Reasoning { text, blob: Some(b) } = items[0] else { panic!("{:?}", items[0]) };
        assert_eq!(text, "**Rewording the README**\n\nThe user wants the tagline changed; one patch does it.");
        assert_eq!((b.provider.as_str(), b.wire_api, b.data["encrypted_content"].as_str()), ("openai", WireApi::OpenaiResponses, Some(providers::ENCRYPTED)));
        assert_eq!(items[1], &Item::AssistantText { text: "I'll update the README.".into() });
        // A freeform call's input is the patch itself, as a string.
        assert_eq!(items[2], &Item::ToolCall { call_id: "call_01ApplyPatch".into(), name: "apply_patch".into(), input: json!(PATCH) });
        for (id, _) in &d.items {
            let started = events.iter().position(|e| matches!(e, EngineEvent::ItemStarted { item_id, .. } if item_id == id)).unwrap();
            let ended = events.iter().position(|e| matches!(e, EngineEvent::ItemCompleted { item_id, .. } if item_id == id)).unwrap();
            assert!(started < ended);
        }
        let deltas = events.iter().filter(|e| matches!(e, EngineEvent::ItemDelta { .. })).count();
        assert_eq!(deltas, 2 + 2 + 2, "two summary, two text and two patch deltas; the encrypted content is never a delta");
    }
    // A failed response is an error with the provider's code, not a turn
    // that silently ends.
    let raw = providers::fixture("openai/failed.sse");
    let mut d = Decoder::new("openai");
    let err = SseParser::default().push(raw.as_bytes()).iter().find_map(|ev| d.apply(ev).err()).unwrap();
    assert_eq!((err.code.as_str(), err.status), ("provider_unavailable", 500));
}

#[test]
fn r_log_3_encrypted_reasoning_round_trips_through_the_log_unmodified() {
    let (d, _) = decode("openai/turn1_apply_patch.sse", 5);
    // Through the log, as the host writes each item and a resume reads it.
    let read_back: Vec<Item> = d
        .items
        .iter()
        .map(|(id, item)| {
            let ev = LogEvent { id: id.clone(), parent_id: None, session_id: id.clone(), time_ms: 0, body: LogBody::ItemCompleted { turn_id: "t".into(), item_id: id.clone(), item: item.clone() } };
            let line = serde_json::to_string(&ev).unwrap();
            match serde_json::from_str::<LogEvent>(&line).unwrap().body {
                LogBody::ItemCompleted { item, .. } => item,
                other => panic!("{other:?}"),
            }
        })
        .collect();
    let mut items = vec![(Item::user("reword the README"), None)];
    items.extend(read_back.into_iter().map(|i| (i, Some(0))));
    items.push((Item::ToolResult { call_id: "call_01ApplyPatch".into(), output: "patched README.md".into(), is_error: false }, None));
    let req = ModelRequest { model: "gpt-5.4".into(), system: "s".into(), tools: gpt_tools(), history: history(&items), session_id: "sess".into(), reasoning: true, ..ModelRequest::default() };
    let body = request_body(&req, "openai");
    let input = body["input"].as_array().unwrap();
    // The reasoning item goes back as it came: encrypted content and summary
    // byte for byte, without the id OpenAI keeps nothing under.
    let original = &d.items[0].1;
    let Item::Reasoning { blob: Some(b), .. } = original else { panic!() };
    let mut want = b.data.clone();
    want.as_object_mut().unwrap().remove("id");
    assert_eq!(input[1], want);
    assert!(body.to_string().contains(&format!("\"encrypted_content\":\"{}\"", providers::ENCRYPTED)), "the encrypted content goes back byte for byte");
    // Then the message, the freeform call and its freeform output.
    assert_eq!(input[2], json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "I'll update the README."}]}));
    assert_eq!(input[3], json!({"type": "custom_tool_call", "call_id": "call_01ApplyPatch", "name": "apply_patch", "input": PATCH}));
    assert_eq!(input[4], json!({"type": "custom_tool_call_output", "call_id": "call_01ApplyPatch", "output": "patched README.md"}));

    // Decided by the blob: another provider's reasoning never replays as
    // encrypted content here, and is downgraded to framed text (R-SWITCH-1).
    let mut foreign = req.clone();
    for h in foreign.history.iter_mut() {
        if let Item::Reasoning { blob: Some(b), .. } = &mut h.item {
            b.provider = "anthropic".into();
            b.wire_api = WireApi::AnthropicMessages;
        }
    }
    let body = request_body(&foreign, "openai");
    assert!(!body.to_string().contains(providers::ENCRYPTED));
    let said = body["input"][1]["content"][0]["text"].as_str().unwrap();
    assert!(said.starts_with("<reasoning from an earlier model>\n**Rewording the README**"), "{said}");
    // Reasoning with nothing readable has nothing to downgrade.
    let mut blank = foreign.clone();
    for h in blank.history.iter_mut() {
        if let Item::Reasoning { text, .. } = &mut h.item {
            text.clear();
        }
    }
    assert_eq!(request_body(&blank, "openai")["input"][1]["role"], "assistant");
    assert_eq!(request_body(&blank, "openai")["input"][1]["content"][0]["text"], "I'll update the README.");
}

#[test]
fn r_prov_3_every_call_is_stateless_keyed_to_the_session_and_appends_to_one_prefix() {
    let turn1 = vec![(Item::user("reword the README"), None)];
    let req = |items: &[(Item, Option<usize>)]| ModelRequest {
        model: "gpt-5.4".into(),
        system: "You are krowk.".into(),
        tools: gpt_tools(),
        history: history(items),
        session_id: "0199a0b0-0000-7000-8000-000000000001".into(),
        effort: Some(Effort::High),
        reasoning: true,
        images: Default::default(),
    };
    let first = request_body(&req(&turn1), "openai");
    assert_eq!(first["store"], false, "stateless: the log is the conversation");
    assert_eq!(first["stream"], true);
    assert_eq!(first["include"], json!(["reasoning.encrypted_content"]));
    assert_eq!(first["reasoning"], json!({"effort": "high", "summary": "auto"}));
    assert_eq!(first["prompt_cache_key"], "0199a0b0-0000-7000-8000-000000000001");
    assert_eq!(first["instructions"], "You are krowk.");
    assert!(first.get("previous_response_id").is_none());
    // apply_patch is a freeform tool with its Lark grammar; the rest are functions.
    let tools = first["tools"].as_array().unwrap();
    let patch = tools.iter().find(|t| t["name"] == "apply_patch").unwrap();
    assert_eq!((patch["type"].as_str(), patch["format"]["type"].as_str(), patch["format"]["syntax"].as_str()), (Some("custom"), Some("grammar"), Some("lark")));
    assert!(patch["format"]["definition"].as_str().unwrap().contains("begin_patch"));
    let read = tools.iter().find(|t| t["name"] == "read").unwrap();
    assert_eq!((read["type"].as_str(), read["parameters"]["type"].as_str()), (Some("function"), Some("object")));

    // A later call of the same session renders everything before it the
    // same: the earlier call's input is a prefix of this one's, and the key
    // and the instructions do not move.
    let (d, _) = decode("openai/turn1_apply_patch.sse", 1 << 20);
    let mut turn2 = turn1.clone();
    turn2.extend(d.items.iter().map(|(_, i)| (i.clone(), Some(0))));
    turn2.push((Item::ToolResult { call_id: "call_01ApplyPatch".into(), output: "ok".into(), is_error: false }, None));
    turn2.push((Item::user("what language is it written in?"), None));
    let second = request_body(&req(&turn2), "openai");
    let (a, b) = (first["input"].as_array().unwrap(), second["input"].as_array().unwrap());
    assert_eq!(a[..], b[..a.len()], "the prefix never moves");
    for k in ["instructions", "tools", "prompt_cache_key", "model"] {
        assert_eq!(first[k], second[k], "{k}");
    }
    // A model that does not reason is asked for no encrypted reasoning,
    // which it would refuse.
    let plain = request_body(&ModelRequest { reasoning: false, effort: None, ..req(&turn1) }, "openai");
    assert!(plain.get("include").is_none() && plain.get("reasoning").is_none());
}

#[test]
fn r_prov_1_a_call_the_turn_stopped_before_answering_is_answered() {
    let items = vec![
        (Item::user("q"), None),
        (Item::ToolCall { call_id: "call_a".into(), name: "read".into(), input: json!({"path": "x"}) }, Some(0)),
        (Item::ToolCall { call_id: "call_b".into(), name: "apply_patch".into(), input: json!(PATCH) }, Some(0)),
        (Item::ToolResult { call_id: "call_a".into(), output: "x".into(), is_error: false }, None),
        (Item::user("go on"), None),
    ];
    let body = request_body(&ModelRequest { model: "gpt-5.4".into(), history: history(&items), ..ModelRequest::default() }, "openai");
    let kinds: Vec<(String, String)> = body["input"].as_array().unwrap().iter().map(|i| (i["type"].as_str().unwrap_or_default().to_string(), i["call_id"].as_str().unwrap_or_default().to_string())).collect();
    assert_eq!(
        kinds,
        [("message", ""), ("function_call", "call_a"), ("custom_tool_call", "call_b"), ("function_call_output", "call_a"), ("custom_tool_call_output", "call_b"), ("message", "")].map(|(a, b)| (a.to_string(), b.to_string()))
    );
    assert_eq!(body["input"][1]["arguments"], Value::String(json!({"path": "x"}).to_string()), "JSON arguments go back as their text");
    // No tools, no reasoning, no session: none of their fields.
    assert!(body.get("tools").is_none() && body.get("prompt_cache_key").is_none());
}

/// A prompt naming two images: one whose bytes were read, one whose file is gone.
fn with_images() -> ModelRequest {
    let r = |n: u32, file: &str| ImageRef { number: n, media_type: "image/png".into(), file: file.into() };
    let item = Item::UserText { text: "what differs between [Image #1] and [Image #2]?".into(), images: vec![r(1, "1-a.png"), r(2, "2-b.png")] };
    let mut req = ModelRequest { model: "m".into(), system: "s".into(), history: vec![HistoryItem { item, response: None }], ..ModelRequest::default() };
    req.images.insert("1-a.png".into(), "iVBORw0K".into());
    req
}

#[test]
fn an_image_is_sent_as_a_data_url_after_its_label() {
    let body = request_body(&with_images(), "openai");
    let content = &body["input"][0]["content"];
    assert_eq!(content[1], json!({ "type": "input_text", "text": "[Image #1]" }));
    assert_eq!(content[2], json!({ "type": "input_image", "image_url": "data:image/png;base64,iVBORw0K" }));
    assert_eq!(content[3]["text"], "[Image #2: the file is gone]");
}
