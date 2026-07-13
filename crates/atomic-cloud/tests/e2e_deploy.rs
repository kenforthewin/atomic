//! End-to-end deploy simulation (plan: "Provisioning lifecycle" → "Schema
//! migration on deploy" + "Failure recovery & the reaper"): the whole
//! deploy-gating story on real provisioned tenants, through the composed
//! HTTP surface.
//!
//! Where `tests/deploy_gating.rs` pins each gate mechanism in isolation and
//! `tests/reaper.rs` pins the retry arm, this suite plays the full arc a
//! production deploy traverses, in order, against accounts created by the
//! real provisioning flow:
//!
//! 1. **A clean deploy** — an "old fleet" (both tenants stamped below the
//!    compiled target, simulating rows written by the previous binary) meets
//!    the new binary: the pod boots in migrating mode (`/ready` 503,
//!    `/health` 200, lagging tenants 503 `account_upgrading` per request),
//!    the fleet run brings every tenant current, readiness flips, and both
//!    tenants serve with no straggler 503.
//! 2. **A degraded deploy** — one tenant's database is gone out from under
//!    it (dropped at the cluster, mapping row intact: the honest injection
//!    of a real connect failure). The next pod's fleet run records the
//!    failure; with 2 tenants the 50% failure rate lands in the policy's
//!    `rollback_required` band and readiness holds 503.
//! 3. **The data plane serves while readiness is down.** Pinned here at the
//!    HTTP level, deliberately: readiness gates the *load balancer*, not the
//!    process (see `atomic_cloud::deploy`, "What readiness does NOT gate").
//!    Traffic that still reaches the not-ready pod — existing connections,
//!    direct pod access — is served normally for healthy tenants; the
//!    per-request safety for broken ones is CloudAuth's straggler gate, not
//!    readiness. A readiness check in the request path would turn one broken
//!    tenant database into a full-fleet outage.
//! 4. **Self-healing** — the reaper's failed-migrations arm retries on its
//!    recorded backoff schedule: still-failing while the database is gone
//!    (horizon re-armed, count bumped), healed once the database is restored
//!    (schema migrated for real, stamp current, failure state cleared), and
//!    the straggler 503 lifts with no operator action and no pod restart.
//!    The held `rollback_required` verdict is per-run and stays held — the
//!    remedy for that band is redeploying a fixed binary, which the final
//!    fresh gate run simulates (nothing lags anymore → ready).
//!
//! A second simulation (`late_stamped_straggler_heals_without_a_pod_reboot`)
//! plays the rolling-deploy arc the first one can't: a tenant that becomes
//! lagging — with no failure state — *after* the new pod's boot run already
//! enumerated, healed by the reaper's lagging-row arm alone.
//!
//! A third (`fully_stamped_fleet_short_circuits_without_tenant_connections`)
//! pins the version-stamp short-circuit (scaling plan §4, remediation (b)):
//! a fleet already stamped at the compiled target deploys on control-plane
//! work alone — no tenant connection — proven by dropping every tenant
//! database before the run and still landing a clean `ready`.
//!
//! Postgres-gated; see `tests/support/mod.rs` for the skip/cleanup
//! conventions and the run command.

mod support;

use std::sync::Arc;
use std::time::Duration;

use actix_web::{App, HttpServer};
use atomic_cloud::{
    configure_cloud_app, issue_token, latest_deploy_run, provision_account, run_fleet_gate,
    run_reaper_pass, tenant_schema_target, AccountCache, AccountCacheConfig, AccountPlane,
    AccountPlaneConfig, ChatStreamLimiter, CloudAuth, ClusterConfig, ControlPlane, DeployPolicy,
    FallbackAppState, FleetMigrationConfig, FleetMigrator, ManagedKeys, NewAccount, QuotaBilling,
    Readiness, ReaperPolicy, TenantPlane, TokenScope, DEFAULT_CHAT_STREAMS_PER_ACCOUNT,
};
use chrono::{DateTime, Utc};
use reqwest::header::HOST;
use reqwest::{Method, StatusCode};
use serde_json::Value;
use sqlx::{Connection, PgConnection};
use support::{create_database, drop_database, with_control_db, CapturingSender};

