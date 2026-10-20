//! A viewer's commands, without IO (R-CLIENT-1; engineering/harness.md →
//! Sync): each gets an id of the viewer's own, shows at once as sent or
//! queued (R-LAG-8), goes again after a reconnect until the host acks it,
//! and runs once because the host dedups by that id. With no host a command
//! waits here, and goes, in order, at the next welcome (R-HAND-4).
//!
//! **Ids** are `<device>-<run id>-<counter>` (R-IOS-5's intent, not its
//! UUIDv7): the run id is random per viewer run, so a restarted viewer's
//! ids never read as repeats of an earlier run's at the host.
//!
//! The harness's tokio viewer and the phone's keep the socket and seal
//! what this hands them; the bookkeeping is the same for both.

use super::viewer::Update;
use super::{Handed, Remote, ViewerFrame};
use crate::e2e::{self, DeviceId};
use crate::protocol::Command;
use std::collections::VecDeque;

/// What a viewer has sent the host and not had answered, and what waits
/// for a host to send it to.
#[derive(Debug)]
pub struct CommandBook {
    device: DeviceId,
    run_id: String,
    next_id: u64,
    /// Commands typed with no host to take them, in the order typed.
    queued: VecDeque<Remote>,
    /// Commands sent and not yet acked, in the order first sent: resent in
    /// that order after a welcome, so two prompts never run swapped.
    unacked: VecDeque<Remote>,
    /// A handoff asked and not yet answered, by its id: asked again of the
    /// next host to welcome this viewer.
    handoff: Option<String>,
}

impl CommandBook {
    /// A book for one viewer run of `device`, with a fresh random run id.
    pub fn new(device: DeviceId) -> CommandBook {
        CommandBook { device, run_id: e2e::hex(&e2e::random::<8>()), next_id: 0, queued: VecDeque::new(), unacked: VecDeque::new(), handoff: None }
    }

    /// The next id: `<device>-<run id>-<n>`, `n` counting from 1. Commands
    /// and handoffs share the counter.
    pub fn mint(&mut self) -> String {
        self.next_id += 1;
        format!("{}-{}-{}", self.device, self.run_id, self.next_id)
    }

    /// A command of the person's; `ready` when a host has welcomed this
    /// connection. The `Remote` to seal and send now (`Some`, unacked from
    /// here) or `None` (queued for the next welcome), and the
    /// `Update::Sent` to show either way.
    pub fn command(&mut self, command: Command, ready: bool) -> (Option<Remote>, Update) {
        let r = Remote { id: self.mint(), command };
        let shown = Update::Sent { id: r.id.clone(), queued: !ready };
        if ready {
            self.unacked.push_back(r.clone());
            (Some(r), shown)
        } else {
            self.queued.push_back(r);
            (None, shown)
        }
    }

    /// A host welcomed this viewer: every unacked command again, in the
    /// order first sent, then every queued one, in the order typed. All of
    /// them are unacked from here on.
    pub fn welcomed(&mut self) -> Vec<Remote> {
        self.unacked.extend(self.queued.drain(..));
        self.unacked.iter().cloned().collect()
    }

    /// The host's ack: the `Update::Acked` to show. An id this book does
    /// not hold still shows — the host's word.
    pub fn acked(&mut self, id: String, error: Option<String>) -> Update {
        self.unacked.retain(|r| r.id != id);
        Update::Acked { id, error }
    }

    /// Ask the host to hand the session here (R-HAND-1): the frame to send
    /// now when `ready`, else it goes at the next welcome. A second ask
    /// replaces the first.
    pub fn ask_handoff(&mut self, ready: bool) -> Option<ViewerFrame> {
        self.handoff = Some(self.mint());
        if ready { self.handoff_frame() } else { None }
    }

    /// The pending handoff's frame, to send after `welcomed()`'s commands.
    pub fn handoff_frame(&self) -> Option<ViewerFrame> {
        self.handoff.clone().map(|id| ViewerFrame::Handoff { id, device: self.device.to_string() })
    }

