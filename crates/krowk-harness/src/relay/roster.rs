//! Who the reference relay trusts: the registry's ticket-signing public
//! keys, and nothing else (canon, engineering/relay.md → Tickets). A device
//! proves what the registry said of it — its signing key, its workspace, the
//! session, the role and, for a host, the lease's fence — by a ticket the
//! registry signed; the relay checks it with these keys, locally, so it
//! holds no roster of devices and asks nobody at a join.
//!
//! ```json
//! {"ticketKeys": {"<16 hex kid>": "<64 hex Ed25519 public key>"}}
//! ```

use krowk_client::relay_ticket;

#[derive(Debug, Clone, Default)]
pub struct Roster {
    pub keys: Vec<([u8; 8], [u8; 32])>,
}

impl Roster {
    /// Reads the JSON above; a key krowk cannot read is refused by name.
    /// Other fields are ignored, so the conformance fixture serves as is.
    pub fn parse(text: &str) -> Result<Roster, String> {
        let v: serde_json::Value = serde_json::from_str(text).map_err(|e| format!("the ticket keys are not JSON: {e}"))?;
        let map = v.get("ticketKeys").and_then(|k| k.as_object()).ok_or("the file names no ticketKeys object: {\"ticketKeys\": {\"<kid>\": \"<key>\"}}")?;
        let keys = map.iter().map(|(kid, key)| relay_ticket::parse_key(kid, key.as_str().unwrap_or_default())).collect::<Result<Vec<_>, _>>()?;
        if keys.is_empty() {
            return Err("ticketKeys names no key, so no device could join".into());
        }
        Ok(Roster { keys })
    }
}
