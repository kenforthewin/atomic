//! The failure-recovery reaper (plan: "Failure recovery & the reaper";
//! "Signup" → safety-net reaper; slice-1 Implementation-log follow-ups).
//!
//! One periodic job. Every serve process runs [`run_reaper_pass`] on an
//! interval (`main.rs` wires the loop with a jittered start; the pass itself
//! is a plain async function so tests and operators can call it directly).
//! A pass has six arms, in order:
//!
//! 1. **Stuck provisions** — `accounts` rows parked in
//!    `status = 'provisioning'` longer than
//!    [`ReaperPolicy::stuck_provision_age`]. A crashed signup leaves exactly
//!    this behind by design (see `account_plane` "Completion semantics").
//!    The reaper first attempts a *resume* via [`provision_account`] —
//!    every provisioning step is idempotent, so a crash at any point re-runs
//!    to completion. Only when the resume itself fails is the provision
//!    *rolled back*: accounts row hard-deleted (freeing the subdomain
//!    immediately), managed runtime key deleted via the provisioning API
//!    (best-effort; read before the row delete — the CASCADE sweeps the
//!    credentials row holding its id), tenant database dropped. There is no
//!    `'failed'`
//!    tombstone — v1 is hard-delete everywhere, and a tombstone row would
//!    keep holding the UNIQUE subdomain hostage (or force a non-additive
//!    constraint migration to stop it). The loud `tracing::error!` carrying
//!    the account's email is the operator's trace. Resume attempts are
//!    capped per pass ([`ReaperPolicy::max_resumes_per_pass`]) because each
//!    one can run tenant migrations for seconds; surplus rows are deferred
//!    to the next pass, and a row that keeps failing never wedges the loop —
//!    every row's outcome is independent.
//!
//!    Rollback is **classified**, not reflexive: a resume that failed at
//!    the managed-key provisioning step (the typed
//!    [`CloudError::ProviderProvisioning`] class) is a *provider outage*,
//!    not a broken provision — every other step already converged
//!    idempotently, and one more pass after the API recovers completes the
//!    signup. Hard-deleting the claim for that would burn a user's
//!    subdomain over OpenRouter downtime. Such rows are deferred
//!    ([`ReaperSummary::stuck_deferred_provider_outage`]) and retried next
//!    pass — up to [`ReaperPolicy::provision_rollback_ceiling`], past which
//!    the row rolls back regardless so an extended outage can't accumulate
//!    unbounded zombie claims. Every other failure class still rolls back
//!    immediately.
//!
//! 2. **Orphaned tenant databases** — `acct_*` databases (the exact
//!    [`is_tenant_db_name`] shape) with **no** `accounts` row and **no**
//!    `account_databases` row. These are the loudly-logged debris of failed
//!    23503 cleanup in `provision_account` and of this module's own rollback
//!    crash window (see below). The accounts-row absence is the safety
//!    proof: provisioning creates its accounts row *before* `CREATE
//!    DATABASE` and re-verifies it pre-CREATE, so any in-flight provision's
//!    database always has its row. Absence is re-checked immediately before
//!    the drop, under the advisory lock derived from the database name's
//!    embedded account id ([`tenant_db_account_id`]).
//!
//! 3. **Interrupted deletions** — `accounts` rows in `status = 'active'`,
//!    older than [`ReaperPolicy::deletion_recovery_grace`], with **no**
//!    `account_databases` row. A healthy active account ALWAYS has a
//!    mapping row — provisioning inserts it (step 10) strictly *before*
//!    activation (step 11), and only `delete_account` (its step 5) or the
//!    accounts-row CASCADE ever removes one — so the predicate cannot
//!    false-positive; active-without-mapping is precisely a
//!    `delete_account` that died between removing the mapping and deleting
//!    the accounts row. (The tempting cheaper signal "all tokens revoked
//!    and no sessions" is *unsound* on its own: a dormant account that
//!    never minted a token and whose sessions expired looks identical.)
//!    Recovery is completing the deletion via [`delete_account`] — it is
//!    idempotent, re-parks the subdomain, and removes the row. This arm
//!    deliberately does **not** depend on the deletion's `subdomains_
//!    reserved` self-reservation marker, so its ordering relative to arm 4
//!    (which clears such markers) is not load-bearing — but it still runs
//!    *before* arm 4 so a half-deleted account's marker is consumed by the
//!    completed deletion rather than transiently cleared and re-parked.
//!    Note the grace is keyed on `created_at` (accounts carry no deletion
//!    timestamp), so on old accounts this arm can race a deletion's
//!    milliseconds-wide step-5→6 window; that is safe — `delete_account`
//!    is idempotent and writes its reservation before the row delete.
//!
//! 4. **Lagging tenant migrations** — every active `account_databases` row
//!    whose `last_migrated_version` lags the compiled target and whose
//!    backoff horizon, if any, has passed
//!    ([`fleet_migration::list_retryable_failures`]). This arm owns *all*
//!    lagging rows, not just those with recorded failure state: the boot
//!    fleet runner enumerates exactly once per pod lifetime, so a row that
//!    becomes lagging afterwards — an old-binary pod completing a signup
//!    mid-rolling-deploy, a lost success/failure recording, a panicked
//!    migration task — has no other owner, and CloudAuth 503s its every
//!    request (`account_upgrading`) until something retries it. This arm is
//!    that something, and it is what heals every straggler short of another
//!    pod boot. Each due row is retried through the boot runner's own
//!    per-tenant step ([`fleet_migration::migrate_tenant`]): success stamps
//!    the target and clears any failure state (the straggler starts serving
//!    on its next request); failure records normal failure state with a
//!    doubled backoff horizon — a never-attempted row that fails its first
//!    reaper retry enters the same backoff discipline as a boot-run
//!    failure. Retries are capped per pass
//!    ([`ReaperPolicy::max_migration_retries_per_pass`]) for the same
//!    reason resumes are; a row whose *recorded* retry count climbs past
//!    [`ReaperPolicy::migration_alert_retries`] is escalated to
//!    `tracing::error!` (plan: "alerts when retry_count > 5") — backoff has
//!    reached its cap and the tenant needs a human
//!    (`atomic-cloud deploy status` shows the stored error).
//!
//! 5. **Self-reservations** — `subdomains_reserved` rows whose subdomain
//!    belongs to an *active* account: the residue of a deletion that crashed
//!    between reserving the subdomain and deleting the accounts row (the
//!    slice-1 follow-up). Cleared only once the reservation is older than
//!    [`ReaperPolicy::self_reservation_grace`] — a *fresh* self-reservation
//!    is a deletion in flight right now, mid-way between its reserve and
//!    row-delete steps, and clearing it would lose the 90-day park
//!    (`delete_account` re-ups `created_at` on its upsert precisely so
//!    retried deletions regain this shield). An account arm 3 just finished
//!    deleting is invisible here (the join needs an *active* accounts row),
//!    which is why the two arms cannot fight.
//!
//! 6. **Hygiene** — purge `magic_links` expired more than
//!    [`ReaperPolicy::magic_link_retention_after_expiry`] ago (the recently
//!    expired keep their forensic value a little longer), expired
//!    `sessions`, and lapsed `subdomains_reserved` rows. All three are
//!    already inert before purging (every consumer filters on expiry); this
//!    is table size, not correctness.
//!
//! # Concurrency
//!
//! Multiple processes run reapers concurrently, against live signups and
//! deletions (none of which take reaper locks). Three mechanisms make that
//! safe; each arm documents which it leans on:
//!
//! - **Per-account advisory locks.** Row processing in arms 1–4 happens
//!   under `pg_try_advisory_lock` on the control plane, keyed by
//!   [`reaper_lock_key`] over the account id. Contention means another pass
//!   owns the row — skip it (recorded in the summary), never wait. The lock
//!   is session-level on a dedicated connection, so releasing is closing
//!   the session: even a cancelled future can't strand it. Keying arm 2 on
//!   the *database name's* account id makes orphan-reclaim and
//!   stuck-rollback mutually exclusive per account — the rollback's
//!   row-deleted-but-not-yet-dropped window is invisible to other passes.
//!   Arm 4's retry can additionally race a *booting pod's* fleet runner
//!   (which takes no reaper locks); that race is safe by the same two
//!   mechanisms the multi-pod boot itself leans on — atomic-core's
//!   per-database migration advisory lock serializes the actual DDL, and
//!   both sides' recordings converge (monotone `GREATEST` stamping on
//!   success; a failure recording that loses to a concurrent success is
//!   dropped by its version guard, and one that wins just re-arms a backoff
//!   the next success clears).
//!
//! - **Under-lock re-checks.** The work lists are read unlocked, so every
//!   row is re-verified after its lock is acquired: arm 1 re-reads the
//!   accounts row (a concurrent pass — or the user's own retried signup —
//!   may have resumed, rolled back, or activated it), arm 2 re-proves the
//!   absence of both control-plane rows, and arm 3 re-proves
//!   active-without-mapping.
//!
//! - **Existing idempotency guarantees.** The resume *is*
//!   [`provision_account`]; the rollback's claim is a status-guarded
//!   `DELETE`; every database drop is `IF EXISTS`. A reaper racing a live
//!   signup or deletion degrades to one side losing cleanly, never to
//!   double-processing (the interleavings are walked through on
//!   [`roll_back_stuck_provision`]).
//!
//! Arms 5 and 6 are single atomic SQL statements — concurrent passes
//! running them twice is harmless (the second deletes nothing), so they
//! take no locks.