const BASE_DOMAIN: &str = "cloudtest.local";

fn cluster_config() -> ClusterConfig {
    ClusterConfig {
        cluster_id: "test-cluster-1".to_string(),
        cluster_url: std::env::var("ATOMIC_TEST_DATABASE_URL")
            .expect("with_control_db verified ATOMIC_TEST_DATABASE_URL"),
    }
}

/// The simulation's run tunables: small fan-out, fast connect failure for
/// the dropped-database tenant, and a ZERO retry-backoff base so the
/// reaper's retries are due on the very next pass instead of a real-time
/// minute later (`migration_backoff_horizon(0 * 2^n) = now` — the row is
/// genuinely due by the arm's own predicate; nothing is backdated by SQL).
fn sim_config() -> FleetMigrationConfig {
    FleetMigrationConfig {
        concurrency: 8,
        tenant_connect_timeout: Duration::from_secs(5),
        wall_clock_limit: Duration::from_secs(120),
        retry_backoff_base: Duration::ZERO,
        ..FleetMigrationConfig::default()
    }
}

/// A provisioned account plus the token the simulation drives it with.
struct Tenant {
    account_id: String,
    subdomain: String,
    db_name: String,
    token: String,
}

/// One "pod": the composed cloud app (exactly as `atomic-cloud serve` wires
/// it) on an ephemeral port, with its own per-process `AccountCache` and the
/// caller's [`Readiness`] — so a test can boot several pods against one
/// control plane, each holding its own deploy verdict.
struct Pod {
    client: reqwest::Client,
    base_url: String,
    handle: actix_web::dev::ServerHandle,
    /// Owns the scratch directory behind the inert fallback `AppState`;
    /// must outlive the server.
    _fallback: FallbackAppState,
}