    /// The host's answer to a handoff: the `Update::Handoff` to show when
    /// it answers the pending one, which clears it; `None` for any other
    /// id. A refusal that gives no reason reads "the host refused the
    /// handoff".
    pub fn handoff_answered(&mut self, id: &str, handed: Option<Handed>, error: Option<String>) -> Option<Update> {
        if self.handoff.as_deref() != Some(id) {
            return None;
        }
        self.handoff = None;
        Some(Update::Handoff(handed.ok_or_else(|| error.unwrap_or_else(|| "the host refused the handoff".into()))))
    }

    /// Commands sent and not yet acked.
    pub fn unacked(&self) -> usize {
        self.unacked.len()
    }

    /// Commands waiting for a host.
    pub fn queued(&self) -> usize {
        self.queued.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEVICE: DeviceId = DeviceId([1; 16]);

    /// A command told apart by `n`.
    fn stop(n: usize) -> Command {
        Command::Interrupt { session_id: n.to_string() }
    }

    fn of(r: &Remote) -> String {
        match &r.command {
            Command::Interrupt { session_id } => session_id.clone(),
            c => panic!("not a test command: {c:?}"),
        }
    }

    fn sent(u: &Update) -> (String, bool) {
        match u {
            Update::Sent { id, queued } => (id.clone(), *queued),
            u => panic!("not Sent: {u:?}"),
        }
    }

    /// R-LAG-8: a command shows at once — sent, to be sealed and sent now,
    /// with a host that welcomed this viewer; queued, held here, with none.
    #[test]
    fn r_lag_8_a_command_shows_at_once_sent_or_queued() {
        let mut b = CommandBook::new(DEVICE);
        let (now, shown) = b.command(stop(1), true);
        let now = now.expect("sent now when a host welcomed this viewer");
        assert_eq!(sent(&shown), (now.id.clone(), false));
        assert_eq!((b.unacked(), b.queued()), (1, 0));

        let (later, shown) = b.command(stop(2), false);
        assert!(later.is_none(), "nothing to send with no host");
        let (id, queued) = sent(&shown);
        assert!(queued);
        assert_ne!(id, now.id);
        assert_eq!((b.unacked(), b.queued()), (1, 1));

        // Settled by the host's ack, by the same id it was shown under (what
        // `krowk sync attach` waits on); an id the book never held shows too.
        assert!(matches!(b.acked(now.id.clone(), None), Update::Acked { id, error: None } if id == now.id));
        assert_eq!(b.unacked(), 0);
        assert!(matches!(b.acked("elsewhere-1".into(), Some("no".into())), Update::Acked { id, error: Some(_) } if id == "elsewhere-1"));
    }

    /// R-HAND-4: what was typed with no host goes at the welcome, in the
    /// order typed, after anything sent before and never acked; and each is
    /// unacked from then, so the next welcome sends it again until acked.
    #[test]
    fn r_hand_4_queued_commands_go_in_order_at_the_welcome() {
        let mut b = CommandBook::new(DEVICE);
        let (first, _) = b.command(stop(1), true);
        for n in 2..=4 {
            assert!(b.command(stop(n), false).0.is_none());
        }
        let sent: Vec<String> = b.welcomed().iter().map(of).collect();
        assert_eq!(sent, ["1", "2", "3", "4"]);
        assert_eq!((b.unacked(), b.queued()), (4, 0));

        b.acked(first.unwrap().id, None);
        let again: Vec<String> = b.welcomed().iter().map(of).collect();
        assert_eq!(again, ["2", "3", "4"], "only what is still unacked, once each");
        assert!(b.welcomed().len() == 3, "a welcome does not duplicate");
    }

    /// R-IOS-5, R-LAG-8: twelve commands sent and none acked come back at a
    /// welcome in the order first sent — `…-10` after `…-9`, not before it.
    #[test]
    fn r_ios_5_unacked_commands_resend_in_first_sent_order_past_ten() {
        let mut b = CommandBook::new(DEVICE);
        for n in 1..=12 {
            assert!(b.command(stop(n), true).0.is_some());
        }
        let sent: Vec<String> = b.welcomed().iter().map(of).collect();
        let want: Vec<String> = (1..=12).map(|n: usize| n.to_string()).collect();
        assert_eq!(sent, want);
        let ids: Vec<u64> = b.welcomed().iter().map(|r| r.id.rsplit('-').next().unwrap().parse().unwrap()).collect();
        assert_eq!(ids, (1..=12).collect::<Vec<u64>>());
    }

    /// R-IOS-5: an id is the device, a random run id and a counter from 1
    /// that commands and handoffs share; another run of the same device —
    /// a restarted viewer — never mints one this run did, so the host
    /// never takes it for a repeat.
    #[test]
    fn r_ios_5_ids_are_device_run_counter_and_two_books_never_share_one() {
        let mut a = CommandBook::new(DEVICE);
        let mut b = CommandBook::new(DEVICE);
        let first = a.mint();
        let parts: Vec<&str> = first.split('-').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0], DEVICE.to_string());
        assert_eq!(parts[1].len(), 16, "eight random bytes, in hex");
        assert_eq!(parts[2], "1");
        let (r, _) = a.command(stop(1), true);
        assert_eq!(r.unwrap().id, format!("{}-{}-2", parts[0], parts[1]));
        let Some(ViewerFrame::Handoff { id, device }) = a.ask_handoff(true) else { panic!("a handoff frame") };
        assert_eq!((id, device), (format!("{}-{}-3", parts[0], parts[1]), DEVICE.to_string()));

