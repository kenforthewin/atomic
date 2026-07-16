//! Operational metrics for the cloud pod (scaling doc: "The recurring
//! remediation is monitoring").
//!
//! A hand-rendered Prometheus registry — ~20 gauges/counters, not a
//! framework, so no metrics crate. Each family maps to a named signal in
//! `docs/plans/atomic-cloud-scaling.md`:
//!
//! - account cache residency + evictions (#2: scan-vs-cache churn)
//! - dispatcher tick/full-scan durations and job counters (#2: tick summary)
//! - worker-pool in-flight vs cap (#8: bounded AI work)
//! - backup pass freshness and staleness (#6 and the launch-week stall)
//! - active export jobs (#5: exports share the data volume)
//! - control-pool and tenant-pool connection counts (#1: the 200-connection
//!   ceiling)
//! - process uptime + build stamp
//!
//! # Serving model
//!
//! The registry is a bag of atomics updated at the natural sites (dispatcher
//! tick end, backup pass end, worker settle); everything that is cheap to
//! read live (cache residency, pool sizes, in-flight counts) is sampled at
//! scrape time instead, so those gauges can never go stale. Every loop that
//! must keep happening (backup pass, staleness check, dispatcher tick and
//! full scan) additionally stores a unix stamp at its site and renders a
//! live age gauge at scrape — +Inf until the first event — because a dead
//! loop freezes its last-value gauges at a healthy reading, and only a
//! computed age makes that death alertable (see [`age_seconds`]). Request-path
//! metrics (per-request histograms, per-route latency) are deliberately OUT
//! of scope for this pass — they cost hot-path work per request and belong
//! to a later item; today's need is the background/ops signals a human
//! can't see without ssh.
//!
//! # Security
//!
//! `/metrics` is served by a SEPARATE internal HTTP listener
//! (`--metrics-bind` / `ATOMIC_CLOUD_METRICS_BIND`, disabled when unset)
//! inside the same process. It is never registered on the public
//! tenant/app listener, so no route-table mistake, auth bug, or SPA
//! fallback can expose it to tenants — scrapers reach it over the docker
//! network only (`expose:`, never `ports:` in the compose file).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use actix_web::{web, HttpResponse};
use chrono::{DateTime, Utc};

use crate::account_cache::AccountCache;
use crate::backups::BackupSummary;
use crate::control_plane::ControlPlane;
use crate::pools::{WorkClass, WorkerPools};
use crate::tenant_plane::TenantPlane;

/// Prometheus text exposition content type (format version 0.0.4).
pub const PROMETHEUS_CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// Event-accumulated metrics: atomics bumped at the natural sites. Values
/// that are cheap to read live are NOT here — they're sampled at scrape
/// time from the [`MetricsState`] handles instead (module docs).
///
/// All ordering is `Relaxed`: every field is an independent statistic, and
/// scrapes tolerate torn cross-field views (a scrape racing a tick may see
/// the new duration with the old counter — harmless for monitoring).
pub struct MetricsRegistry {
    started: Instant,
    /// Duration of the most recent dispatcher tick, in microseconds.
    dispatcher_last_tick_us: AtomicU64,
    /// Unix seconds of the most recent tick end. 0 = no tick since boot
    /// (rendered as +Inf age). This is the dispatcher's liveness signal:
    /// `run_loop` is a `tokio::spawn`ed task, so a panic kills it silently
    /// while the duration gauges above freeze at their last healthy value
    /// — only a computed age can distinguish a dead loop from a fast one.
    dispatcher_last_tick_unix: AtomicU64,
    /// Duration of the most recent tick that ran a slow full scan, in
    /// microseconds (scaling doc #2's "tick-duration logs" signal).
    dispatcher_last_full_scan_us: AtomicU64,
    /// Unix seconds of the most recent COMPLETED full scan. 0 = none since
    /// boot. A dispatcher whose candidate scan errors every tick still
    /// stamps ticks (the scan-failure early return records tick metrics),
    /// so tick age alone can't see scans that never complete — this can.
    dispatcher_last_full_scan_unix: AtomicU64,
    /// Tenants polled by the most recent full scan.
    dispatcher_last_full_scan_tenants: AtomicU64,
    /// Worker executions that ran to settle (success, or failed with the
    /// ledger's retry-or-abandon decision taken).
    dispatcher_jobs_executed: AtomicU64,
    /// Jobs seen but not admitted this tick: atom-limit-gate deferrals plus
    /// items parked by pool saturation. They stay in the durable ledgers
    /// and re-derive next tick.
    dispatcher_jobs_deferred: AtomicU64,
    /// Unix seconds of the last backup pass that completed with no tenant
    /// failure and no error. 0 = no successful pass since boot (rendered as
    /// +Inf age, so "backup age > 36h" alerts fire on a pod that never
    /// managed a pass at all — the launch-week silent-stall lesson).
    backup_last_success_unix: AtomicU64,
    /// Tenants past the staleness horizon at the last check.
    backup_stale_tenants: AtomicU64,
    /// Unix seconds of the last staleness check that ran to completion
    /// (any count, including 0). 0 = no completed check since boot. The
    /// stale-tenants gauge above freezes at its last value when the check
    /// errors (the backup loop only logs, and logs are not shipped) — and
    /// the check is the ONLY signal for the clean-but-skipping class of
    /// bug, where a due-filter regression skips a tenant while every pass
    /// reports clean and keeps advancing the success stamp. A broken
    /// check must therefore be alertable through this stamp's age.
    backup_staleness_check_unix: AtomicU64,
    /// Tenants backed up / failed in the most recent pass.
    backup_last_pass_backed_up: AtomicU64,
    backup_last_pass_failed: AtomicU64,
}

