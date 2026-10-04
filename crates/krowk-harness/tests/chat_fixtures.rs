//! The Chat Completions client, offline, against recorded SSE streams
//! (R-PROV-1, R-PROV-3, R-LOG-3): xAI's, with `reasoning_content` and a tool
//! call whose arguments arrive in pieces, and OpenRouter's, with
//! `reasoning_details` carrying a signature and no `[DONE]` at the end.
//! Written out from each API's reference, not captured from a live key.

#[path = "common/mock.rs"]
mod mock;
#[path = "common/providers.rs"]
mod providers;

use krowk_harness::chat::request_body;
use krowk_harness::chat::stream::Decoder;
use krowk_harness::engine::{EngineEvent, HistoryItem};
use krowk_harness::http::Decode;
use krowk_harness::native::ModelRequest;
use krowk_harness::protocol::{Effort, ImageRef, Item, WireApi};
use krowk_harness::sse::SseParser;
use serde_json::json;

fn decode(name: &str, provider: &str, chunk: usize) -> (Decoder, Vec<EngineEvent>) {
    let raw = providers::fixture(name);
    let mut p = SseParser::default();
    let mut d = Decoder::new(provider);
    let mut out = Vec::new();
    for c in raw.as_bytes().chunks(chunk) {
        for ev in p.push(c) {
            out.extend(d.apply(&ev).unwrap());
        }
    }
    if let Some(ev) = p.finish() {
        out.extend(d.apply(&ev).unwrap());
    }
    (d, out)
}

fn history(items: &[(Item, Option<usize>)]) -> Vec<HistoryItem> {
    items.iter().map(|(item, response)| HistoryItem { item: item.clone(), response: *response }).collect()
}

#[test]
fn r_prov_1_an_xai_stream_decodes_reasoning_text_and_a_tool_call_whatever_the_chunking() {
    for chunk in [1, 9, 128, 1 << 20] {
        let (d, events) = decode("chat/xai_tool_call.sse", "xai", chunk);
        assert!(d.done());
        assert_eq!((d.response_id.as_deref(), d.model.as_str(), d.stop_reason.as_deref()), (Some("chatcmpl-01XaiToolCall"), "grok-4.7", Some("tool_use")));
        // xAI counts reasoning beside completion tokens (the total says so).
        assert_eq!((d.usage.input_tokens, d.usage.cache_read_tokens, d.usage.output_tokens, d.usage.reasoning_tokens), (1290, 0, 58, 54));
        let items: Vec<&Item> = d.items.iter().map(|(_, i)| i).collect();
        let Item::Reasoning { text, blob: Some(b) } = items[0] else { panic!("{items:?}") };
        assert_eq!(text, "The tagline is in README.md. One search_replace changes it.");
        assert_eq!((b.provider.as_str(), b.wire_api), ("xai", WireApi::ChatCompletions));
        assert_eq!(b.data, json!({"reasoning_content": "The tagline is in README.md. One search_replace changes it."}), "under the vendor's own name");
        assert_eq!(items[1], &Item::AssistantText { text: "Updating the README.".into() });
        let Item::ToolCall { call_id, name, input } = items[2] else { panic!() };
        assert_eq!((call_id.as_str(), name.as_str(), input["file_path"].as_str()), ("call_01SearchReplace", "search_replace", Some("README.md")));
        for (id, _) in &d.items {
            let started = events.iter().position(|e| matches!(e, EngineEvent::ItemStarted { item_id, .. } if item_id == id)).unwrap();
            let ended = events.iter().position(|e| matches!(e, EngineEvent::ItemCompleted { item_id, .. } if item_id == id)).unwrap();
            assert!(started < ended);
        }
    }
}

#[test]
fn r_log_3_openrouter_reasoning_details_are_merged_and_replayed_as_they_came() {
    let (mut d, _) = decode("chat/openrouter_reasoning.sse", "openrouter", 3);
    assert!(!d.done(), "no [DONE]: OpenRouter's stream just ends");
    let last = d.ended().expect("a finish_reason makes what arrived whole");
    assert_eq!(last.iter().filter(|e| matches!(e, EngineEvent::ItemCompleted { .. })).count(), 2);
    // OpenAI's convention: reasoning inside completion tokens.
    assert_eq!((d.usage.input_tokens, d.usage.cache_read_tokens, d.usage.output_tokens, d.usage.reasoning_tokens), (132, 768, 15, 25));
    let Item::Reasoning { text, blob: Some(b) } = &d.items[0].1 else { panic!("{:?}", d.items) };
    assert_eq!(text, "Checking the file.");
    let details = json!([{"type": "reasoning.text", "text": "Checking the file.", "format": "anthropic-claude-v1", "index": 0, "signature": "ErUBCkYIBxgCIkB0cjdy+c2lnbmF0dXJl/Q=="}]);
    assert_eq!(b.data["reasoning_details"], details, "the fragments of one entry are one entry, signature and all");
    assert_eq!(b.data["reasoning"], "Checking the file.");

    let items = vec![
        (Item::user("what language?"), None),
        (d.items[0].1.clone(), Some(0)),
        (d.items[1].1.clone(), Some(0)),
        (Item::user("thanks"), None),
    ];
    let req = ModelRequest { model: "anthropic/claude-sonnet-4.6".into(), system: "s".into(), history: history(&items), effort: Some(Effort::Low), ..ModelRequest::default() };
    let body = request_body(&req, "openrouter");
    let m = &body["messages"];
    assert_eq!(m[0], json!({"role": "system", "content": "s"}));
    assert_eq!(m[2]["role"], "assistant");
    assert_eq!(m[2]["content"], "It is written in Rust.");
    assert_eq!(m[2]["reasoning_details"], details, "sent back on the message it came with, unmodified");
    assert_eq!(body["reasoning"], json!({"effort": "low"}), "OpenRouter's effort, normalized under reasoning");
    assert!(body.get("reasoning_effort").is_none());
    // To another Chat Completions provider the blob is foreign: no vendor
    // fields, the reasoning downgraded to framed text (R-SWITCH-1).
    let body = request_body(&req, "xai");
    assert!(body["messages"][2].get("reasoning_details").is_none());
    assert!(body["messages"][2]["content"].as_str().unwrap().starts_with("<reasoning from an earlier model>\nChecking the file.\n</reasoning>\n\nIt is written in Rust."));
    assert_eq!(body["reasoning_effort"], "low");
}

