//! Provider backpressure: the per-tenant circuit breaker and the
//! interactive `out_of_ai_credits` surface (plan: "Worker fairness & job
//! queue" → "Provider rate-limit handling"; "Provider management" →
//! "Managed key lifecycle" / "Blocked states").
//!
//! # Two layers
//!
//! **Layer 1 — local backoff** lives with the ledgers, not here: a
//! rate-limited `task_runs` execution gets its `next_attempt_at` pushed out
//! by the ledger's own exponential backoff (`scheduler::ledger::fail`), and
//! a rate-limited pipeline job is re-enqueued by the dispatcher's executor
//! with `not_before` honoring the provider's `Retry-After` hint (see
//! [`crate::dispatcher::CoreExecutor`]). This module is **layer 2**: when
//! one tenant's provider keeps rate-limiting, per-row backoff alone would
//! keep burning dispatch capacity (and the provider's goodwill) on a tenant
//! that needs a real pause.
//!
//! # The breaker state machine
//!
//! - **Detection is in-memory, per pod**: a sliding window of
//!   rate-limit-classified failures per tenant. Three within 60 seconds
//!   trips the breaker. Per-pod detection is deliberate — sharing failure
//!   counts across pods would buy a slightly earlier trip at the cost of a
//!   control-plane write per failure; a noisy tenant trips every pod's
//!   window within seconds anyway. The map self-bounds: a trip and a
//!   provider-class success both drop the tenant's window, and every
//!   failure note also sweeps out windows whose newest entry has aged past
//!   the detection horizon, so tenants that fail a couple of times and go
//!   quiet don't accumulate.
//! - **Only provider-touching work feeds detection.** Maintenance tasks
//!   and non-inline feed polls never call the provider, so their successes
//!   say nothing about the provider's health — they must neither reset the
//!   streak nor clear the window (or interleaved housekeeping would keep a
//!   chronically rate-limited tenant from ever tripping, and keep a
//!   tripped one from ever escalating). The dispatcher's executor owns
//!   that gating (see `crate::dispatcher`): `record_healthy` is called
//!   only for provider-class executions that genuinely succeeded.
//! - **The pause itself is control plane**, so every pod honors it:
//!   `accounts.provider_paused_until` (+ `provider_pause_kind`,
//!   `provider_pause_streak`; migration 007). The dispatcher skips paused
//!   tenants wholesale — their ledger work *sits*, it never fails.
//! - **Cooldown doubles per consecutive trip** (60s, 120s, …, capped at
//!   1h), tracked by `provider_pause_streak`. A healthy run resets the
//!   streak ([`ProviderBreaker::record_healthy`]); so does any provider
//!   mutation (BYOK save / activate / models write — rotation step 6, in
//!   [`crate::provider_credentials`]).
//! - **Credit exhaustion (402)** pauses immediately with
//!   `kind = 'credits'`, until the allowance's reset when known, else a
//!   recheck horizon (1h). It does not touch the streak — it isn't a
//!   rate-limit escalation, it's a billing wall that lifts on reset,
//!   upgrade, or key rotation.
//! - **Credential rejection (401/403)** — a BYOK key expired, revoked, or
//!   mis-scoped — pauses immediately with `kind = 'provider'` (the plan's
//!   "BYOK key expired" breaker case): no retry succeeds until the user
//!   fixes the key, so background dispatch holds until a provider mutation
//!   clears the pause (or the recheck horizon re-probes, in case the 401
//!   was the provider's own hiccup). Like credits, it never touches the
//!   rate-limit streak; unlike credits, interactive routes keep their
//!   current behavior — the user gets the provider's real auth error,
//!   which is the actionable one.
//!
//! # The rate-limit / credits asymmetry
//!
//! A **rate-limit** pause stops *background dispatch only*. Interactive
//! traffic (chat, an explicit wiki generate) still goes to the provider —
//! it has its own provider-side limiting, and a user staring at a chat box
//! deserves the provider's real answer, not a synthetic one. The breaker
//! protects the *background* budget.
//!
//! A **credits** pause additionally turns the AI-interactive routes into a
//! structured 402 ([`out_of_credits_guard`]): the provider would refuse
//! anyway, and "out of AI credits, resets at X" is a better answer than a
//! provider error string. Atom CRUD — and everything else non-AI — stays
//! fully functional; the ledger holds the background work for when the
//! allowance returns.
//!
//! One last-writer-wins residue: the two pause kinds share one column pair,
//! so a rate-limit trip while a credits pause is in force overwrites the
//! kind, briefly re-opening the interactive routes. The next interactive or
//! dispatched call hits the provider's 402 and re-pauses as credits —
//! self-healing, and not worth a second column pair.

