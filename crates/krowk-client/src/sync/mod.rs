//! The wire and at-rest shapes of a synced session, shared by every client
//! (R-CLIENT-1; engineering/harness.md → Sync): what a viewer's frames and
//! the host's answers, welcome and batches hold, sealed over the relay by
//! `relay_link`, and (`store`) the sealed index and chunks a session's log
//! is kept in, with the reader a device attaching opens them by. The wire
//! shapes are types only, with no IO: the harness's host and viewer, and
//! the phone's viewer later, read and write the same JSON. `join` is the
//! relay's join handshake, `viewer` what a viewer shows, and `commands` the
//! book a viewer keeps of the commands it sends, all without IO.

pub mod commands;
pub mod join;
pub mod store;
pub mod viewer;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::net::IpAddr;

/// A viewer's command to the host, sealed in its frames: `id` is the
/// viewer's own, random per viewer run, which the host's `ack` names back
/// (R-LAG-8) and dedups by, so a command sent again after a reconnect —
/// its ack lost — runs once.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Remote {
    pub id: String,
    pub command: crate::protocol::Command,
}

/// What a viewer's frame holds.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ViewerFrame {
    Command(Remote),
    /// The logged events after `after` (the last this viewer holds; none
    /// for all the host has), answered in this viewer's routed chain: what
    /// a viewer asks after every welcome and on any gap in the stream, so a
    /// batch the relay dropped, a resync or a new stream loses nothing.
    #[serde(rename_all = "camelCase")]
    CatchUp { after: Option<String> },
    /// Move the session to `device`, the viewer's own (R-HAND-1): the host
    /// checkpoints, ships the bundle as a transport artifact and hands its
    /// lease over, answering `Answer::Handoff` under `id`.
    #[serde(rename_all = "camelCase")]
    Handoff { id: String, device: String },
}

/// What the host routes back to one viewer.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Answer {
    /// The command was taken (`error` none) or refused, by the viewer's id.
    Ack { id: String, error: Option<String> },
    /// Logged events a viewer asked for, in order; `more` when another
    /// page follows.
    CatchUp { events: Vec<crate::protocol::LogEvent>, more: bool },
    /// The handoff `id` asked for: done, with what the new host needs, or
    /// refused, the session staying here.
    #[serde(rename_all = "camelCase")]
    Handoff { id: String, handed: Option<Handed>, error: Option<String> },
}

/// What a host hands the device taking its session over, inside the E2E
/// link and nowhere else: the transport artifact and the key it is sealed
/// under, and the lease the registry minted for the new holder.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Handed {
    pub transport: String,
    pub key: String,
    pub token: String,
    pub fence: u64,
    pub ticket: String,
    /// The device that handed it over.
    pub from_device: String,
}

/// The body of the host's welcome to one viewer.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Welcome {
    /// The log's head as the host sealed it: what the viewer's reading of
    /// the chunks must reach (the prefix check).
    pub head: Option<store::Head>,
    /// Approvals waiting for an answer, sent again to each viewer that
    /// arrives (R-PERM-2).
    pub approvals: Vec<crate::protocol::ApprovalRequest>,
    /// Where else the viewer may reach the host directly (R-NET-1): sealed
    /// in the welcome under the session key and bound to this connection's
    /// challenge, so only the host could have sent it.
    #[serde(default)]
    pub candidates: Vec<Candidate>,
}

/// A batch's body: the frames the host sent in one display frame.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Batch {
    /// Read one by one: a line this viewer does not know — a frame a newer
    /// host sends — is skipped, and the rest of the batch still applies.
    #[serde(deserialize_with = "known_lines")]
    pub lines: Vec<crate::protocol::StreamLine>,
    /// The log's head once the chunks written so far are in.
    pub head: Option<store::Head>,
}

/// The lines of a batch this krowk can read, in order, each decoded alone.
fn known_lines<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<crate::protocol::StreamLine>, D::Error> {
    let raw = Vec::<Value>::deserialize(d)?;
    Ok(raw.into_iter().filter_map(|l| serde_json::from_value(l).ok()).collect())
}

/// One address a viewer may dial the host at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candidate {
    pub url: String,
    /// `tailscale` (the tailnet address), `magicdns` or `lan`.
    pub via: String,
}

impl Candidate {
    /// How a path over it is named on screen.
    pub fn path(&self) -> &'static str {
        if self.on_lan() { "direct over LAN" } else { "direct over Tailscale" }
    }

    /// An address on the LAN: the host's own, or one mDNS answered with.
    pub fn on_lan(&self) -> bool {
        matches!(self.via.as_str(), "lan" | "mdns")
    }
}

/// A machine's tailnet node, as its host keeps it in a session's sealed
/// index for `krowk hosts` to find on the tailnet.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tailnet {
    /// Tailscale's name for it.
    pub name: String,
    /// Its MagicDNS name, without the root's dot; empty without MagicDNS.
    #[serde(default)]
    pub dns_name: String,
    pub ips: Vec<IpAddr>,
}

/// A session id's sixteen bytes; zero for none, or one that is no UUID.
pub fn uuid(id: &str) -> [u8; 16] {
    let hex: Vec<u8> = id.bytes().filter(|b| *b != b'-').collect();
    let mut out = [0u8; 16];
    if hex.len() != 32 {
        return out;
    }
    for (i, pair) in hex.chunks(2).enumerate() {
        match std::str::from_utf8(pair).ok().and_then(|p| u8::from_str_radix(p, 16).ok()) {
            Some(b) => out[i] = b,
            None => return [0u8; 16],
        }
    }
    out
}
