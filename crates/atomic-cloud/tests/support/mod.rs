//! Shared infrastructure for atomic-cloud integration tests.
//!
//! Postgres-gated, mirroring the workspace convention: every test that needs
//! a cluster skips with a message when `ATOMIC_TEST_DATABASE_URL` is unset.
//! Run single-threaded against the test cluster:
//!
//! ```sh
//! ATOMIC_TEST_DATABASE_URL=postgres://atomic:atomic_test@localhost:5433/atomic_test \
//!     cargo test -p atomic-cloud -- --test-threads=1
//! ```
//!
//! Each test creates a uniquely named control-plane database under the
//! `atomic_cloud_test_` prefix and drops it afterwards even when the test
//! body panics ([`with_control_db`] catches the unwind, cleans up, then
//! resumes it). Tenant databases provisioned during a test are discovered
//! through that control database (its `account_databases` rows plus names
//! derived from `accounts.id`) and dropped in the same guard. A
//! once-per-process sweep removes leftovers stranded by prior crashed runs —
//! it matches only the dedicated test prefix, then chases each leftover
//! control database's tenant references, so it can never touch real data.

#![allow(dead_code)] // Helpers are per-test; not every test binary uses every helper.

use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use atomic_cloud::provision::is_tenant_db_name;
use atomic_cloud::{
    tenant_db_name, CloudError, CreatedRuntimeKey, EmailSender, EnvMasterKeyVault,
    MagicLinkPurpose, ManagedKeyConfig, ManagedKeys, ProvisioningApi, RuntimeKeyUsage, SecretKey,
};
use futures::FutureExt;
use sqlx::{Connection, PgConnection};

/// Dedicated prefix for control-plane databases created by this suite — the
/// startup sweep matches it and nothing else.
pub const TEST_DB_PREFIX: &str = "atomic_cloud_test_";

/// Swap the database name in the test-cluster URL. The conventional test URL
/// (`postgres://atomic:atomic_test@localhost:5433/atomic_test`) always ends
/// in `/<database>` with no query string, so a path swap is a string splice.
pub fn with_db_name(base_url: &str, db_name: &str) -> String {
    let (prefix, _) = base_url
        .rsplit_once('/')
        .expect("test database URL ends in /<database>");
    format!("{prefix}/{db_name}")
}

/// Tenant databases referenced by a control-plane database: explicit
/// `account_databases.db_name` rows plus names derived from `accounts.id`
/// (covering provisions that crashed before the mapping row was written).
/// Best-effort — a missing database or absent tables yields an empty list.
async fn referenced_tenant_dbs(control_url: &str) -> Vec<String> {
    let Ok(mut conn) = PgConnection::connect(control_url).await else {
        return Vec::new();
    };
    let mut names: Vec<String> = sqlx::query_scalar("SELECT db_name FROM account_databases")
        .fetch_all(&mut conn)
        .await
        .unwrap_or_default();
    let account_ids: Vec<String> = sqlx::query_scalar("SELECT id FROM accounts")
        .fetch_all(&mut conn)
        .await
        .unwrap_or_default();
    let _ = conn.close().await;

    for id in account_ids {
        if let Ok(uuid) = uuid::Uuid::parse_str(&id) {
            let derived = tenant_db_name(uuid);
            if !names.contains(&derived) {
                names.push(derived);
            }
        }
    }
    // Belt and braces: only ever drop names with the exact generated shape.
    names.retain(|name| is_tenant_db_name(name));
    names
}

/// Best-effort drop of leftover `atomic_cloud_test_*` databases — and the
/// tenant databases they reference — from prior crashed runs. Runs once per
/// test process, before the first database is created, so it cannot race a
/// live test under `--test-threads=1` (or any schedule — every creation
/// happens after the sweep completes).
pub async fn sweep_leftovers(base_url: &str) {
    static SWEEP: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();
    SWEEP
        .get_or_init(|| async {
            let Ok(mut conn) = PgConnection::connect(base_url).await else {
                return;
            };
            let pattern = format!("{}%", TEST_DB_PREFIX);
            let leftovers: Vec<String> =
                sqlx::query_scalar("SELECT datname FROM pg_database WHERE datname LIKE $1")
                    .bind(&pattern)
                    .fetch_all(&mut conn)
                    .await
                    .unwrap_or_default();
            let _ = conn.close().await;

            for db_name in leftovers {
                eprintln!("sweeping leftover test database {db_name}");
                for tenant in referenced_tenant_dbs(&with_db_name(base_url, &db_name)).await {
                    eprintln!("sweeping leftover tenant database {tenant}");
                    try_drop_database(base_url, &tenant).await;
                }
                try_drop_database(base_url, &db_name).await;
            }
        })
        .await;
}