use std::collections::{HashMap, VecDeque};
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use actix_web::body::MessageBody;
use actix_web::dev::{ServiceRequest, ServiceResponse};
use actix_web::http::{header, Method};
use actix_web::middleware::Next;
use actix_web::{HttpMessage, HttpResponse};
use chrono::{DateTime, Utc};

use atomic_core::providers::{classify_provider_failure, ProviderFailureClass};
use atomic_core::scheduler::ledger::{FailureDisposition, FailureDispositionPolicy};

use crate::auth::ResolvedTenant;
use crate::control_plane::ControlPlane;
use crate::error::CloudError;

/// Layer-1 re-dispatch delay for rate-limited work when the provider sent
/// no `Retry-After` hint. Matches the task ledger's backoff base unit
/// (`scheduler::ledger::BACKOFF_BASE`) so both ledgers retry on the same
/// conventions.
pub const RATE_LIMIT_REQUEUE_DELAY: Duration = Duration::from_secs(60);

/// Default ceiling on a provider-supplied `Retry-After` hint
/// ([`DispatcherConfig::retry_after_cap`](crate::dispatcher::DispatcherConfig)).
/// A hostile or buggy provider must not be able to strand a tenant's work
/// behind an arbitrary horizon; anything longer is the breaker's job.
pub const DEFAULT_RETRY_AFTER_CAP: Duration = Duration::from_secs(15 * 60);

/// Why a tenant's provider dispatch is paused. Serialized to text in
/// `accounts.provider_pause_kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PauseKind {
    /// Repeated provider rate limiting tripped the circuit breaker.
    /// Background dispatch pauses; interactive routes are unaffected.
    RateLimit,
    /// The provider refused for billing reasons (402 — exhausted managed
    /// allowance, or a BYOK key out of its own credits). Background
    /// dispatch pauses AND the AI-interactive routes return the structured
    /// `out_of_ai_credits` error.
    Credits,
    /// The provider rejected the stored credentials (401/403 — an expired,
    /// revoked, or mis-scoped key): the provider configuration itself is
    /// broken. Background dispatch pauses until a provider mutation clears
    /// it (or the recheck horizon re-probes); interactive routes are
    /// unaffected — the provider's own auth error is the actionable answer.
    Provider,
}

impl PauseKind {
    /// The text stored in `accounts.provider_pause_kind`.
    pub fn as_str(self) -> &'static str {
        match self {
            PauseKind::RateLimit => "rate_limit",
            PauseKind::Credits => "credits",
            PauseKind::Provider => "provider",
        }
    }
}

impl std::fmt::Display for PauseKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for PauseKind {
    type Err = CloudError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "rate_limit" => Ok(PauseKind::RateLimit),
            "credits" => Ok(PauseKind::Credits),
            "provider" => Ok(PauseKind::Provider),
            other => Err(CloudError::InvalidPauseKind(other.to_string())),
        }
    }
}

/// A tenant's provider pause, as read off the accounts row. Carried on
/// [`ResolvedTenant`] by CloudAuth (the lookup it already makes per
/// request); consumers compare `until` against their own `now` — a pause is
/// only in force while `until` is in the future.
#[derive(Debug, Clone, Copy)]
pub struct ProviderPause {
    pub until: DateTime<Utc>,
    pub kind: PauseKind,
}

