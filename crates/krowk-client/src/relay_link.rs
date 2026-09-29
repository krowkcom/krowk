//! Sealing a session across the relay (engineering/relay.md → Epochs across
//! the relay, crypto.md → Session content): the host's end and a viewer's
//! end of one channel, as state machines over the envelopes the relay
//! carries. They hold the session key and nothing else of the transport,
//! so the desktop app and the phones link the same rules the terminal does
//! (R-CLIENT-1).
//!
//! Three chains per viewer, each an epoch and a strictly increasing counter:
//!
//! - **The stream**, host → every viewer: the host picks its epoch, the
//!   stream's 16 bytes, which it names to the relay in its join. Batch `n`
//!   of a stream is sealed with counter `n − 1` and carries header `seq` `n`,
//!   so the relay's count and the sealed one advance together, and a viewer
//!   refuses a batch whose header disagrees with its counter.
//! - **The routed chain**, host → one viewer: the epoch is the viewer's own
//!   challenge, 16 random bytes it draws for each connection and sends in its
//!   hello. The host's first routed message, the welcome, announces the
//!   stream and the epoch of the viewer's frames, so the announcement is
//!   bound to this connection's challenge: a recorded welcome opens on no
//!   later connection. A viewer accepts a stream epoch once and keeps its
//!   counter across reconnects, so a recording of an earlier stream, or of
//!   this one's earlier batches, is refused.
//! - **Frames**, viewer → host: the host picks the epoch per viewer link
//!   and announces it in the welcome (receiver-chosen, crypto.md's rule).
//!   The one frame sent before it, the hello, is sealed under an epoch both
//!   ends derive from the viewer's link number (`hello_epoch`) and must be
//!   counter 0. It carries the challenge and nothing a session key
//!   protects: a hello replayed makes the host announce to a challenge
//!   nobody holds, and no command rides in it.
//!
//! Every sealed plaintext starts with one flags byte; bit 0 says the chain
//! ends here. A stream that stops without it was cut short, however cleanly
//! the connection closed (crypto.md → the end of a stream).

use crate::e2e::{self, Direction, Error, Opener, Sealer, SessionKey};
use crate::protocol::frame::{ENC_XCHACHA20_POLY1305, HEADER, KIND_BATCH, KIND_FRAME, KIND_ROUTED, V};
use std::collections::HashMap;

/// The flags byte's bit saying a chain ends with this message.
pub const FINAL: u8 = 1;

fn err(s: impl Into<String>) -> Error {
    Error(s.into())
}

/// The epoch a viewer's hello is sealed under: derived from its link, the
/// one value both ends know before either has spoken.
pub fn hello_epoch(link: u64) -> [u8; 16] {
    e2e::id(b"krowk/relay-hello/v1", &link.to_be_bytes())
}

/// A sealed envelope's header: `enc` 1, the session, no flags.
pub fn header(kind: u8, session: &[u8; 16], seq: u64) -> [u8; HEADER] {
    let mut h = [0u8; HEADER];
    h[..4].copy_from_slice(&[V, kind, 0, ENC_XCHACHA20_POLY1305]);
    h[4..20].copy_from_slice(session);
    h[20..].copy_from_slice(&seq.to_be_bytes());
    h
}

fn split(bytes: &[u8]) -> Result<([u8; HEADER], &[u8]), Error> {
    if bytes.len() < HEADER {
        return Err(err("an envelope shorter than its header"));
    }
    Ok((bytes[..HEADER].try_into().expect("the header"), &bytes[HEADER..]))
}

fn seq_of(h: &[u8; HEADER]) -> u64 {
    u64::from_be_bytes(h[20..].try_into().expect("eight bytes"))
}

fn check(h: &[u8; HEADER], kind: u8, session: &[u8; 16]) -> Result<(), Error> {
    if h[0] != V || h[1] != kind || h[3] != ENC_XCHACHA20_POLY1305 || &h[4..20] != session {
        return Err(err(format!("an envelope of kind {} for another session, kind or encryption, where a sealed kind {kind} of this session belongs", h[1])));
    }
    Ok(())
}