impl MetricsRegistry {
    #[allow(clippy::new_without_default)] // Construction is deliberate: one per process, in serve().
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
            dispatcher_last_tick_us: AtomicU64::new(0),
            dispatcher_last_tick_unix: AtomicU64::new(0),
            dispatcher_last_full_scan_us: AtomicU64::new(0),
            dispatcher_last_full_scan_unix: AtomicU64::new(0),
            dispatcher_last_full_scan_tenants: AtomicU64::new(0),
            dispatcher_jobs_executed: AtomicU64::new(0),
            dispatcher_jobs_deferred: AtomicU64::new(0),
            backup_last_success_unix: AtomicU64::new(0),
            backup_stale_tenants: AtomicU64::new(0),
            backup_staleness_check_unix: AtomicU64::new(0),
            backup_last_pass_backed_up: AtomicU64::new(0),
            backup_last_pass_failed: AtomicU64::new(0),
        }
    }

    /// Record one dispatcher tick (called at tick end — the natural site).
    /// `full_scan` marks a tick whose candidate scan covered every active
    /// account, in which case `polled` is the fleet-wide poll count.
    pub fn record_dispatcher_tick(
        &self,
        elapsed: Duration,
        full_scan: bool,
        polled: usize,
        deferred: usize,
    ) {
        let us = elapsed.as_micros().min(u128::from(u64::MAX)) as u64;
        let now_unix = Utc::now().timestamp().max(0) as u64;
        self.dispatcher_last_tick_us.store(us, Ordering::Relaxed);
        self.dispatcher_last_tick_unix
            .store(now_unix, Ordering::Relaxed);
        if full_scan {
            self.dispatcher_last_full_scan_us
                .store(us, Ordering::Relaxed);
            self.dispatcher_last_full_scan_unix
                .store(now_unix, Ordering::Relaxed);
            self.dispatcher_last_full_scan_tenants
                .store(polled as u64, Ordering::Relaxed);
        }
        self.dispatcher_jobs_deferred
            .fetch_add(deferred as u64, Ordering::Relaxed);
    }

    /// Record one settled worker execution (success or ledger-settled
    /// failure). Called from the worker task, not the tick.
    pub fn record_job_executed(&self) {
        self.dispatcher_jobs_executed
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Record a completed backup pass (called at pass end). A pass with no
    /// tenant failure and no error advances the success stamp; per-pass
    /// counts are gauges of the latest pass either way.
    pub fn record_backup_pass(&self, summary: &BackupSummary, now: DateTime<Utc>) {
        self.backup_last_pass_backed_up
            .store(summary.tenants_backed_up.len() as u64, Ordering::Relaxed);
        self.backup_last_pass_failed
            .store(summary.tenants_failed.len() as u64, Ordering::Relaxed);
        if summary.tenants_failed.is_empty() && summary.errors.is_empty() {
            self.backup_last_success_unix
                .store(now.timestamp().max(0) as u64, Ordering::Relaxed);
        }
    }

    /// Record the post-pass staleness check's result (count of tenants past
    /// the horizon). Called only when the check ran to completion — an
    /// erroring check must NOT advance the stamp, so its age keeps growing
    /// past the alert threshold (the whole point of the stamp).
    pub fn record_backup_staleness(&self, stale_tenants: usize) {
        self.backup_stale_tenants
            .store(stale_tenants as u64, Ordering::Relaxed);
        self.backup_staleness_check_unix
            .store(Utc::now().timestamp().max(0) as u64, Ordering::Relaxed);
    }
}

