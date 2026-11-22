//! The protocol's JSON Schema, generated from the Rust types (R-PROTO-2).
//! The copies in `crates/gruntas-harness/schema/` are what clients in other
//! languages are written against; a test regenerates them and fails when a
//! checked-in copy differs, so a type change that forgets the schema cannot
//! pass CI. `make schema` rewrites them.

use crate::instances::InstancesConfig;
use crate::protocol::{ClientFrame, Command, ContextRecord, LogEvent, ServerFrame, StreamLine};
use schemars::JsonSchema;
use serde_json::Value;

/// Every schema file, by name, rendered as it is checked in.
pub fn files() -> Vec<(&'static str, String)> {
    let render = |s: schemars::Schema| serde_json::to_string_pretty(&s).expect("a schema serializes") + "\n";
    vec![
        ("command.schema.json", render(schemars::schema_for!(Command))),
        ("stream-line.schema.json", render(schemars::schema_for!(StreamLine))),
        ("log-event.schema.json", render(schemars::schema_for!(LogEvent))),
        ("context-record.schema.json", render(schemars::schema_for!(ContextRecord))),
        ("instances.schema.json", render(schemars::schema_for!(InstancesConfig))),
        // The host daemon's socket: what wraps a command and a stream line.
        ("client-frame.schema.json", render(schemars::schema_for!(ClientFrame))),
        ("server-frame.schema.json", render(schemars::schema_for!(ServerFrame))),
    ]
}

/// The schema of a tool's input — an MCP meta tool's, a bridge tool's — as
/// the Anthropic API wants it: an object
/// schema, without the `$schema` and `title` noise schemars adds, and
/// tidied, since every byte of it rides on every model call: a nested type
/// is written in place rather than referenced, and an optional field is its
/// type alone — `required` already says it may be left out, so `null` and
/// `default: null` beside it tell the model nothing.
pub(crate) fn input_schema<T: JsonSchema>() -> Value {
    let mut v = schemars::schema_for!(T).to_value();
    let defs = match &mut v {
        Value::Object(m) => {
            m.remove("$schema");
            m.remove("title");
            m.remove("description");
            m.remove("$defs")
        }
        _ => None,
    };
    tidy(&mut v, defs.as_ref());
    v
}

fn tidy(v: &mut Value, defs: Option<&Value>) {
    match v {
        Value::Object(m) => {
            if let Some(Value::String(r)) = m.get("$ref")
                && let Some(def) = r.strip_prefix("#/$defs/").and_then(|name| defs?.get(name))
            {
                let mut def = def.clone();
                if let Value::Object(d) = &mut def {
                    // The field's own description says what it is for.
                    d.remove("description");
                }
                m.remove("$ref");
                if let Value::Object(d) = def {
                    for (k, x) in d {
                        m.entry(k).or_insert(x);
                    }
                }
            }
            if let Some(Value::Array(types)) = m.get("type")
                && types.len() == 2
                && types.iter().any(|t| t == "null")
            {
                let t = types.iter().find(|t| *t != "null").cloned().unwrap_or(Value::Null);
                m.insert("type".into(), t);
                if m.get("default") == Some(&Value::Null) {
                    m.remove("default");
                }
            }
            for x in m.values_mut() {
                tidy(x, defs);
            }
        }
        Value::Array(a) => a.iter_mut().for_each(|x| tidy(x, defs)),
        _ => {}
    }
}