fn seal(sealer: &mut Sealer, h: [u8; HEADER], flags: u8, body: &[u8]) -> Result<Vec<u8>, Error> {
    let plain = [&[flags][..], body].concat();
    Ok([&h[..], &sealer.seal(&h, &plain)?].concat())
}

/// What the host opened from one viewer.
#[derive(Debug, PartialEq)]
pub enum Inbound {
    /// A new viewer said hello: answer with `HostLink::welcome`.
    Hello { body: Vec<u8> },
    Frame { body: Vec<u8>, last: bool },
}

struct Chain {
    routed: Sealer,
    frames: Opener,
    welcomed: bool,
}

/// The host's end: one stream it seals every batch under, and a chain each
/// way per viewer link.
pub struct HostLink {
    key: SessionKey,
    session: [u8; 16],
    stream: [u8; 16],
    batches: Sealer,
    viewers: HashMap<u64, Chain>,
    /// Every challenge this host has opened a routed chain under: a hello
    /// replayed — to the same link after it was forgotten, or to another —
    /// never opens a second chain under an epoch already used.
    challenges: std::collections::HashSet<[u8; 16]>,
}

impl HostLink {
    /// A fresh stream: 16 random bytes, its batches numbered from 1.
    pub fn new(key: &SessionKey, session: [u8; 16]) -> HostLink {
        let stream = e2e::random();
        HostLink { key: key.clone(), session, stream, batches: Sealer::new(key, session, Direction::HostToClient, stream), viewers: HashMap::new(), challenges: Default::default() }
    }

    pub fn stream(&self) -> [u8; 16] {
        self.stream
    }

    /// The header `seq` of the last batch sealed, 0 for none.
    pub fn last_seq(&self) -> u64 {
        self.batches.next_counter()
    }

    /// Whether this stream can carry on after a relay answered `joined`
    /// with `seq`: its next batch is `max(joined.seq, the last sent) + 1`
    /// (relay.md → The stream), and since the sealed counter is the header's
    /// less one, a relay that says it holds more than this host sealed is
    /// one this stream cannot follow — start another.
    pub fn continues_after(&self, joined_seq: u64) -> bool {
        joined_seq <= self.last_seq()
    }

    /// The next batch of the stream, as the envelope to send. `last` ends it.
    pub fn batch(&mut self, body: &[u8], last: bool) -> Result<Vec<u8>, Error> {
        let seq = self.batches.next_counter() + 1;
        seal(&mut self.batches, header(KIND_BATCH, &self.session, seq), if last { FINAL } else { 0 }, body)
    }

    /// Opens what the relay routed from viewer `link` (the routed
    /// envelope's payload: the viewer's own envelope, whole).
    pub fn open(&mut self, link: u64, envelope: &[u8]) -> Result<Inbound, Error> {
        let (h, sealed) = split(envelope)?;
        check(&h, KIND_FRAME, &self.session)?;
        if let Some(chain) = self.viewers.get_mut(&link) {
            let plain = chain.frames.open(&h, sealed)?;
            let (flags, body) = plain.split_first().ok_or_else(|| err("an empty sealed frame"))?;
            return Ok(Inbound::Frame { body: body.to_vec(), last: flags & FINAL != 0 });
        }
        let mut hello = Opener::for_epoch(&self.key, self.session, Direction::ClientToHost, hello_epoch(link));
        let plain = hello.open(&h, sealed)?;
        if hello.last() != Some(0) || plain.len() < 17 {
            return Err(err("a viewer's first frame is its hello: counter 0, its challenge first"));
        }
        let challenge: [u8; 16] = plain[1..17].try_into().expect("sixteen bytes");
        if !self.challenges.insert(challenge) {
            return Err(err("a hello whose challenge this host has answered before: a replay, refused"));
        }
        let chain = Chain { routed: Sealer::new(&self.key, self.session, Direction::HostToClient, challenge), frames: Opener::new(&self.key, self.session, Direction::ClientToHost), welcomed: false };
        self.viewers.insert(link, chain);
        Ok(Inbound::Hello { body: plain[17..].to_vec() })
    }