/// Create a database on the test cluster (plain `CREATE DATABASE`; the name
/// comes from test code, not user input). Pair with [`with_db_guard`] so it
/// is dropped even when the test panics.
pub async fn create_database(base_url: &str, db_name: &str) {
    let mut conn = PgConnection::connect(base_url)
        .await
        .expect("connect for test-database creation");
    sqlx::raw_sql(&format!("CREATE DATABASE \"{db_name}\""))
        .execute(&mut conn)
        .await
        .expect("create test database");
    let _ = conn.close().await;
}

pub async fn drop_database(base_url: &str, db_name: &str) {
    let mut conn = PgConnection::connect(base_url)
        .await
        .expect("connect for test-database cleanup");
    // WITH (FORCE) terminates any straggler pool connections; sqlx pool drop
    // is asynchronous, so some may still be open when cleanup runs.
    sqlx::raw_sql(&format!(
        "DROP DATABASE IF EXISTS \"{db_name}\" WITH (FORCE)"
    ))
    .execute(&mut conn)
    .await
    .expect("drop test database");
    let _ = conn.close().await;
}

/// Non-panicking [`drop_database`], for sweep paths where a failed drop
/// should not mask the test result.
async fn try_drop_database(base_url: &str, db_name: &str) {
    let Ok(mut conn) = PgConnection::connect(base_url).await else {
        return;
    };
    let _ = sqlx::raw_sql(&format!(
        "DROP DATABASE IF EXISTS \"{db_name}\" WITH (FORCE)"
    ))
    .execute(&mut conn)
    .await;
    let _ = conn.close().await;
}