impl ProviderPause {
    /// Reconstruct from the raw column pair. `None` when not paused; a
    /// present timestamp with a missing/unknown kind degrades to a
    /// rate-limit-shaped pause (background-only — the conservative reading:
    /// it never blocks a user) with a loud log, rather than failing the
    /// request over one corrupt column.
    pub fn from_columns(until: Option<DateTime<Utc>>, kind: Option<&str>) -> Option<Self> {
        let until = until?;
        let kind = match kind.map(PauseKind::from_str) {
            Some(Ok(kind)) => kind,
            other => {
                tracing::warn!(
                    ?other,
                    "provider_pause_kind missing or unknown; treating pause as rate_limit"
                );
                PauseKind::RateLimit
            }
        };
        Some(ProviderPause { until, kind })
    }

    /// Whether the pause is in force at `now`.
    pub fn active_at(&self, now: DateTime<Utc>) -> bool {
        self.until > now
    }
}

/// Tuning knobs for the circuit breaker. Defaults are the plan's numbers.
#[derive(Debug, Clone)]
pub struct BreakerConfig {
    /// Sliding detection window for rate-limit failures.
    pub window: Duration,
    /// Failures within [`Self::window`] that trip the breaker.
    pub threshold: usize,
    /// First trip's cooldown; doubles per consecutive trip.
    pub base_cooldown: Duration,
    /// Cooldown ceiling.
    pub max_cooldown: Duration,
    /// Credits-pause horizon when the provider exposes no reset time: how
    /// long until dispatch re-probes the allowance.
    pub credits_recheck: Duration,
}

impl Default for BreakerConfig {
    fn default() -> Self {
        Self {
            window: Duration::from_secs(60),
            threshold: 3,
            base_cooldown: Duration::from_secs(60),
            max_cooldown: Duration::from_secs(60 * 60),
            credits_recheck: Duration::from_secs(60 * 60),
        }
    }
}

/// The per-pod circuit breaker (module docs: the breaker state machine).
/// Detection state is in-memory; pauses are control-plane writes every pod
/// honors. Cheap to share via `Arc`.
pub struct ProviderBreaker {
    control: ControlPlane,
    config: BreakerConfig,
    /// Per-tenant sliding windows of recent rate-limit failures. Bounded:
    /// at most `threshold` entries per tenant (a trip clears the window),
    /// and tenants prune to nothing as their entries age out.
    windows: Mutex<HashMap<String, VecDeque<Instant>>>,
}

impl ProviderBreaker {
    pub fn new(control: ControlPlane, config: BreakerConfig) -> Self {
        Self {
            control,
            config,
            windows: Mutex::new(HashMap::new()),
        }
    }

    /// Record one rate-limit-classified failure for `account_id`. When this
    /// failure is the `threshold`-th within the window, trip the breaker:
    /// pause the tenant in the control plane (cooldown doubled by the
    /// stored streak) and return the pause horizon. `Ok(None)` means "noted,
    /// not tripped" — or the account vanished under a concurrent deletion.
    pub async fn record_rate_limited(
        &self,
        account_id: &str,
    ) -> Result<Option<DateTime<Utc>>, CloudError> {
        if !self.note_failure(account_id, Instant::now()) {
            return Ok(None);
        }

        // Trip. One statement so the streak read and the doubled cooldown
        // can't interleave with a concurrent trip from another pod: the
        // RHS sees the pre-update streak, so cooldown = base * 2^streak,
        // capped. The exponent is clamped well below f64 overflow.
        let paused_until: Option<DateTime<Utc>> = sqlx::query_scalar(
            "UPDATE accounts SET \
                 provider_pause_streak = provider_pause_streak + 1, \
                 provider_pause_kind = 'rate_limit', \
                 provider_paused_until = NOW() + make_interval(secs => LEAST( \
                     $2, \
                     $3 * power(2::double precision, \
                                LEAST(provider_pause_streak, 30)::double precision))) \
             WHERE id = $1 \
             RETURNING provider_paused_until",
        )
        .bind(account_id)
        .bind(self.config.max_cooldown.as_secs_f64())
        .bind(self.config.base_cooldown.as_secs_f64())
        .fetch_optional(self.control.pool())
        .await
        .map_err(CloudError::db("tripping provider circuit breaker"))?;

        match paused_until {
            Some(until) => {
                tracing::warn!(
                    account_id,
                    paused_until = %until,
                    "provider circuit breaker tripped; tenant dispatch paused"
                );
                Ok(Some(until))
            }
            // The account was deleted between the failure and the trip.
            None => Ok(None),
        }
    }

