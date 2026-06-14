//! End-to-end disaster-recovery rehearsal for the backup machinery (plan:
//! "Backups & disaster recovery" → "Restore runbook" — *write and rehearse
//! before launch*).
//!
//! Where [`tests/backup.rs`](backup.rs) unit-tests each seam, this suite runs
//! the **whole rehearsal through the composed cloud server** with two tenants,
//! `alpha` and `beta`, and proves the load-bearing guarantees the plan calls
//! mandatory before real user data exists:
//!
//! - a nightly pass dumps **both** tenants plus the control plane, stamps
//!   `last_backup_at`, and records a `backup_runs` row;
//! - deleting `alpha` takes the **final dump before the drop** (the operator's
//!   only undo under hard-delete v1), `alpha` then 404s through the composed
//!   app and its database is gone from `pg_database`, while `beta` keeps
//!   serving its atom and keeps its backups;
//! - restoring `alpha` from that exact final dump into a **fresh** database,
//!   repointing `account_databases.db_name`, and **evicting** the running
//!   server's `AccountCache` entry brings `alpha` back — its atom rehydrated,
//!   served live through the same app — with `beta` untouched throughout.
//!
//! **Per-tenant isolation is asserted at every step**: a backup, delete, or
//! restore of one tenant never reads or writes another tenant's data, control
//! rows, or store keys beyond its own prefix.
//!
//! A second test pins the staleness monitor: a tenant with a manufactured-old
//! `last_backup_at` surfaces in the staleness alert and `backup status`, while
//! a freshly-backed-up one does not.
//!
//! Postgres-gated and `pg_dump`/`pg_restore`-gated (real dumps + restores run
//! locally against the pg16 cluster; a bare CI image skips with a message).
//! All dump bytes land in a unique temp dir (the local [`BackupStore`]) removed
//! on drop; tenant databases — including the fresh restore target, which no
//! control row tracks — are dropped by guards. Nothing is left behind. This
//! suite **never** starts an `atomic-cloud serve` process (its reaper/backup
//! loops would contend the test cluster); it drives `run_backup_pass`,
//! `delete_account`, and `restore_database` directly and routes HTTP through an
//! in-process composed app.

mod support;

use std::sync::Arc;
use std::time::Duration;

use actix_web::{App, HttpServer};
use atomic_cloud::backup::backup_tools_available;
use atomic_cloud::{
    configure_cloud_app, delete_account, issue_token, provision_account, restore_database,
    run_backup_pass, set_active_provider, stale_tenant_backups, tenant_backup_status,
    tenant_db_name, upsert_credentials, AccountCache, AccountCacheConfig, AccountPlane,
    AccountPlaneConfig, BackupConfig, BackupStore, ChatStreamLimiter, CloudAuth, ClusterConfig,
    ControlPlane, CredentialOrigin, DumpConnection, FallbackAppState, LocalFileSystemStore,
    ManagedKeys, NewAccount, NewCredentials, Provider, QuotaBilling, Readiness, SecretKey,
    TenantPlane, TokenScope, DEFAULT_CHAT_STREAMS_PER_ACCOUNT,
};
use atomic_core::{CreateAtomRequest, DatabaseManager};
use atomic_test_support::MockAiServer;
use reqwest::header::HOST;
use reqwest::{Method, StatusCode};
use serde_json::{json, Value};
use sqlx::{Connection, PgConnection};
use support::{with_control_db, with_db_guard};

/// Base domain the composition is configured with; tenants are addressed as
/// `<subdomain>.cloudtest.local` while TCP goes to `127.0.0.1`.
const BASE_DOMAIN: &str = "cloudtest.local";

/// A provisioned tenant, plus the marker atom seeded into it and the auth
/// token the composed app drives it with.
struct Tenant {
    account_id: String,
    subdomain: String,
    db_name: String,
    token: String,
    atom_id: String,
    /// The recognizable content the seeded atom carries, asserted after a
    /// backup→restore roundtrip.
    marker: String,
}

