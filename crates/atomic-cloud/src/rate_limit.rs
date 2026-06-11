//! Per-pod, in-memory sliding-window rate limiting (plan: "Observability,
//! quotas, billing" → "Quotas" → anti-abuse rate limits).
//!
//! The plan's anti-abuse limits ("5 signup attempts per IP per hour",
//! "3 magic-link requests per email per hour") need only approximate,
//! per-pod consistency — a noisy client's effective fleet-wide allowance is
//! `limit × pod count`, which is fine at small pod counts and exactly the
//! consistency class the plan assigns this category.
//!
//! This is a hand-rolled sliding **log** (a timestamp deque per key) rather
//! than the plan's suggested `governor` crate, a deliberate substitution:
//! the windows here are long (an hour) and the limits tiny (3–5), so the
//! log costs a few dozen `Instant`s per active key while giving *exact*
//! window semantics and a directly computable `Retry-After` (oldest
//! timestamp + window − now). `governor`'s keyed GCRA approximates the
//! window differently than the plan's table reads, needs its own
//! housekeeping story for the keyed store, and would be this crate's only
//! use of the dependency tree it brings. Revisit if a high-rate limit (the
//! plan's 600 req/min API limit) lands on this type — at that point GCRA's
//! O(1) state wins and `governor` earns its keep.
//!
//! Only **admitted** requests are recorded: a client hammering a 429 does
//! not push its own reset further out (the standard sliding-log behavior,
//! and the friendlier one for a user whose mailbox is just slow).

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Once the key map grows past this, admission does a full sweep of expired
/// keys first. Bounds memory against an attacker rotating keys (spoofed
/// IPs, throwaway emails): the map holds at most the sweep threshold plus
/// the keys genuinely active inside one window.
const SWEEP_THRESHOLD: usize = 4096;

/// A sliding-window rate limiter over string keys (an IP, a lowercased
/// email). Cheap interior mutability; share one instance per limit.
pub struct SlidingWindow {
    limit: u32,
    window: Duration,
    hits: Mutex<HashMap<String, VecDeque<Instant>>>,
}

impl SlidingWindow {
    /// Allow `limit` admissions per `window` per key.
    pub fn new(limit: u32, window: Duration) -> Self {
        Self {
            limit,
            window,
            hits: Mutex::new(HashMap::new()),
        }
    }

    /// Admit (and record) one request for `key`, or return how long until
    /// the oldest recorded admission leaves the window — the `Retry-After`
    /// value.
    pub fn check(&self, key: &str) -> Result<(), Duration> {
        self.check_at(key, Instant::now())
    }

    /// [`check`](Self::check) with an explicit clock, so tests are
    /// deterministic instead of sleeping through real windows.
    fn check_at(&self, key: &str, now: Instant) -> Result<(), Duration> {
        let mut hits = self.hits.lock().expect("rate limiter lock poisoned");

        if hits.len() >= SWEEP_THRESHOLD && !hits.contains_key(key) {
            let window = self.window;
            hits.retain(|_, stamps| {
                stamps
                    .back()
                    .is_some_and(|&last| now.saturating_duration_since(last) < window)
            });
        }

        let stamps = hits.entry(key.to_string()).or_default();
        while stamps
            .front()
            .is_some_and(|&first| now.saturating_duration_since(first) >= self.window)
        {
            stamps.pop_front();
        }

        if stamps.len() as u64 >= u64::from(self.limit) {
            let retry_after = stamps
                .front()
                .map(|&first| {
                    self.window
                        .saturating_sub(now.saturating_duration_since(first))
                })
                .unwrap_or(self.window); // limit == 0: nothing ever admits.
            return Err(retry_after);
        }
        stamps.push_back(now);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW: Duration = Duration::from_secs(3600);

    #[test]
    fn enforces_limit_and_reports_retry_after() {
        let limiter = SlidingWindow::new(3, WINDOW);
        let start = Instant::now();
        for i in 0..3 {
            assert!(
                limiter
                    .check_at("203.0.113.7", start + Duration::from_secs(i))
                    .is_ok(),
                "admission {i} within limit"
            );
        }
        // Fourth request 10s in: the oldest admission (t=0) leaves the
        // window at t=3600, so Retry-After is 3590s.
        let retry = limiter
            .check_at("203.0.113.7", start + Duration::from_secs(10))
            .expect_err("over limit");
        assert_eq!(retry, Duration::from_secs(3590));
        // Other keys are unaffected.
        assert!(limiter
            .check_at("203.0.113.8", start + Duration::from_secs(10))
            .is_ok());
    }

    #[test]
    fn window_slides_rather_than_resetting() {
        let limiter = SlidingWindow::new(2, WINDOW);
        let start = Instant::now();
        assert!(limiter.check_at("k", start).is_ok());
        assert!(limiter
            .check_at("k", start + Duration::from_secs(1800))
            .is_ok());
        // t=3500: the t=0 admission is still in the window → refused.
        assert!(limiter
            .check_at("k", start + Duration::from_secs(3500))
            .is_err());
        // t=3601: the t=0 admission expired; one slot free again.
        assert!(limiter
            .check_at("k", start + Duration::from_secs(3601))
            .is_ok());
        // …but the t=1800 and t=3601 admissions now fill the window.
        assert!(limiter
            .check_at("k", start + Duration::from_secs(3602))
            .is_err());
    }

    #[test]
    fn refused_requests_do_not_extend_the_window() {
        let limiter = SlidingWindow::new(1, WINDOW);
        let start = Instant::now();
        assert!(limiter.check_at("k", start).is_ok());
        // Hammering while refused…
        for i in 1..100 {
            assert!(limiter
                .check_at("k", start + Duration::from_secs(i))
                .is_err());
        }
        // …doesn't delay the reset past the original admission's expiry.
        assert!(limiter
            .check_at("k", start + WINDOW + Duration::from_secs(1))
            .is_ok());
    }

    #[test]
    fn zero_limit_never_admits() {
        let limiter = SlidingWindow::new(0, WINDOW);
        let retry = limiter.check_at("k", Instant::now()).expect_err("never");
        assert_eq!(retry, WINDOW);
    }

    #[test]
    fn sweep_evicts_expired_keys() {
        let limiter = SlidingWindow::new(1, WINDOW);
        let start = Instant::now();
        for i in 0..SWEEP_THRESHOLD {
            assert!(limiter.check_at(&format!("key-{i}"), start).is_ok());
        }
        assert_eq!(limiter.hits.lock().unwrap().len(), SWEEP_THRESHOLD);
        // A new key past the threshold, after every entry expired, sweeps
        // the map down to just itself.
        assert!(limiter
            .check_at("fresh", start + WINDOW + Duration::from_secs(1))
            .is_ok());
        assert_eq!(limiter.hits.lock().unwrap().len(), 1);
    }
}