use std::time::Duration;

use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use sqlx::{Connection, PgConnection};
use uuid::Uuid;

use crate::control_plane::ControlPlane;
use crate::error::CloudError;
use crate::managed_keys::ManagedKeys;
use crate::provision::{
    is_tenant_db_name, provision_account, tenant_db_account_id, tenant_db_name,
    terminate_and_drop_database, ClusterConfig, NewAccount,
};

/// Maximum tenant databases the orphan arm (arm 2) will drop in a single
/// pass. The orphan drop is irreversible, so a pass that suddenly believes
/// dozens of live tenants are unreferenced — the signature of a misdirected
/// `--control-url` after a failover or a half-restored control plane — must
/// not be able to take the whole fleet down in one sweep. Surplus orphans
/// are left for the next pass, which only proceeds if the control plane
/// still disowns them; a genuine backlog drains a few passes later, but a
/// misconfiguration is caught with at most this many casualties.
const MAX_ORPHAN_DROPS_PER_PASS: usize = 10;

/// Orphan-candidate count above which the arm logs a loud `warn!` even when
/// the control plane is non-empty. A healthy fleet accrues orphans one at a
/// time (a crashed signup, a rollback window); a count this high is debris
/// worth an operator's attention regardless of the drop cap.
const IMPLAUSIBLE_ORPHAN_COUNT: usize = 10;

/// Tunables for a reaper pass. [`Default`] is the production configuration;
/// tests shrink fields to drive specific arms. `Debug` is hand-written (the
/// backup store is a trait object); see the impl below.
#[derive(Clone)]
pub struct ReaperPolicy {
    /// How long an account may sit in `status='provisioning'` before it
    /// counts as stuck (plan: 5 minutes). Anything younger is an in-flight
    /// signup and is left strictly alone.
    pub stuck_provision_age: Duration,

    /// Maximum resume attempts (and therefore rollbacks — a rollback only
    /// follows a failed resume) per pass. Each resume can run tenant
    /// migrations for seconds; the cap keeps a backlog of stuck rows from
    /// turning a 60-second job into a minutes-long one. Surplus rows are
    /// deferred to the next pass, recorded in
    /// [`ReaperSummary::stuck_deferred`]. Rows found already settled under
    /// their lock cost one indexed read and do **not** count against the
    /// cap — settled rows must not crowd out real work.
    pub max_resumes_per_pass: usize,

    /// Ceiling on how long a stuck provision may keep being deferred when
    /// its resume fails at the managed-key provisioning step (a provider
    /// outage; module docs). Rows younger than this are left for the next
    /// pass — the claim survives the outage; rows older roll back
    /// regardless, bounding zombie-claim accumulation. Default 60 minutes.
    pub provision_rollback_ceiling: Duration,

    /// Minimum account age before arm 3 treats an active-without-mapping
    /// account as an interrupted deletion. The predicate itself is sound at
    /// any age (see the module docs); the grace is defensive depth — it
    /// keeps the arm out of brand-new accounts entirely and bounds how soon
    /// the reaper competes with a deletion that is still being retried by
    /// its own caller.
    pub deletion_recovery_grace: Duration,

    /// Maximum lagging-migration retries (arm 4) per pass — the same
    /// rationale as [`max_resumes_per_pass`](Self::max_resumes_per_pass):
    /// each retry can run tenant migrations for seconds (or burn a connect
    /// timeout on an unreachable database), and a backlog must not turn the
    /// 60-second job into a minutes-long one. Surplus rows are deferred to
    /// the next pass ([`ReaperSummary::migrations_deferred`]); a *large*
    /// backlog of lagging tenants is the boot runner's problem (a restart
    /// re-runs the whole fleet under real concurrency) — the reaper is the
    /// per-row backstop, not a second fleet runner.
    pub max_migration_retries_per_pass: usize,

    /// Escalation threshold for arm 4 (plan: "alerts when
    /// `retry_count > 5`"): after a failed retry leaves a row's
    /// `migration_retry_count` *above* this, the row is logged at error
    /// level and recorded in [`ReaperSummary::migration_alerts`]. By count
    /// 5 the backoff has hit its cap — every further failure means the
    /// tenant needs a human, and re-alerting once per capped horizon is the
    /// alert staying honest, not spam.
    pub migration_alert_retries: i32,

    /// Per-tenant knobs for arm 4's retries — deliberately the boot
    /// runner's own config type, so `serve` can hand the reaper the exact
    /// `--fleet-*` configuration and the two writers of
    /// `migration_retry_after` can never disagree on backoff arithmetic.
    /// Only the per-tenant fields apply (`tenant_connect_timeout`,
    /// `retry_backoff_base`, `retry_backoff_cap`); the run-shaping fields
    /// (`concurrency`, `wall_clock_limit`) govern fleet runs and are unused
    /// here — arm 4 is serial under its per-pass cap.
    pub migration_retry: crate::fleet_migration::FleetMigrationConfig,

    /// Minimum age of a self-reservation before arm 5 clears it. Younger
    /// rows are presumed to be a `delete_account` in flight between its
    /// reserve and row-delete steps.
    pub self_reservation_grace: Duration,

    /// How long expired magic links are retained before the hygiene arm
    /// purges them. The 24 hours are this implementation's choice (the plan
    /// doesn't fix a retention). Links are inert the moment they expire;
    /// the retention only preserves the forensic breadcrumbs
    /// (`request_ip`, timing) for a debugging window.
    pub magic_link_retention_after_expiry: Duration,

    /// Backup store handed to arm 3's [`delete_account`] so an interrupted
    /// deletion the reaper *completes* still takes the final pre-drop dump
    /// (plan: "Account deletion" step 4) — a deletion that died before the
    /// drop holds real user data that must be backed up before the reaper
    /// finishes destroying it. `None` (the [`Default`]) takes no dump, which
    /// is correct only for paths with no real data to lose; `serve` always
    /// sets it. The stuck-provision rollback and orphan-reclaim arms drop
    /// never-activated databases and deliberately don't route through
    /// `delete_account`, so they never dump regardless of this field.
    pub backup_store: Option<std::sync::Arc<dyn crate::backup_store::BackupStore>>,

