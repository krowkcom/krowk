//! The part vocabulary every source normalises into.

use gruntas_store::Part;
use serde_json::{json, Value};

pub const PART_TEXT: &str = "text";
pub const PART_THINKING: &str = "thinking";
pub const PART_TOOL_CALL: &str = "tool_call";
pub const PART_TOOL_RESULT: &str = "tool_result";
pub const PART_IMAGE: &str = "image";
pub const PART_FILE: &str = "file";
pub const PART_PATCH: &str = "patch";
pub const PART_STEP: &str = "step";
pub const PART_UNKNOWN: &str = "unknown";

pub const PART_TYPES: [&str; 9] =
    [PART_TEXT, PART_THINKING, PART_TOOL_CALL, PART_TOOL_RESULT, PART_IMAGE, PART_FILE, PART_PATCH, PART_STEP, PART_UNKNOWN];

pub fn known_part_type(t: &str) -> bool {
    PART_TYPES.contains(&t)
}

pub fn new_tool_call_part(call_id: &str, name: &str, input: Option<&Value>) -> Part {
    Part {
        kind: PART_TOOL_CALL.into(),
        tool_call_id: call_id.into(),
        data: json!({ "name": name, "input": input.cloned().unwrap_or(Value::Null) }).to_string(),
        ..Part::default()
    }
}

pub fn new_tool_result_part(call_id: &str, output: Option<&Value>, is_error: bool) -> Part {
    Part {
        kind: PART_TOOL_RESULT.into(),
        tool_call_id: call_id.into(),
        data: json!({ "output": output.cloned().unwrap_or(Value::Null), "is_error": is_error }).to_string(),
        ..Part::default()
    }
}

pub fn new_tool_result_text_part(call_id: &str, output: &str, is_error: bool) -> Part {
    new_tool_result_part(call_id, Some(&Value::String(output.into())), is_error)
}

/// A part as its source typed it: kept as it is when krowk knows the type,
/// wrapped as `unknown` with the source's type and raw payload when not.
/// The bool says whether the type was known.
pub fn normalize_part(raw_type: &str, raw: Option<&Value>) -> (Part, bool) {
    if known_part_type(raw_type) {
        let data = raw.map(|v| v.to_string()).unwrap_or_default();
        return (Part { kind: raw_type.into(), data, ..Part::default() }, true);
    }
    let mut data = serde_json::Map::new();
    data.insert("source_type".into(), json!(raw_type));
    if let Some(v) = raw {
        data.insert("raw".into(), v.clone());
    }
    (Part { kind: PART_UNKNOWN.into(), data: Value::Object(data).to_string(), ..Part::default() }, false)
}
