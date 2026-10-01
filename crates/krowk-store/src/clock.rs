//! The clock every time column is written from, and the ids minted beside
//! it. One place for both: an id carries a millisecond timestamp and a row
//! carries time columns, and a frozen-clock test must never see them disagree.

use std::io::Read;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Mutex, OnceLock};

/// KROWK_TEST_NOW_MS starts the clock at a fixed moment and advances it a
/// millisecond per read — the black-box golden cases hold this build and the
/// Go one to the same output through it. A clock that never moved would tie
/// every row and leave a listing ordered by time in SQLite's tie order. Not a
/// user setting.
fn test_clock() -> Option<&'static AtomicI64> {
    static CLOCK: OnceLock<Option<AtomicI64>> = OnceLock::new();
    CLOCK.get_or_init(|| std::env::var("KROWK_TEST_NOW_MS").ok()?.parse().ok().map(AtomicI64::new)).as_ref()
}

/// Milliseconds since the Unix epoch, UTC — the unit of every time column.
pub fn now_ms() -> i64 {
    match test_clock() {
        Some(next) => next.fetch_add(1, Ordering::SeqCst),
        None => std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64),
    }
}

/// The highest millisecond an id has been minted for, and the counter within it.
static LAST: Mutex<(i64, u16)> = Mutex::new((-1, 0));

/// A canonical lowercase UUIDv7: 48 bits of milliseconds, version 7, a 12-bit
/// counter that makes string order the issue order within one process, the
/// variant, and 62 random bits. A clock that steps back, or a millisecond
/// whose 4096 counter values are spent, takes the millisecond after the last
/// one used — an id never sorts before one already issued.
pub fn new_id() -> String {
    let ms = now_ms().max(0);
    let (ms, counter) = {
        let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
        if ms > last.0 {
            *last = (ms, 0);
        } else if last.1 < 0xfff {
            last.1 += 1;
        } else {
            *last = (last.0 + 1, 0);
        }
        *last
    };
    let mut b = [0u8; 16];
    b[..6].copy_from_slice(&(ms as u64).to_be_bytes()[2..]);
    let _ = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b[8..]));
    b[6] = 0x70 | (counter >> 8) as u8;
    b[7] = counter as u8;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

/// Whether `id` has the shape `new_id` mints: canonical lowercase,
/// version 7, the RFC 9562 variant.
pub fn is_id(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(i, b)| if matches!(i, 8 | 13 | 18 | 23) { b == b'-' } else { b.is_ascii_hexdigit() && !b.is_ascii_uppercase() })
        && &id[14..15] == "7"
        && matches!(id.as_bytes()[19], b'8' | b'9' | b'a' | b'b')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_v7_and_sort_in_issue_order() {
        let ids: Vec<String> = (0..50).map(|_| new_id()).collect();
        assert!(ids.windows(2).all(|w| w[0] < w[1]), "{ids:?}");
        assert!(ids.iter().all(|id| id.len() == 36 && &id[14..15] == "7" && matches!(&id[19..20], "8" | "9" | "a" | "b")));
    }
}