/// The composed cloud server on an ephemeral port plus the handles a backup
/// rehearsal needs: the control plane and cluster (to run a pass / delete /
/// restore), the live `AccountCache` (the restore path must evict it), and a
/// unique local [`BackupStore`] (where every dump lands).
struct BackupHarness {
    control: ControlPlane,
    cluster: ClusterConfig,
    cache: Arc<AccountCache>,
    mock: MockAiServer,
    store: Arc<dyn BackupStore>,
    client: reqwest::Client,
    base_url: String,
    handle: actix_web::dev::ServerHandle,
    /// The local store's base dir, removed on drop — no dump file survives.
    _store_dir: tempfile::TempDir,
    _fallback: FallbackAppState,
}

impl BackupHarness {
    /// Spawn the composition exactly as `atomic-cloud serve` wires it (minus
    /// the backup/reaper loops, which this suite drives by hand), backed by a
    /// fresh local backup store.
    async fn spawn(control_url: &str) -> Self {
        let control = ControlPlane::connect(control_url)
            .await
            .expect("connect control plane");
        control.initialize().await.expect("migrate control plane");
        let cluster = ClusterConfig {
            cluster_id: "test-cluster-1".to_string(),
            cluster_url: std::env::var("ATOMIC_TEST_DATABASE_URL")
                .expect("with_control_db verified ATOMIC_TEST_DATABASE_URL"),
        };
        let mock = MockAiServer::start().await;
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

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let port = listener.local_addr().expect("local addr").port();
        let state = fallback.data();
        let control_for_app = control.clone();
        let chat_streams = ChatStreamLimiter::new(DEFAULT_CHAT_STREAMS_PER_ACCOUNT);
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
            ))
        })
        .workers(1)
        .listen(listener)
        .expect("attach listener")
        .run();
        let handle = server.handle();
        actix_web::rt::spawn(server);

        let store_dir = tempfile::tempdir().expect("create temp backup dir");
        let store: Arc<dyn BackupStore> =
            Arc::new(LocalFileSystemStore::new(store_dir.path().to_path_buf()));

        BackupHarness {
            control,
            cluster,
            cache,
            mock,
            store,
            client: reqwest::Client::new(),
            base_url: format!("http://127.0.0.1:{port}"),
            handle,
            _store_dir: store_dir,
            _fallback: fallback,
        }
    }

    async fn stop(self) {
        self.handle.stop(false).await;
    }

    /// Provision a tenant pointing at the mock AI server, issue an account
    /// token, and seed a recognizable atom **through its own tenant manager**
    /// (the task's "alpha via its tenant manager; beta too" — the atom is the
    /// thing the backup must capture and the restore must rehydrate).
    async fn provision_with_atom(&self, subdomain: &str) -> Tenant {
        let account = provision_account(
            &self.control,
            &self.cluster,
            &ManagedKeys::Disabled,
            NewAccount {
                email: format!("{subdomain}@example.com"),
                subdomain: subdomain.to_string(),
            },
        )
        .await
        .expect("provision account");

        // Point the tenant at the mock provider through the control plane (the
        // cache resolves provider config from there), so a live HTTP request
        // never reaches a real provider.
        let vault = support::test_vault();
        upsert_credentials(
            &self.control,
            vault.as_ref(),
            &account.account_id,
            NewCredentials {
                provider: Provider::OpenAiCompat,
                origin: CredentialOrigin::User,
                api_key: SecretKey::new("test-key".to_string()),
                external_key_id: None,
                model_config: json!({
                    "embedding_model": "mock-embed",
                    "llm_model": "mock-llm",
                    "openai_compat_base_url": self.mock.base_url(),
                    "embedding_dimension": 1536,
                }),
            },
        )
        .await
        .expect("store mock provider credentials");
        set_active_provider(
            &self.control,
            &account.account_id,
            Some((Provider::OpenAiCompat, CredentialOrigin::User)),
        )
        .await
        .expect("activate mock provider credentials");

        // Seed the marker atom directly through the tenant manager. A
        // subdomain-unique marker makes cross-tenant leakage impossible to miss.
        let marker = format!("backup-e2e-marker-{subdomain}-3f9a1c");
        let source_url = format!("https://example.com/{subdomain}-source");
        let tenant_url = self
            .cluster
            .tenant_db_url(&account.db_name)
            .expect("tenant url");
        let atom_id = {
            let manager = DatabaseManager::new_postgres(".", &tenant_url)
                .await
                .expect("open tenant manager");
            let core = manager.active_core().await.expect("active core");
            // Don't run the embedding pipeline while seeding: a tenant manager
            // opened only to write the marker atom should not kick off
            // background provider calls whose in-flight tasks could outlive the
            // dropped manager. The atom ROW is what the backup must capture.
            core.set_inline_pipeline(false);
            let created = core
                .create_atom(
                    CreateAtomRequest {
                        content: format!("# {subdomain}\n\n{marker} body text"),
                        source_url: Some(source_url),
                        ..Default::default()
                    },
                    |_| {},
                )
                .await
                .expect("create atom")
                .expect("atom inserted");
            drop(core);
            drop(manager);
            created.atom.id
        };

        let token = issue_token(
            &self.control,
            &account.account_id,
            TokenScope::Account,
            None,
            "e2e-backup",
        )
        .await
        .expect("issue account token");

        Tenant {
            account_id: account.account_id,
            subdomain: subdomain.to_string(),
            db_name: account.db_name,
            token,
            atom_id,
            marker,
        }
    }

    /// Request builder addressed at `subdomain.<BASE_DOMAIN>` over loopback.
    fn api(&self, method: Method, subdomain: &str, path: &str) -> reqwest::RequestBuilder {
        self.client
            .request(method, format!("{}{path}", self.base_url))
            .header(HOST, format!("{subdomain}.{BASE_DOMAIN}"))
    }

    /// Fetch a tenant's atom by id through the composed app, returning the
    /// HTTP status and the body text. The cross-tenant chokepoint runs for
    /// real — a deleted tenant 404s, a live one serves its own data.
    async fn get_atom(&self, tenant: &Tenant) -> (StatusCode, String) {
        let resp = self
            .api(
                Method::GET,
                &tenant.subdomain,
                &format!("/api/atoms/{}", tenant.atom_id),
            )
            .bearer_auth(&tenant.token)
            .send()
            .await
            .expect("send get atom");
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        (status, body)
    }

    /// Assert a tenant serves its own marker atom live through the app. A
    /// correctly provisioned (or correctly restored + repointed) tenant is
    /// immediately serveable — the only way to a 503 here is a real fault
    /// (e.g. a restored mapping that forgot to record `last_migrated_version`,
    /// which CloudAuth's straggler gate would 503 forever), so the body is
    /// surfaced on failure.
    async fn assert_serves_marker(&self, tenant: &Tenant) {
        let (status, body) = self.get_atom(tenant).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "{} must serve its atom; body={body}",
            tenant.subdomain
        );
        let json: Value = serde_json::from_str(&body).expect("atom json");
        assert!(
            json["content"]
                .as_str()
                .unwrap_or("")
                .contains(&tenant.marker),
            "{} must serve ITS OWN marker {:?}, got {:?}",
            tenant.subdomain,
            tenant.marker,
            json["content"]
        );
    }
}

