//! The venue's clock, as seen from here — shared by Binance spot and
//! USDⓈ-M futures.
//!
//! Every signed Binance call carries a `timestamp` that the venue checks
//! against its own clock: it refuses a request whose timestamp is more than
//! `recvWindow` behind its clock, or more than one second ahead of it
//! (`-1021`). Signing with the local clock works only while the local clock
//! happens to be right (`SPEC.md` §6, README defect 3), so every timestamp
//! is the local clock plus an offset read from the venue's own time
//! endpoint.
//!
//! The offset is read again once it is `refresh_every` old, and at once
//! after the venue answers `-1021` (the caller calls
//! [`ServerClock::invalidate`]). A reading whose round trip does not fit
//! inside `recvWindow` is refused: an offset that uncertain could put a
//! signed request outside the window on its own.

use anyhow::{bail, Result};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy)]
struct Reading {
    /// Venue clock minus local clock, in ms.
    offset_ms: i64,
    /// The round trip of the request that read it. The offset may be off
    /// by up to half of this.
    rtt: Duration,
    taken_at: Instant,
}

#[derive(Debug)]
pub(crate) struct ServerClock {
    recv_window: Duration,
    refresh_every: Duration,
    reading: Mutex<Option<Reading>>,
}

impl ServerClock {
    pub(crate) fn new(recv_window: Duration, refresh_every: Duration) -> Self {
        Self {
            recv_window,
            refresh_every,
            reading: Mutex::new(None),
        }
    }

    /// Records one answer from the venue's time endpoint: `server_ms` came
    /// back for a request sent at local time `sent_local_ms` and answered
    /// `rtt` later. The venue read its clock somewhere inside that round
    /// trip; the midpoint is the best estimate.
    pub(crate) fn record(&self, server_ms: i64, sent_local_ms: i64, rtt: Duration) -> Result<()> {
        if rtt >= self.recv_window {
            bail!(
                "reading the venue's clock took {} ms, which does not fit inside recvWindow ({} ms): \
                 a signed request could arrive outside the window",
                rtt.as_millis(),
                self.recv_window.as_millis()
            );
        }
        let midpoint = sent_local_ms + (rtt.as_millis() / 2) as i64;
        *self.reading.lock().unwrap() = Some(Reading {
            offset_ms: server_ms - midpoint,
            rtt,
            taken_at: Instant::now(),
        });
        Ok(())
    }

    /// The venue's time now, in Unix ms, or `None` when there is no reading
    /// yet or the last one is due for a refresh.
    pub(crate) fn fresh_now_ms(&self) -> Option<i64> {
        let reading = (*self.reading.lock().unwrap())?;
        if reading.taken_at.elapsed() >= self.refresh_every {
            return None;
        }
        Some(local_now_ms() + reading.offset_ms)
    }

    /// The best estimate of the venue's time now, even from a stale
    /// reading, and the local clock when there has never been one. For
    /// labelling a read, never for signing.
    pub(crate) fn estimate_now_ms(&self) -> i64 {
        let offset = self
            .reading
            .lock()
            .unwrap()
            .map_or(0, |reading| reading.offset_ms);
        local_now_ms() + offset
    }

    /// Forgets the reading, so the next signed call reads the clock first.
    pub(crate) fn invalidate(&self) {
        *self.reading.lock().unwrap() = None;
    }

    /// How far the estimate may be from the venue's clock: the last
    /// reading's whole round trip, twice the worst case, so a margin built
    /// on it errs long.
    pub(crate) fn error_bound(&self) -> Duration {
        self.reading
            .lock()
            .unwrap()
            .map_or(Duration::ZERO, |reading| reading.rtt)
    }
}

/// The local clock, in Unix ms.
pub(crate) fn local_now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        // A clock before 1970 makes every signed call fail at the venue,
        // which says so; there is nothing better to sign with here.
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clock() -> ServerClock {
        ServerClock::new(Duration::from_millis(5_000), Duration::from_secs(600))
    }

    #[test]
    fn the_offset_is_taken_at_the_midpoint_of_the_round_trip() {
        let clock = clock();
        let sent = local_now_ms();
        // The venue answered 10 s ahead of the midpoint of a 100 ms trip.
        clock
            .record(sent + 50 + 10_000, sent, Duration::from_millis(100))
            .unwrap();

        let ahead = clock.fresh_now_ms().unwrap() - local_now_ms();
        assert!((9_990..=10_010).contains(&ahead), "offset was {ahead}");
        assert_eq!(clock.error_bound(), Duration::from_millis(100));
    }

    #[test]
    fn a_round_trip_that_does_not_fit_recv_window_is_refused() {
        let clock = clock();
        let err = clock
            .record(0, 0, Duration::from_millis(5_000))
            .unwrap_err();
        assert!(err.to_string().contains("recvWindow"));
        assert!(clock.fresh_now_ms().is_none());
    }

    #[test]
    fn a_reading_goes_stale_after_the_refresh_interval() {
        let clock = ServerClock::new(Duration::from_millis(5_000), Duration::ZERO);
        // The venue is a minute behind.
        let now = local_now_ms();
        clock
            .record(now - 60_000, now, Duration::from_millis(1))
            .unwrap();
        assert!(clock.fresh_now_ms().is_none());
        // Stale, but still the best estimate for labelling a read.
        assert!(clock.estimate_now_ms() < local_now_ms() - 59_000);
    }

    #[test]
    fn invalidate_forces_a_new_reading() {
        let clock = clock();
        clock
            .record(local_now_ms(), local_now_ms(), Duration::from_millis(1))
            .unwrap();
        assert!(clock.fresh_now_ms().is_some());
        clock.invalidate();
        assert!(clock.fresh_now_ms().is_none());
    }
}
