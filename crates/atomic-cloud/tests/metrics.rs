//! E2E tests for the internal metrics listener (`atomic_cloud::metrics`).
//!
//! Boots the real composition — `configure_cloud_app` on one ephemeral port,
//! `configure_metrics_app` on a second, exactly as `atomic-cloud serve`
//! wires them — and asserts the two security-relevant properties end to end:
//!
//! 1. the metrics listener serves the Prometheus exposition with every
//!    expected family, and
//! 2. the PUBLIC listener does NOT serve `/metrics` on any host — the
//!    ops surface exists only on the separate internal listener.
//!
//! Plus liveness of the numbers: an authenticated tenant request faults the
//! account cache and the residency gauge moves; a dispatcher tick moves the
//! tick/full-scan gauges; a recorded backup pass flips the backup-age gauge
//! from +Inf to a finite value.
//!
//! Postgres-gated; see `tests/support/mod.rs` for the run command.

mod support;

use std::sync::Arc;

use actix_web::{App, HttpServer};
use atomic_cloud::{
    configure_cloud_app, configure_metrics_app, issue_token, provision_account, AccountCache,
    AccountCacheConfig, AccountPlane, AccountPlaneConfig, BackupSummary, ChatStreamLimiter,
    CloudAuth, ClusterConfig, ControlPlane, Dispatcher, DispatcherConfig, FallbackAppState,
    ManagedKeys, MetricsRegistry, MetricsState, NewAccount, QuotaBilling, Readiness, TenantPlane,
    TokenScope, DEFAULT_CHAT_STREAMS_PER_ACCOUNT, PROMETHEUS_CONTENT_TYPE,
};
use reqwest::header::HOST;
use reqwest::StatusCode;
use support::with_control_db;

/// Base domain the composition is configured with (matches e2e_cloud.rs).
const BASE_DOMAIN: &str = "cloudtest.local";

/// The metric families the listener promises. Grafana dashboards key on
/// these names — a rename must be deliberate, so the list is pinned here.
const EXPECTED_FAMILIES: &[&str] = &[
    "atomic_cloud_build_info",
    "atomic_cloud_uptime_seconds",
    "atomic_cloud_account_cache_entries",
    "atomic_cloud_account_cache_evictions_total",
    "atomic_cloud_dispatcher_last_tick_seconds",
    "atomic_cloud_dispatcher_last_tick_age_seconds",
    "atomic_cloud_dispatcher_last_full_scan_seconds",
    "atomic_cloud_dispatcher_last_full_scan_age_seconds",
    "atomic_cloud_dispatcher_last_full_scan_tenants",
    "atomic_cloud_dispatcher_jobs_executed_total",
    "atomic_cloud_dispatcher_jobs_deferred_total",
    "atomic_cloud_worker_pool_in_flight",
    "atomic_cloud_worker_pool_cap",
    "atomic_cloud_backup_last_success_age_seconds",
    "atomic_cloud_backup_stale_tenants",
    "atomic_cloud_backup_staleness_check_age_seconds",
    "atomic_cloud_backup_last_pass_backed_up",
    "atomic_cloud_backup_last_pass_failed",
    "atomic_cloud_export_jobs_active",
    "atomic_cloud_control_pool_connections",
    "atomic_cloud_control_pool_idle",
    "atomic_cloud_tenant_pool_connections",
];

/// The composed cloud server plus the internal metrics listener, each on
/// its own ephemeral loopback port — the `serve` topology in miniature.
struct MetricsHarness {
    control: ControlPlane,
    cluster: ClusterConfig,
    cache: Arc<AccountCache>,
    registry: Arc<MetricsRegistry>,
    dispatcher: Arc<Dispatcher>,
    client: reqwest::Client,
    app_url: String,
    metrics_url: String,
    app_handle: actix_web::dev::ServerHandle,
    metrics_handle: actix_web::dev::ServerHandle,
    /// Owns the scratch directory behind the inert fallback `AppState`.
    _fallback: FallbackAppState,
}