/// Whether `db_name` exists on the test cluster.
async fn database_exists(db_name: &str) -> bool {
    let base_url = std::env::var("ATOMIC_TEST_DATABASE_URL").expect("env");
    let mut conn = PgConnection::connect(&base_url).await.expect("connect");
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_database WHERE datname = $1)")
            .bind(db_name)
            .fetch_one(&mut conn)
            .await
            .expect("query pg_database");
    let _ = conn.close().await;
    exists
}

/// The dated nightly-tree keys present in the store for a given day.
async fn nightly_keys(
    store: &Arc<dyn BackupStore>,
    now: chrono::DateTime<chrono::Utc>,
) -> Vec<String> {
    store
        .list(&format!("backups/{}/", now.format("%Y-%m-%d")))
        .await
        .expect("list nightly keys")
}

// ==================== The full disaster-recovery rehearsal ====================

/// Provision alpha + beta with recognizable atoms, run a nightly pass (both
/// dumped, both stamped, a `backup_runs` row), simulate the disaster by
/// deleting alpha (final dump taken before the drop), confirm alpha is gone
/// (404 via the app, absent from `pg_database`) while **beta is wholly
/// unaffected** (still serves its atom, its nightly dump still present), then
/// restore alpha from its final dump into a fresh DB, repoint
/// `account_databases.db_name`, evict the cache entry, and confirm alpha's atom
/// is recovered and served live — beta untouched throughout.
///
/// Per-tenant isolation is asserted at every step.
#[actix_web::test]
async fn full_disaster_recovery_rehearsal() {
    if !backup_tools_available().await {
        eprintln!("full_disaster_recovery_rehearsal: skipping (pg_dump/pg_restore not on PATH)");
        return;
    }
    with_control_db("full_disaster_recovery_rehearsal", |url| async move {
        let h = BackupHarness::spawn(&url).await;
        // "beta" is on the reserved-subdomain blocklist; use distinct,
        // unreserved subdomains while keeping the alpha/beta roles in the
        // variable names and markers.
        let alpha = h.provision_with_atom("alphakb").await;
        let beta = h.provision_with_atom("betakb").await;

        // Sanity: both tenants serve their OWN atom live, and neither leaks
        // into the other (the cross-tenant chokepoint runs for real).
        h.assert_serves_marker(&alpha).await;
        h.assert_serves_marker(&beta).await;
        let (cross, _) = h
            .api(
                Method::GET,
                &beta.subdomain,
                &format!("/api/atoms/{}", alpha.atom_id),
            )
            .bearer_auth(&beta.token)
            .send()
            .await
            .map(|r| (r.status(), ()))
            .expect("send cross-tenant probe");
        assert_eq!(
            cross,
            StatusCode::NOT_FOUND,
            "alpha's atom id must not resolve inside beta's database"
        );

        // ---- Step 1: a nightly backup pass. ----
        let pass_at = chrono::Utc::now();
        let summary =
            run_backup_pass(&h.control, &h.cluster, &h.store, &BackupConfig::default(), pass_at)
                .await;
        assert_eq!(
            summary.tenants_backed_up.len(),
            2,
            "both tenants backed up: {summary:?}"
        );
        assert!(summary.tenants_backed_up.contains(&alpha.account_id));
        assert!(summary.tenants_backed_up.contains(&beta.account_id));
        assert!(summary.control_backed_up, "control plane backed up");
        assert!(summary.errors.is_empty(), "no errors: {summary:?}");

        // Both tenants' dumps + the control dump physically landed under the
        // day's prefix, each a real custom-format blob; each tenant key names
        // ONLY that tenant's db (no key carries the other's name).
        let keys = nightly_keys(&h.store, pass_at).await;
        assert_eq!(keys.len(), 3, "alpha + beta + control: {keys:?}");
        for key in &keys {
            assert_eq!(
                &h.store.get(key).await.unwrap()[..5],
                b"PGDMP",
                "{key} is a custom-format dump"
            );
        }
        assert!(
            keys.iter().any(|k| k.contains(&alpha.db_name)),
            "alpha's dump is present"
        );
        assert!(
            keys.iter().any(|k| k.contains(&beta.db_name)),
            "beta's dump is present"
        );

        // Both tenants were stamped (last_backup_at set), and a backup_runs
        // row recorded the pass.
        for t in [&alpha, &beta] {
            let last: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(
                "SELECT last_backup_at FROM account_databases WHERE account_id = $1",
            )
            .bind(&t.account_id)
            .fetch_one(h.control.pool())
            .await
            .unwrap();
            assert!(last.is_some(), "{} stamped after the pass", t.subdomain);
        }
        let (kind, total, succeeded, failed): (String, i32, i32, i32) = sqlx::query_as(
            "SELECT kind, total, succeeded, failed FROM backup_runs \
             ORDER BY started_at DESC LIMIT 1",
        )
        .fetch_one(h.control.pool())
        .await
        .unwrap();
        assert_eq!((kind.as_str(), total, succeeded, failed), ("nightly", 2, 2, 0));

        // ---- Step 2: the disaster — delete alpha with the store configured. ----
        // The final dump is taken BEFORE the drop (the operator's only undo).
        delete_account(
            &h.control,
            &h.cluster,
            &ManagedKeys::Disabled,
            atomic_cloud::BackupPolicy::Required(&h.store),
            atomic_cloud::DeleteLock::Acquire,
            &alpha.account_id,
            atomic_cloud::DEFAULT_BACKUP_TIMEOUT,
        )
        .await
        .expect("delete alpha takes the final dump then drops");

        // Alpha's database is gone from the cluster and its subdomain 404s
        // through the app.
        assert!(
            !database_exists(&alpha.db_name).await,
            "alpha's tenant database must be dropped"
        );
        let (alpha_status, _) = h.get_atom(&alpha).await;
        assert_eq!(
            alpha_status,
            StatusCode::NOT_FOUND,
            "deleted alpha 404s through the composed app"
        );

        // Exactly ONE final dump, named by ALPHA's account id — no final dump
        // for beta exists (beta wasn't deleted).
        let finals = h.store.list("backups/final/").await.unwrap();
        assert_eq!(finals.len(), 1, "exactly one final dump: {finals:?}");
        let final_key = finals[0].clone();
        assert!(
            final_key.contains(&alpha.account_id),
            "the final dump names alpha: {final_key}"
        );
        assert!(
            !final_key.contains(&beta.account_id),
            "no final dump for beta"
        );

        // ---- Isolation under the disaster: BETA is wholly unaffected. ----
        // Beta still serves its OWN atom live, its database still exists, and
        // its nightly dump is still present and untouched.
        h.assert_serves_marker(&beta).await;
        assert!(
            database_exists(&beta.db_name).await,
            "beta's database survives alpha's deletion"
        );
        let beta_nightly = format!("/{}.dump", beta.db_name);
        assert!(
            h.store
                .list("backups/")
                .await
                .unwrap()
                .iter()
                .any(|k| k.ends_with(&beta_nightly)),
            "beta's nightly dump is still present after alpha's deletion"
        );
        // The delete touched only alpha's control rows; beta's mapping stands.
        let alpha_rows: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM account_databases WHERE account_id = $1")
                .bind(&alpha.account_id)
                .fetch_one(h.control.pool())
                .await
                .unwrap();
        assert_eq!(alpha_rows, 0, "alpha's mapping row is gone");
        let beta_rows: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM account_databases WHERE account_id = $1")
                .bind(&beta.account_id)
                .fetch_one(h.control.pool())
                .await
                .unwrap();
        assert_eq!(beta_rows, 1, "beta's mapping row is untouched");

        // ---- Step 3: restore alpha from its final dump (the runbook). ----
        // 3a — restore into a FRESH database (a new UUID's name). The guard
        // drops it whatever happens — no control row tracks it until 3b.
        let restore_uuid = uuid::Uuid::new_v4();
        let restore_db = tenant_db_name(restore_uuid);
        let conn = DumpConnection::for_cluster(&h.cluster).unwrap();
        let dump_bytes = h.store.get(&final_key).await.expect("read final dump");
        let base_url = std::env::var("ATOMIC_TEST_DATABASE_URL").unwrap();
        with_db_guard(&base_url, &restore_db, || async {
            restore_database(
                &h.cluster,
                &conn,
                &restore_db,
                &dump_bytes,
                atomic_cloud::DEFAULT_BACKUP_TIMEOUT,
            )
            .await
            .expect("restore alpha's final dump into a fresh db");

            // 3b — reinstate alpha's account + repoint its mapping to the
            // restored database (the runbook's control-plane step). We reuse
            // alpha's subdomain so the app routes to it again.
            sqlx::query(
                "INSERT INTO accounts (id, subdomain, email, status, plan, created_at) \
                 VALUES ($1, $2, 'alpha@example.com', 'active', 'free', NOW())",
            )
            .bind(restore_uuid.to_string())
            .bind(&alpha.subdomain)
            .execute(h.control.pool())
            .await
            .expect("reinstate alpha's account row");
            // The restored database carries the dump's schema (the current
            // compiled target — the final dump was taken from a tenant the
            // running binary had already migrated), so the repointed mapping
            // must record that version. Omitting it leaves it at 0, and
            // CloudAuth's straggler gate would 503 the restored tenant as
            // perpetually "upgrading" — a real runbook trap this asserts away.
            sqlx::query(
                "INSERT INTO account_databases \
                     (account_id, cluster_id, db_name, status, last_migrated_version, last_migrated_at) \
                 VALUES ($1, 'test-cluster-1', $2, 'active', $3, NOW())",
            )
            .bind(restore_uuid.to_string())
            .bind(&restore_db)
            .bind(atomic_cloud::tenant_schema_target())
            .execute(h.control.pool())
            .await
            .expect("repoint alpha's mapping to the restored database");

            // 3c — point the restored account at the mock provider and mint a
            // fresh token so the app can serve it (the old account id is gone).
            let vault = support::test_vault();
            upsert_credentials(
                &h.control,
                vault.as_ref(),
                &restore_uuid.to_string(),
                NewCredentials {
                    provider: Provider::OpenAiCompat,
                    origin: CredentialOrigin::User,
                    api_key: SecretKey::new("test-key".to_string()),
                    external_key_id: None,
                    model_config: json!({
                        "embedding_model": "mock-embed",
                        "llm_model": "mock-llm",
                        "openai_compat_base_url": h.mock.base_url(),
                        "embedding_dimension": 1536,
                    }),
                },
            )
            .await
            .expect("store provider for restored alpha");
            set_active_provider(
                &h.control,
                &restore_uuid.to_string(),
                Some((Provider::OpenAiCompat, CredentialOrigin::User)),
            )
            .await
            .expect("activate provider for restored alpha");
            let restored_token = issue_token(
                &h.control,
                &restore_uuid.to_string(),
                TokenScope::Account,
                None,
                "e2e-backup-restored",
            )
            .await
            .expect("issue restored token");

            // 3d — evict the running server's cache entry. CloudAuth cached
            // nothing for the brand-new restored account id, but the runbook
            // step is exercised here against the live cache exactly as the
            // operator would (an evict of the OLD entry would be a no-op since
            // alpha's old account id was already removed by the delete; this
            // proves the eviction call path the runbook documents).
            let _ = h.cache.evict(&restore_uuid.to_string()).await;
            let _ = h.cache.evict(&alpha.account_id).await;

            // 3e — alpha serves its recovered atom live through the SAME app,
            // by its original atom id (the dump rehydrated it verbatim).
            let restored = Tenant {
                account_id: restore_uuid.to_string(),
                subdomain: alpha.subdomain.clone(),
                db_name: restore_db.clone(),
                token: restored_token,
                atom_id: alpha.atom_id.clone(),
                marker: alpha.marker.clone(),
            };
            h.assert_serves_marker(&restored).await;

            // The restore touched only alpha's tier: the restored database is
            // a NEW name (beta's db is unchanged), and beta is STILL serving.
            assert_ne!(restore_db, beta.db_name, "restore target is a fresh db");
            h.assert_serves_marker(&beta).await;
            assert!(
                database_exists(&beta.db_name).await,
                "beta's database is untouched by alpha's restore"
            );

            // The restore wrote exactly one new mapping (alpha's restored one);
            // beta's single mapping is still the only beta row.
            let beta_rows: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM account_databases WHERE account_id = $1",
            )
            .bind(&beta.account_id)
            .fetch_one(h.control.pool())
            .await
            .unwrap();
            assert_eq!(beta_rows, 1, "beta's mapping is still singular and untouched");

            // Clean up the reinstated control rows so the guard's tenant sweep
            // (keyed off account_databases) drops the restored db, and nothing
            // dangles. (The guard also drops it by name, belt and braces.)
            sqlx::query("DELETE FROM accounts WHERE id = $1")
                .bind(restore_uuid.to_string())
                .execute(h.control.pool())
                .await
                .ok();
        })
        .await;

        h.stop().await;
    })
    .await;
}

