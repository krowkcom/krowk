//! A new machine that signed in to the person's account (R-EASE-5, D6): the
//! host daemon on this machine opened a pairing for it, and its code is
//! shown here, in the TUI, as it is in `krowk machines`. A thread asks the
//! daemon now and then — never starting one — and tells the TUI when the
//! offer changes. The question that follows the code, once it is typed on
//! the new machine and its confirmation checked, is answered in `krowk
//! machines`, which asks it on a terminal of its own.

use krowk_harness::protocol::{OfferStage, PairingOffer};
use tokio::sync::mpsc;

/// How often the daemon is asked.
#[cfg(unix)]
const EVERY: std::time::Duration = std::time::Duration::from_secs(3);

/// The line the TUI shows for an offer, none for one that needs no line.
pub fn line(o: &PairingOffer) -> Option<String> {
    let clean = |s: &str| s.chars().filter(|c| !c.is_control()).collect::<String>().trim().to_string();
    match o.stage {
        OfferStage::Waiting => Some(format!("A new machine signed in to your account. Its code: {}", clean(&o.code))),
        OfferStage::Confirm => Some(format!("Add '{}' ({}) to your devices? Answer it in `krowk machines`.", clean(&o.name), clean(&o.os))),
        OfferStage::Added | OfferStage::Failed => Some(clean(&o.note)),
    }
}

/// The lines to show, as the offer changes, until the receiver is dropped.
#[cfg(unix)]
pub fn watch(version: String) -> mpsc::UnboundedReceiver<String> {
    let (tx, rx) = mpsc::unbounded_channel();
    let spawned = std::thread::Builder::new().name("krowk-new-machine".into()).spawn(move || {
        let env = |k: &str| std::env::var(k).unwrap_or_default();
        let mut shown: Option<(OfferStage, String, String)> = None;
        while !tx.is_closed() {
            let offer = krowk_harness::daemon::pairing_offer(&env, &version).ok().flatten();
            let key = offer.as_ref().map(|o| (o.stage, o.pairing.clone(), o.note.clone()));
            if key != shown {
                if let Some(l) = offer.as_ref().and_then(line)
                    && tx.send(l).is_err()
                {
                    return;
                }
                shown = key;
            }
            std::thread::sleep(EVERY);
        }
    });
    drop(spawned);
    rx
}

#[cfg(not(unix))]
pub fn watch(_: String) -> mpsc::UnboundedReceiver<String> {
    mpsc::unbounded_channel().1
}

/// The next line, or never.
pub async fn next(rx: &mut mpsc::UnboundedReceiver<String>) -> String {
    match rx.recv().await {
        Some(l) => l,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offer(stage: OfferStage) -> PairingOffer {
        PairingOffer { pairing: "p".into(), code: "K7QF-9M3X".into(), stage, name: "B\x1b[2J".into(), os: "Linux".into(), note: "Added 'B' (Linux).".into() }
    }

    /// The code is told as the spec words it; the new machine's name only at
    /// the question, and with nothing a terminal would act on.
    #[test]
    fn the_code_and_then_the_question_are_told() {
        assert_eq!(line(&offer(OfferStage::Waiting)).unwrap(), "A new machine signed in to your account. Its code: K7QF-9M3X");
        assert_eq!(line(&offer(OfferStage::Confirm)).unwrap(), "Add 'B[2J' (Linux) to your devices? Answer it in `krowk machines`.");
        assert_eq!(line(&offer(OfferStage::Added)).unwrap(), "Added 'B' (Linux).");
    }
}