/// Everything the scrape endpoint reads: the event-accumulated registry
/// plus live handles sampled per scrape. Cloned into each metrics-listener
/// worker.
#[derive(Clone)]
pub struct MetricsState {
    pub registry: Arc<MetricsRegistry>,
    /// Residency + eviction stats, and the tenant-pool connection aggregate.
    pub cache: Arc<AccountCache>,
    /// Control-plane sqlx pool (size/idle — scaling doc #1).
    pub control: ControlPlane,
    /// The dispatcher's worker pools; `None` when `--dispatcher=false`
    /// (the in-flight/cap families are simply absent then).
    pub pools: Option<Arc<WorkerPools>>,
    /// Export-job probe (scaling doc #5: exports share the data volume).
    pub tenant_plane: TenantPlane,
}

/// Register the metrics app on a [`web::ServiceConfig`] — the internal
/// listener's entire route table. Mirrors `configure_cloud_app`'s shape so
/// `serve` and the e2e harness wire it identically.
pub fn configure_metrics_app(state: MetricsState) -> impl FnOnce(&mut web::ServiceConfig) {
    move |cfg: &mut web::ServiceConfig| {
        cfg.app_data(web::Data::new(state))
            .route("/metrics", web::get().to(metrics_route));
    }
}

async fn metrics_route(state: web::Data<MetricsState>) -> HttpResponse {
    HttpResponse::Ok()
        .content_type(PROMETHEUS_CONTENT_TYPE)
        .body(render(&state).await)
}