// ==================== Staleness e2e ====================

/// A tenant with a manufactured-old `last_backup_at` surfaces in the staleness
/// alert and in `backup status`; a freshly-backed-up one does not (plan: alert
/// "when any tenant's last successful backup is >36h old"). Driven through the
/// composed app's provisioning + the real `run_backup_pass`, then the
/// `stale_tenant_backups` / `tenant_backup_status` monitor queries the CLI's
/// `backup status` arm prints.
#[actix_web::test]
async fn staleness_alert_surfaces_only_the_stale_tenant() {
    if !backup_tools_available().await {
        eprintln!(
            "staleness_alert_surfaces_only_the_stale_tenant: skipping \
             (pg_dump/pg_restore not on PATH)"
        );
        return;
    }
    with_control_db(
        "staleness_alert_surfaces_only_the_stale_tenant",
        |url| async move {
            let h = BackupHarness::spawn(&url).await;
            // "beta" is reserved; use unreserved subdomains.
            let fresh = h.provision_with_atom("alphakb").await;
            let stale = h.provision_with_atom("betakb").await;

            // Both tenants must be older than the 36h horizon for a missed
            // window to count — a tenant provisioned minutes ago hasn't missed
            // its nightly slot yet (the monitor gates on accounts.created_at).
            sqlx::query("UPDATE accounts SET created_at = NOW() - INTERVAL '3 days'")
                .execute(h.control.pool())
                .await
                .unwrap();

            // Run a real nightly pass: BOTH get a fresh last_backup_at, so at
            // the 36h horizon NEITHER is stale yet.
            let pass_at = chrono::Utc::now();
            let summary = run_backup_pass(
                &h.control,
                &h.cluster,
                &h.store,
                &BackupConfig::default(),
                pass_at,
            )
            .await;
            assert_eq!(summary.tenants_backed_up.len(), 2, "{summary:?}");

            let horizon = Duration::from_secs(36 * 60 * 60);
            let none_stale = stale_tenant_backups(&h.control, horizon).await.unwrap();
            assert!(
                none_stale.is_empty(),
                "right after a pass, no tenant is stale: {none_stale:?}"
            );

            // Now manufacture staleness on ONE tenant: backdate its last
            // successful backup well past the horizon, leaving the other fresh.
            sqlx::query(
                "UPDATE account_databases SET last_backup_at = NOW() - INTERVAL '48 hours' \
                 WHERE account_id = $1",
            )
            .bind(&stale.account_id)
            .execute(h.control.pool())
            .await
            .unwrap();

            // The staleness alert surfaces EXACTLY the stale tenant — never the
            // fresh one (per-tenant: the query reads each tenant's own
            // last_backup_at, never conflating them).
            let alerted = stale_tenant_backups(&h.control, horizon).await.unwrap();
            assert_eq!(alerted.len(), 1, "exactly one stale tenant: {alerted:?}");
            assert_eq!(
                alerted[0].account_id, stale.account_id,
                "the manufactured-old tenant is the one alerted"
            );
            assert!(
                !alerted.iter().any(|s| s.account_id == fresh.account_id),
                "the freshly-backed-up tenant must NOT be alerted"
            );

            // `backup status` shows both, with the stale one's last_backup_at
            // older than the fresh one's (and stale-first ordering surfaces it).
            let statuses = tenant_backup_status(&h.control).await.unwrap();
            assert_eq!(statuses.len(), 2, "both active tenants listed");
            let fresh_at = statuses
                .iter()
                .find(|s| s.account_id == fresh.account_id)
                .and_then(|s| s.last_backup_at)
                .expect("fresh stamped");
            let stale_at = statuses
                .iter()
                .find(|s| s.account_id == stale.account_id)
                .and_then(|s| s.last_backup_at)
                .expect("stale stamped (just backdated)");
            assert!(
                stale_at < fresh_at,
                "the stale tenant's last backup is older: {stale_at} < {fresh_at}"
            );
            assert_eq!(
                statuses[0].account_id, stale.account_id,
                "status lists the stale tenant first"
            );

            h.stop().await;
        },
    )
    .await;
}
