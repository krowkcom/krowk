//! A host's announcement of itself to the person's other devices
//! (R-HOST-3): its name, OS and krowk version, its instances and their
//! models, the directories it starts sessions in, and the ids of the
//! sessions it runs. The registry carries it, and what the registry lists
//! for a session is content, so the record is sealed to the person's
//! devices (`e2e::seal_to_person`) and bound to the device it is about.
//!
//! Pure code: the endpoint that carries it and the daemon that publishes
//! it are elsewhere.

use crate::device_chain::{refused_in_name, Chain, Kind};
use crate::e2e::{self, DeviceId, Label};
use crate::pairing::{MAX_NAME, MAX_OS};
use crate::protocol::LimitStatus;
use crate::user_key::UserKeys;
use serde::{Deserialize, Serialize};

/// The most instances one record lists.
pub const MAX_INSTANCES: usize = 32;
/// The most models one instance lists, newest first.
pub const MAX_MODELS: usize = 20;
/// The most directories one record lists.
pub const MAX_DIRECTORIES: usize = 64;
/// The most session ids one record lists.
pub const MAX_SESSIONS: usize = 256;
/// The most bytes a record's JSON takes, before padding.
pub const MAX_JSON: usize = 12 * 1024;
/// The JSON is padded with spaces to a multiple of this before it is
/// sealed, so a record's length says little about what it lists.
pub const PAD: usize = 1024;

/// What a host says about itself. Every field past the first release's is
/// `#[serde(default)]`, and unknown fields are ignored, so a newer host's
/// record still opens on an older device.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Announcement {
    /// This device's id, in hex (`DeviceId`'s display form).
    pub device: String,
    /// The device's name, as on the device list. On open it is the list's.
    pub name: String,
    /// Its OS, as on the device list. On open it is the list's.
    pub os: String,
    /// krowk's version: "0.31.0", or "dev".
    pub version: String,
    /// When the daemon started, in milliseconds since the Unix epoch.
    pub online_since_ms: i64,
    #[serde(default)]
    pub instances: Vec<AnnouncedInstance>,
    /// Absolute paths the host will start a session in.
    #[serde(default)]
    pub directories: Vec<String>,
    /// The ids of the sessions the daemon holds now. Never a title.
    #[serde(default)]
    pub sessions: Vec<String>,
}

/// One of the host's instances, and whether it is ready.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AnnouncedInstance {
    /// The instance's name: "anthropic".
    pub name: String,
    /// Its readiness report's kind.
    pub kind: String,
    pub ready: bool,
    /// Its readiness state: "ready", "keyNotSet".
    pub state: String,
    /// The readiness report's fix line, when it is not ready.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
    /// Model ids, newest first.
    #[serde(default)]
    pub models: Vec<String>,
    /// The last limits frame seen for it, if any. One an older device
    /// cannot read (a state added since) reads as none, rather than
    /// refusing the whole record.
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "lenient")]
    pub limit: Option<LimitStatus>,
}

fn lenient<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<LimitStatus>, D::Error> {
    let v = Option::<serde_json::Value>::deserialize(d)?;
    Ok(v.and_then(|v| serde_json::from_value(v).ok()))
}

impl Announcement {
    /// The record's bounds, held on seal (the host trims before it seals)
    /// and on open.
    pub fn check(&self) -> Result<(), String> {
        // First, so nothing the host wrote reaches an error below unchecked.
        // The device list's rule for a name: no control, format or
        // separator characters, so a path cannot be spoofed by a bidi
        // override either.
        if self.strings().any(|s| s.chars().any(refused_in_name)) {
            return Err("the announcement has a control character in it".into());
        }
        if self.name.chars().count() > MAX_NAME {
            return Err(format!("the announcement's name is longer than {MAX_NAME} characters"));
        }
        if self.os.chars().count() > MAX_OS {
            return Err(format!("the announcement's OS is longer than {MAX_OS} characters"));
        }
        if self.instances.len() > MAX_INSTANCES {
            return Err(format!("the announcement lists more than {MAX_INSTANCES} instances"));
        }
        if let Some(i) = self.instances.iter().find(|i| i.models.len() > MAX_MODELS) {
            return Err(format!("the announcement lists more than {MAX_MODELS} models for {}", i.name));
        }
        if self.directories.len() > MAX_DIRECTORIES {
            return Err(format!("the announcement lists more than {MAX_DIRECTORIES} directories"));
        }
        if self.sessions.len() > MAX_SESSIONS {
            return Err(format!("the announcement lists more than {MAX_SESSIONS} sessions"));
        }
        let json = serde_json::to_vec(self).map_err(|e| format!("the announcement does not encode: {e}"))?;
        if json.len() > MAX_JSON {
            return Err(format!("the announcement is {} bytes, more than {MAX_JSON}", json.len()));
        }
        Ok(())
    }