/// Render the full exposition. Async because the cache stats take the cache
/// lock; everything else is atomic loads and pool counters.
pub async fn render(state: &MetricsState) -> String {
    let reg = &state.registry;
    let mut out = String::with_capacity(4096);

    // ---- process ----
    family(
        &mut out,
        "atomic_cloud_build_info",
        "Build stamp; value is always 1, the labels carry the stamp.",
        "gauge",
    );
    sample(
        &mut out,
        "atomic_cloud_build_info",
        &[
            ("version", env!("CARGO_PKG_VERSION")),
            (
                "sha",
                option_env!("ATOMIC_CLOUD_BUILD_SHA").unwrap_or("unknown"),
            ),
        ],
        1.0,
    );
    family(
        &mut out,
        "atomic_cloud_uptime_seconds",
        "Seconds since this pod's process started.",
        "gauge",
    );
    sample(
        &mut out,
        "atomic_cloud_uptime_seconds",
        &[],
        reg.started.elapsed().as_secs_f64(),
    );
    // Open file descriptors vs the soft limit — computed at scrape time
    // (same never-stale principle as the age gauges). A slow FD leak is a
    // fuse: the 2026-07-16 outage was the pod hitting Docker's default
    // 1024 soft limit after ~28h of a socket leak, at which point accept()
    // fails and the pod serves nothing while looking "up". Alert on the
    // ratio (recipe in deploy/MONITORING.md) and the next leak announces
    // itself days before it detonates.
    let (open_fds, fd_limit) = process_fd_usage();
    family(
        &mut out,
        "atomic_cloud_process_open_fds",
        "Open file descriptors held by this pod's process.",
        "gauge",
    );
    sample(&mut out, "atomic_cloud_process_open_fds", &[], open_fds);
    family(
        &mut out,
        "atomic_cloud_process_fd_limit",
        "Soft limit on open file descriptors (RLIMIT_NOFILE).",
        "gauge",
    );
    sample(&mut out, "atomic_cloud_process_fd_limit", &[], fd_limit);

    // ---- account cache (scaling doc #2) ----
    let cache_stats = state.cache.stats().await;
    family(
        &mut out,
        "atomic_cloud_account_cache_entries",
        "Resident account-cache entries by kind (serving = promoted, \
         background = unpromoted dispatch faults).",
        "gauge",
    );
    sample(
        &mut out,
        "atomic_cloud_account_cache_entries",
        &[("kind", "serving")],
        cache_stats.serving as f64,
    );
    sample(
        &mut out,
        "atomic_cloud_account_cache_entries",
        &[("kind", "background")],
        cache_stats.background as f64,
    );
    family(
        &mut out,
        "atomic_cloud_account_cache_evictions_total",
        "Cache entries evicted since boot (idle sweep, hard cap, and \
         explicit deletion evictions).",
        "counter",
    );
    sample(
        &mut out,
        "atomic_cloud_account_cache_evictions_total",
        &[],
        cache_stats.evictions as f64,
    );

    // ---- dispatcher (scaling doc #2) ----
    family(
        &mut out,
        "atomic_cloud_dispatcher_last_tick_seconds",
        "Duration of the most recent dispatcher tick.",
        "gauge",
    );
    sample(
        &mut out,
        "atomic_cloud_dispatcher_last_tick_seconds",
        &[],
        reg.dispatcher_last_tick_us.load(Ordering::Relaxed) as f64 / 1e6,
    );
    family(
        &mut out,
        "atomic_cloud_dispatcher_last_tick_age_seconds",
        "Seconds since the last dispatcher tick completed; +Inf until the \
         first tick. Grows without bound if the dispatcher task dies, \
         while the duration gauges freeze at their last healthy value. \
         Stays +Inf by design on a pod running with --dispatcher=false.",
        "gauge",
    );
    sample(
        &mut out,
        "atomic_cloud_dispatcher_last_tick_age_seconds",
        &[],
        age_seconds(reg.dispatcher_last_tick_unix.load(Ordering::Relaxed)),
    );
    family(
        &mut out,
        "atomic_cloud_dispatcher_last_full_scan_seconds",
        "Duration of the most recent tick that ran the slow full scan of \
         all active accounts.",
        "gauge",
    );
    sample(
        &mut out,
        "atomic_cloud_dispatcher_last_full_scan_seconds",
        &[],
        reg.dispatcher_last_full_scan_us.load(Ordering::Relaxed) as f64 / 1e6,
    );
    family(
        &mut out,
        "atomic_cloud_dispatcher_last_full_scan_age_seconds",
        "Seconds since the last COMPLETED full scan; +Inf until the first. \
         Keeps growing while candidate scans fail even though ticks stay \
         fast (a failed scan records tick metrics but no full scan).",
        "gauge",
    );
    sample(
        &mut out,
        "atomic_cloud_dispatcher_last_full_scan_age_seconds",
        &[],
        age_seconds(reg.dispatcher_last_full_scan_unix.load(Ordering::Relaxed)),
    );
    family(
        &mut out,
        "atomic_cloud_dispatcher_last_full_scan_tenants",
        "Tenants polled by the most recent full scan.",
        "gauge",
    );
    sample(
        &mut out,
        "atomic_cloud_dispatcher_last_full_scan_tenants",
        &[],
        reg.dispatcher_last_full_scan_tenants
            .load(Ordering::Relaxed) as f64,
    );
    family(
        &mut out,
        "atomic_cloud_dispatcher_jobs_executed_total",
        "Worker executions settled since boot (success or ledger-settled \
         failure).",
        "counter",
    );
    sample(
        &mut out,
        "atomic_cloud_dispatcher_jobs_executed_total",
        &[],
        reg.dispatcher_jobs_executed.load(Ordering::Relaxed) as f64,
    );
    family(
        &mut out,
        "atomic_cloud_dispatcher_jobs_deferred_total",
        "Jobs deferred at tick time since boot (atom-limit gate or pool \
         saturation); they stay in the ledgers and re-derive next tick.",
        "counter",
    );
    sample(
        &mut out,
        "atomic_cloud_dispatcher_jobs_deferred_total",
        &[],
        reg.dispatcher_jobs_deferred.load(Ordering::Relaxed) as f64,
    );

    // ---- worker pools (scaling doc #8) ----
    if let Some(pools) = &state.pools {
        family(
            &mut out,
            "atomic_cloud_worker_pool_in_flight",
            "Jobs currently in flight per work class.",
            "gauge",
        );
        for class in WorkClass::ALL {
            sample(
                &mut out,
                "atomic_cloud_worker_pool_in_flight",
                &[("class", class.label())],
                pools.total_in_flight(class) as f64,
            );
        }
        family(
            &mut out,
            "atomic_cloud_worker_pool_cap",
            "Configured total in-flight cap per work class.",
            "gauge",
        );
        for class in WorkClass::ALL {
            sample(
                &mut out,
                "atomic_cloud_worker_pool_cap",
                &[("class", class.label())],
                pools.caps(class).total as f64,
            );
        }
    }

    // ---- backups (scaling doc #6; the launch-week silent stall) ----
    family(
        &mut out,
        "atomic_cloud_backup_last_success_age_seconds",
        "Seconds since the last backup pass that completed with no failure; \
         +Inf until the first successful pass after boot.",
        "gauge",
    );
    sample(
        &mut out,
        "atomic_cloud_backup_last_success_age_seconds",
        &[],
        age_seconds(reg.backup_last_success_unix.load(Ordering::Relaxed)),
    );
    family(
        &mut out,
        "atomic_cloud_backup_stale_tenants",
        "Active tenants whose last successful backup is past the staleness \
         horizon, at the last check.",
        "gauge",
    );
    sample(
        &mut out,
        "atomic_cloud_backup_stale_tenants",
        &[],
        reg.backup_stale_tenants.load(Ordering::Relaxed) as f64,
    );
    family(
        &mut out,
        "atomic_cloud_backup_staleness_check_age_seconds",
        "Seconds since the staleness check last ran to completion; +Inf \
         until the first check. The stale-tenants gauge freezes at its \
         last value when the check errors, so the check's own age is what \
         makes a broken check alertable.",
        "gauge",
    );
    sample(
        &mut out,
        "atomic_cloud_backup_staleness_check_age_seconds",
        &[],
        age_seconds(reg.backup_staleness_check_unix.load(Ordering::Relaxed)),
    );
    family(
        &mut out,
        "atomic_cloud_backup_last_pass_backed_up",
        "Tenants backed up in the most recent pass.",
        "gauge",
    );
    sample(
        &mut out,
        "atomic_cloud_backup_last_pass_backed_up",
        &[],
        reg.backup_last_pass_backed_up.load(Ordering::Relaxed) as f64,
    );
    family(
        &mut out,
        "atomic_cloud_backup_last_pass_failed",
        "Tenants whose dump or upload failed in the most recent pass.",
        "gauge",
    );
    sample(
        &mut out,
        "atomic_cloud_backup_last_pass_failed",
        &[],
        reg.backup_last_pass_failed.load(Ordering::Relaxed) as f64,
    );

    // ---- exports (scaling doc #5) ----
    family(
        &mut out,
        "atomic_cloud_export_jobs_active",
        "Export jobs currently queued or running across all accounts.",
        "gauge",
    );
    sample(
        &mut out,
        "atomic_cloud_export_jobs_active",
        &[],
        state.tenant_plane.active_export_jobs() as f64,
    );

    // ---- connection pools (scaling doc #1) ----
    family(
        &mut out,
        "atomic_cloud_control_pool_connections",
        "Open connections in the control-plane sqlx pool.",
        "gauge",
    );
    sample(
        &mut out,
        "atomic_cloud_control_pool_connections",
        &[],
        state.control.pool().size() as f64,
    );
    family(
        &mut out,
        "atomic_cloud_control_pool_idle",
        "Idle connections in the control-plane sqlx pool.",
        "gauge",
    );
    sample(
        &mut out,
        "atomic_cloud_control_pool_idle",
        &[],
        state.control.pool().num_idle() as f64,
    );
    family(
        &mut out,
        "atomic_cloud_tenant_pool_connections",
        "Open tenant-pool connections summed across all cached accounts \
         (each capped at the per-tenant pool max).",
        "gauge",
    );
    sample(
        &mut out,
        "atomic_cloud_tenant_pool_connections",
        &[],
        state.cache.tenant_pool_connections().await as f64,
    );

    out
}