impl MetricsHarness {
    async fn spawn(control_url: &str) -> Self {
        let control = ControlPlane::connect(
            control_url,
            atomic_cloud::control_plane::DEFAULT_CONTROL_POOL_MAX_CONNECTIONS,
        )
        .await
        .expect("connect control plane");
        control.initialize().await.expect("migrate control plane");
        let cluster = ClusterConfig {
            cluster_id: "test-cluster-1".to_string(),
            cluster_url: std::env::var("ATOMIC_TEST_DATABASE_URL")
                .expect("with_control_db verified ATOMIC_TEST_DATABASE_URL"),
        };
        let cache = Arc::new(AccountCache::new(
            control.clone(),
            cluster.clone(),
            support::test_vault(),
            AccountCacheConfig::default(),
        ));
        let auth = CloudAuth::new(control.clone(), Arc::clone(&cache), BASE_DOMAIN);
        let account_plane = AccountPlane::new(
            control.clone(),
            cluster.clone(),
            ManagedKeys::Disabled,
            Arc::new(support::CapturingSender::default()),
            AccountPlaneConfig::new(BASE_DOMAIN),
        )
        .expect("build account plane");
        let tenant_plane = TenantPlane::new(
            control.clone(),
            cluster.clone(),
            ManagedKeys::Disabled,
            support::test_vault(),
            Arc::clone(&cache),
        );
        let fallback = FallbackAppState::build().expect("build fallback state");

        // The registry + dispatcher, wired the way serve() does it: metrics
        // attached, pools shared with the scrape state. The dispatcher is
        // NOT run_loop'd — tests drive tick() deterministically.
        let registry = Arc::new(MetricsRegistry::new());
        let dispatcher = Arc::new(
            Dispatcher::new(
                control.clone(),
                Arc::clone(&cache),
                DispatcherConfig::default(),
            )
            .with_metrics(Arc::clone(&registry)),
        );

        // Public listener: the full composition, no SPA (unmatched paths
        // 404, which is exactly what the security assertion wants to see).
        let app_listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind app port");
        let app_port = app_listener.local_addr().expect("app addr").port();
        let state = fallback.data();
        let control_for_app = control.clone();
        let readiness = Readiness::ready(control.clone());
        let quota_billing = QuotaBilling::for_tests(control.clone(), BASE_DOMAIN)
            .await
            .expect("plans");
        let oauth_plane = atomic_cloud::OAuthPlane::new(
            control.clone(),
            BASE_DOMAIN,
            "http",
            format!("http://app.{BASE_DOMAIN}"),
        );
        let mcp_transport = fallback.mcp_transport(atomic_cloud::DEFAULT_MCP_SSE_KEEP_ALIVE);
        let tenant_plane_for_app = tenant_plane.clone();
        let chat_streams = ChatStreamLimiter::new(DEFAULT_CHAT_STREAMS_PER_ACCOUNT);
        let app_server = HttpServer::new(move || {
            App::new().configure(configure_cloud_app(
                state.clone(),
                auth.clone(),
                account_plane.clone(),
                tenant_plane_for_app.clone(),
                oauth_plane.clone(),
                mcp_transport.clone(),
                control_for_app.clone(),
                chat_streams.clone(),
                readiness.clone(),
                quota_billing.clone(),
                None,
            ))
        })
        .workers(1)
        .listen(app_listener)
        .expect("attach app listener")
        .run();
        let app_handle = app_server.handle();
        actix_web::rt::spawn(app_server);

        // Internal metrics listener: a SEPARATE HTTP server, as in serve().
        let metrics_listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("bind metrics port");
        let metrics_port = metrics_listener.local_addr().expect("metrics addr").port();
        let metrics_state = MetricsState {
            registry: Arc::clone(&registry),
            cache: Arc::clone(&cache),
            control: control.clone(),
            pools: Some(Arc::clone(dispatcher.pools())),
            tenant_plane,
        };
        let metrics_server = HttpServer::new(move || {
            App::new().configure(configure_metrics_app(metrics_state.clone()))
        })
        .workers(1)
        .listen(metrics_listener)
        .expect("attach metrics listener")
        .run();
        let metrics_handle = metrics_server.handle();
        actix_web::rt::spawn(metrics_server);

        MetricsHarness {
            control,
            cluster,
            cache,
            registry,
            dispatcher,
            client: reqwest::Client::new(),
            app_url: format!("http://127.0.0.1:{app_port}"),
            metrics_url: format!("http://127.0.0.1:{metrics_port}"),
            app_handle,
            metrics_handle,
            _fallback: fallback,
        }
    }

    async fn stop(self) {
        self.app_handle.stop(false).await;
        self.metrics_handle.stop(false).await;
    }

