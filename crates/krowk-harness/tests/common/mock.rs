//! Stand-ins for the provider APIs: an HTTP/1.1 server on a loopback port
//! that answers each request with a recorded SSE body (or JSON, or a
//! redirect), and keeps every request it was sent so a test can pin what
//! krowk asked. The Anthropic Messages scripts are here; the OpenAI
//! Responses, Chat Completions and OAuth ones are in `providers.rs`.
//! Standard library only, and a thread per server, so the harness tests,
//! the CLI's tests and the evidence example all run the same one.
#![allow(dead_code)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

/// One request as it arrived.
#[derive(Debug, Clone)]
pub struct Seen {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: serde_json::Value,
    /// The body as sent, for a form.
    pub raw: String,
}

impl Seen {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

/// What the server answers a request with. A 200 is an event stream unless
/// `content_type` says otherwise; anything else is JSON.
pub struct Reply {
    pub status: u16,
    pub body: String,
    pub content_type: Option<&'static str>,
    pub headers: Vec<(String, String)>,
    /// Sent one SSE event at a time with this pause between, rather than
    /// whole: a model typing at a known rate.
    pub pace: Option<std::time::Duration>,
    /// A paced reply stops before its first event holding this, until the
    /// gate opens: a model mid-answer for as long as the test looks.
    pub hold: Option<(&'static str, Gate)>,
}

/// Shut until opened, then open for good.
#[derive(Clone, Default)]
pub struct Gate(Arc<(Mutex<bool>, std::sync::Condvar)>);

impl Gate {
    pub fn open(&self) {
        *self.0.0.lock().unwrap() = true;
        self.0.1.notify_all();
    }

    fn wait(&self) {
        let mut open = self.0.0.lock().unwrap();
        while !*open {
            open = self.0.1.wait(open).unwrap();
        }
    }
}

impl Reply {
    pub fn sse(body: &str) -> Reply {
        Reply::status(200, body)
    }

    pub fn status(status: u16, body: &str) -> Reply {
        Reply { status, body: body.to_string(), content_type: None, headers: Vec::new(), pace: None, hold: None }
    }

    pub fn json(status: u16, body: &serde_json::Value) -> Reply {
        Reply { content_type: Some("application/json"), ..Reply::status(status, &body.to_string()) }
    }

    pub fn redirect(to: &str) -> Reply {
        Reply { headers: vec![("location".into(), to.into())], ..Reply::status(302, "") }
    }