/// Live age of a stored unix-seconds event stamp: +Inf when the stamp is 0
/// (the event has never happened in this process), else seconds since it.
///
/// Stored-stamp/age-at-scrape is the liveness pattern for every "loop that
/// must keep happening" here (backup passes, staleness checks, dispatcher
/// ticks and scans): a last-value gauge frozen by a dead loop reads healthy
/// forever, while a computed age grows until an alert fires.
fn age_seconds(stamp_unix: u64) -> f64 {
    if stamp_unix == 0 {
        f64::INFINITY
    } else {
        (Utc::now().timestamp().max(0) as u64).saturating_sub(stamp_unix) as f64
    }
}

/// Write one family's `# HELP` / `# TYPE` header.
/// (open fd count, soft RLIMIT_NOFILE), read from /proc at scrape time.
/// NaN on platforms without /proc (macOS dev) — Prometheus drops NaN
/// samples, so the families are simply absent rather than lying.
fn process_fd_usage() -> (f64, f64) {
    let open = std::fs::read_dir("/proc/self/fd")
        .map(|d| d.count().saturating_sub(1) as f64) // read_dir itself holds one
        .unwrap_or(f64::NAN);
    let limit = std::fs::read_to_string("/proc/self/limits")
        .ok()
        .and_then(|s| {
            s.lines().find(|l| l.starts_with("Max open files")).and_then(|l| {
                l.split_whitespace().nth(3).and_then(|v| v.parse::<f64>().ok())
            })
        })
        .unwrap_or(f64::NAN);
    (open, limit)
}