    /// Per-`pg_dump` budget for arm 3's final dump (adversarial-review issue
    /// 1); see [`BackupConfig::backup_timeout`](crate::BackupConfig::backup_timeout).
    /// [`Default`] is [`DEFAULT_BACKUP_TIMEOUT`](crate::backup::DEFAULT_BACKUP_TIMEOUT).
    pub backup_timeout: Duration,
}

impl std::fmt::Debug for ReaperPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The backup store is a trait object (not Debug); render only its
        // presence so `?summary`-style logs stay useful without requiring a
        // Debug bound on every BackupStore impl.
        f.debug_struct("ReaperPolicy")
            .field("stuck_provision_age", &self.stuck_provision_age)
            .field("max_resumes_per_pass", &self.max_resumes_per_pass)
            .field(
                "provision_rollback_ceiling",
                &self.provision_rollback_ceiling,
            )
            .field("deletion_recovery_grace", &self.deletion_recovery_grace)
            .field(
                "max_migration_retries_per_pass",
                &self.max_migration_retries_per_pass,
            )
            .field("migration_alert_retries", &self.migration_alert_retries)
            .field("migration_retry", &self.migration_retry)
            .field("self_reservation_grace", &self.self_reservation_grace)
            .field(
                "magic_link_retention_after_expiry",
                &self.magic_link_retention_after_expiry,
            )
            .field("backup_store", &self.backup_store.is_some())
            .finish()
    }
}

impl Default for ReaperPolicy {
    fn default() -> Self {
        Self {
            stuck_provision_age: Duration::from_secs(5 * 60),
            // One pass is at most as provision-heavy as the signup plane.
            max_resumes_per_pass: crate::account_plane::DEFAULT_MAX_CONCURRENT_PROVISIONS,
            provision_rollback_ceiling: Duration::from_secs(60 * 60),
            deletion_recovery_grace: Duration::from_secs(5 * 60),
            // Worst case ~80s of fast-fail connect timeouts per pass under
            // the default 10s tenant_connect_timeout.
            max_migration_retries_per_pass: 8,
            // The plan's number ("alerts when retry_count > 5").
            migration_alert_retries: 5,
            migration_retry: crate::fleet_migration::FleetMigrationConfig::default(),
            self_reservation_grace: Duration::from_secs(5 * 60),
            magic_link_retention_after_expiry: Duration::from_secs(24 * 60 * 60),
            backup_store: None,
            backup_timeout: crate::backup::DEFAULT_BACKUP_TIMEOUT,
        }
    }
}

/// Everything a pass did (and declined to do), for logging and for tests —
/// notably, advisory-lock skips are observable here, which is how the
/// concurrent-pass tests prove rows are never double-processed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReaperSummary {
    /// Stuck provisions completed by re-running [`provision_account`]
    /// (account ids; the accounts are now `'active'`).
    pub stuck_resumed: Vec<String>,
    /// Stuck provisions whose resume failed and were rolled back (account
    /// ids; rows hard-deleted, databases dropped, subdomains freed).
    pub stuck_rolled_back: Vec<String>,
    /// Stuck provisions skipped because another pass holds their lock.
    pub stuck_skipped_locked: Vec<String>,
    /// Stuck provisions past [`ReaperPolicy::max_resumes_per_pass`],
    /// deferred to the next pass untouched.
    pub stuck_deferred: Vec<String>,
    /// Stuck provisions whose resume failed at the managed-key provisioning
    /// step (a provider outage) and are still inside
    /// [`ReaperPolicy::provision_rollback_ceiling`]: rollback skipped, row
    /// left for the next pass (module docs: classified rollback).
    pub stuck_deferred_provider_outage: Vec<String>,
    /// Orphaned tenant databases dropped (database names).
    pub orphan_dbs_dropped: Vec<String>,
    /// Orphan candidates skipped because another pass holds the lock for
    /// their embedded account id.
    pub orphan_dbs_skipped_locked: Vec<String>,
    /// `true` when the orphan arm refused to reclaim anything this pass
    /// because the control plane reported **zero** accounts while the
    /// cluster held tenant databases — the signature of a misdirected
    /// `--control-url` (failover / half-restored control plane), not of a
    /// genuinely empty fleet. The arm warns and skips rather than dropping.
    pub orphan_reclaim_refused: bool,
    /// Orphan candidates left undropped this pass because the per-pass cap
    /// ([`MAX_ORPHAN_DROPS_PER_PASS`]) was reached (database names). They are
    /// reconsidered next pass — and only dropped if still disowned.
    pub orphan_dbs_capped: Vec<String>,
    /// Interrupted deletions completed (account ids; rows gone, subdomains
    /// re-parked).
    pub deletions_completed: Vec<String>,
    /// Interrupted-deletion candidates skipped because another pass holds
    /// their lock.
    pub deletions_skipped_locked: Vec<String>,
    /// Lagging tenant migrations retried to success (account ids; stamps
    /// current, failure state cleared — the stragglers stop 503ing).
    pub migrations_recovered: Vec<String>,
    /// Lagging-migration retries whose rows are still lagging after the
    /// attempt (account ids) — the migration failed (failure state and a
    /// fresh backoff horizon recorded, when that write itself landed), or
    /// it succeeded but the success recording was lost.
    pub migrations_still_failing: Vec<String>,
    /// Lagging-migration rows skipped because another pass holds their lock.
    pub migrations_skipped_locked: Vec<String>,
    /// Lagging-migration rows past
    /// [`ReaperPolicy::max_migration_retries_per_pass`], deferred untouched.
    pub migrations_deferred: Vec<String>,
    /// Lagging-migration rows whose *recorded* retry count climbed past
    /// [`ReaperPolicy::migration_alert_retries`] this pass — each was
    /// escalated with a `tracing::error!` naming the tenant. A failure
    /// whose recording write itself failed does not bump the count and so
    /// never inflates this alert (the row's stored count is the truth).
    pub migration_alerts: Vec<String>,
    /// Reservations cleared because their subdomain belongs to an active
    /// account (subdomains).
    pub self_reservations_cleared: Vec<String>,
    /// Hygiene: long-expired magic links purged.
    pub expired_magic_links_purged: u64,
    /// Hygiene: expired sessions purged.
    pub expired_sessions_purged: u64,
    /// Hygiene: lapsed subdomain reservations purged.
    pub expired_reservations_purged: u64,
    /// Per-row and per-arm failures, with context. A failure never stops
    /// the pass — the row stays for the next one — but it is recorded here
    /// (and warned) rather than silently swallowed.
    pub errors: Vec<String>,
}

impl ReaperSummary {
    /// True when the pass found nothing to act on (the steady state) —
    /// callers log quiet passes at debug instead of info.
    pub fn is_quiet(&self) -> bool {
        self.stuck_resumed.is_empty()
            && self.stuck_rolled_back.is_empty()
            && self.stuck_skipped_locked.is_empty()
            && self.stuck_deferred.is_empty()
            && self.stuck_deferred_provider_outage.is_empty()
            && self.orphan_dbs_dropped.is_empty()
            && self.orphan_dbs_skipped_locked.is_empty()
            && !self.orphan_reclaim_refused
            && self.orphan_dbs_capped.is_empty()
            && self.deletions_completed.is_empty()
            && self.deletions_skipped_locked.is_empty()
            && self.migrations_recovered.is_empty()
            && self.migrations_still_failing.is_empty()
            && self.migrations_skipped_locked.is_empty()
            && self.migrations_deferred.is_empty()
            && self.migration_alerts.is_empty()
            && self.self_reservations_cleared.is_empty()
            && self.expired_magic_links_purged == 0
            && self.expired_sessions_purged == 0
            && self.expired_reservations_purged == 0
            && self.errors.is_empty()
    }
}