    /// The answer to a viewer's hello, routed to it alone: the stream it
    /// is to accept and the epoch its frames go under, sealed with its
    /// challenge bound in, then `body`.
    pub fn welcome(&mut self, link: u64, body: &[u8]) -> Result<Vec<u8>, Error> {
        let (stream, session) = (self.stream, self.session);
        let chain = self.viewers.get_mut(&link).ok_or_else(|| err(format!("viewer {link} has not said hello")))?;
        if chain.welcomed {
            return Err(err(format!("viewer {link} was welcomed already")));
        }
        chain.welcomed = true;
        let announce = [&stream[..], &chain.frames.epoch(), body].concat();
        routed(chain, &session, link, 0, &announce)
    }

    /// A message to one viewer alone, in its own chain: an answer to its
    /// command, or the log it asked to be caught up from.
    pub fn to_viewer(&mut self, link: u64, body: &[u8], last: bool) -> Result<Vec<u8>, Error> {
        let session = self.session;
        let chain = self.viewers.get_mut(&link).filter(|c| c.welcomed).ok_or_else(|| err(format!("viewer {link} has not been welcomed")))?;
        routed(chain, &session, link, if last { FINAL } else { 0 }, body)
    }

    /// The links welcomed and still here.
    pub fn viewers(&self) -> Vec<u64> {
        self.viewers.iter().filter(|(_, c)| c.welcomed).map(|(l, _)| *l).collect()
    }

    /// A viewer left: its chains go with it.
    pub fn forget(&mut self, link: u64) {
        self.viewers.remove(&link);
    }

    /// Every link with a chain, welcomed or not.
    pub fn links(&self) -> Vec<u64> {
        self.viewers.keys().copied().collect()
    }
}

fn routed(chain: &mut Chain, session: &[u8; 16], link: u64, flags: u8, body: &[u8]) -> Result<Vec<u8>, Error> {
    let inner = seal(&mut chain.routed, header(KIND_BATCH, session, 0), flags, body)?;
    // The outer header is the relay's to read: kind 5, `seq` the link.
    let mut out = header(KIND_ROUTED, session, link).to_vec();
    out.extend_from_slice(&inner);
    Ok(out)
}

/// What a viewer opened from the host.
#[derive(Debug, PartialEq)]
pub enum Outbound {
    /// The host's answer to this connection's hello: the stream from here on.
    Welcome { stream: [u8; 16], body: Vec<u8> },
    Routed { body: Vec<u8>, last: bool },
}

/// A viewer's end, for one connection; `reconnect` carries the streams it
/// has accepted over to the next.
pub struct ViewerLink {
    key: SessionKey,
    session: [u8; 16],
    challenge: [u8; 16],
    hello: Sealer,
    routed: Opener,
    frames: Option<Sealer>,
    streams: HashMap<[u8; 16], Opener>,
    current: Option<[u8; 16]>,
}

impl ViewerLink {
    /// The viewer joined as `link`: a fresh challenge for this connection.
    pub fn new(key: &SessionKey, session: [u8; 16], link: u64) -> ViewerLink {
        ViewerLink::with_streams(key, session, link, HashMap::new())
    }

    fn with_streams(key: &SessionKey, session: [u8; 16], link: u64, streams: HashMap<[u8; 16], Opener>) -> ViewerLink {
        let challenge: [u8; 16] = e2e::random();
        ViewerLink {
            key: key.clone(),
            session,
            challenge,
            hello: Sealer::new(key, session, Direction::ClientToHost, hello_epoch(link)),
            routed: Opener::for_epoch(key, session, Direction::HostToClient, challenge),
            frames: None,
            streams,
            current: None,
        }
    }

    /// The next connection, as `link`. Every stream accepted so far stays
    /// accepted, its counter where it was: a batch of it opens only past
    /// what this viewer already opened.
    pub fn reconnect(self, link: u64) -> ViewerLink {
        ViewerLink::with_streams(&self.key, self.session, link, self.streams)
    }