impl Pod {
    async fn spawn(control: &ControlPlane, cluster: &ClusterConfig, readiness: Readiness) -> Self {
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
            Arc::new(CapturingSender::default()),
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

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let port = listener.local_addr().expect("local addr").port();
        let state = fallback.data();
        let oauth_plane = atomic_cloud::OAuthPlane::new(
            control.clone(),
            BASE_DOMAIN,
            "http",
            format!("http://app.{BASE_DOMAIN}"),
        );
        let mcp_transport = fallback.mcp_transport(atomic_cloud::DEFAULT_MCP_SSE_KEEP_ALIVE);
        let control_for_app = control.clone();
        let chat_streams = ChatStreamLimiter::new(DEFAULT_CHAT_STREAMS_PER_ACCOUNT);
        let quota_billing = QuotaBilling::for_tests(control.clone(), BASE_DOMAIN)
            .await
            .expect("plans");
        let server = HttpServer::new(move || {
            App::new().configure(configure_cloud_app(
                state.clone(),
                auth.clone(),
                account_plane.clone(),
                tenant_plane.clone(),
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
        .listen(listener)
        .expect("attach listener")
        .run();
        let handle = server.handle();
        actix_web::rt::spawn(server);

        Pod {
            client: reqwest::Client::new(),
            base_url: format!("http://127.0.0.1:{port}"),
            handle,
            _fallback: fallback,
        }
    }

    async fn stop(self) {
        self.handle.stop(false).await;
    }

    /// `GET /ready` — public, no auth, no tenant Host.
    async fn ready(&self) -> (StatusCode, Value) {
        let resp = self
            .client
            .get(format!("{}/ready", self.base_url))
            .send()
            .await
            .expect("GET /ready");
        let status = resp.status();
        (status, resp.json().await.expect("ready body is json"))
    }

    /// `GET /health` — liveness, public throughout.
    async fn health_status(&self) -> StatusCode {
        self.client
            .get(format!("{}/health", self.base_url))
            .send()
            .await
            .expect("GET /health")
            .status()
    }

    /// Drive one authenticated data-plane request (`GET /api/atoms`) as the
    /// tenant and return the raw response — the probe for "does this account
    /// serve right now?".
    async fn list_atoms_response(&self, tenant: &Tenant) -> reqwest::Response {
        self.client
            .request(Method::GET, format!("{}/api/atoms", self.base_url))
            .header(HOST, format!("{}.{BASE_DOMAIN}", tenant.subdomain))
            .bearer_auth(&tenant.token)
            .send()
            .await
            .expect("send list atoms")
    }

    /// Assert the tenant serves: 200 with an atoms listing.
    async fn assert_serves(&self, tenant: &Tenant, context: &str) {
        let resp = self.list_atoms_response(tenant).await;
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "{}: {context}",
            tenant.subdomain
        );
        let body: Value = resp.json().await.expect("atoms json");
        assert!(body["atoms"].is_array(), "listing shape");
    }

    /// Assert the tenant is held by the straggler gate: the plan's
    /// structured 503 `account_upgrading` with `Retry-After`.
    async fn assert_upgrading(&self, tenant: &Tenant, context: &str) {
        let resp = self.list_atoms_response(tenant).await;
        assert_eq!(
            resp.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "{}: {context}",
            tenant.subdomain
        );
        assert_eq!(
            resp.headers()
                .get(reqwest::header::RETRY_AFTER)
                .map(|v| v.as_bytes()),
            Some("60".as_bytes()),
            "straggler 503 carries Retry-After"
        );
        let body: Value = resp.json().await.expect("error json");
        assert_eq!(body["error"], "account_upgrading", "{context}");
    }
}

/// Provision an account through the real flow (keyless — the deploy story
/// needs no AI provider; the cache serves CRUD on a keyless config) and
/// issue an account-scope token.
async fn provision(control: &ControlPlane, cluster: &ClusterConfig, subdomain: &str) -> Tenant {
    let account = provision_account(
        control,
        cluster,
        &ManagedKeys::Disabled,
        NewAccount {
            email: format!("{subdomain}@example.com"),
            subdomain: subdomain.to_string(),
        },
    )
    .await
    .expect("provision account");
    let token = issue_token(
        control,
        &account.account_id,
        TokenScope::Account,
        None,
        "e2e-deploy",
    )
    .await
    .expect("issue account token");
    Tenant {
        account_id: account.account_id,
        subdomain: subdomain.to_string(),
        db_name: account.db_name,
        token,
    }
}

/// Stamp every mapping row at `version` — the simulation of an old fleet
/// meeting a new binary (the previous binary stamped the rows at its own,
/// lower target). The tenants' actual schemas stay current, so the fleet
/// run's `initialize()` no-ops idempotently and re-records success — exactly
/// the correct behavior for an already-current schema.
async fn stamp_fleet(control: &ControlPlane, version: i32) {
    sqlx::query("UPDATE account_databases SET last_migrated_version = $1")
        .bind(version)
        .execute(control.pool())
        .await
        .expect("stamp fleet below target");
}

/// One mapping row's migration-tracking state: (last_migrated_version,
/// migration_failed_at, last_migration_error, migration_retry_after,
/// migration_retry_count).
type RowState = (
    i32,
    Option<DateTime<Utc>>,
    Option<String>,
    Option<DateTime<Utc>>,
    i32,
);

async fn row_state(control: &ControlPlane, account_id: &str) -> RowState {
    sqlx::query_as(
        "SELECT last_migrated_version, migration_failed_at, last_migration_error, \
                migration_retry_after, migration_retry_count \
         FROM account_databases WHERE account_id = $1",
    )
    .bind(account_id)
    .fetch_one(control.pool())
    .await
    .expect("read mapping row state")
}

/// The tenant database's real schema version — the honest check that a
/// stamp corresponds to applied DDL, not just bookkeeping.
async fn tenant_schema_version(cluster: &ClusterConfig, db_name: &str) -> i32 {
    let url = cluster.tenant_db_url(db_name).expect("tenant url");
    let mut conn = PgConnection::connect(&url).await.expect("connect tenant");
    let version: i32 = sqlx::query_scalar("SELECT MAX(version) FROM schema_version")
        .fetch_one(&mut conn)
        .await
        .expect("read tenant schema version");
    conn.close().await.expect("close");
    version
}

/// The full deploy story, in one arc — see the module docs for the phases.
#[actix_web::test]
async fn full_deploy_simulation() {
    with_control_db("full_deploy_simulation", |url| async move {
        let control = ControlPlane::connect(
            &url,
            atomic_cloud::control_plane::DEFAULT_CONTROL_POOL_MAX_CONNECTIONS,
        )
        .await
        .expect("connect control");
        control.initialize().await.expect("migrate control plane");
        let cluster = cluster_config();
        let target = tenant_schema_target();

        let alpha = provision(&control, &cluster, "alpha").await;
        let bravo = provision(&control, &cluster, "bravo").await;

        // ============ Phase 1: an old fleet meets a new binary ============

        stamp_fleet(&control, target - 1).await;

        // Pod A boots in migrating mode, exactly as `serve` starts.
        let readiness_a = Readiness::new(control.clone());
        let pod_a = Pod::spawn(&control, &cluster, readiness_a.clone()).await;

        // Liveness up, readiness 503 — the split orchestrators key on.
        assert_eq!(pod_a.health_status().await, StatusCode::OK);
        let (status, body) = pod_a.ready().await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["status"], "migrating");

        // While the fleet migrates, lagging tenants hold per request.
        pod_a
            .assert_upgrading(&alpha, "stale-stamped tenant holds while the pod migrates")
            .await;

        // The boot fleet gate — the same future `serve` spawns.
        run_fleet_gate(
            control.clone(),
            cluster.clone(),
            sim_config(),
            DeployPolicy::default(),
            readiness_a.clone(),
        )
        .await;

        // 0% failures: readiness flips and the run is recorded ready.
        let (status, body) = pod_a.ready().await;
        assert_eq!(status, StatusCode::OK, "clean fleet run flips ready");
        assert_eq!(body["status"], "ready");
        let run = latest_deploy_run(&control)
            .await
            .expect("read latest run")
            .expect("run recorded");
        assert_eq!(run.deploy_status, "ready");
        assert_eq!(run.target_version, target);
        assert_eq!(
            (run.total, run.migrated, run.failed),
            (Some(2), Some(2), Some(0))
        );

        // Both tenants serve — no straggler 503 — and their stamps are
        // clean and current.
        pod_a.assert_serves(&alpha, "serves after the deploy").await;
        pod_a.assert_serves(&bravo, "serves after the deploy").await;
        for tenant in [&alpha, &bravo] {
            let (version, failed_at, error, retry_after, retries) =
                row_state(&control, &tenant.account_id).await;
            assert_eq!(version, target, "{} stamped current", tenant.subdomain);
            assert!(
                failed_at.is_none() && error.is_none() && retry_after.is_none() && retries == 0,
                "{}: no failure state after a clean run",
                tenant.subdomain
            );
        }

        // Pod A's part is over. (In a rolling deploy it would keep serving
        // while the next deploy's pods boot; one pod per phase keeps the
        // simulation's assertions unambiguous.)
        pod_a.stop().await;

        // ====== Phase 2: a degraded deploy — one tenant database gone ======

        // bravo's database is dropped out from under its (intact) mapping
        // row — the honest injection: the fleet run gets a real connect
        // failure, not a mock.
        drop_database(&cluster.cluster_url, &bravo.db_name).await;
        stamp_fleet(&control, target - 1).await;

        let readiness_b = Readiness::new(control.clone());
        let pod_b = Pod::spawn(&control, &cluster, readiness_b.clone()).await;
        let (status, body) = pod_b.ready().await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["status"], "migrating");

        run_fleet_gate(
            control.clone(),
            cluster.clone(),
            sim_config(),
            DeployPolicy::default(),
            readiness_b.clone(),
        )
        .await;

        // 1 failure / 2 tenants = 50% ≥ the 10% threshold: the policy holds
        // the pod at rollback_required.
        let (status, body) = pod_b.ready().await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["status"], "rollback_required");
        let run = latest_deploy_run(&control)
            .await
            .expect("read latest run")
            .expect("run recorded");
        assert_eq!(run.deploy_status, "rollback_required");
        assert_eq!(
            (run.total, run.migrated, run.failed),
            (Some(2), Some(1), Some(1))
        );

        // THE PINNED BEHAVIOR (module docs, point 3): the data plane serves
        // while readiness is down. alpha — migrated and stamped by the run
        // before the verdict landed — serves normally through the not-ready
        // pod; readiness only tells the load balancer to stop routing here.
        // bravo is held by CloudAuth's per-request straggler gate, which is
        // the per-tenant safety layer readiness never was.
        assert_eq!(pod_b.health_status().await, StatusCode::OK);
        pod_b
            .assert_serves(&alpha, "healthy tenant serves through a not-ready pod")
            .await;
        pod_b
            .assert_upgrading(&bravo, "broken tenant is gated per request, not per pod")
            .await;

        // The failure is fully recorded for the reaper: stamp untouched,
        // error text, first retry, and a horizon already due (zero base).
        let (version, failed_at, error, retry_after, retries) =
            row_state(&control, &bravo.account_id).await;
        assert_eq!(version, target - 1, "a failed tenant's stamp is untouched");
        assert!(failed_at.is_some());
        assert!(!error.expect("error recorded").is_empty());
        assert!(retry_after.expect("horizon recorded") <= Utc::now());
        assert_eq!(retries, 1);

        // ============ Phase 3: the reaper heals the straggler ============

        let policy = ReaperPolicy {
            migration_retry: sim_config(),
            ..ReaperPolicy::default()
        };

        // While the database is still gone, the retry fails honestly: the
        // count bumps and a fresh horizon is re-armed (still due — zero
        // base) — the arm keeps trying, it never gives up on the row.
        let summary = run_reaper_pass(&control, &cluster, &ManagedKeys::Disabled, &policy).await;
        assert_eq!(
            summary.migrations_still_failing,
            vec![bravo.account_id.clone()]
        );
        assert!(summary.migrations_recovered.is_empty());
        assert!(summary.errors.is_empty(), "errors: {:?}", summary.errors);
        let (_, _, _, _, retries) = row_state(&control, &bravo.account_id).await;
        assert_eq!(retries, 2, "the failed retry bumped the count");
        pod_b
            .assert_upgrading(&bravo, "still held while the database is gone")
            .await;

        // The operator restores the database (an empty one is the minimal
        // honest restore — the reaper's retry runs the real migrations and
        // the first tenant load re-seeds the default knowledge base).
        create_database(&cluster.cluster_url, &bravo.db_name).await;

        let summary = run_reaper_pass(&control, &cluster, &ManagedKeys::Disabled, &policy).await;
        assert_eq!(summary.migrations_recovered, vec![bravo.account_id.clone()]);
        assert!(summary.migrations_still_failing.is_empty());
        assert!(summary.errors.is_empty(), "errors: {:?}", summary.errors);

        // Healed for real: schema applied, stamp current, failure state
        // cleared — and the straggler 503 lifts with no pod restart.
        assert_eq!(
            tenant_schema_version(&cluster, &bravo.db_name).await,
            target
        );
        let (version, failed_at, error, retry_after, retries) =
            row_state(&control, &bravo.account_id).await;
        assert_eq!(version, target);
        assert!(failed_at.is_none() && error.is_none() && retry_after.is_none() && retries == 0);
        pod_b
            .assert_serves(&bravo, "straggler serves again after the reaper heals it")
            .await;
        pod_b
            .assert_serves(&alpha, "alpha unaffected throughout")
            .await;

        // The verdict is per-run and stays held: healing a tenant does not
        // rewrite a rollback_required review. The remedy is redeploying a
        // fixed binary — whose boot gate now finds nothing lagging and goes
        // straight to ready.
        let (status, body) = pod_b.ready().await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["status"], "rollback_required");

        let readiness_c = Readiness::new(control.clone());
        run_fleet_gate(
            control.clone(),
            cluster.clone(),
            sim_config(),
            DeployPolicy::default(),
            readiness_c.clone(),
        )
        .await;
        assert!(
            readiness_c.is_ready().await,
            "the redeployed binary's gate finds a healthy fleet and goes ready"
        );

        pod_b.stop().await;
    })
    .await;
}