/// Advisory-lock key for an account id (canonical hyphenated-lowercase UUID
/// string): the first 8 bytes of SHA-256 over a domain-separated input.
///
/// Public so tests can hold a contending lock and observe the skip. A hash
/// collision (with another account or with the control plane's migration
/// lock) costs a spurious skip retried next pass — never a safety problem,
/// because the lock only ever *defers* work.
pub fn reaper_lock_key(account_id: &str) -> i64 {
    let digest = Sha256::digest(format!("atomic-cloud:reaper:{account_id}").as_bytes());
    i64::from_be_bytes(digest[..8].try_into().expect("SHA-256 yields 32 bytes"))
}

/// Run one reaper pass. Never fails as a whole: per-row and per-arm errors
/// land in [`ReaperSummary::errors`] (and a `tracing::warn!`), and the
/// remaining work proceeds — a row that keeps failing must not wedge the
/// loop, and a broken arm must not starve the others.
pub async fn run_reaper_pass(
    control: &ControlPlane,
    cluster: &ClusterConfig,
    managed: &ManagedKeys,
    policy: &ReaperPolicy,
) -> ReaperSummary {
    let mut summary = ReaperSummary::default();
    if let Err(e) = reap_stuck_provisions(control, cluster, managed, policy, &mut summary).await {
        record_error(&mut summary, "stuck-provision arm", &e);
    }
    if let Err(e) = reap_orphaned_tenant_databases(control, cluster, &mut summary).await {
        record_error(&mut summary, "orphaned-database arm", &e);
    }
    if let Err(e) =
        complete_interrupted_deletions(control, cluster, managed, policy, &mut summary).await
    {
        record_error(&mut summary, "interrupted-deletion arm", &e);
    }
    if let Err(e) = retry_failed_migrations(control, cluster, policy, &mut summary).await {
        record_error(&mut summary, "failed-migration arm", &e);
    }
    if let Err(e) = clear_self_reservations(control, policy, &mut summary).await {
        record_error(&mut summary, "self-reservation arm", &e);
    }
    if let Err(e) = purge_expired_rows(control, policy, &mut summary).await {
        record_error(&mut summary, "hygiene arm", &e);
    }
    summary
}

fn record_error(summary: &mut ReaperSummary, context: &str, e: &CloudError) {
    tracing::warn!(error = %e, "reaper: {context} failed");
    summary.errors.push(format!("{context}: {e}"));
}

/// `NOW() - age`, saturating: an absurdly large policy duration yields the
/// minimum timestamp, which matches nothing — the fail-safe direction.
fn cutoff(age: Duration) -> DateTime<Utc> {
    let age = chrono::Duration::from_std(age).unwrap_or(chrono::Duration::MAX);
    Utc::now()
        .checked_sub_signed(age)
        .unwrap_or(DateTime::<Utc>::MIN_UTC)
}

/// Try to take the per-account advisory lock on a dedicated, pool-detached
/// control-plane session, returning the held connection. `Ok(None)` means
/// another holder (a concurrent reaper pass on another pod, or the nightly
/// backup pass — both key on [`reaper_lock_key`]) owns it: skip the work,
/// never wait. The lock is session-level, so the caller releases it by
/// **closing the returned connection** (`conn.close().await`); even a
/// cancelled future dropping the connection ends the session and frees the
/// lock — it can never strand on a connection returned to the pool.
///
/// Public so the nightly backup pass ([`crate::backups`]) **and every
/// active-account deletion path** take the *same* per-account lock the reaper
/// does. A backup of an active tenant and a drop of that same tenant are then
/// mutually exclusive, so a dump can never race a `DROP DATABASE`:
///
/// - the **reaper's** interrupted-deletion arm takes this lock per row before
///   calling [`delete_account`](crate::provision::delete_account) (in
///   [`DeleteLock::AlreadyHeld`](crate::provision::DeleteLock::AlreadyHeld)
///   mode, so it doesn't self-deadlock re-acquiring its own hold);
/// - the **HTTP route** and the **CLI** call `delete_account` in
///   [`DeleteLock::Acquire`](crate::provision::DeleteLock::Acquire) mode, which
///   takes this same lock itself around the final-dump-and-drop window.
///
/// So the invariant — a backup pass mid-`pg_dump` of tenant X can never race a
/// delete that drops X — now holds for the route and CLI too, not just the
/// reaper's own drop (adversarial-review issue 2). A delete that finds the lock
/// held by a backup pass waits briefly, then returns [`CloudError::Busy`]
/// (route → 503 retry) rather than racing.
pub async fn try_account_advisory_lock(
    control: &ControlPlane,
    account_id: &str,
) -> Result<Option<PgConnection>, CloudError> {
    let mut conn = control
        .pool()
        .acquire()
        .await
        .map_err(CloudError::db("acquiring advisory-lock connection"))?
        .detach();
    let acquired: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
        .bind(reaper_lock_key(account_id))
        .fetch_one(&mut conn)
        .await
        .map_err(CloudError::db("taking account advisory lock"))?;
    if acquired {
        Ok(Some(conn))
    } else {
        let _ = conn.close().await;
        Ok(None)
    }
}

/// A held per-account reaper lock: a dedicated control-plane session owning
/// `pg_try_advisory_lock(reaper_lock_key(account_id))`. Session-level on a
/// connection detached from the pool, so the lock can never leak back into
/// the pool on a returned connection; releasing is closing the session,
/// which Postgres honors even when the future holding this guard is dropped
/// mid-flight.
struct AccountLock {
    conn: PgConnection,
}

impl AccountLock {
    /// Try to take the lock; `Ok(None)` means another pass holds it — skip
    /// the row, never wait.
    async fn try_acquire(
        control: &ControlPlane,
        account_id: &str,
    ) -> Result<Option<Self>, CloudError> {
        Ok(try_account_advisory_lock(control, account_id)
            .await?
            .map(|conn| Self { conn }))
    }

    /// Release by ending the session — unconditional, no unlock call that
    /// could itself fail and strand the lock.
    async fn release(self) {
        let _ = self.conn.close().await;
    }
}

/// What arm 1 decided about one stuck row, post-lock.
enum StuckOutcome {
    Resumed,
    RolledBack,
    /// Resume failed at the managed-key provisioning step (provider
    /// outage), row inside the rollback ceiling: rollback skipped, row left
    /// intact for the next pass (module docs: classified rollback).
    DeferredProviderOutage,
    /// The under-lock re-check found the row already handled (activated,
    /// rolled back, or refreshed) by a concurrent actor — nothing to do.
    AlreadySettled,
}

/// Arm 1 — stuck provisions (plan: "accounts WHERE status='provisioning'
/// AND created_at < now() - interval '5 minutes'").
async fn reap_stuck_provisions(
    control: &ControlPlane,
    cluster: &ClusterConfig,
    managed: &ManagedKeys,
    policy: &ReaperPolicy,
    summary: &mut ReaperSummary,
) -> Result<(), CloudError> {
    let stale_before = cutoff(policy.stuck_provision_age);
    let stale: Vec<String> = sqlx::query_scalar(
        "SELECT id FROM accounts \
         WHERE status = 'provisioning' AND created_at < $1 \
         ORDER BY created_at",
    )
    .bind(stale_before)
    .fetch_all(control.pool())
    .await
    .map_err(CloudError::db("listing stuck provisions"))?;

    let mut resumes_used = 0usize;
    for account_id in stale {
        if resumes_used >= policy.max_resumes_per_pass {
            summary.stuck_deferred.push(account_id);
            continue;
        }
        let lock = match AccountLock::try_acquire(control, &account_id).await {
            Ok(Some(lock)) => lock,
            Ok(None) => {
                summary.stuck_skipped_locked.push(account_id);
                continue;
            }
            Err(e) => {
                record_error(
                    summary,
                    &format!("locking stuck provision {account_id}"),
                    &e,
                );
                continue;
            }
        };
        // Process under the lock, then ALWAYS release — outcomes (including
        // errors) are values here precisely so no path skips the release.
        let outcome =
            process_stuck_provision(control, cluster, managed, policy, &account_id, stale_before)
                .await;
        lock.release().await;
        // Only attempts that actually did resume work count against the
        // cap. AlreadySettled cost one indexed re-read — charging it would
        // let rows settled by concurrent actors crowd real work out of the
        // pass. Errors count: a failed resume may well have burned the
        // multi-second budget the cap exists to bound — as does a deferred
        // provider outage, whose resume attempt ran in full before failing.
        if !matches!(outcome, Ok(StuckOutcome::AlreadySettled)) {
            resumes_used += 1;
        }
        match outcome {
            Ok(StuckOutcome::Resumed) => summary.stuck_resumed.push(account_id),
            Ok(StuckOutcome::RolledBack) => summary.stuck_rolled_back.push(account_id),
            Ok(StuckOutcome::DeferredProviderOutage) => {
                summary.stuck_deferred_provider_outage.push(account_id)
            }
            Ok(StuckOutcome::AlreadySettled) => {}
            Err(e) => record_error(
                summary,
                &format!("processing stuck provision {account_id}"),
                &e,
            ),
        }
    }
    Ok(())
}