    /// One scrape of the internal listener, asserting transport-level
    /// correctness (status + content type) on every call.
    async fn scrape(&self) -> String {
        let resp = self
            .client
            .get(format!("{}/metrics", self.metrics_url))
            .send()
            .await
            .expect("scrape metrics");
        assert_eq!(resp.status(), StatusCode::OK, "metrics scrape status");
        assert_eq!(
            resp.headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok()),
            Some(PROMETHEUS_CONTENT_TYPE),
            "metrics content type"
        );
        resp.text().await.expect("metrics body")
    }
}

/// Parse the value of the sample whose name (plus label set, when present)
/// is exactly `sample_name`, e.g. `atomic_cloud_uptime_seconds` or
/// `atomic_cloud_account_cache_entries{kind="serving"}`.
fn metric_value(body: &str, sample_name: &str) -> Option<f64> {
    body.lines().find_map(|line| {
        let value = line.strip_prefix(sample_name)?.strip_prefix(' ')?;
        match value {
            "+Inf" => Some(f64::INFINITY),
            "-Inf" => Some(f64::NEG_INFINITY),
            other => other.parse().ok(),
        }
    })
}

#[actix_web::test]
async fn metrics_listener_serves_every_family_and_main_listener_serves_none() {
    with_control_db(
        "metrics_listener_serves_every_family_and_main_listener_serves_none",
        |control_url| async move {
            let harness = MetricsHarness::spawn(&control_url).await;

            let body = harness.scrape().await;
            for family in EXPECTED_FAMILIES {
                assert!(
                    body.contains(&format!("# TYPE {family} ")),
                    "family {family} missing from exposition:\n{body}"
                );
            }
            // Spot-check exposition shape: labeled samples and the info gauge.
            assert!(
                metric_value(
                    &body,
                    "atomic_cloud_account_cache_entries{kind=\"serving\"}"
                )
                .is_some(),
                "serving-entries sample missing"
            );
            assert!(
                body.contains("atomic_cloud_worker_pool_cap{class=\"embedding\"}"),
                "per-class pool cap sample missing"
            );
            assert!(
                body.lines()
                    .any(|l| l.starts_with("atomic_cloud_build_info{") && l.ends_with(" 1")),
                "build info gauge missing"
            );
            // Boot state of every liveness age: +Inf, so a loop that never
            // runs at all (or a dispatcher task that died before its first
            // tick) is alertable from the very first scrape — never a
            // healthy-looking zero.
            for age_family in [
                "atomic_cloud_backup_last_success_age_seconds",
                "atomic_cloud_backup_staleness_check_age_seconds",
                "atomic_cloud_dispatcher_last_tick_age_seconds",
                "atomic_cloud_dispatcher_last_full_scan_age_seconds",
            ] {
                assert_eq!(
                    metric_value(&body, age_family),
                    Some(f64::INFINITY),
                    "{age_family} must be +Inf before its first event"
                );
            }

            // The security property: the PUBLIC listener serves no /metrics
            // on any host — app host, tenant-shaped host, or bare IP.
            for host in [
                format!("app.{BASE_DOMAIN}"),
                format!("someone.{BASE_DOMAIN}"),
                BASE_DOMAIN.to_string(),
            ] {
                let resp = harness
                    .client
                    .get(format!("{}/metrics", harness.app_url))
                    .header(HOST, &host)
                    .send()
                    .await
                    .expect("probe main listener");
                assert_eq!(
                    resp.status(),
                    StatusCode::NOT_FOUND,
                    "/metrics must not exist on the public listener (host {host})"
                );
                let text = resp.text().await.expect("probe body");
                assert!(
                    !text.contains("atomic_cloud_uptime_seconds"),
                    "public listener leaked metrics content (host {host})"
                );
            }

            harness.stop().await;
        },
    )
    .await;
}