    /// Whether the host has welcomed this connection.
    pub fn welcomed(&self) -> bool {
        self.frames.is_some()
    }

    /// The stream this viewer reads and the header `seq` of the last batch
    /// it opened of it: its resume cursor at the relay (`afterSeq`).
    pub fn cursor(&self) -> Option<([u8; 16], u64)> {
        let s = self.current?;
        Some((s, self.streams.get(&s)?.last().map_or(0, |c| c + 1)))
    }

    /// The first frame of the connection: this connection's challenge, then
    /// `body`, which must hold nothing a session key protects.
    pub fn hello(&mut self, body: &[u8]) -> Result<Vec<u8>, Error> {
        if self.hello.next_counter() > 0 {
            return Err(err("a connection says hello once"));
        }
        let plain = [&self.challenge[..], body].concat();
        seal(&mut self.hello, header(KIND_FRAME, &self.session, 0), 0, &plain)
    }

    /// A frame to the host, once it has welcomed this connection.
    pub fn frame(&mut self, body: &[u8], last: bool) -> Result<Vec<u8>, Error> {
        let session = self.session;
        let s = self.frames.as_mut().ok_or_else(|| err("the host has not answered this connection's hello yet"))?;
        seal(s, header(KIND_FRAME, &session, 0), if last { FINAL } else { 0 }, body)
    }

    /// Opens a routed envelope's payload: the welcome first, then the rest
    /// of this viewer's own chain.
    pub fn open_routed(&mut self, envelope: &[u8]) -> Result<Outbound, Error> {
        let (h, sealed) = split(envelope)?;
        check(&h, KIND_BATCH, &self.session)?;
        let plain = self.routed.open(&h, sealed)?;
        let (flags, body) = plain.split_first().ok_or_else(|| err("an empty routed message"))?;
        if self.frames.is_some() {
            return Ok(Outbound::Routed { body: body.to_vec(), last: flags & FINAL != 0 });
        }
        if self.routed.last() != Some(0) || body.len() < 32 {
            return Err(err("the host's first routed message is its welcome: counter 0, the stream and the frames' epoch first"));
        }
        let stream: [u8; 16] = body[..16].try_into().expect("sixteen bytes");
        let epoch: [u8; 16] = body[16..32].try_into().expect("sixteen bytes");
        if stream == self.challenge || epoch == self.challenge || stream == epoch {
            return Err(err("the welcome reuses this connection's challenge as an epoch, refused"));
        }
        self.frames = Some(Sealer::new(&self.key, self.session, Direction::ClientToHost, epoch));
        // Accepted once: an epoch seen before keeps its counter.
        let (key, session) = (self.key.clone(), self.session);
        self.streams.entry(stream).or_insert_with(|| Opener::for_epoch(&key, session, Direction::HostToClient, stream));
        self.current = Some(stream);
        Ok(Outbound::Welcome { stream, body: body[32..].to_vec() })
    }

    /// Opens a batch of the stream the host announced, answering its body,
    /// whether it ends the stream, and whether it followed the last batch
    /// this viewer opened directly: a gap opens (the relay may drop, and a
    /// viewer catches up from the host), but the caller must know of it.
    /// Before the welcome there is no stream to open it with: the caller
    /// holds it until then.
    pub fn open_batch_gap(&mut self, envelope: &[u8]) -> Result<(Vec<u8>, bool, bool), Error> {
        let stream = self.current.ok_or_else(|| err("a batch before the host announced its stream"))?;
        let before = self.streams.get(&stream).and_then(|o| o.last());
        let (body, last) = self.open_batch(envelope)?;
        let now = self.streams.get(&stream).and_then(|o| o.last()).expect("just opened");
        let gap = match before { Some(b) => now != b + 1, None => false };
        Ok((body, last, gap))
    }