        let mine: Vec<String> = (0..3).map(|_| a.mint()).collect();
        let theirs: Vec<String> = (0..6).map(|_| b.mint()).collect();
        assert!(mine.iter().all(|m| !theirs.contains(m)), "{mine:?} {theirs:?}");
    }

    /// R-HAND-1, R-HAND-4: a handoff asked with no host is sent at the
    /// welcome, after the commands; only an answer under its own id
    /// settles it, and until then each welcome asks it again.
    #[test]
    fn a_handoff_asked_before_any_welcome_goes_after_the_commands_and_only_its_answer_clears_it() {
        let mut b = CommandBook::new(DEVICE);
        assert!(b.command(stop(1), false).0.is_none());
        assert!(b.ask_handoff(false).is_none(), "no frame with no host");
        assert!(b.command(stop(2), false).0.is_none());

        let commands: Vec<String> = b.welcomed().iter().map(of).collect();
        assert_eq!(commands, ["1", "2"]);
        let Some(ViewerFrame::Handoff { id, device }) = b.handoff_frame() else { panic!("the handoff goes after the commands") };
        assert_eq!(device, DEVICE.to_string());

        assert!(b.handoff_answered("someone-else-9", None, Some("no".into())).is_none());
        assert!(b.handoff_frame().is_some(), "another id's answer leaves it pending");

        match b.handoff_answered(&id, None, None) {
            Some(Update::Handoff(Err(e))) => assert_eq!(e, "the host refused the handoff"),
            u => panic!("a refusal: {u:?}"),
        }
        assert!(b.handoff_frame().is_none());
        assert!(b.handoff_answered(&id, None, None).is_none(), "answered once");

        let id = match b.ask_handoff(true) {
            Some(ViewerFrame::Handoff { id, .. }) => id,
            f => panic!("sent now: {f:?}"),
        };
        let handed = Handed { transport: "t".into(), key: "k".into(), token: "tok".into(), fence: 2, ticket: "tk".into(), from_device: "d".into() };
        assert!(matches!(b.handoff_answered(&id, Some(handed.clone()), None), Some(Update::Handoff(Ok(h))) if h == handed));
    }
}