/// One stuck row, lock held: re-check, resume, and only if the resume fails
/// — for a reason that isn't a deferrable provider outage — roll back.
async fn process_stuck_provision(
    control: &ControlPlane,
    cluster: &ClusterConfig,
    managed: &ManagedKeys,
    policy: &ReaperPolicy,
    account_id: &str,
    stale_before: DateTime<Utc>,
) -> Result<StuckOutcome, CloudError> {
    // Re-read under the lock: between the unlocked listing and lock
    // acquisition, a concurrent pass (or the user's own retried signup) may
    // have resumed, rolled back, or activated the account.
    let row: Option<(String, String, String, DateTime<Utc>)> =
        sqlx::query_as("SELECT email, subdomain, status, created_at FROM accounts WHERE id = $1")
            .bind(account_id)
            .fetch_optional(control.pool())
            .await
            .map_err(CloudError::db("re-reading stuck provision under lock"))?;
    let Some((email, subdomain, status, created_at)) = row else {
        return Ok(StuckOutcome::AlreadySettled);
    };
    if status != "provisioning" || created_at >= stale_before {
        return Ok(StuckOutcome::AlreadySettled);
    }

    // First choice: resume. provision_account is idempotent end-to-end, so
    // whatever step the original signup died at, the re-run converges — and
    // a resume preserves the user's claim where a rollback burns it.
    let resume_err = match provision_account(
        control,
        cluster,
        managed,
        NewAccount {
            email: email.clone(),
            subdomain: subdomain.clone(),
        },
    )
    .await
    {
        Ok(provisioned) => {
            tracing::info!(
                account_id,
                subdomain,
                db_name = provisioned.db_name,
                "reaper resumed stuck provision"
            );
            return Ok(StuckOutcome::Resumed);
        }
        Err(e) => e,
    };

    // Classify before rolling back (module docs): a resume that failed at
    // the managed-key provisioning step is a provider outage — every other
    // step converged idempotently, and the next pass after the API recovers
    // finishes the signup. Rolling back would burn the user's claim over
    // third-party downtime, so defer instead — unless the row has been
    // deferred past the hard ceiling, at which point the outage is no
    // longer "transient" and the claim must stop accumulating.
    if matches!(resume_err, CloudError::ProviderProvisioning { .. })
        && created_at >= cutoff(policy.provision_rollback_ceiling)
    {
        tracing::warn!(
            account_id,
            subdomain,
            resume_error = %resume_err,
            "stuck provision's resume failed at managed-key provisioning \
             (provider outage); deferring rollback to the next pass"
        );
        return Ok(StuckOutcome::DeferredProviderOutage);
    }

    if roll_back_stuck_provision(
        control,
        cluster,
        managed,
        account_id,
        &email,
        &subdomain,
        &resume_err,
    )
    .await?
    {
        Ok(StuckOutcome::RolledBack)
    } else {
        // The status-guarded DELETE matched nothing: a concurrent actor
        // activated (or removed) the account after our resume attempt
        // failed. Their outcome stands; ours is a no-op.
        Ok(StuckOutcome::AlreadySettled)
    }
}

/// Roll back a stuck provision whose resume failed: hard-delete the accounts
/// row, then drop the tenant database(s). Returns `false` when the
/// status-guarded delete found the row already settled.
///
/// # Ordering: row first, database second
///
/// The plan sketches drop-then-free; this inverts it, deliberately. The
/// status-guarded `DELETE ... WHERE status = 'provisioning'` is the atomic
/// claim on the rollback. Once the row is gone, a concurrently *resumed*
/// provision (the user retrying signup doesn't take reaper locks) cannot
/// re-activate the account: its pre-CREATE re-verify fails; a later
/// `account_databases` INSERT hits the FK (SQLSTATE 23503) and self-cleans
/// the database it created; its final activation UPDATE matches zero rows.
/// Dropping the database *first* would open the reverse window — a resume
/// activating the row just after its data was destroyed, leaving a live
/// account with no tenant database.
///
/// The inverted ordering's own crash window (row deleted, drop never ran)
/// is precisely the orphan predicate: the next pass's orphan arm reclaims
/// the database. The same applies when the drop here *fails* — the error is
/// recorded and the orphan arm finishes the job.
///
/// No subdomain reservation is written: a provision that never activated
/// never served anything, so no external client can be pointing at the
/// name, and the whole point of freeing is that the user can immediately
/// retry signup with the same slug. CASCADE FKs sweep any `cloud_tokens`,
/// `sessions`, `account_databases`, and `provider_credentials` rows with
/// the accounts row — which is why any managed runtime key is read before,
/// and deleted after, the row delete (see below).
#[allow(clippy::too_many_arguments)] // One rollback, fully spelled out.
async fn roll_back_stuck_provision(
    control: &ControlPlane,
    cluster: &ClusterConfig,
    managed: &ManagedKeys,
    account_id: &str,
    email: &str,
    subdomain: &str,
    resume_err: &CloudError,
) -> Result<bool, CloudError> {
    // Collect database names BEFORE the delete — the CASCADE wipes the
    // mapping rows with the accounts row. The derived name is unioned in,
    // mirroring delete_account: a provision that crashed before writing the
    // mapping row still gets its database cleaned up.
    let mut db_names: Vec<String> =
        sqlx::query_scalar("SELECT db_name FROM account_databases WHERE account_id = $1")
            .bind(account_id)
            .fetch_all(control.pool())
            .await
            .map_err(CloudError::db("listing stuck provision's databases"))?;
    if let Ok(uuid) = Uuid::parse_str(account_id) {
        let derived = tenant_db_name(uuid);
        if !db_names.contains(&derived) {
            db_names.push(derived);
        }
    }

    // Likewise the managed runtime key id(s): the credentials rows go with
    // the CASCADE, and the database name encodes nothing about them — read
    // now, delete only once the rollback's claim (the status-guarded DELETE
    // below) has actually won. A resume racing this read could mint a key
    // *after* it; that residue is the documented accepted orphan (see
    // crate::managed_keys), bounded by the per-key credit limit.
    let managed_key_ids = managed.managed_key_ids(control, account_id).await?;

    let deleted = sqlx::query("DELETE FROM accounts WHERE id = $1 AND status = 'provisioning'")
        .bind(account_id)
        .execute(control.pool())
        .await
        .map_err(CloudError::db(
            "hard-deleting stuck provision's account row",
        ))?
        .rows_affected();
    if deleted == 0 {
        return Ok(false);
    }

    // Loud, before the drops: the accounts row is already gone, so this log
    // line — with the email — is the operator's only trace of who the
    // failed signup belonged to (no 'failed' tombstone; see module docs).
    tracing::error!(
        account_id,
        email,
        subdomain,
        resume_error = %resume_err,
        "reaper rolled back stuck provision: accounts row hard-deleted, \
         subdomain freed for immediate reuse; this log line is the \
         operator's only trace of the failed signup"
    );

    // The claim won; the CASCADE has swept the credentials rows, so the
    // locally held ids are the last reference to the runtime keys. Delete
    // them BEFORE the database drops below — a drop failure returns early,
    // and the orphan arm that later reclaims the database knows nothing
    // about keys. Best-effort: a provider outage must not wedge the
    // rollback (the row is already correctly gone).
    for external_key_id in &managed_key_ids {
        managed
            .delete_external_key_best_effort(account_id, external_key_id)
            .await;
    }

    let mut conn = cluster.connect_maintenance().await?;
    let mut dropped = Ok(());
    for db_name in &db_names {
        dropped = terminate_and_drop_database(&mut conn, db_name).await;
        if dropped.is_err() {
            break;
        }
    }
    let _ = conn.close().await;
    // A failed drop leaves an orphaned database the next pass's orphan arm
    // reclaims; propagate so the failure is recorded, the row itself is
    // already (correctly) gone.
    dropped?;
    Ok(true)
}