fn family(out: &mut String, name: &str, help: &str, kind: &str) {
    // HELP text: Prometheus requires backslash and newline escaping.
    let help = help.replace('\\', "\\\\").replace('\n', "\\n");
    out.push_str("# HELP ");
    out.push_str(name);
    out.push(' ');
    out.push_str(&help);
    out.push_str("\n# TYPE ");
    out.push_str(name);
    out.push(' ');
    out.push_str(kind);
    out.push('\n');
}

/// Write one sample line, with optional labels. Values render as Prometheus
/// expects: integral floats without an exponent, `+Inf` for infinity.
fn sample(out: &mut String, name: &str, labels: &[(&str, &str)], value: f64) {
    out.push_str(name);
    if !labels.is_empty() {
        out.push('{');
        for (i, (key, val)) in labels.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(key);
            out.push_str("=\"");
            out.push_str(&escape_label_value(val));
            out.push('"');
        }
        out.push('}');
    }
    out.push(' ');
    if value.is_infinite() {
        out.push_str(if value > 0.0 { "+Inf" } else { "-Inf" });
    } else {
        // `{}` on f64 renders 3.0 as "3" and 0.25 as "0.25" — both valid
        // Prometheus floats.
        out.push_str(&value.to_string());
    }
    out.push('\n');
}