/// The ownerless-straggler arc: an old-binary pod completes a signup
/// mid-rolling-deploy and stamps its own *lower* target — strictly after
/// this (new-binary, already-ready) pod's boot fleet run enumerated. The
/// boot runner enumerates exactly once per pod lifetime, so under the old
/// "boot runner owns unattempted rows / reaper owns failure state" split
/// this tenant had no owner: no failure state for the reaper, no future
/// enumeration from any pod — CloudAuth 503s (`account_upgrading`) forever.
/// The reaper's lagging-row arm owns it now: one ordinary pass lifts the
/// 503 with no operator action and **no pod reboot**.
#[actix_web::test]
async fn late_stamped_straggler_heals_without_a_pod_reboot() {
    with_control_db(
        "late_stamped_straggler_heals_without_a_pod_reboot",
        |url| async move {
            let control = ControlPlane::connect(
                &url,
                atomic_cloud::control_plane::DEFAULT_CONTROL_POOL_MAX_CONNECTIONS,
            )
            .await
            .expect("connect control");
            control.initialize().await.expect("migrate control plane");
            let cluster = cluster_config();
            let target = tenant_schema_target();

            // The new binary boots, finds nothing lagging, and goes ready.
            let readiness = Readiness::new(control.clone());
            let pod = Pod::spawn(&control, &cluster, readiness.clone()).await;
            run_fleet_gate(
                control.clone(),
                cluster.clone(),
                sim_config(),
                DeployPolicy::default(),
                readiness.clone(),
            )
            .await;
            let (status, _) = pod.ready().await;
            assert_eq!(status, StatusCode::OK, "clean boot gate goes ready");

            // An old-binary pod, still serving during the rolling deploy,
            // completes a signup and stamps its own lower target. (The
            // provision is real — the tenant's schema is genuinely current;
            // only the control-plane stamp lags, exactly as an old binary
            // writing its compiled target leaves it.)
            let gamma = provision(&control, &cluster, "gamma").await;
            stamp_fleet(&control, target - 1).await;

            // Held per request by the straggler gate; readiness — which was
            // never the per-tenant safety layer — stays ready.
            pod.assert_upgrading(&gamma, "late-stamped tenant holds per request")
                .await;
            let (status, _) = pod.ready().await;
            assert_eq!(status, StatusCode::OK);

            // The ownerless shape: lagging with NO failure state at all.
            let (version, failed_at, error, retry_after, retries) =
                row_state(&control, &gamma.account_id).await;
            assert_eq!(version, target - 1);
            assert!(
                failed_at.is_none() && error.is_none() && retry_after.is_none() && retries == 0,
                "no failure state — nothing but the lagging-row arm will ever retry this"
            );

            // One ordinary reaper pass. No reboot, no redeploy, no operator.
            let policy = ReaperPolicy {
                migration_retry: sim_config(),
                ..ReaperPolicy::default()
            };
            let summary =
                run_reaper_pass(&control, &cluster, &ManagedKeys::Disabled, &policy).await;
            assert!(summary.errors.is_empty(), "errors: {:?}", summary.errors);
            assert_eq!(summary.migrations_recovered, vec![gamma.account_id.clone()]);

            // Healed: stamp current, and the 503 lifts on the same pod.
            let (version, failed_at, _, _, retries) = row_state(&control, &gamma.account_id).await;
            assert_eq!(version, target);
            assert!(failed_at.is_none() && retries == 0);
            pod.assert_serves(&gamma, "straggler serves again; the pod never rebooted")
                .await;

            pod.stop().await;
        },
    )
    .await;
}