/// Arm 2 — orphaned tenant databases. See the module docs for the safety
/// proof; mechanically: list `acct_*`-shaped databases, and for each with
/// no `accounts` row and no `account_databases` row, re-prove the absence
/// under the account-keyed advisory lock and drop it.
async fn reap_orphaned_tenant_databases(
    control: &ControlPlane,
    cluster: &ClusterConfig,
    summary: &mut ReaperSummary,
) -> Result<(), CloudError> {
    let mut conn = cluster.connect_maintenance().await?;
    let result = scan_orphans(control, &mut conn, summary).await;
    let _ = conn.close().await;
    result
}

async fn scan_orphans(
    control: &ControlPlane,
    conn: &mut PgConnection,
    summary: &mut ReaperSummary,
) -> Result<(), CloudError> {
    // `\_` keeps LIKE's underscore literal; is_tenant_db_name below is the
    // authoritative shape check either way (and the guard every drop
    // re-asserts before interpolating the name into DDL).
    let candidates: Vec<String> = sqlx::query_scalar::<_, String>(
        "SELECT datname FROM pg_database WHERE datname LIKE 'acct\\_%'",
    )
    .fetch_all(&mut *conn)
    .await
    .map_err(CloudError::db("listing tenant-shaped databases"))?
    .into_iter()
    .filter(|name| is_tenant_db_name(name))
    .collect();

    // Data-loss guardrail (REL-4). The under-lock re-check re-queries the
    // SAME control plane that produced the unlocked pre-check, so a control
    // plane that is wrong about *every* account — a `--control-url`
    // misdirected after a failover, or one pointed at a half-restored /
    // empty database — would have this arm disown the entire fleet and drop
    // it within one pass. Before touching anything, ask the control plane how
    // many accounts it knows about: a fleet with live tenant databases but
    // ZERO accounts is implausible enough to be a misconfiguration, so we
    // refuse the whole arm and warn rather than reclaim.
    if !candidates.is_empty() {
        let account_count = control_account_count(control).await?;
        if account_count == 0 {
            tracing::warn!(
                tenant_databases = candidates.len(),
                "reaper: orphan reclaim REFUSED — control plane reports zero \
                 accounts but the cluster holds tenant databases; this is the \
                 signature of a misdirected control-plane URL (failover / \
                 half-restored control plane), not an empty fleet. Dropping \
                 nothing this pass; verify --control-url points at the live \
                 control plane"
            );
            summary.orphan_reclaim_refused = true;
            return Ok(());
        }
        // A non-empty control plane can still accrue a few orphans (a crashed
        // signup, a rollback crash window), but a pile this large is debris
        // worth an operator's eyes regardless of the per-pass cap below.
        if candidates.len() >= IMPLAUSIBLE_ORPHAN_COUNT {
            tracing::warn!(
                orphan_candidates = candidates.len(),
                account_count,
                cap = MAX_ORPHAN_DROPS_PER_PASS,
                "reaper: implausibly high orphan-database count — dropping at \
                 most the per-pass cap and deferring the rest; investigate \
                 whether the control plane is healthy before assuming these \
                 are genuine debris"
            );
        }
    }

    let mut dropped_this_pass = 0usize;
    for db_name in candidates {
        // Per-pass drop cap (REL-4): once we have dropped the cap's worth,
        // every remaining disowned candidate is deferred — recorded, not
        // dropped — so a single misconfigured pass can fell at most
        // `MAX_ORPHAN_DROPS_PER_PASS` tenants before an operator can react. A
        // genuine backlog drains over the following passes (each re-proving
        // the orphan status), which is the safe direction to be slow in.
        if dropped_this_pass >= MAX_ORPHAN_DROPS_PER_PASS {
            summary.orphan_dbs_capped.push(db_name);
            continue;
        }
        let Some(account_uuid) = tenant_db_account_id(&db_name) else {
            continue; // unreachable post-filter; belt and braces
        };
        let account_id = account_uuid.to_string();

        // Unlocked pre-check keeps the common case (every database owned)
        // free of lock traffic.
        match is_referenced(control, &account_id, &db_name).await {
            Ok(true) => continue,
            Ok(false) => {}
            Err(e) => {
                record_error(summary, &format!("checking orphan candidate {db_name}"), &e);
                continue;
            }
        }

        let lock = match AccountLock::try_acquire(control, &account_id).await {
            Ok(Some(lock)) => lock,
            Ok(None) => {
                // Another pass owns this account — possibly a rollback in
                // its row-deleted-but-not-yet-dropped window. Skip; if the
                // database is still orphaned next pass, it gets reclaimed.
                summary.orphan_dbs_skipped_locked.push(db_name);
                continue;
            }
            Err(e) => {
                record_error(summary, &format!("locking orphan candidate {db_name}"), &e);
                continue;
            }
        };
        let outcome: Result<bool, CloudError> = async {
            // The double-check the plan demands: immediately before the
            // drop, under the lock, the rows must STILL be absent — a
            // provision claiming this exact account id between pre-check
            // and lock would have inserted its accounts row first.
            if is_referenced(control, &account_id, &db_name).await? {
                return Ok(false);
            }
            terminate_and_drop_database(&mut *conn, &db_name).await?;
            Ok(true)
        }
        .await;
        lock.release().await;
        match outcome {
            Ok(true) => {
                tracing::warn!(
                    db_name,
                    account_id,
                    "reaper dropped orphaned tenant database (no accounts or \
                     account_databases row referenced it)"
                );
                summary.orphan_dbs_dropped.push(db_name);
                dropped_this_pass += 1;
            }
            Ok(false) => {}
            Err(e) => record_error(
                summary,
                &format!("reclaiming orphaned database {db_name}"),
                &e,
            ),
        }
    }
    Ok(())
}

/// Whether anything in the control plane still claims this database: an
/// `accounts` row for its embedded id, or any `account_databases` row for
/// its name. Either one disqualifies the orphan drop.
async fn is_referenced(
    control: &ControlPlane,
    account_id: &str,
    db_name: &str,
) -> Result<bool, CloudError> {
    sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM accounts WHERE id = $1) \
             OR EXISTS(SELECT 1 FROM account_databases WHERE db_name = $2)",
    )
    .bind(account_id)
    .bind(db_name)
    .fetch_one(control.pool())
    .await
    .map_err(CloudError::db("checking orphan candidate references"))
}

/// Total `accounts` rows the control plane knows about — every status, not
/// just `'active'`, so an in-flight `'provisioning'` row counts. The orphan
/// arm's zero-accounts refusal (REL-4) keys on this: a cluster holding
/// tenant databases while the control plane reports *no* accounts at all is
/// the fingerprint of a misdirected control-plane URL, not an empty fleet.
async fn control_account_count(control: &ControlPlane) -> Result<i64, CloudError> {
    sqlx::query_scalar("SELECT COUNT(*) FROM accounts")
        .fetch_one(control.pool())
        .await
        .map_err(CloudError::db("counting control-plane accounts"))
}

