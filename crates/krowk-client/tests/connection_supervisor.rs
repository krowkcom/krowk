//! ConnectionSupervisorTests (ticket 86; R-IOS-2, R-OFF-2, R-LAG-9): the
//! phone's connection lifecycle, pure, on a fake clock — the backoff and
//! its cut, when it starts over, the wake rules, the first path callback
//! left to the caller, the long background and the suspend.

use krowk_client::sync::supervisor::{jittered, ConnectionSupervisor, Link, Wake, BACKOFF_BASE, BACKOFF_CAP, LONG_BACKGROUND, PROBE, STAYED_UP};
use krowk_client::sync::DEAD;

const T0: u64 = 1_000_000;

fn ms(d: std::time::Duration) -> u64 {
    d.as_millis() as u64
}

/// The full waits, uncut (random 0), of `n` losses in a row.
fn waits(s: &mut ConnectionSupervisor, now: u64, n: usize) -> Vec<u64> {
    (0..n).map(|_| s.lost(now, 0).expect("in the foreground") - now).collect()
}

#[test]
fn r_off_2_the_backoff_starts_at_250_ms_doubles_and_stops_at_30_s() {
    let mut s = ConnectionSupervisor::new(T0);
    assert_eq!(ms(BACKOFF_BASE), 250);
    assert_eq!(ms(BACKOFF_CAP), 30_000);
    assert_eq!(waits(&mut s, T0, 10), [250, 500, 1000, 2000, 4000, 8000, 16_000, 30_000, 30_000, 30_000]);
}

#[test]
fn r_off_2_each_wait_is_cut_by_up_to_half_at_random() {
    assert_eq!(jittered(1000, 0), 1000);
    assert_eq!(jittered(1000, 255), 500);
    for r in 0..=255u8 {
        let w = jittered(30_000, r);
        assert!((15_000..=30_000).contains(&w), "{r}: {w}");
        assert!(jittered(30_000, r.saturating_add(1)) <= w, "more random, never a longer wait");
    }
    let mut s = ConnectionSupervisor::new(T0);
    for (i, full) in [250u64, 500, 1000, 2000].into_iter().enumerate() {
        let at = s.lost(T0, (i * 80) as u8).unwrap();
        assert!((T0 + full / 2..=T0 + full).contains(&at), "{full}: waited {}", at - T0);
    }
}

#[test]
fn r_off_2_the_backoff_starts_over_only_after_a_connection_stayed_up_30_s() {
    let mut s = ConnectionSupervisor::new(T0);
    waits(&mut s, T0, 4);
    // A link that flaps: joined, lost a moment later, keeps backing off.
    s.joined(T0);
    assert_eq!(s.lost(T0 + 1000, 0), Some(T0 + 1000 + 4000));
    s.joined(T0 + 2000);
    assert_eq!(s.lost(T0 + 2000 + ms(STAYED_UP) - 1, 0).map(|a| a - (T0 + 2000 + ms(STAYED_UP) - 1)), Some(8000));
    // Up 30 s: the next loss starts over.
    s.joined(T0 + 100_000);
    let at = T0 + 100_000 + ms(STAYED_UP);
    assert_eq!(s.lost(at, 0), Some(at + 250));
    assert_eq!(s.lost(at, 0), Some(at + 500), "and doubles again from there");
}

#[test]
fn r_off_2_a_connection_replaced_after_30_s_up_starts_the_backoff_over_too() {
    let mut s = ConnectionSupervisor::new(T0);
    waits(&mut s, T0, 8);
    // Up 31 s, then replaced (a long background, a silent link, a probe
    // unanswered): its replacement failing waits the first wait again.
    s.joined(T0);
    s.replaced(T0 + 31_000);
    assert_eq!(s.lost(T0 + 31_100, 255), Some(T0 + 31_100 + 125));
    // Up less: the backoff goes on from where it was.
    let mut s = ConnectionSupervisor::new(T0);
    waits(&mut s, T0, 3);
    s.joined(T0);
    s.replaced(T0 + 1000);
    assert_eq!(s.lost(T0 + 1000, 0), Some(T0 + 1000 + 2000));
}

#[test]
fn r_off_2_a_wake_with_no_connection_joins_now_with_no_backoff() {
    let mut s = ConnectionSupervisor::new(T0);
    waits(&mut s, T0, 6);
    assert_eq!(s.wake(T0 + 10, Link::Down), Wake::Connect);
    // The backoff itself stays where it was: the next loss goes on from it.
    assert_eq!(s.lost(T0 + 20, 0), Some(T0 + 20 + 16_000));
    assert_eq!(s.wake(T0 + 30, Link::Joining), Wake::Nothing, "a connection being made is left to finish");
}

