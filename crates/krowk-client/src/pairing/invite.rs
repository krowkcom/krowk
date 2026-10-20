//! The invite URI: what the paired device's screen shows as a QR code
//! beside the pairing code, for a phone to scan instead of typing it
//! (canon, engineering/devices.md → Adding a device).
//!
//! `krowk://pair?v=1&code=K7QF9M3X&user=<user id>&device=<32 hex>`
//!
//! It carries the code, the one secret, already on the screen, and two
//! public ids: the person's user id and the paired device's id. With them
//! the phone checks that it is signed in as the same person and that the
//! registry's open pairing names the same device before `PairB::start`
//! spends the code, so a mismatch costs no guess. No key, no token, no
//! workspace. The URI goes to the screen only, never to the registry or a
//! log, and the string holding it is wiped on drop.
//!
//! Parsing is strict: the scheme, the host and `v=1` exactly, the four
//! keys each once, in `uri`'s order, and nothing else, and every value
//! already in the form `uri` writes it (no percent-encoding, no case
//! folding, no separators). Anything else is `None`.

use zeroize::Zeroizing;

use super::{CODE_LEN, MAX_USER_ID, PairingCode};
use crate::e2e::DeviceId;

const PREFIX: &str = "krowk://pair?";

/// A scanned invite, for the side that parses it. The side that shows one
/// builds the string with `uri`, from the code `PairA` only lends it.
#[derive(Debug)]
pub struct Invite {
    pub code: PairingCode,
    pub user_id: String,
    pub a_device: DeviceId,
}

impl Invite {
    pub fn to_uri(&self) -> Zeroizing<String> {
        uri(&self.code, &self.user_id, self.a_device)
    }
}

/// The invite URI for `code`, the person `user_id` and the paired device
/// `a_device`. A user id `parse` would refuse (empty, over `MAX_USER_ID`,
/// or not RFC 3986's unreserved characters) gives a URI that does not
/// parse: check with `parse` before showing one built from a value the
/// caller does not control.
pub fn uri(code: &PairingCode, user_id: &str, a_device: DeviceId) -> Zeroizing<String> {
    let mut s = Zeroizing::new(String::with_capacity(PREFIX.len() + 4 + 6 + CODE_LEN + 6 + user_id.len() + 8 + 32));
    s.push_str(PREFIX);
    s.push_str("v=1&code=");
    // ALPHABET is ASCII, so each byte is a char.
    s.extend(code.as_bytes().iter().map(|&b| char::from(b)));
    s.push_str("&user=");
    s.push_str(user_id);
    s.push_str("&device=");
    s.push_str(&a_device.to_string());
    s
}

/// An invite URI, read strictly. `None` for anything `uri` would not have
/// written from a valid code, user id and device id.
pub fn parse(uri: &str) -> Option<Invite> {
    let query = uri.strip_prefix(PREFIX)?;
    let (mut v, mut code, mut user, mut device) = (None, None, None, None);
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=')?;
        let slot = match key {
            "v" => &mut v,
            "code" => &mut code,
            "user" => &mut user,
            "device" => &mut device,
            _ => return None,
        };
        if slot.replace(value).is_some() {
            return None;
        }
    }
    if v? != "1" {
        return None;
    }
    let code = code?;
    if code.len() != CODE_LEN || !code.bytes().all(|b| super::ALPHABET.contains(&b)) {
        return None;
    }
    let code = PairingCode::parse(code)?;
    let user = user?;
    if user.is_empty() || user.len() > MAX_USER_ID || !user.bytes().all(unreserved) {
        return None;
    }
    let device = device?;
    if device.len() != 32 || !device.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return None;
    }
    let a_device = DeviceId::parse(device)?;
    let invite = Invite { code, user_id: user.to_owned(), a_device };
    // The keys in `uri`'s order too: one spelling per invite.
    (*invite.to_uri() == uri).then_some(invite)
}

/// RFC 3986's unreserved characters: the ones a URI carries as they are.
fn unreserved(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~')
}