/// Escape a label value per the exposition format: backslash, double
/// quote, and newline.
fn escape_label_value(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            other => escaped.push(other),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_label_values() {
        assert_eq!(escape_label_value("plain"), "plain");
        assert_eq!(escape_label_value(r#"a"b"#), r#"a\"b"#);
        assert_eq!(escape_label_value(r"a\b"), r"a\\b");
        assert_eq!(escape_label_value("a\nb"), r"a\nb");
    }

    #[test]
    fn sample_renders_labels_and_values() {
        let mut out = String::new();
        sample(&mut out, "m", &[("class", "llm"), ("k", "v\"x")], 3.0);
        assert_eq!(out, "m{class=\"llm\",k=\"v\\\"x\"} 3\n");

        let mut out = String::new();
        sample(&mut out, "m", &[], 0.25);
        assert_eq!(out, "m 0.25\n");

        let mut out = String::new();
        sample(&mut out, "m", &[], f64::INFINITY);
        assert_eq!(out, "m +Inf\n");
    }

    #[test]
    fn family_renders_help_and_type() {
        let mut out = String::new();
        family(&mut out, "m_total", "line one\nline two", "counter");
        assert_eq!(
            out,
            "# HELP m_total line one\\nline two\n# TYPE m_total counter\n"
        );
    }

    #[test]
    fn backup_pass_advances_success_stamp_only_when_clean() {
        let reg = MetricsRegistry::new();
        let now = Utc::now();

        // A failing pass records counts but never the success stamp.
        let failing = BackupSummary {
            tenants_backed_up: vec!["a".into()],
            tenants_failed: vec!["b".into()],
            errors: vec!["dump failed".into()],
            ..BackupSummary::default()
        };
        reg.record_backup_pass(&failing, now);
        assert_eq!(reg.backup_last_success_unix.load(Ordering::Relaxed), 0);
        assert_eq!(reg.backup_last_pass_backed_up.load(Ordering::Relaxed), 1);
        assert_eq!(reg.backup_last_pass_failed.load(Ordering::Relaxed), 1);

        // A clean pass (even an empty, nothing-due one) advances the stamp.
        reg.record_backup_pass(&BackupSummary::default(), now);
        assert_eq!(
            reg.backup_last_success_unix.load(Ordering::Relaxed),
            now.timestamp().max(0) as u64
        );
    }

    #[test]
    fn dispatcher_tick_updates_full_scan_families_only_on_full_scans() {
        let reg = MetricsRegistry::new();
        reg.record_dispatcher_tick(Duration::from_millis(1500), true, 7, 2);
        assert_eq!(
            reg.dispatcher_last_full_scan_us.load(Ordering::Relaxed),
            1_500_000
        );
        assert_eq!(
            reg.dispatcher_last_full_scan_tenants
                .load(Ordering::Relaxed),
            7
        );
        assert_eq!(reg.dispatcher_jobs_deferred.load(Ordering::Relaxed), 2);

        // A fast tick moves the tick gauge but leaves the full-scan pair.
        reg.record_dispatcher_tick(Duration::from_millis(3), false, 1, 0);
        assert_eq!(reg.dispatcher_last_tick_us.load(Ordering::Relaxed), 3_000);
        assert_eq!(
            reg.dispatcher_last_full_scan_us.load(Ordering::Relaxed),
            1_500_000
        );
    }

    /// The liveness stamps: 0 (+Inf age) until their event happens, then a
    /// current unix timestamp — so a dead loop's age grows while its
    /// last-value gauges freeze. This is the spec for the launch-week
    /// "silent stall" class: death must be distinguishable from quiet.
    #[test]
    fn liveness_stamps_advance_only_at_their_sites() {
        let reg = MetricsRegistry::new();
        assert_eq!(age_seconds(0), f64::INFINITY);
        let recent = |stamp: u64| {
            let now = Utc::now().timestamp().max(0) as u64;
            stamp != 0 && now.saturating_sub(stamp) < 60
        };

        // Boot state: every loop reads dead-until-proven-alive.
        assert_eq!(reg.dispatcher_last_tick_unix.load(Ordering::Relaxed), 0);
        assert_eq!(
            reg.dispatcher_last_full_scan_unix.load(Ordering::Relaxed),
            0
        );
        assert_eq!(reg.backup_staleness_check_unix.load(Ordering::Relaxed), 0);

        // A non-full-scan tick stamps the tick but NOT the full scan: a
        // dispatcher whose candidate scan errors every tick (which records
        // tick metrics via the early return) must not read as scanning.
        reg.record_dispatcher_tick(Duration::from_millis(3), false, 0, 0);
        assert!(recent(
            reg.dispatcher_last_tick_unix.load(Ordering::Relaxed)
        ));
        assert_eq!(
            reg.dispatcher_last_full_scan_unix.load(Ordering::Relaxed),
            0
        );
        reg.record_dispatcher_tick(Duration::from_millis(1500), true, 7, 0);
        assert!(recent(
            reg.dispatcher_last_full_scan_unix.load(Ordering::Relaxed)
        ));

        // The staleness stamp advances on any completed check — including
        // an all-clear — because the alertable failure is the check not
        // running, not the check finding stale tenants.
        reg.record_backup_staleness(0);
        assert!(recent(
            reg.backup_staleness_check_unix.load(Ordering::Relaxed)
        ));
        assert!(age_seconds(reg.backup_staleness_check_unix.load(Ordering::Relaxed)) < 60.0);
    }
}