    fn strings(&self) -> impl Iterator<Item = &str> {
        let top = [&self.device, &self.name, &self.os, &self.version].into_iter().map(String::as_str);
        let instances = self.instances.iter().flat_map(|i| {
            let own = [&i.name, &i.kind, &i.state].into_iter().chain(&i.fix).chain(&i.models).chain(i.limit.as_ref().and_then(|l| l.window.as_ref()));
            own.map(String::as_str)
        });
        top.chain(instances).chain(self.directories.iter().map(String::as_str)).chain(self.sessions.iter().map(String::as_str))
    }
}

/// `a` sealed to the person's devices, bound to `me`, this device: the
/// record must be within its bounds and about `me`, and is padded with
/// spaces to a multiple of `PAD` bytes first.
pub fn seal(a: &Announcement, keys: &UserKeys, current_generation: u32, me: &DeviceId) -> Result<Vec<u8>, String> {
    a.check()?;
    if a.device != me.to_string() {
        return Err(format!("the announcement is about device {}, not this one ({me})", a.device));
    }
    let mut json = serde_json::to_vec(a).map_err(|e| format!("the announcement does not encode: {e}"))?;
    json.resize(json.len().div_ceil(PAD) * PAD, b' ');
    e2e::seal_to_person(keys, current_generation, Label::Presence, &me.0, &json).map_err(|e| e.0)
}

/// The announcement `device` sealed, opened. It must come from a device on
/// the verified list `chain` (not the recovery device, which runs nothing),
/// open bound to `device`, be about `device` and be within its bounds. The
/// name and OS returned are the list's: the record's copy is the host's
/// claim, and the list is the word on what a device is called.
pub fn open(sealed: &[u8], device: &DeviceId, keys: &UserKeys, chain: &Chain) -> Result<Announcement, String> {
    let listed = chain.devices().iter().find(|d| d.id() == *device).ok_or_else(|| format!("device {device} is not on your device list"))?;
    if listed.kind == Kind::Recovery {
        return Err(format!("device {device} is your recovery kit, which announces nothing"));
    }
    if sealed.len() > e2e::PERSON_OVERHEAD + MAX_JSON.div_ceil(PAD) * PAD {
        return Err(format!("the announcement from {device} is longer than {MAX_JSON} bytes"));
    }
    // Any device that ever held an older generation could have sealed one
    // under it, a device removed since included.
    let generation = e2e::person_generation(sealed).map_err(|e| e.0)?;
    if generation < chain.generation() {
        return Err(format!("the announcement from {device} is sealed under user key generation {generation}, older than the generation {} your device list names current; it opens once {device} announces itself again", chain.generation()));
    }
    let json = e2e::open_from_person(sealed, Label::Presence, &device.0, keys).map_err(|e| e.0)?;
    // serde's message can quote what the host wrote, so it is escaped.
    let mut a: Announcement = serde_json::from_slice(&json).map_err(|e| format!("the announcement from {device} does not decode: {}", e.to_string().escape_debug()))?;
    a.check()?;
    if a.device != device.to_string() {
        return Err(format!("the announcement from {device} is about another device ({})", a.device));
    }
    a.name = listed.name.clone();
    a.os = listed.os.clone();
    Ok(a)
}