/// Arm 3 — interrupted deletions: active accounts with no
/// `account_databases` row, older than the grace. See the module docs for
/// the soundness proof (a healthy active account always has a mapping row —
/// provision step 10 precedes step 11; only `delete_account` or the
/// accounts-row CASCADE removes mappings). Recovery is the idempotent
/// [`delete_account`] under the same per-account advisory lock arms 1 and 2
/// use; no per-pass cap — completion is a handful of queries plus an
/// `IF EXISTS` drop of an already-dropped database, and half-deleted
/// accounts are rare by construction (the HTTP route is cancellation-proof;
/// this arm exists for pod crashes).
async fn complete_interrupted_deletions(
    control: &ControlPlane,
    cluster: &ClusterConfig,
    managed: &ManagedKeys,
    policy: &ReaperPolicy,
    summary: &mut ReaperSummary,
) -> Result<(), CloudError> {
    let abandoned_before = cutoff(policy.deletion_recovery_grace);
    let candidates: Vec<String> = sqlx::query_scalar(
        "SELECT a.id FROM accounts a \
         WHERE a.status = 'active' \
           AND a.created_at < $1 \
           AND NOT EXISTS (SELECT 1 FROM account_databases ad \
                           WHERE ad.account_id = a.id) \
         ORDER BY a.created_at",
    )
    .bind(abandoned_before)
    .fetch_all(control.pool())
    .await
    .map_err(CloudError::db("listing interrupted deletions"))?;

    for account_id in candidates {
        let lock = match AccountLock::try_acquire(control, &account_id).await {
            Ok(Some(lock)) => lock,
            Ok(None) => {
                summary.deletions_skipped_locked.push(account_id);
                continue;
            }
            Err(e) => {
                record_error(
                    summary,
                    &format!("locking interrupted deletion {account_id}"),
                    &e,
                );
                continue;
            }
        };
        let outcome: Result<bool, CloudError> = async {
            // Under-lock re-check: the unlocked listing may have raced a
            // provision activating... no — activation requires the mapping
            // row to exist first; what it can race is another pass (or the
            // user's own deletion retry) finishing the job, leaving the row
            // gone or re-provisioned. Re-prove active-without-mapping.
            let still_interrupted: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM accounts a \
                 WHERE a.id = $1 AND a.status = 'active' \
                   AND NOT EXISTS (SELECT 1 FROM account_databases ad \
                                   WHERE ad.account_id = a.id))",
            )
            .bind(&account_id)
            .fetch_one(control.pool())
            .await
            .map_err(CloudError::db(
                "re-checking interrupted deletion under lock",
            ))?;
            if !still_interrupted {
                return Ok(false);
            }
            // delete_account's own step 3 deletes any managed runtime key —
            // the credentials row survives an interrupted deletion (only
            // the accounts-row CASCADE removes it), so the retry here can
            // still find the external id even when the original attempt's
            // key delete failed.
            // Pass the backup store so a deletion that died before its drop
            // still takes the final dump before this completes the
            // destruction (module docs on ReaperPolicy::backup_store). The
            // policy is an explicit decision: a configured store means a
            // fail-closed final dump; no store means acknowledged-disabled
            // (dev) — never a silent skip (adversarial-review issue 3).
            //
            // We ALREADY hold this account's advisory lock (taken per row
            // above), so delete_account runs in DeleteLock::AlreadyHeld mode —
            // re-acquiring the same session-level lock on a second connection
            // would self-deadlock against our own hold (adversarial-review
            // issue 2).
            let backup_timeout = policy.backup_timeout;
            let backup_policy = match policy.backup_store.as_ref() {
                Some(store) => crate::backups::BackupPolicy::Required(store),
                None => crate::backups::BackupPolicy::DisabledAcknowledged,
            };
            crate::provision::delete_account(
                control,
                cluster,
                managed,
                // The reaper carries no billing provider, so it cannot fire a
                // Stripe cancel here (the DEL-1 `billing` is `None`); a
                // subscription left by an interrupted deletion is reconciled
                // from the Stripe dashboard, the same best-effort discipline the
                // CLI path follows.
                None,
                backup_policy,
                crate::provision::DeleteLock::AlreadyHeld,
                &account_id,
                backup_timeout,
            )
            .await?;
            Ok(true)
        }
        .await;
        lock.release().await;
        match outcome {
            Ok(true) => {
                tracing::warn!(
                    account_id,
                    "reaper completed an interrupted account deletion \
                     (active account with no tenant-database mapping)"
                );
                summary.deletions_completed.push(account_id);
            }
            Ok(false) => {}
            Err(e) => record_error(
                summary,
                &format!("completing interrupted deletion {account_id}"),
                &e,
            ),
        }
    }
    Ok(())
}

/// What arm 4 decided about one due lagging-migration row, post-lock.
enum MigrationRetryOutcome {
    /// The retry succeeded: stamp current, failure state cleared.
    Recovered,
    /// The row is still lagging after the attempt. `recorded_retry_count`
    /// is the count actually stored on the row now — bumped when the
    /// failure recording landed, unchanged when the recording itself
    /// failed (or when the migration succeeded but its success recording
    /// was lost). The alert threshold compares against this honest number.
    StillFailing { recorded_retry_count: i32 },
    /// The under-lock re-check found the row no longer due — a concurrent
    /// pass (or a booting pod's fleet runner) already healed it, or pushed
    /// its horizon. Nothing to do.
    AlreadySettled,
}

/// Arm 4 — lagging tenant migrations (plan: "Failure recovery & the
/// reaper", with ownership widened to every lagging row — see the module
/// docs and [`fleet_migration::list_retryable_failures`]). Each due row is
/// retried through the boot fleet runner's own per-tenant step under the
/// per-account advisory lock; see the module docs for why racing a booting
/// pod is safe.
async fn retry_failed_migrations(
    control: &ControlPlane,
    cluster: &ClusterConfig,
    policy: &ReaperPolicy,
    summary: &mut ReaperSummary,
) -> Result<(), CloudError> {
    let target = crate::fleet_migration::tenant_schema_target();
    let due = crate::fleet_migration::list_retryable_failures(control, target).await?;

    let mut retries_used = 0usize;
    for tenant in due {
        if retries_used >= policy.max_migration_retries_per_pass {
            summary.migrations_deferred.push(tenant.account_id);
            continue;
        }
        let account_id = tenant.account_id.clone();
        let lock = match AccountLock::try_acquire(control, &account_id).await {
            Ok(Some(lock)) => lock,
            Ok(None) => {
                summary.migrations_skipped_locked.push(account_id);
                continue;
            }
            Err(e) => {
                record_error(
                    summary,
                    &format!("locking failed migration {account_id}"),
                    &e,
                );
                continue;
            }
        };
        let outcome = process_failed_migration(control, cluster, policy, &tenant).await;
        lock.release().await;
        // Same charging rule as arm 1: settled rows cost one indexed
        // re-read and must not crowd real retries out of the pass; a failed
        // retry burned its connect timeout (or a real migration's runtime)
        // and counts.
        if !matches!(outcome, Ok(MigrationRetryOutcome::AlreadySettled)) {
            retries_used += 1;
        }
        match outcome {
            Ok(MigrationRetryOutcome::Recovered) => summary.migrations_recovered.push(account_id),
            Ok(MigrationRetryOutcome::StillFailing {
                recorded_retry_count,
            }) => {
                if recorded_retry_count > policy.migration_alert_retries {
                    tracing::error!(
                        account_id,
                        db_name = tenant.db_name,
                        retry_count = recorded_retry_count,
                        "failed tenant migration has exhausted its backoff \
                         schedule and needs an operator (see `atomic-cloud \
                         deploy status` for the stored error)"
                    );
                    summary.migration_alerts.push(account_id.clone());
                }
                summary.migrations_still_failing.push(account_id);
            }
            Ok(MigrationRetryOutcome::AlreadySettled) => {}
            Err(e) => record_error(
                summary,
                &format!("retrying failed migration {account_id}"),
                &e,
            ),
        }
    }
    Ok(())
}