#[test]
fn r_lag_9_a_wake_probes_a_live_connection_and_replaces_one_that_stays_silent() {
    let mut s = ConnectionSupervisor::new(T0);
    s.joined(T0);
    s.heard(T0 + 1000);
    assert_eq!(s.wake(T0 + 2000, Link::Up), Wake::Probe);
    assert_eq!(s.next_wake(), Some(T0 + 2000 + ms(PROBE)));
    assert_eq!(s.wake(T0 + 2500, Link::Up), Wake::Nothing, "one probe out at a time");
    assert!(!s.probe_due(T0 + 2000 + ms(PROBE) - 1));
    assert!(s.probe_due(T0 + 2000 + ms(PROBE)), "nothing back in 5 s: replace");
    let mut taken = s.clone();
    assert!(!taken.take_probe_due(T0 + 2000 + ms(PROBE) - 1));
    assert!(taken.take_probe_due(T0 + 2000 + ms(PROBE)));
    assert!(!taken.take_probe_due(T0 + 60_000) && taken.next_wake().is_none(), "asked once, then done with");
    // Anything back, a pong included, answers it.
    assert_eq!(s.wake(T0 + 10_000, Link::Up), Wake::Nothing);
    s.heard(T0 + 8000);
    assert!(!s.probe_due(T0 + 60_000));
    assert_eq!(s.next_wake(), None);
}

#[test]
fn r_lag_9_a_wake_replaces_a_connection_silent_past_dead_at_once() {
    let mut s = ConnectionSupervisor::new(T0);
    s.joined(T0);
    assert_eq!(s.wake(T0 + ms(DEAD), Link::Up), Wake::Probe, "30 s is not past it");
    s.heard(T0 + 1);
    assert_eq!(s.wake(T0 + 1 + ms(DEAD) + 1, Link::Up), Wake::Replace);
    assert_eq!(s.next_wake(), None, "no probe is out for one replaced");
}

#[test]
fn r_ios_2_a_connection_lost_in_the_background_is_held_until_the_foreground_and_a_live_one_is_kept() {
    let mut s = ConnectionSupervisor::new(T0);
    s.joined(T0);
    s.background(T0 + 1000);
    // Kept: nothing in the background asks to drop it, nor probes it.
    assert_eq!(s.wake(T0 + 2000, Link::Up), Wake::Nothing);
    assert_eq!(s.wake(T0 + 2000, Link::Down), Wake::Nothing, "no retry in the background, whatever wakes it");
    // Lost: not tried again.
    assert_eq!(s.lost(T0 + 3000, 0), None);
    assert!(s.in_background());
    // Forward again: joined at once.
    assert_eq!(s.foreground(T0 + 4000, Link::Down), Wake::Connect);
    assert!(!s.in_background());
}

#[test]
fn r_ios_2_ten_seconds_or_more_away_replaces_the_connection_on_the_foreground() {
    let mut s = ConnectionSupervisor::new(T0);
    s.joined(T0);
    s.background(T0);
    assert_eq!(s.foreground(T0 + ms(LONG_BACKGROUND), Link::Up), Wake::Replace);
    // Shorter: a wake, which probes.
    s.joined(T0 + 20_000);
    s.background(T0 + 20_000);
    assert_eq!(s.foreground(T0 + 20_000 + ms(LONG_BACKGROUND) - 1, Link::Up), Wake::Probe);
    // A second background notice doesn't move when it began.
    let mut s = ConnectionSupervisor::new(T0);
    s.joined(T0);
    s.background(T0);
    s.background(T0 + 9000);
    assert_eq!(s.foreground(T0 + 10_000, Link::Up), Wake::Replace);
}

#[test]
fn r_off_2_a_wake_after_the_foreground_joins_one_lost_while_away_even_without_a_foreground() {
    // The scene's foreground and its activation both wake: the second
    // finds the join under way and leaves it.
    let mut s = ConnectionSupervisor::new(T0);
    s.background(T0);
    assert_eq!(s.lost(T0 + 1, 0), None);
    assert_eq!(s.foreground(T0 + 2, Link::Down), Wake::Connect);
    assert_eq!(s.wake(T0 + 3, Link::Joining), Wake::Nothing);
}