    /// Record a credit-exhaustion (402) failure: pause immediately with
    /// `kind = 'credits'` until `resets_at` (when the provider exposed one)
    /// or the configured recheck horizon. Returns the pause horizon;
    /// `Ok(None)` when the account vanished concurrently. The rate-limit
    /// streak is untouched (module docs).
    pub async fn record_payment_required(
        &self,
        account_id: &str,
        resets_at: Option<DateTime<Utc>>,
    ) -> Result<Option<DateTime<Utc>>, CloudError> {
        let until = resets_at.unwrap_or_else(|| {
            Utc::now() + chrono::Duration::from_std(self.config.credits_recheck).unwrap()
        });
        let paused = self
            .pause_immediately(account_id, PauseKind::Credits, until)
            .await?;
        if paused {
            tracing::warn!(
                account_id,
                paused_until = %until,
                "tenant out of AI credits; dispatch paused and interactive AI routes blocked"
            );
            Ok(Some(until))
        } else {
            Ok(None)
        }
    }

    /// Record a credential-rejection (401/403) failure: pause immediately
    /// with `kind = 'provider'` for the recheck horizon (module docs: a
    /// broken key is fixed by a provider mutation, which clears the pause;
    /// the horizon is only the re-probe bound for a provider-side hiccup).
    /// Returns the pause horizon; `Ok(None)` when the account vanished
    /// concurrently. The rate-limit streak is untouched.
    pub async fn record_auth_failed(
        &self,
        account_id: &str,
    ) -> Result<Option<DateTime<Utc>>, CloudError> {
        let until = Utc::now() + chrono::Duration::from_std(self.config.credits_recheck).unwrap();
        let paused = self
            .pause_immediately(account_id, PauseKind::Provider, until)
            .await?;
        if paused {
            tracing::warn!(
                account_id,
                paused_until = %until,
                "provider rejected the tenant's credentials; dispatch paused until rotation"
            );
            Ok(Some(until))
        } else {
            Ok(None)
        }
    }

    /// Shared immediate-pause write for the non-escalating kinds (credits,
    /// provider). Returns whether the account row still existed.
    async fn pause_immediately(
        &self,
        account_id: &str,
        kind: PauseKind,
        until: DateTime<Utc>,
    ) -> Result<bool, CloudError> {
        let updated = sqlx::query(
            "UPDATE accounts SET \
                 provider_paused_until = $2, \
                 provider_pause_kind = $3 \
             WHERE id = $1",
        )
        .bind(account_id)
        .bind(until)
        .bind(kind.as_str())
        .execute(self.control.pool())
        .await
        .map_err(CloudError::db("pausing tenant on provider failure"))?;
        Ok(updated.rows_affected() > 0)
    }

    /// Record a healthy (provider-failure-free) execution: drop the
    /// in-memory window and reset the consecutive-trip streak, so the next
    /// trip starts back at the base cooldown. The streak write is
    /// conditioned on `<> 0`, so the steady state is a no-op row match.
    pub async fn record_healthy(&self, account_id: &str) -> Result<(), CloudError> {
        {
            let mut windows = self.windows.lock().expect("breaker windows poisoned");
            windows.remove(account_id);
        }
        sqlx::query(
            "UPDATE accounts SET provider_pause_streak = 0 \
             WHERE id = $1 AND provider_pause_streak <> 0",
        )
        .bind(account_id)
        .execute(self.control.pool())
        .await
        .map_err(CloudError::db("resetting provider pause streak"))?;
        Ok(())
    }