/// One due lagging-migration row, lock held: re-check, then retry through
/// [`fleet_migration::migrate_tenant`] — the boot runner's exact per-tenant
/// step, so what runs and what gets recorded are identical either way.
///
/// [`fleet_migration::migrate_tenant`]: crate::fleet_migration::migrate_tenant
async fn process_failed_migration(
    control: &ControlPlane,
    cluster: &ClusterConfig,
    policy: &ReaperPolicy,
    tenant: &crate::fleet_migration::UnmigratedTenant,
) -> Result<MigrationRetryOutcome, CloudError> {
    use crate::fleet_migration::TenantMigrationOutcome;

    let target = crate::fleet_migration::tenant_schema_target();
    // Re-read under the lock, with the worklist's own predicate: between
    // the unlocked listing and lock acquisition, a concurrent pass or a
    // booting pod's fleet runner may have healed the row (stamped current)
    // or re-failed it (horizon pushed into the future). The fresh row also
    // carries the current retry count, which the backoff arithmetic below
    // must not understate.
    let row: Option<crate::fleet_migration::UnmigratedTenant> = sqlx::query_as(
        "SELECT account_id, db_name, last_migrated_version, migration_failed_at, \
                migration_retry_after, migration_retry_count \
         FROM account_databases \
         WHERE account_id = $1 AND db_name = $2 AND status = 'active' \
           AND last_migrated_version < $3 \
           AND (migration_retry_after IS NULL OR migration_retry_after <= NOW())",
    )
    .bind(&tenant.account_id)
    .bind(&tenant.db_name)
    .bind(target)
    .fetch_optional(control.pool())
    .await
    .map_err(CloudError::db("re-reading lagging migration under lock"))?;
    let Some(fresh) = row else {
        return Ok(MigrationRetryOutcome::AlreadySettled);
    };

    let retry_count = fresh.migration_retry_count;
    let outcome = crate::fleet_migration::migrate_tenant(
        control,
        cluster,
        &policy.migration_retry,
        fresh,
        target,
    )
    .await;
    match outcome {
        TenantMigrationOutcome::Migrated => {
            tracing::info!(
                account_id = tenant.account_id,
                db_name = tenant.db_name,
                "reaper recovered a lagging tenant migration; the straggler \
                 is current and serving again"
            );
            Ok(MigrationRetryOutcome::Recovered)
        }
        // Honest accounting (the summary reports what is recorded, not what
        // ran): a bumped count only when the failure recording landed; an
        // unchanged count when that write failed or when the success
        // recording was lost. Either way the row stays lagging and due, so
        // the next pass owns it again.
        TenantMigrationOutcome::Failed {
            failure_recorded: true,
        } => Ok(MigrationRetryOutcome::StillFailing {
            recorded_retry_count: retry_count + 1,
        }),
        TenantMigrationOutcome::Failed {
            failure_recorded: false,
        }
        | TenantMigrationOutcome::SuccessRecordingFailed => {
            Ok(MigrationRetryOutcome::StillFailing {
                recorded_retry_count: retry_count,
            })
        }
    }
}

/// Arm 5 — self-reservations: one atomic DELETE, age-guarded (see module
/// docs for why the grace shields in-flight deletions).
async fn clear_self_reservations(
    control: &ControlPlane,
    policy: &ReaperPolicy,
    summary: &mut ReaperSummary,
) -> Result<(), CloudError> {
    let settled_before = cutoff(policy.self_reservation_grace);
    let cleared: Vec<String> = sqlx::query_scalar(
        "DELETE FROM subdomains_reserved sr USING accounts a \
         WHERE a.subdomain = sr.subdomain \
           AND a.status = 'active' \
           AND sr.created_at < $1 \
         RETURNING sr.subdomain",
    )
    .bind(settled_before)
    .fetch_all(control.pool())
    .await
    .map_err(CloudError::db("clearing self-reservations"))?;
    for subdomain in &cleared {
        tracing::info!(
            subdomain,
            "reaper cleared self-reservation (active account's own \
             subdomain was parked — crashed-deletion residue)"
        );
    }
    summary.self_reservations_cleared.extend(cleared);
    Ok(())
}

/// Arm 6 — hygiene purges. Each target is already inert before deletion
/// (every reader filters on expiry/consumption), so these are independent
/// single statements with no locking.
async fn purge_expired_rows(
    control: &ControlPlane,
    policy: &ReaperPolicy,
    summary: &mut ReaperSummary,
) -> Result<(), CloudError> {
    let links_expired_before = cutoff(policy.magic_link_retention_after_expiry);
    summary.expired_magic_links_purged =
        sqlx::query("DELETE FROM magic_links WHERE expires_at < $1")
            .bind(links_expired_before)
            .execute(control.pool())
            .await
            .map_err(CloudError::db("purging expired magic links"))?
            .rows_affected();

    summary.expired_sessions_purged = sqlx::query("DELETE FROM sessions WHERE expires_at <= NOW()")
        .execute(control.pool())
        .await
        .map_err(CloudError::db("purging expired sessions"))?
        .rows_affected();

    summary.expired_reservations_purged =
        sqlx::query("DELETE FROM subdomains_reserved WHERE expires_at <= NOW()")
            .execute(control.pool())
            .await
            .map_err(CloudError::db("purging lapsed subdomain reservations"))?
            .rows_affected();

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_key_is_stable_and_account_specific() {
        let a = "0e1a2b3c-4d5e-6f70-8192-a3b4c5d6e7f8";
        let b = "ffffffff-ffff-ffff-ffff-ffffffffffff";
        assert_eq!(reaper_lock_key(a), reaper_lock_key(a), "deterministic");
        assert_ne!(reaper_lock_key(a), reaper_lock_key(b));
    }

    #[test]
    fn default_policy_numbers() {
        let policy = ReaperPolicy::default();
        // The plan's numbers ("created_at < now() - interval '5 minutes'";
        // "alerts when retry_count > 5").
        assert_eq!(policy.stuck_provision_age, Duration::from_secs(300));
        assert_eq!(policy.migration_alert_retries, 5);
        // Our choices (the plan fixes none of these).
        assert_eq!(policy.deletion_recovery_grace, Duration::from_secs(300));
        assert_eq!(policy.max_migration_retries_per_pass, 8);
        assert_eq!(
            policy.magic_link_retention_after_expiry,
            Duration::from_secs(24 * 60 * 60)
        );
        // Arm 4's backoff knobs default to the boot fleet runner's, so the
        // two writers of migration_retry_after agree out of the box.
        let fleet = crate::fleet_migration::FleetMigrationConfig::default();
        assert_eq!(
            policy.migration_retry.retry_backoff_base,
            fleet.retry_backoff_base
        );
        assert_eq!(
            policy.migration_retry.retry_backoff_cap,
            fleet.retry_backoff_cap
        );
    }

    #[test]
    fn empty_summary_is_quiet_and_any_action_is_not() {
        assert!(ReaperSummary::default().is_quiet());
        let acted = ReaperSummary {
            expired_sessions_purged: 1,
            ..ReaperSummary::default()
        };
        assert!(!acted.is_quiet());
        let skipped = ReaperSummary {
            stuck_skipped_locked: vec!["id".into()],
            ..ReaperSummary::default()
        };
        assert!(!skipped.is_quiet());
        let still_failing = ReaperSummary {
            migrations_still_failing: vec!["id".into()],
            ..ReaperSummary::default()
        };
        assert!(!still_failing.is_quiet());
    }
}