/// The version-stamp short-circuit (scaling plan §4, remediation (b)): a
/// deploy where every tenant's stamp already equals the compiled target
/// must not open a single tenant connection — the fleet run is
/// control-plane work alone, and the gate goes straight to ready.
///
/// The no-connection proof is injected, not inferred from counters: both
/// tenant databases are **dropped** before the run, so any connection the
/// runner opened would fail and be recorded (2/2 failures →
/// `rollback_required`). The clean `ready` verdict over dropped databases
/// is reachable only through the short-circuit. The below-target contrast
/// at the end re-runs over the same dropped databases and shows lagging
/// rows still get real connection attempts — the skip is exactly
/// stamp-deep, never broader.
#[actix_web::test]
async fn fully_stamped_fleet_short_circuits_without_tenant_connections() {
    with_control_db("fleet_stamp_short_circuit", |url| async move {
        let control = ControlPlane::connect(
            &url,
            atomic_cloud::control_plane::DEFAULT_CONTROL_POOL_MAX_CONNECTIONS,
        )
        .await
        .expect("connect control");
        control.initialize().await.expect("migrate control plane");
        let cluster = cluster_config();
        let target = tenant_schema_target();

        let delta = provision(&control, &cluster, "delta").await;
        let echo = provision(&control, &cluster, "echo").await;
        // provision_account already stamps the compiled target; restamp
        // explicitly so the precondition is the test's own spec, not a
        // provisioning detail.
        stamp_fleet(&control, target).await;

        drop_database(&cluster.cluster_url, &delta.db_name).await;
        drop_database(&cluster.cluster_url, &echo.db_name).await;

        // The runner's own summary: the whole fleet skipped on stamps,
        // nothing attempted, nothing failed — over databases that do not
        // exist to connect to.
        let outcome = FleetMigrator::new(control.clone(), cluster.clone(), sim_config())
            .run()
            .await;
        assert_eq!(
            outcome.skipped_current, 2,
            "both tenants counted skipped-current"
        );
        assert_eq!(outcome.total, 0, "zero tenants attempted");
        assert_eq!(
            (outcome.migrated, outcome.failed),
            (0, 0),
            "a connect to a dropped database would have recorded a failure"
        );
        assert_eq!(outcome.unattempted(), 0);
        assert!(!outcome.timed_out);

        // The full boot gate over the same fleet: a 100%-skipped run is a
        // clean success — readiness flips with no operator action, and the
        // recorded run row shows the empty attempted set PLUS the skipped
        // count (migration 022) that makes 0/0/0 legible as "fleet fully
        // current" rather than "the gate saw no fleet at all" — durably,
        // for the `deploy status` an incident reads after logs rotate.
        let readiness = Readiness::new(control.clone());
        run_fleet_gate(
            control.clone(),
            cluster.clone(),
            sim_config(),
            DeployPolicy::default(),
            readiness.clone(),
        )
        .await;
        assert!(
            readiness.is_ready().await,
            "fully-skipped gate flips readiness"
        );
        let run = latest_deploy_run(&control)
            .await
            .expect("read latest run")
            .expect("run recorded");
        assert_eq!(run.deploy_status, "ready");
        assert_eq!(
            (run.total, run.migrated, run.failed),
            (Some(0), Some(0), Some(0))
        );
        assert_eq!(
            run.skipped_current,
            Some(2),
            "the ledger row carries the skipped count — 0/0/0 alone is \
             indistinguishable from a run that enumerated no fleet"
        );

        // The mapping rows are untouched: current stamps, no failure state
        // — the dropped databases were never reached for.
        for tenant in [&delta, &echo] {
            let (version, failed_at, error, retry_after, retries) =
                row_state(&control, &tenant.account_id).await;
            assert_eq!(version, target, "{} stamp untouched", tenant.subdomain);
            assert!(
                failed_at.is_none() && error.is_none() && retry_after.is_none() && retries == 0,
                "{}: no failure state — no connection was attempted",
                tenant.subdomain
            );
        }

        // Contrast: one stamp below target and the same fleet gets real
        // connection attempts — the dropped databases now surface as two
        // recorded connect failures, proving the skip keys on the stamp
        // and nothing else.
        stamp_fleet(&control, target - 1).await;
        let outcome = FleetMigrator::new(control.clone(), cluster.clone(), sim_config())
            .run()
            .await;
        assert_eq!(outcome.skipped_current, 0);
        assert_eq!(outcome.total, 2, "lagging rows are enumerated");
        assert_eq!(
            outcome.failed, 2,
            "lagging rows get real connection attempts"
        );
    })
    .await;
}
