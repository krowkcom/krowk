//! What gruntas-mcp's steering tools need of the command line beyond
//! `agents::ask_holder` (god agent D5): this machine's name, which a steer's
//! origin carries.

use super::sync::printable;
use super::synced::keys;

/// This machine's name on the verified device list; none without sync set
/// up here (`ask_holder` then says how to set it up).
pub fn this_machine() -> Option<String> {
    super::host::own_ctx(&super::flags::Flags::default(), |ctx| {
        let k = keys(ctx).ok()?;
        k.chain.devices().iter().find(|d| d.id() == k.device).map(|d| printable(&d.name))
    })
}
