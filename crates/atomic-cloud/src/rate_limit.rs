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

use actix_web::body::{BoxBody, MessageBody};
use actix_web::dev::{ServiceRequest, ServiceResponse};
use actix_web::http::header;
use actix_web::middleware::Next;
use actix_web::{web, HttpMessage, HttpResponse};

use crate::auth::ResolvedTenant;

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

// --- The data-plane anti-abuse rate-limit rows -------------------------------
//
// The plan's "Quotas" → anti-abuse table lists five sliding-window limits.
// Slice 2 landed the two signup-surface rows (signup-per-IP, magic-link
// -per-email) in the account plane. The remaining three are per-account
// data-plane limits, keyed by `account_id` and applied in the data-plane
// guard ([`crate::server::data_plane_rate_limit_guard`]):
//
// | Limit          | Window  | Default |
// |----------------|---------|---------|
// | API requests   | per min | 600     |
// | Atom creates   | per min | 60      |
// | URL ingestion  | per min | 30      |
//
// Per-pod approximate consistency, exactly like the signup limiters and the
// chat-stream/circuit-breaker counters: an account's effective fleet-wide
// allowance is `limit × pod count`, which the plan assigns to this
// consistency class. A reset is simply the sliding window emptying as old
// admissions age out (the [`SlidingWindow`] semantics above), so no separate
// rollover job is needed for these.

/// The three per-account data-plane limits, with windows exposed so tests
/// can shrink them instead of sleeping through real minutes. Production
/// callers use [`Default`].
#[derive(Debug, Clone)]
pub struct DataPlaneRateLimits {
    /// All authenticated data-plane requests, per account per window.
    pub requests: u32,
    /// Atom creates (`POST /api/atoms`, `/api/atoms/bulk`), per account.
    pub atom_creates: u32,
    /// URL ingestion (`POST /api/ingest/url`, `/api/ingest/urls`), per account.
    pub url_ingestion: u32,
    pub window: Duration,
}

impl Default for DataPlaneRateLimits {
    fn default() -> Self {
        Self {
            requests: 600,
            atom_creates: 60,
            url_ingestion: 30,
            window: Duration::from_secs(60),
        }
    }
}

/// Which per-account data-plane limit, if any, a refused request tripped —
/// surfaced in the 429 body so a client can tell "slow down generally" from
/// "you're creating atoms too fast".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataPlaneLimit {
    Requests,
    AtomCreates,
    UrlIngestion,
}

impl DataPlaneLimit {
    /// Stable text form for the 429 body.
    pub fn as_str(self) -> &'static str {
        match self {
            DataPlaneLimit::Requests => "requests",
            DataPlaneLimit::AtomCreates => "atom_creates",
            DataPlaneLimit::UrlIngestion => "url_ingestion",
        }
    }
}

/// The per-account data-plane limiter set. One instance per process —
/// construct once and clone into every worker (a per-worker instance would
/// multiply every limit by the worker count, exactly like the chat-stream
/// semaphore). Cheap to clone (each [`SlidingWindow`] is shared by `Arc`).
#[derive(Clone)]
pub struct DataPlaneRateLimiter {
    requests: std::sync::Arc<SlidingWindow>,
    atom_creates: std::sync::Arc<SlidingWindow>,
    url_ingestion: std::sync::Arc<SlidingWindow>,
}

impl DataPlaneRateLimiter {
    pub fn new(limits: DataPlaneRateLimits) -> Self {
        Self {
            requests: std::sync::Arc::new(SlidingWindow::new(limits.requests, limits.window)),
            atom_creates: std::sync::Arc::new(SlidingWindow::new(
                limits.atom_creates,
                limits.window,
            )),
            url_ingestion: std::sync::Arc::new(SlidingWindow::new(
                limits.url_ingestion,
                limits.window,
            )),
        }
    }

    /// Charge the limits that apply to a `(method, path)` for `account_id`,
    /// in the order most-specific-first so the returned [`DataPlaneLimit`]
    /// names the binding constraint. Every authenticated data-plane request
    /// charges the broad request limit; atom-create and URL-ingestion routes
    /// *additionally* charge their narrow limit.
    ///
    /// Returns `Err((which, retry_after))` for the first limit the request
    /// exceeds. Charging the broad limit first means a flood of any request
    /// type is caught by `requests` before a narrow limit is even consulted,
    /// and — like the signup limiters — only **admitted** requests are
    /// recorded, so a client hammering its own 429 doesn't push its reset
    /// further out.
    ///
    /// One consequence of broad-first: a request that *passes* the broad
    /// limit but then trips a narrow limit (e.g. atom-create #61 with the
    /// request window still open) has already recorded an admission on the
    /// broad window. The broad counter therefore slightly over-counts on
    /// narrow-limit rejections — it can only ever over-count, never
    /// under-count, so it stays fail-safe (an account is never granted more
    /// than its broad allowance). Given per-pod approximate consistency this
    /// is immaterial; exact broad accounting would require checking the
    /// narrow limit first (or a non-recording peek), which would in turn let
    /// the broad limit under-count — the wrong trade for an anti-abuse cap.
    pub fn check(
        &self,
        account_id: &str,
        method: &str,
        path: &str,
    ) -> Result<(), (DataPlaneLimit, Duration)> {
        // Broad limit on every authenticated request.
        self.requests
            .check(account_id)
            .map_err(|d| (DataPlaneLimit::Requests, d))?;

        if method == "POST" {
            if path == "/api/atoms" || path == "/api/atoms/bulk" {
                self.atom_creates
                    .check(account_id)
                    .map_err(|d| (DataPlaneLimit::AtomCreates, d))?;
            } else if path == "/api/ingest/url" || path == "/api/ingest/urls" {
                self.url_ingestion
                    .check(account_id)
                    .map_err(|d| (DataPlaneLimit::UrlIngestion, d))?;
            }
        }
        Ok(())
    }
}