#[test]
fn r_prov_3_the_history_renders_as_one_prefix_and_every_call_is_answered() {
    let (d, _) = decode("chat/xai_tool_call.sse", "xai", 1 << 20);
    let mut items = vec![(Item::user("reword the README"), None)];
    items.extend(d.items.iter().map(|(_, i)| (i.clone(), Some(0))));
    let req = |items: &[(Item, Option<usize>)]| ModelRequest { model: "grok-4.7".into(), system: "s".into(), history: history(items), session_id: "sess-1".into(), ..ModelRequest::default() };
    // The turn stopped before running the call: it is answered all the same.
    let first = request_body(&req(&items), "xai");
    let m = first["messages"].as_array().unwrap();
    assert_eq!(m.len(), 4);
    assert_eq!(m[2]["role"], "assistant");
    assert_eq!(m[2]["reasoning_content"], "The tagline is in README.md. One search_replace changes it.", "xAI's reasoning goes back to xAI");
    assert_eq!(m[2]["tool_calls"][0]["function"]["name"], "search_replace");
    assert_eq!(m[3], json!({"role": "tool", "tool_call_id": "call_01SearchReplace", "content": "no result was recorded: the turn stopped first"}));
    assert_eq!(first["stream_options"], json!({"include_usage": true}));
    assert!(first.get("prompt_cache_key").is_none(), "xAI's cache key is a header, not a field");
    // With the result, the next call's messages begin with this one's.
    items.push((Item::ToolResult { call_id: "call_01SearchReplace".into(), output: "edited".into(), is_error: false }, None));
    let second = request_body(&req(&items), "xai");
    let n = second["messages"].as_array().unwrap();
    assert_eq!(m[..3], n[..3], "the prefix never moves");
    assert_eq!(n[3], json!({"role": "tool", "tool_call_id": "call_01SearchReplace", "content": "edited"}));
    // OpenAI's Chat Completions takes the session as prompt_cache_key.
    assert_eq!(request_body(&req(&items), "openai")["prompt_cache_key"], "sess-1");
}

#[test]
fn r_prov_1_a_reused_tool_call_index_with_a_new_id_is_a_new_call() {
    // Some servers stream every call at index 0, one after another, and some
    // send a call's name only after its id.
    let chunk = |tc: serde_json::Value| format!("data: {}\n\n", json!({"id": "c", "model": "m", "choices": [{"index": 0, "delta": {"tool_calls": [tc]}, "finish_reason": null}]}));
    let raw = [
        chunk(json!({"index": 0, "id": "call_a", "type": "function", "function": {"name": "read", "arguments": "{\"path\":"}})),
        chunk(json!({"index": 0, "function": {"arguments": "\"a.txt\"}"}})),
        chunk(json!({"index": 0, "id": "call_b", "type": "function", "function": {"arguments": ""}})),
        chunk(json!({"index": 0, "function": {"name": "glob", "arguments": "{\"pattern\":\"*.rs\"}"}})),
        "data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\ndata: [DONE]\n\n".to_string(),
    ]
    .concat();
    let mut d = Decoder::new("xai");
    for ev in SseParser::default().push(raw.as_bytes()) {
        d.apply(&ev).unwrap();
    }
    let calls: Vec<&Item> = d.items.iter().map(|(_, i)| i).collect();
    assert_eq!(
        calls,
        [
            &Item::ToolCall { call_id: "call_a".into(), name: "read".into(), input: json!({"path": "a.txt"}) },
            &Item::ToolCall { call_id: "call_b".into(), name: "glob".into(), input: json!({"pattern": "*.rs"}) },
        ]
    );
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
fn an_image_turns_the_prompt_into_parts_and_plain_text_stays_a_string() {
    let body = request_body(&with_images(), "xai");
    let content = &body["messages"][1]["content"];
    assert_eq!(content[1]["text"], "[Image #1]");
    assert_eq!(content[2], json!({ "type": "image_url", "image_url": { "url": "data:image/png;base64,iVBORw0K" } }));
    assert_eq!(content[3]["text"], "[Image #2: the file is gone]");
    let plain = ModelRequest { history: history(&[(Item::user("hi"), None)]), ..ModelRequest::default() };
    assert_eq!(request_body(&plain, "xai")["messages"][1]["content"], "hi");
}