    /// Slide the window and decide whether this failure trips the breaker.
    /// Pure in-memory bookkeeping, split out (and instant-parameterized) so
    /// the threshold/window semantics are unit-testable without a clock.
    /// Each call also prunes *other* tenants' fully aged-out windows
    /// (module docs: the map self-bounds) — failures are rare enough that
    /// the O(tenants) sweep is free in practice.
    fn note_failure(&self, account_id: &str, now: Instant) -> bool {
        let mut windows = self.windows.lock().expect("breaker windows poisoned");
        // Drop every window whose NEWEST entry has aged past the detection
        // horizon — nothing in it can ever count toward a trip again.
        windows.retain(|_, window| {
            window
                .back()
                .is_some_and(|newest| now.duration_since(*newest) <= self.config.window)
        });
        let window = windows.entry(account_id.to_string()).or_default();
        window.push_back(now);
        while let Some(front) = window.front() {
            if now.duration_since(*front) > self.config.window {
                window.pop_front();
            } else {
                break;
            }
        }
        if window.len() >= self.config.threshold {
            // Trip consumes the window: the next failure starts a fresh
            // count rather than instantly re-tripping on stale entries.
            windows.remove(account_id);
            true
        } else {
            false
        }
    }

    /// Number of tenants with live detection windows. Metrics/tests.
    pub fn tracked_windows(&self) -> usize {
        self.windows.lock().expect("breaker windows poisoned").len()
    }
}

// --- The task-ledger failure-disposition policy ------------------------------

/// Build the [`FailureDispositionPolicy`] cloud tenants run with: the
/// layer-1 half of "jobs sit in the ledger, never fail" for the `task_runs`
/// ledger (plan: "Allowance exhausted" — and its rate-limit/auth siblings).
///
/// Provider failures are *environmental* — a month-long credit exhaustion
/// must not burn through `max_attempts` in three probes and terminally
/// abandon work (wiki regeneration above all: nothing but a ledger scan
/// ever re-fires it). The policy classifies each failure message with
/// [`classify_provider_failure`] and defers instead of failing:
///
/// - **Rate limit** → defer to the provider's `Retry-After` (clamped to
///   `retry_after_cap`), or [`RATE_LIMIT_REQUEUE_DELAY`] without a hint —
///   the same horizon the pipeline ledger's re-enqueue uses, and the
///   layer-1 contract ("record the rate-limit-reset header into
///   `task_runs.next_attempt_at`") made literal.
/// - **Credits / auth** → defer to `pause_recheck` (the breaker's recheck
///   horizon — [`BreakerConfig::credits_recheck`]); the matching tenant
///   pause holds dispatch anyway, and a provider mutation re-arms both
///   (see `AtomicCore::rearm_provider_blocked_task_runs`).
/// - **Transient** (5xx / timeout) → defer to [`RATE_LIMIT_REQUEUE_DELAY`]:
///   a server-side or network outage recovers on its own, so the run waits
///   it out without consuming retry budget rather than terminally failing a
///   wiki/report run on a passing provider hiccup.
/// - **Anything else** → [`FailureDisposition::Fail`]: logic failures keep
///   consuming retry budget exactly as before.
///
/// Installed per tenant manager by the [`crate::account_cache`] build path.
pub fn provider_failure_policy(
    pause_recheck: Duration,
    retry_after_cap: Duration,
) -> FailureDispositionPolicy {
    Arc::new(move |error: &str| match classify_provider_failure(error) {
        ProviderFailureClass::RateLimited { retry_after_secs } => {
            let delay = retry_after_secs
                .map(|secs| Duration::from_secs(secs).min(retry_after_cap))
                .unwrap_or(RATE_LIMIT_REQUEUE_DELAY);
            FailureDisposition::DeferUntil(
                Utc::now() + chrono::Duration::from_std(delay).unwrap_or_default(),
            )
        }
        ProviderFailureClass::PaymentRequired | ProviderFailureClass::AuthFailed => {
            FailureDisposition::DeferUntil(
                Utc::now() + chrono::Duration::from_std(pause_recheck).unwrap_or_default(),
            )
        }
        // A transient provider outage (5xx/timeout) recovers on its own, so
        // defer to the base re-dispatch delay rather than burning a
        // `max_attempts` slot — a sustained outage must not terminally
        // abandon wiki/report runs (nothing but a ledger scan re-fires them).
        // No `Retry-After` accompanies a 5xx/network fault, so the fixed base
        // horizon applies, matching the pipeline ledger's transient re-enqueue.
        ProviderFailureClass::Transient => FailureDisposition::DeferUntil(
            Utc::now() + chrono::Duration::from_std(RATE_LIMIT_REQUEUE_DELAY).unwrap_or_default(),
        ),
        ProviderFailureClass::Other => FailureDisposition::Fail,
    })
}