/// Data-plane middleware: charge the per-account anti-abuse rate limits
/// (module docs: the three data-plane rows) and 429 a request that exceeds
/// one. Wired inside CloudAuth and the plane guard, so [`ResolvedTenant`] is
/// installed; a missing extension is skipped defensively (the plane guard
/// fails such requests closed already). Registered *outside* the quota and
/// dispatch-hint guards, so a rate-limited request never reaches a handler
/// and never marks a hint — the cheapest possible rejection.
///
/// The 429 carries `Retry-After` and a structured body naming which limit
/// bound, so a client can distinguish "slow down generally" from "you're
/// creating atoms too fast".
pub async fn data_plane_rate_limit_guard(
    limiter: web::Data<DataPlaneRateLimiter>,
    req: ServiceRequest,
    next: Next<impl MessageBody + 'static>,
) -> Result<ServiceResponse<BoxBody>, actix_web::Error> {
    // Demo visitors are exempt from the per-ACCOUNT windows: every
    // anonymous visitor shares the demo account's key, so a traffic spike
    // would collectively 429 the public demo. Their limits are per-IP,
    // enforced at admission time (`crate::demo_plane`), with the managed
    // key's credit cap bounding AI spend regardless.
    let account_id = req
        .extensions()
        .get::<ResolvedTenant>()
        .filter(|t| t.principal.source != crate::auth::CredentialSource::DemoVisitor)
        .map(|t| t.principal.account_id.clone());
    let Some(account_id) = account_id else {
        return next.call(req).await.map(|res| res.map_into_boxed_body());
    };

    let method = req.method().as_str().to_string();
    let path = req.path().to_string();
    if let Err((which, retry_after)) = limiter.check(&account_id, &method, &path) {
        // Round up so a client told to retry isn't a second early.
        let seconds = retry_after.as_secs() + u64::from(retry_after.subsec_nanos() > 0);
        let denial = HttpResponse::TooManyRequests()
            .insert_header((header::RETRY_AFTER, seconds.to_string()))
            .json(serde_json::json!({
                "error": "rate_limited",
                "limit": which.as_str(),
                "message": "Too many requests for this account. Try again shortly.",
                "retry_after_seconds": seconds,
            }));
        return Ok(req.into_response(denial));
    }
    next.call(req).await.map(|res| res.map_into_boxed_body())
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
    fn data_plane_limiter_routes_and_isolates() {
        // Generous request limit so the narrow atom/ingest limits are the
        // binding constraint; tiny narrow limits so we can exhaust them.
        let limiter = DataPlaneRateLimiter::new(DataPlaneRateLimits {
            requests: 1000,
            atom_creates: 2,
            url_ingestion: 1,
            window: WINDOW,
        });

        // A non-create POST and any GET only charge the broad request limit.
        assert!(limiter.check("a", "GET", "/api/atoms").is_ok());
        assert!(limiter.check("a", "POST", "/api/tags").is_ok());

        // Atom creates charge the narrow limit (2): third is refused as
        // AtomCreates, not Requests.
        assert!(limiter.check("a", "POST", "/api/atoms").is_ok());
        assert!(limiter.check("a", "POST", "/api/atoms/bulk").is_ok());
        let (which, _) = limiter
            .check("a", "POST", "/api/atoms")
            .expect_err("third atom create over the narrow limit");
        assert_eq!(which, DataPlaneLimit::AtomCreates);

        // URL ingestion has its own (1): second is refused as UrlIngestion.
        assert!(limiter.check("a", "POST", "/api/ingest/urls").is_ok());
        let (which, _) = limiter
            .check("a", "POST", "/api/ingest/url")
            .expect_err("second ingestion over the narrow limit");
        assert_eq!(which, DataPlaneLimit::UrlIngestion);

        // A different account is completely unaffected by a's saturation.
        assert!(limiter.check("b", "POST", "/api/atoms").is_ok());
        assert!(limiter.check("b", "POST", "/api/ingest/url").is_ok());
    }

    #[test]
    fn data_plane_request_limit_binds_first() {
        // A request limit of 1 catches the second request of ANY type before
        // a narrow limit is consulted (broad-first ordering).
        let limiter = DataPlaneRateLimiter::new(DataPlaneRateLimits {
            requests: 1,
            atom_creates: 100,
            url_ingestion: 100,
            window: WINDOW,
        });
        assert!(limiter.check("a", "GET", "/api/atoms").is_ok());
        let (which, _) = limiter
            .check("a", "POST", "/api/atoms")
            .expect_err("second request over the broad limit");
        assert_eq!(which, DataPlaneLimit::Requests);
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