#[actix_web::test]
async fn metrics_move_with_cache_faults_dispatcher_ticks_and_backup_passes() {
    with_control_db(
        "metrics_move_with_cache_faults_dispatcher_ticks_and_backup_passes",
        |control_url| async move {
            let harness = MetricsHarness::spawn(&control_url).await;

            let before = harness.scrape().await;
            assert_eq!(
                metric_value(
                    &before,
                    "atomic_cloud_account_cache_entries{kind=\"serving\"}"
                ),
                Some(0.0),
                "cache starts empty"
            );

            // Provision a (keyless) account and drive one authenticated
            // tenant request through the PUBLIC listener: auth resolves the
            // tenant and faults its handle into the cache as a serving entry.
            let account = provision_account(
                &harness.control,
                &harness.cluster,
                &ManagedKeys::Disabled,
                NewAccount {
                    email: "watchtower@example.com".to_string(),
                    // Not "metrics" — that subdomain is reserved
                    // (reserved_subdomains.rs), which is its own kind of
                    // reassuring.
                    subdomain: "watchtower".to_string(),
                },
            )
            .await
            .expect("provision account");
            let token = issue_token(
                &harness.control,
                &account.account_id,
                TokenScope::Account,
                None,
                "metrics-test",
            )
            .await
            .expect("issue token");
            let resp = harness
                .client
                .get(format!("{}/api/atoms", harness.app_url))
                .header(HOST, format!("watchtower.{BASE_DOMAIN}"))
                .bearer_auth(&token)
                .send()
                .await
                .expect("tenant request");
            assert_eq!(resp.status(), StatusCode::OK, "tenant request served");

            let after_fault = harness.scrape().await;
            assert_eq!(
                metric_value(
                    &after_fault,
                    "atomic_cloud_account_cache_entries{kind=\"serving\"}"
                ),
                Some(1.0),
                "the authenticated request faulted one serving entry"
            );
            // The faulted tenant holds pool connections against the cluster.
            assert!(
                metric_value(&after_fault, "atomic_cloud_tenant_pool_connections")
                    .expect("tenant pool sample")
                    >= 1.0,
                "tenant pool aggregate counts the faulted tenant's pool"
            );

            // One dispatcher tick (the first always full-scans): the tick
            // and full-scan gauges move, and the scan saw our tenant.
            let outcome = harness.dispatcher.tick().await;
            assert!(outcome.full_scan, "first tick full-scans");
            for handle in outcome.handles {
                let _ = handle.await;
            }
            let after_tick = harness.scrape().await;
            assert!(
                metric_value(&after_tick, "atomic_cloud_dispatcher_last_tick_seconds")
                    .expect("tick gauge")
                    > 0.0,
                "tick duration recorded"
            );
            assert!(
                metric_value(
                    &after_tick,
                    "atomic_cloud_dispatcher_last_full_scan_tenants"
                )
                .expect("full-scan tenants gauge")
                    >= 1.0,
                "full scan polled the provisioned tenant"
            );
            // The liveness ages flip from +Inf to a live (small) value at
            // tick end — the signal that distinguishes a running dispatcher
            // from a dead task whose duration gauges froze.
            for age_family in [
                "atomic_cloud_dispatcher_last_tick_age_seconds",
                "atomic_cloud_dispatcher_last_full_scan_age_seconds",
            ] {
                let age = metric_value(&after_tick, age_family).expect("dispatcher age sample");
                assert!(
                    age.is_finite() && age < 60.0,
                    "{age_family} finite after a tick (got {age})"
                );
            }

            // A recorded clean backup pass flips the age gauge from +Inf to
            // a finite (small) value — the pass-end site the serve loop
            // drives in production.
            assert_eq!(
                metric_value(&after_tick, "atomic_cloud_backup_last_success_age_seconds"),
                Some(f64::INFINITY)
            );
            harness
                .registry
                .record_backup_pass(&BackupSummary::default(), chrono::Utc::now());
            harness.registry.record_backup_staleness(0);
            let after_backup = harness.scrape().await;
            let age = metric_value(
                &after_backup,
                "atomic_cloud_backup_last_success_age_seconds",
            )
            .expect("backup age sample");
            assert!(
                age.is_finite() && age < 60.0,
                "backup age finite after a clean pass (got {age})"
            );
            // The staleness check's own age flips finite too — a completed
            // all-clear check is an event, so a check that later starts
            // erroring (which never records) reads as a growing age, not a
            // frozen zero on the stale-tenants gauge.
            let check_age = metric_value(
                &after_backup,
                "atomic_cloud_backup_staleness_check_age_seconds",
            )
            .expect("staleness check age sample");
            assert!(
                check_age.is_finite() && check_age < 60.0,
                "staleness check age finite after a completed check (got {check_age})"
            );

            // Eviction counter: dropping the tenant's entry (the deletion
            // path's evict) bumps the counter and empties the gauge.
            assert!(harness.cache.evict(&account.account_id).await);
            let after_evict = harness.scrape().await;
            assert_eq!(
                metric_value(
                    &after_evict,
                    "atomic_cloud_account_cache_entries{kind=\"serving\"}"
                ),
                Some(0.0)
            );
            assert!(
                metric_value(&after_evict, "atomic_cloud_account_cache_evictions_total")
                    .expect("evictions sample")
                    >= 1.0,
                "eviction counted"
            );

            harness.stop().await;
        },
    )
    .await;
}
