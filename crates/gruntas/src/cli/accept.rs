//! `krowk machines accept on|off` (R-ALLOC-12): whether this machine takes
//! tasks another of the person's machines hands over. Stored as
//! `sync.accept` in config.json; on is the default, so `on` removes the key
//! and `off` writes `false`. The announcement and the `run` command's
//! admission read it. Sessions started here with `krowk sync new --host`
//! are not affected.

use super::Ctx;
use crate::output::Format;
use gruntas_api::{fail, Error};
use serde_json::{json, Value};

pub(super) fn accept(ctx: &mut Ctx, args: &[String]) -> Result<(), Error> {
    let on = match args {
        [a] if a == "on" => true,
        [a] if a == "off" => false,
        _ => return Err(fail("bad_argument", "`krowk machines accept` takes `on` or `off`")),
    };
    let path = super::providers::config_path()?;
    crate::config::edit(&path, |raw| {
        let mut sync = match raw.remove("sync") {
            Some(Value::Object(m)) => m,
            _ => serde_json::Map::new(),
        };
        if on {
            sync.remove("accept");
        } else {
            sync.insert("accept".into(), Value::Bool(false));
        }
        if !sync.is_empty() {
            raw.insert("sync".into(), Value::Object(sync));
        }
    })
    .map_err(|e| fail("config_unwritable", format!("could not write {}: {e}", path.display())))?;
    let summary = if on {
        "This machine takes work from your other machines in trusted repositories."
    } else {
        "This machine no longer takes work from your other machines. Sessions you start here are not affected; turn it on again with krowk machines accept on."
    };
    if ctx.format == Format::Human {
        let _ = writeln!(ctx.io.stdout, "{summary}");
        return Ok(());
    }
    super::sessions::emit_data(ctx, json!({ "accepting": on }), summary.into())
}