/// Run `test` against a fresh, uniquely named control-plane database URL,
/// dropping that database — and every tenant database it references —
/// afterwards, panic or not. Skips (with a message) when
/// `ATOMIC_TEST_DATABASE_URL` is unset.
pub async fn with_control_db<F, Fut>(test_name: &str, test: F)
where
    F: FnOnce(String) -> Fut,
    Fut: Future<Output = ()>,
{
    let Ok(base_url) = std::env::var("ATOMIC_TEST_DATABASE_URL") else {
        eprintln!("{test_name}: skipping (ATOMIC_TEST_DATABASE_URL not set)");
        return;
    };
    // Mock AI providers live at http://127.0.0.1:<port>, which the BYOK SSRF
    // gate rejects in production. Tests run single-threaded, so setting the
    // dev/test escape here (idempotent) lets the mock-backed BYOK rotations
    // through without weakening the real gate.
    std::env::set_var("ATOMIC_CLOUD_ALLOW_PRIVATE_PROVIDER_URLS", "1");
    sweep_leftovers(&base_url).await;

    let db_name = format!("{TEST_DB_PREFIX}{}", uuid::Uuid::new_v4().simple());
    let control_url = with_db_name(&base_url, &db_name);

    let result = AssertUnwindSafe(test(control_url.clone()))
        .catch_unwind()
        .await;

    // Tenant databases first (their names live in the control database),
    // then the control database itself.
    for tenant in referenced_tenant_dbs(&control_url).await {
        try_drop_database(&base_url, &tenant).await;
    }
    drop_database(&base_url, &db_name).await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

/// Run `body`, then drop `db_name` from the cluster even if `body` panics.
/// For tests that create an extra database (e.g. a reference schema) outside
/// [`with_control_db`]'s bookkeeping.
pub async fn with_db_guard<F, Fut>(base_url: &str, db_name: &str, body: F)
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = ()>,
{
    let result = AssertUnwindSafe(body()).catch_unwind().await;
    drop_database(base_url, db_name).await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

/// A magic-link email captured by [`CapturingSender`] — nothing was sent.
#[derive(Debug, Clone)]
pub struct SentEmail {
    pub to: String,
    /// The exact link the message carries; tests extract the `aml_` token
    /// from it.
    pub link: String,
    pub purpose: MagicLinkPurpose,
}

/// Test-side [`EmailSender`]: records every message for assertion, never
/// sends anything. NO REAL EMAIL, EVER — every server harness in this suite
/// uses this (or the lib's `LogSender`).
#[derive(Clone, Default)]
pub struct CapturingSender {
    sent: Arc<Mutex<Vec<SentEmail>>>,
}

impl CapturingSender {
    /// Snapshot of everything "sent" so far.
    pub fn sent(&self) -> Vec<SentEmail> {
        self.sent.lock().expect("capture lock").clone()
    }
}

#[async_trait::async_trait]
impl EmailSender for CapturingSender {
    async fn send_magic_link(
        &self,
        to: &str,
        link: &str,
        purpose: MagicLinkPurpose,
    ) -> Result<(), CloudError> {
        self.sent.lock().expect("capture lock").push(SentEmail {
            to: to.to_string(),
            link: link.to_string(),
            purpose,
        });
        Ok(())
    }
}

/// A deliberately slow [`EmailSender`] for timing assertions: sleeps for
/// `delay`, then records into the wrapped [`CapturingSender`]. Lets a test
/// prove a route returned *without* awaiting the send (the response arrives
/// in far less than `delay`; the captured email lands afterwards).
pub struct DelayedSender {
    pub inner: CapturingSender,
    pub delay: std::time::Duration,
}

#[async_trait::async_trait]
impl EmailSender for DelayedSender {
    async fn send_magic_link(
        &self,
        to: &str,
        link: &str,
        purpose: MagicLinkPurpose,
    ) -> Result<(), CloudError> {
        tokio::time::sleep(self.delay).await;
        self.inner.send_magic_link(to, link, purpose).await
    }
}

/// One call recorded by [`RecordingProvisioning`].
#[derive(Debug, Clone, PartialEq)]
pub enum ProvisioningCall {
    Create {
        name: String,
        credit_limit_cents: u32,
        monthly_reset: bool,
    },
    UpdateLimit {
        external_key_id: String,
        credit_limit_cents: u32,
    },
    Delete {
        external_key_id: String,
    },
    GetUsage {
        external_key_id: String,
    },
}

/// Test-side [`ProvisioningApi`]: records every call for assertion, mints
/// deterministic fake keys, never talks to a provider. NO REAL PROVIDERS,
/// EVER. Failures are injectable per method (`fail_create` / `fail_delete`)
/// so tests can prove the typed-error and best-effort paths.
#[derive(Default)]
pub struct RecordingProvisioning {
    calls: Mutex<Vec<ProvisioningCall>>,
    counter: AtomicU32,
    /// When set, `create_key` fails with a typed provisioning error
    /// (simulating a provider outage / exhausted master balance).
    pub fail_create: AtomicBool,
    /// When set, `delete_key` fails (simulating a provider outage during a
    /// deletion or rollback — the best-effort paths must proceed anyway).
    pub fail_delete: AtomicBool,
}

impl RecordingProvisioning {
    /// Snapshot of every call so far, in order.
    pub fn calls(&self) -> Vec<ProvisioningCall> {
        self.calls.lock().expect("provisioning call lock").clone()
    }

    /// The recorded `Create` calls.
    pub fn creates(&self) -> Vec<ProvisioningCall> {
        self.calls()
            .into_iter()
            .filter(|c| matches!(c, ProvisioningCall::Create { .. }))
            .collect()
    }

    /// The key ids passed to `delete_key`, in order.
    pub fn deleted_key_ids(&self) -> Vec<String> {
        self.calls()
            .into_iter()
            .filter_map(|c| match c {
                ProvisioningCall::Delete { external_key_id } => Some(external_key_id),
                _ => None,
            })
            .collect()
    }

    /// The deterministic plaintext/id pair the `n`th create mints (0-based)
    /// — lets tests predict key material without threading state.
    pub fn nth_key(n: u32) -> (String, String) {
        (
            format!("sk-or-v1-fake-{n:04}"),
            format!("orkey-fake-{n:04}"),
        )
    }

    fn record(&self, call: ProvisioningCall) {
        self.calls
            .lock()
            .expect("provisioning call lock")
            .push(call);
    }

    fn injected_failure(context: &str) -> CloudError {
        CloudError::ProviderProvisioning {
            context: context.to_string(),
            message: "injected test failure".to_string(),
        }
    }
}

#[async_trait::async_trait]
impl ProvisioningApi for RecordingProvisioning {
    async fn create_key(
        &self,
        name: &str,
        credit_limit_cents: u32,
        monthly_reset: bool,
    ) -> Result<CreatedRuntimeKey, CloudError> {
        self.record(ProvisioningCall::Create {
            name: name.to_string(),
            credit_limit_cents,
            monthly_reset,
        });
        if self.fail_create.load(Ordering::SeqCst) {
            return Err(Self::injected_failure("creating runtime key"));
        }
        let n = self.counter.fetch_add(1, Ordering::SeqCst);
        let (plaintext, external_key_id) = Self::nth_key(n);
        Ok(CreatedRuntimeKey {
            plaintext_key: SecretKey::new(plaintext),
            external_key_id,
        })
    }

    async fn update_key_limit(
        &self,
        external_key_id: &str,
        credit_limit_cents: u32,
    ) -> Result<(), CloudError> {
        self.record(ProvisioningCall::UpdateLimit {
            external_key_id: external_key_id.to_string(),
            credit_limit_cents,
        });
        Ok(())
    }

    async fn delete_key(&self, external_key_id: &str) -> Result<(), CloudError> {
        self.record(ProvisioningCall::Delete {
            external_key_id: external_key_id.to_string(),
        });
        if self.fail_delete.load(Ordering::SeqCst) {
            return Err(Self::injected_failure("deleting runtime key"));
        }
        // Idempotent like the real client: deleting an unknown id succeeds.
        Ok(())
    }

    async fn get_key_usage(&self, external_key_id: &str) -> Result<RuntimeKeyUsage, CloudError> {
        self.record(ProvisioningCall::GetUsage {
            external_key_id: external_key_id.to_string(),
        });
        Ok(RuntimeKeyUsage {
            usage_usd: 0.0,
            limit_usd: Some(0.5),
            limit_remaining_usd: Some(0.5),
            disabled: false,
        })
    }
}

/// The deterministic test master key shared by managed-key tests, so
/// assertions can decrypt rows written through [`managed_keys_with`].
pub const TEST_MASTER_KEY: [u8; 32] = [7u8; 32];

/// A [`KeyVault`] over [`TEST_MASTER_KEY`], in the `Arc<dyn _>` shape the
/// compositions take (`AccountCache::new`, `TenantPlane::new`).
///
/// [`KeyVault`]: atomic_cloud::KeyVault
pub fn test_vault() -> Arc<dyn atomic_cloud::KeyVault> {
    Arc::new(EnvMasterKeyVault::new(TEST_MASTER_KEY))
}

/// An `Enabled` [`ManagedKeys`] over a recording API, the [`TEST_MASTER_KEY`]
/// vault, and the default config (50¢ monthly allowance).
pub fn managed_keys_with(api: Arc<RecordingProvisioning>) -> ManagedKeys {
    managed_keys_with_config(api, ManagedKeyConfig::default())
}

/// [`managed_keys_with`] with an explicit [`ManagedKeyConfig`] — e2e suites
/// use this to seed managed `model_config`s whose base-URL override points
/// the pipeline at the shared `MockAiServer` (NO REAL PROVIDERS).
pub fn managed_keys_with_config(
    api: Arc<RecordingProvisioning>,
    config: ManagedKeyConfig,
) -> ManagedKeys {
    ManagedKeys::Enabled {
        api,
        vault: test_vault(),
        config,
    }
}

/// Whether `needle` appears in ANY text/varchar column of ANY public-schema
/// table in the control database. The exhaustive form of "the plaintext is
/// never persisted": scanning every column means a future table or column
/// can't quietly start storing secrets without tripping the assertion.
pub async fn control_db_contains(control_url: &str, needle: &str) -> bool {
    let mut conn = PgConnection::connect(control_url)
        .await
        .expect("connect for control-db scan");
    let columns: Vec<(String, String)> = sqlx::query_as(
        "SELECT table_name, column_name FROM information_schema.columns \
         WHERE table_schema = 'public' \
           AND data_type IN ('text', 'character varying')",
    )
    .fetch_all(&mut conn)
    .await
    .expect("list text columns");
    assert!(
        !columns.is_empty(),
        "control database should have text columns to scan"
    );

    let mut found = false;
    for (table, column) in columns {
        // Identifiers come from information_schema, not user input;
        // position() avoids LIKE-escaping the needle.
        let hit: bool = sqlx::query_scalar(&format!(
            "SELECT EXISTS(SELECT 1 FROM \"{table}\" WHERE position($1 in \"{column}\") > 0)"
        ))
        .bind(needle)
        .fetch_one(&mut conn)
        .await
        .expect("scan column");
        if hit {
            eprintln!("needle {needle:?} found in {table}.{column}");
            found = true;
        }
    }
    let _ = conn.close().await;
    found
}

/// Build a local SQLite knowledge base holding `contents` as atoms and
/// return the slim migration-snapshot bytes — the exact artifact the desktop
/// push flow uploads to `/api/migrations/sqlite`.
pub async fn sqlite_snapshot_fixture(contents: &[&str]) -> Vec<u8> {
    let dir = tempfile::TempDir::new().expect("fixture dir");
    let core = atomic_core::AtomicCore::open_or_create(dir.path().join("fixture.db"))
        .expect("open fixture core");
    for content in contents {
        core.create_atom(
            atomic_core::CreateAtomRequest {
                content: content.to_string(),
                ..Default::default()
            },
            |_| {},
        )
        .await
        .expect("create fixture atom")
        .expect("fixture atom inserted");
    }
    let snapshot = dir.path().join("upload.db");
    core.create_migration_snapshot(&snapshot)
        .await
        .expect("create fixture snapshot");
    tokio::fs::read(&snapshot)
        .await
        .expect("read fixture snapshot")
}