// --- The interactive out-of-credits surface ---------------------------------

/// Whether `(method, path)` is one of the AI-interactive routes a credits
/// pause must answer with the structured `out_of_ai_credits` error: the
/// synchronous, user-facing operations whose handlers would otherwise call
/// the provider inline (plan: "chat/wiki/reports return a structured 'out
/// of AI credits' error"). Everything else — atom CRUD above all — stays
/// fully functional; background work is held by the dispatcher's pause
/// gate, not by this guard.
///
/// Deliberately NOT guarded: semantic search and tag compaction also spend
/// credits, but they degrade with the provider's own error rather than
/// pre-empting — they're accents, not the product's blocked surface; and
/// the embedding-management retry routes only *enqueue* ledger work, which
/// the pause already holds.
pub fn ai_interactive_route(method: &Method, path: &str) -> bool {
    if *method != Method::POST {
        return false;
    }
    // Chat: POST /api/conversations/{id}/messages.
    if let Some(rest) = path.strip_prefix("/api/conversations/") {
        if let Some((id, tail)) = rest.split_once('/') {
            return !id.is_empty() && tail == "messages";
        }
        return false;
    }
    // Wiki synthesis: POST /api/wiki/{tag}/(generate|update|propose).
    if let Some(rest) = path.strip_prefix("/api/wiki/") {
        if let Some((tag, tail)) = rest.split_once('/') {
            return !tag.is_empty() && matches!(tail, "generate" | "update" | "propose");
        }
        return false;
    }
    // Reports: POST /api/reports/{id}/run.
    if let Some(rest) = path.strip_prefix("/api/reports/") {
        if let Some((id, tail)) = rest.split_once('/') {
            return !id.is_empty() && tail == "run";
        }
        return false;
    }
    false
}

/// Placeholder upgrade link for the `out_of_ai_credits` body, derived from
/// the request's host (`<sub>.<base>` → `https://app.<base>/billing`). The
/// billing slice owns the real destination; the *shape* of the response is
/// this slice's contract.
fn upgrade_url(host: &str) -> String {
    let host = host.split(':').next().unwrap_or(host);
    let base = host.split_once('.').map(|(_, base)| base).unwrap_or(host);
    format!("https://app.{base}/billing")
}