    /// `open_batch_gap` without the gap.
    pub fn open_batch(&mut self, envelope: &[u8]) -> Result<(Vec<u8>, bool), Error> {
        let (h, sealed) = split(envelope)?;
        check(&h, KIND_BATCH, &self.session)?;
        let stream = self.current.ok_or_else(|| err("a batch before the host announced its stream"))?;
        let opener = self.streams.get_mut(&stream).expect("the current stream is accepted");
        let plain = opener.open(&h, sealed)?;
        let counter = opener.last().expect("just opened");
        if seq_of(&h) != counter + 1 {
            return Err(err(format!("batch seq {} carries sealed counter {counter}: the relay's count and the host's disagree, refused", seq_of(&h))));
        }
        let (flags, body) = plain.split_first().ok_or_else(|| err("an empty batch"))?;
        Ok((body.to_vec(), flags & FINAL != 0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION: [u8; 16] = [7; 16];

    fn pair(link: u64) -> (SessionKey, HostLink, ViewerLink) {
        let key = SessionKey::generate();
        let host = HostLink::new(&key, SESSION);
        let viewer = ViewerLink::new(&key, SESSION, link);
        (key, host, viewer)
    }

    /// The relay's part: a viewer's frame reaches the host as a routed
    /// envelope's payload, and the host's routed envelope reaches the viewer
    /// as its payload.
    fn inner(routed: &[u8]) -> &[u8] {
        &routed[HEADER..]
    }

    fn welcomed(link: u64) -> (SessionKey, HostLink, ViewerLink) {
        let (key, mut host, mut viewer) = pair(link);
        let hello = viewer.hello(b"{}").unwrap();
        assert_eq!(host.open(link, &hello).unwrap(), Inbound::Hello { body: b"{}".to_vec() });
        let w = host.welcome(link, b"head").unwrap();
        assert_eq!(seq_of(&w[..HEADER].try_into().unwrap()), link, "the relay routes by the link");
        assert_eq!(viewer.open_routed(inner(&w)).unwrap(), Outbound::Welcome { stream: host.stream(), body: b"head".to_vec() });
        (key, host, viewer)
    }

    /// R-E2E-1: the stream, a routed answer and a frame each open at their
    /// one receiver, and the header seq is the sealed counter plus one.
    #[test]
    fn r_e2e_1_the_three_chains_open_where_they_belong() {
        let (_, mut host, mut viewer) = welcomed(3);
        for n in 1..=3u64 {
            let b = host.batch(format!("batch {n}").as_bytes(), false).unwrap();
            assert_eq!(seq_of(&b[..HEADER].try_into().unwrap()), n);
            assert_eq!(viewer.open_batch(&b).unwrap(), (format!("batch {n}").into_bytes(), false));
        }
        assert_eq!(viewer.cursor(), Some((host.stream(), 3)));
        let f = viewer.frame(b"prompt", false).unwrap();
        assert_eq!(host.open(3, &f).unwrap(), Inbound::Frame { body: b"prompt".to_vec(), last: false });
        let r = host.to_viewer(3, b"ack", false).unwrap();
        assert_eq!(viewer.open_routed(inner(&r)).unwrap(), Outbound::Routed { body: b"ack".to_vec(), last: false });
        let end = host.batch(b"", true).unwrap();
        assert_eq!(viewer.open_batch(&end).unwrap(), (Vec::new(), true), "the stream's end is sealed");
    }

    /// R-E2E-1: a batch replayed, one whose header seq was changed, and a
    /// frame sent back at its sender are all refused.
    #[test]
    fn r_e2e_1_a_replay_a_changed_seq_or_a_reflection_is_refused() {
        let (_, mut host, mut viewer) = welcomed(1);
        let b1 = host.batch(b"one", false).unwrap();
        viewer.open_batch(&b1).unwrap();
        assert!(viewer.open_batch(&b1).is_err(), "a replay");
        let mut b2 = host.batch(b"two", false).unwrap();
        b2[27] = 9;
        assert!(viewer.open_batch(&b2).is_err(), "a changed seq is in the associated data");
        let f = viewer.frame(b"x", false).unwrap();
        assert!(viewer.open_routed(&f).is_err(), "a frame is not the host's");
    }

    /// The welcome is bound to the connection's challenge: a recorded one
    /// opens on no later connection, even on the same link number.
    #[test]
    fn r_e2e_1_a_recorded_welcome_opens_on_no_later_connection() {
        let (key, mut host, mut viewer) = pair(5);
        host.open(5, &viewer.hello(b"").unwrap()).unwrap();
        let w = host.welcome(5, b"").unwrap();
        viewer.open_routed(inner(&w)).unwrap();
        let mut later = ViewerLink::new(&key, SESSION, 5);
        later.hello(b"").unwrap();
        assert!(later.open_routed(inner(&w)).is_err());
        assert!(later.frame(b"prompt", false).is_err(), "nothing is sent before a welcome");
    }

    /// A viewer accepts a stream epoch once: reconnected to the same stream
    /// it carries on past what it opened, so the relay replaying buffered
    /// batches it already saw is refused, and new ones open.
    #[test]
    fn r_e2e_1_a_stream_accepted_once_keeps_its_counter_across_reconnects() {
        let (_, mut host, mut viewer) = welcomed(1);
        let old: Vec<_> = (0..3).map(|n| host.batch(&[n], false).unwrap()).collect();
        for b in &old {
            viewer.open_batch(b).unwrap();
        }
        host.forget(1);
        let mut viewer = viewer.reconnect(2);
        host.open(2, &viewer.hello(b"").unwrap()).unwrap();
        viewer.open_routed(inner(&host.welcome(2, b"").unwrap())).unwrap();
        assert!(viewer.open_batch(&old[2]).is_err(), "a batch already opened");
        assert_eq!(viewer.open_batch(&host.batch(b"new", false).unwrap()).unwrap().0, b"new");
        assert_eq!(viewer.cursor().unwrap().1, 4);
    }

    /// A batch of another stream does not open under this one.
    #[test]
    fn r_e2e_1_another_streams_batch_does_not_open() {
        let (key, _host, mut viewer) = welcomed(1);
        let mut other = HostLink::new(&key, SESSION);
        assert!(viewer.open_batch(&other.batch(b"x", false).unwrap()).is_err());
    }

    /// A hello only opens as counter 0 on its own link, and a frame of a
    /// viewer the host has not heard from is read as a hello and refused.
    #[test]
    fn r_e2e_1_a_hello_is_bound_to_its_link() {
        let (_, mut host, mut viewer) = pair(4);
        let hello = viewer.hello(b"").unwrap();
        assert!(host.open(9, &hello).is_err(), "another link's hello");
        assert!(host.welcome(9, b"").is_err());
        assert!(viewer.hello(b"").is_err(), "once a connection");
    }

    /// A hello replayed after its link was forgotten, or to another link,
    /// opens no second routed chain under the challenge it carries.
    #[test]
    fn r_e2e_1_a_replayed_hello_opens_no_second_chain() {
        let (_, mut host, mut viewer) = pair(4);
        let hello = viewer.hello(b"").unwrap();
        host.open(4, &hello).unwrap();
        host.forget(4);
        assert!(host.open(4, &hello).is_err(), "the same link, forgotten");
        assert!(host.welcome(4, b"").is_err());
    }

    /// A batch that skips one opens, and says so.
    #[test]
    fn r_e2e_1_a_skipped_batch_opens_and_shows_as_a_gap() {
        let (_, mut host, mut viewer) = welcomed(1);
        let b1 = host.batch(b"1", false).unwrap();
        let _b2 = host.batch(b"2", false).unwrap();
        let b3 = host.batch(b"3", false).unwrap();
        assert!(!viewer.open_batch_gap(&b1).unwrap().2);
        assert!(viewer.open_batch_gap(&b3).unwrap().2, "batch 2 was dropped");
    }

    #[test]
    fn a_stream_carries_on_only_after_a_relay_holding_no_more_than_it_sent() {
        let (_, mut host, _) = pair(1);
        host.batch(b"", false).unwrap();
        host.batch(b"", false).unwrap();
        assert!(host.continues_after(0) && host.continues_after(2) && !host.continues_after(3));
    }
}