    pub fn paced(body: String, pace: std::time::Duration) -> Reply {
        Reply { pace: Some(pace), ..Reply::status(200, &body) }
    }
}

/// A whole Messages-API stream answering `text`, one `text_delta` per
/// word-sized piece — roughly one token each — so a paced reply streams at
/// a known token rate.
pub fn text_stream(text: &str) -> String {
    let mut out = String::from(
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_01Streamed\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-sonnet-4-6\",\"content\":[],\"stop_reason\":null,\"usage\":{\"input_tokens\":12,\"output_tokens\":1}}}\n\n\
         event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    );
    let mut pieces = 0;
    for piece in text.split_inclusive([' ', '\n']) {
        pieces += 1;
        let delta = serde_json::json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": piece}});
        out.push_str(&format!("event: content_block_delta\ndata: {delta}\n\n"));
    }
    out.push_str("event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n");
    out.push_str(&format!(
        "event: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\",\"stop_sequence\":null}},\"usage\":{{\"output_tokens\":{pieces}}}}}\n\n"
    ));
    out.push_str("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");
    out
}

/// `n` numbered lines of prose, about twelve tokens each: the long answer
/// the scrollback test streams and then looks for, line by line.
pub fn numbered_lines(n: usize) -> String {
    (1..=n).map(|i| format!("line {i:05}: the quick brown fox jumps over the lazy dog again\n")).collect()
}

pub struct Mock {
    pub url: String,
    pub seen: Arc<Mutex<Vec<Seen>>>,
}

/// Serves `answer(request, n)` for the n-th request (from 0), forever.
pub fn serve(answer: impl Fn(&serde_json::Value, usize) -> Reply + Send + 'static) -> Mock {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind a loopback port");
    serve_on(listener, answer)
}

pub fn serve_on(listener: TcpListener, answer: impl Fn(&serde_json::Value, usize) -> Reply + Send + 'static) -> Mock {
    serve_seen_on(listener, move |seen, n| answer(&seen.body, n))
}

/// Serves `answer(request, n)`, handing it the whole request.
pub fn serve_seen(answer: impl Fn(&Seen, usize) -> Reply + Send + 'static) -> Mock {
    serve_seen_on(TcpListener::bind("127.0.0.1:0").expect("bind a loopback port"), answer)
}

pub fn serve_seen_on(listener: TcpListener, answer: impl Fn(&Seen, usize) -> Reply + Send + 'static) -> Mock {
    let url = format!("http://{}", listener.local_addr().unwrap());
    let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
    let log = seen.clone();
    std::thread::spawn(move || {
        for (n, conn) in listener.incoming().enumerate() {
            let Ok(mut conn) = conn else { continue };
            let mut reader = BufReader::new(conn.try_clone().unwrap());
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            let method = line.split_whitespace().next().unwrap_or_default().to_string();
            let path = line.split_whitespace().nth(1).unwrap_or_default().to_string();
            let mut headers = Vec::new();
            let mut length = 0usize;
            loop {
                let mut h = String::new();
                if reader.read_line(&mut h).unwrap_or(0) == 0 || h == "\r\n" {
                    break;
                }
                if let Some((k, v)) = h.trim_end().split_once(':') {
                    if k.eq_ignore_ascii_case("content-length") {
                        length = v.trim().parse().unwrap_or(0);
                    }
                    headers.push((k.trim().to_string(), v.trim().to_string()));
                }
            }
            let mut body = vec![0u8; length];
            let _ = reader.read_exact(&mut body);
            let raw = String::from_utf8_lossy(&body).into_owned();
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
            let seen = Seen { method, path, headers, body, raw };
            let reply = answer(&seen, n);
            log.lock().unwrap().push(seen);
            let kind = reply.content_type.unwrap_or(if reply.status == 200 { "text/event-stream" } else { "application/json" });
            let extra: String = reply.headers.iter().map(|(k, v)| format!("{k}: {v}\r\n")).collect();
            let head = format!("HTTP/1.1 {} X\r\ncontent-type: {kind}\r\ncontent-length: {}\r\n{extra}connection: close\r\n\r\n", reply.status, reply.body.len());
            let _ = conn.write_all(head.as_bytes());
            match reply.pace {
                // Its own thread, so a slow stream does not hold up the next
                // request — a connectivity probe above all.
                Some(pace) => {
                    std::thread::spawn(move || {
                        for event in reply.body.split_inclusive("\n\n") {
                            if let Some((at, gate)) = &reply.hold
                                && event.contains(at)
                            {
                                gate.wait();
                            }
                            if conn.write_all(event.as_bytes()).and_then(|()| conn.flush()).is_err() {
                                return;
                            }
                            std::thread::sleep(pace);
                        }
                    });
                }
                None => {
                    let _ = conn.write_all(reply.body.as_bytes());
                    let _ = conn.flush();
                }
            }
        }
    });
    Mock { url, seen }
}

/// A recorded fixture by name. Compiled in, so every crate that includes
/// this file finds them wherever it builds.
pub fn fixture(name: &str) -> String {
    match name {
        "turn1_tool_use.sse" => include_str!("../fixtures/anthropic/turn1_tool_use.sse"),
        "turn1_answer.sse" => include_str!("../fixtures/anthropic/turn1_answer.sse"),
        "turn2_answer.sse" => include_str!("../fixtures/anthropic/turn2_answer.sse"),
        "redacted_thinking.sse" => include_str!("../fixtures/anthropic/redacted_thinking.sse"),
        "overloaded.sse" => include_str!("../fixtures/anthropic/overloaded.sse"),
        other => panic!("no fixture {other}"),
    }
    .to_string()
}

/// The script the end-to-end runs follow, chosen by what the request holds
/// rather than by its number, so it answers a real client in any order: a
/// conversation whose last message carries a tool result gets the answer,
/// a second prompt gets the follow-up, and a first prompt reads README.md.
pub fn readme_script(body: &serde_json::Value, _n: usize) -> Reply {
    let messages = body["messages"].as_array().cloned().unwrap_or_default();
    let last = messages.last().cloned().unwrap_or_default();
    let has_result = last["content"].as_array().is_some_and(|c| c.iter().any(|b| b["type"] == "tool_result"));
    let prompts = messages.iter().filter(|m| m["role"] == "user" && m["content"].as_array().is_some_and(|c| c.iter().any(|b| b["type"] == "text"))).count();
    if has_result {
        Reply::sse(&fixture("turn1_answer.sse"))
    } else if prompts > 1 {
        Reply::sse(&fixture("turn2_answer.sse"))
    } else {
        Reply::sse(&fixture("turn1_tool_use.sse"))
    }
}

/// A response that calls one tool, streamed the way the API streams one:
/// the input as JSON deltas, split mid-string.
pub fn tool_use(id: &str, name: &str, input: &serde_json::Value) -> String {
    let json = input.to_string();
    let cut = json.char_indices().map(|(i, _)| i).nth(json.chars().count() / 2).unwrap_or(0);
    let delta = |part: &str| {
        format!(
            "event: content_block_delta\ndata: {}\n\n",
            serde_json::json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": part}})
        )
    };
    format!(
        "event: message_start\ndata: {}\n\nevent: content_block_start\ndata: {}\n\n{}{}event: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":0}}\n\nevent: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"tool_use\",\"stop_sequence\":null}},\"usage\":{{\"output_tokens\":40}}}}\n\nevent: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n",
        serde_json::json!({"type": "message_start", "message": {"id": format!("msg_{id}"), "type": "message", "role": "assistant", "model": "claude-sonnet-4-6", "content": [], "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 20, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0, "output_tokens": 1}}}),
        serde_json::json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": id, "name": name, "input": {}}}),
        delta(&json[..cut]),
        delta(&json[cut..]),
    )
}

/// The edit the edit script makes to README.md, in each edit tool's format.
pub fn readme_edit(tool: &str) -> serde_json::Value {
    match tool {
        "str_replace" => serde_json::json!({"path": "README.md", "old_str": "Permalinks for agent output.", "new_str": "Permalinks for everything agents make."}),
        "search_replace" => serde_json::json!({"file_path": "README.md", "old_string": "Permalinks for agent output.", "new_string": "Permalinks for everything agents make."}),
        "apply_patch" => serde_json::json!({"input": "*** Begin Patch\n*** Update File: README.md\n@@ # krowk\n \n-Permalinks for agent output.\n+Permalinks for everything agents make.\n*** End Patch\n"}),
        other => panic!("no edit tool {other}"),
    }
}

/// A model that edits README.md with whichever edit tool the request
/// offers, then answers once it has the result.
pub fn edit_script(body: &serde_json::Value, _n: usize) -> Reply {
    let messages = body["messages"].as_array().cloned().unwrap_or_default();
    let has_result = messages.last().and_then(|m| m["content"].as_array().cloned()).is_some_and(|c| c.iter().any(|b| b["type"] == "tool_result"));
    if has_result {
        return Reply::sse(&fixture("turn2_answer.sse"));
    }
    let tools: Vec<String> = body["tools"].as_array().into_iter().flatten().filter_map(|t| t["name"].as_str().map(String::from)).collect();
    let tool = ["str_replace", "apply_patch", "search_replace"].into_iter().find(|t| tools.iter().any(|n| n == t)).expect("an edit tool is offered");
    Reply::sse(&tool_use("toolu_01EditReadme", tool, &readme_edit(tool)))
}