/// Data-plane middleware: while the tenant is paused with
/// `kind = 'credits'`, the AI-interactive routes return the structured 402
/// (module docs: the rate-limit / credits asymmetry). Reads the pause off
/// [`ResolvedTenant`] — CloudAuth loaded it from the accounts row this
/// request already paid for — so the guard itself does no I/O. Wired inside
/// CloudAuth and the plane guard; a missing extension is skipped
/// defensively (the plane guard already fails such requests closed).
pub async fn out_of_credits_guard(
    req: ServiceRequest,
    next: Next<impl MessageBody + 'static>,
) -> Result<ServiceResponse<actix_web::body::BoxBody>, actix_web::Error> {
    let pause = req
        .extensions()
        .get::<ResolvedTenant>()
        .and_then(|tenant| tenant.provider_pause);
    if let Some(pause) = pause {
        if pause.kind == PauseKind::Credits
            && pause.active_at(Utc::now())
            && ai_interactive_route(req.method(), req.path())
        {
            let host = req
                .headers()
                .get(header::HOST)
                .and_then(|v| v.to_str().ok())
                .or_else(|| req.uri().host())
                .unwrap_or_default();
            let denial = HttpResponse::PaymentRequired().json(serde_json::json!({
                "error": "out_of_ai_credits",
                "message": "This account is out of AI credits. AI features resume \
                            when the allowance resets or after an upgrade; your \
                            notes remain fully accessible and editable.",
                "resets_at": pause.until.to_rfc3339(),
                "upgrade_url": upgrade_url(host),
            }));
            return Ok(req.into_response(denial));
        }
    }
    next.call(req).await.map(|res| res.map_into_boxed_body())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pause_kind_round_trips_through_text() {
        for kind in [
            PauseKind::RateLimit,
            PauseKind::Credits,
            PauseKind::Provider,
        ] {
            assert_eq!(kind.as_str().parse::<PauseKind>().unwrap(), kind);
        }
        assert!(matches!(
            "blocked".parse::<PauseKind>(),
            Err(CloudError::InvalidPauseKind(_))
        ));
    }

    #[test]
    fn pause_from_columns() {
        assert!(ProviderPause::from_columns(None, None).is_none());
        assert!(ProviderPause::from_columns(None, Some("credits")).is_none());

        let until = Utc::now();
        let pause = ProviderPause::from_columns(Some(until), Some("credits")).unwrap();
        assert_eq!(pause.kind, PauseKind::Credits);
        assert_eq!(pause.until, until);

        // Unknown/missing kind degrades to the conservative reading.
        for kind in [None, Some("mystery")] {
            let pause = ProviderPause::from_columns(Some(until), kind).unwrap();
            assert_eq!(pause.kind, PauseKind::RateLimit);
        }
    }

    /// The detection window: exactly `threshold` failures inside the window
    /// trip; fewer don't; stale failures age out. (`tokio::test` because
    /// even a lazy, never-used sqlx pool wants a runtime to exist in.)
    #[tokio::test]
    async fn window_trips_on_threshold_not_before() {
        // A control-plane-free breaker: note_failure never touches the DB.
        let breaker = breaker_without_db();
        let t0 = Instant::now();

        assert!(!breaker.note_failure("acct", t0));
        assert!(!breaker.note_failure("acct", t0 + Duration::from_secs(10)));
        assert!(
            breaker.note_failure("acct", t0 + Duration::from_secs(20)),
            "third failure within 60s must trip"
        );

        // The trip consumed the window: the count restarts.
        assert!(!breaker.note_failure("acct", t0 + Duration::from_secs(21)));

        // Aged-out failures don't count: two old + one new ≠ trip.
        let breaker = breaker_without_db();
        assert!(!breaker.note_failure("acct", t0));
        assert!(!breaker.note_failure("acct", t0 + Duration::from_secs(1)));
        assert!(
            !breaker.note_failure("acct", t0 + Duration::from_secs(120)),
            "failures older than the window must not count toward a trip"
        );
    }

    /// Tenants are isolated: one tenant's failures never advance another's
    /// window.
    #[tokio::test]
    async fn windows_are_per_tenant() {
        let breaker = breaker_without_db();
        let t0 = Instant::now();
        assert!(!breaker.note_failure("a", t0));
        assert!(!breaker.note_failure("a", t0));
        assert!(
            !breaker.note_failure("b", t0),
            "b has one failure, not three"
        );
        assert!(breaker.note_failure("a", t0));
    }

    /// Stale tenants don't accumulate: a tenant that failed a couple of
    /// times and went quiet is swept out of the in-memory map once its
    /// newest failure ages past the window (module docs: the map
    /// self-bounds). Pre-fix, such entries lived until process exit.
    #[tokio::test]
    async fn stale_windows_are_pruned() {
        let breaker = breaker_without_db();
        let t0 = Instant::now();
        assert!(!breaker.note_failure("quiet", t0));
        assert!(!breaker.note_failure("noisy", t0 + Duration::from_secs(30)));
        assert_eq!(breaker.tracked_windows(), 2);

        // 90s later: "quiet"'s newest entry (t0) is past the 60s window —
        // pruned by the next note; "noisy" (30s old... now 60s) survives
        // exactly at the boundary.
        assert!(!breaker.note_failure("noisy", t0 + Duration::from_secs(90)));
        assert_eq!(
            breaker.tracked_windows(),
            1,
            "fully aged-out tenants must be swept from the map"
        );
    }

    /// The failure-disposition policy: provider-class failures defer
    /// (Retry-After clamped; credits/auth on the recheck horizon), logic
    /// failures keep failing.
    #[test]
    fn provider_failure_policy_classifies_and_clamps() {
        let recheck = Duration::from_secs(3600);
        let cap = Duration::from_secs(900);
        let policy = provider_failure_policy(recheck, cap);
        let until = |disposition: FailureDisposition| match disposition {
            FailureDisposition::DeferUntil(ts) => (ts - Utc::now()).num_seconds(),
            FailureDisposition::Fail => panic!("expected a deferral"),
        };

        // Retry-After honored…
        let secs = until(policy("Rate limited, retry after 120 seconds"));
        assert!((115..=125).contains(&secs), "got {secs}s");
        // …and clamped: a hostile 24h hint lands at the 15-minute cap.
        let secs = until(policy("Rate limited, retry after 86400 seconds"));
        assert!(
            (895..=905).contains(&secs),
            "hint must clamp to cap, got {secs}s"
        );
        // No hint → the base delay.
        let secs = until(policy("Rate limited"));
        assert!((55..=65).contains(&secs), "got {secs}s");
        // Credits and auth → the recheck horizon.
        for environmental in [
            "Embedding error: API error (402): out of credits",
            "Wiki error: API error (401): bad key",
        ] {
            let secs = until(policy(environmental));
            assert!(
                (3595..=3605).contains(&secs),
                "got {secs}s for {environmental:?}"
            );
        }
        // Transient 5xx/timeout faults defer to the base delay (no
        // Retry-After hint accompanies them) rather than burning budget.
        for transient in [
            "Embedding error: API error (503): service unavailable",
            "Wiki error: Network error: timed out",
        ] {
            let secs = until(policy(transient));
            assert!((55..=65).contains(&secs), "got {secs}s for {transient:?}");
        }
        // Logic failures keep consuming retry budget.
        assert_eq!(
            policy("Parse error: bad JSON"),
            FailureDisposition::Fail,
            "non-provider failures must not defer"
        );
    }

    /// `note_failure` is pure in-memory bookkeeping, so a lazy pool that is
    /// never used lets the window logic run without a database.
    fn breaker_without_db() -> ProviderBreaker {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://unused:unused@127.0.0.1:1/unused")
            .expect("lazy pool");
        ProviderBreaker::new(
            ControlPlane::from_pool_for_tests(pool),
            BreakerConfig::default(),
        )
    }

    #[test]
    fn interactive_route_matcher() {
        let post = Method::POST;
        for guarded in [
            "/api/conversations/abc-123/messages",
            "/api/wiki/tag-1/generate",
            "/api/wiki/tag-1/update",
            "/api/wiki/tag-1/propose",
            "/api/reports/r-1/run",
        ] {
            assert!(
                ai_interactive_route(&post, guarded),
                "{guarded} must be guarded"
            );
        }
        for open in [
            "/api/atoms",
            "/api/conversations",
            "/api/conversations/abc-123/scope",
            "/api/conversations//messages",
            "/api/wiki/tag-1/proposal/accept",
            "/api/wiki/tag-1",
            "/api/reports",
            "/api/reports/r-1/findings",
            "/api/search",
        ] {
            assert!(!ai_interactive_route(&post, open), "{open} must stay open");
        }
        // Only POST is interactive; reads on the same paths stay open.
        assert!(!ai_interactive_route(
            &Method::GET,
            "/api/conversations/abc-123/messages"
        ));
    }

    #[test]
    fn upgrade_url_derives_app_host() {
        assert_eq!(
            upgrade_url("kenny.atomic.cloud"),
            "https://app.atomic.cloud/billing"
        );
        assert_eq!(
            upgrade_url("kenny.cloudtest.local:8080"),
            "https://app.cloudtest.local/billing"
        );
    }
}
