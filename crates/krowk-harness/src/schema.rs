//! The protocol's JSON Schema, generated from the Rust types (R-PROTO-2).
//! The copies in `crates/krowk-harness/schema/` are what clients in other
//! languages are written against; a test regenerates them and fails when a
//! checked-in copy differs, so a type change that forgets the schema cannot
//! pass CI. `make schema` rewrites them.

use crate::instances::InstancesConfig;
use crate::protocol::{ClientFrame, Command, ContextRecord, LogEvent, ServerFrame, StreamLine};

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
