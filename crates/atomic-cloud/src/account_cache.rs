//! Per-account tenant cache (plan: "Auth & tenant routing" → "AccountCache").
//!
//! Every active account holds open resources — a [`DatabaseManager`] whose
//! pool points at the account's tenant database, and a broadcast channel its
//! WebSocket clients subscribe to. [`AccountCache`] owns those resources:
//! the auth middleware resolves an account to a [`TenantHandle`] here, and
//! requests for accounts nobody has touched in a while release their pools
//! back to the cluster.
//!
//! # Eviction
//!
//! The sweep runs in two places. It runs **inline** while inserting a
//! freshly loaded entry (the only moment the cache grows), so steady-state
//! hits pay nothing. And because a stable working set produces no inserts —
//! idle entries would otherwise hold their pools forever — the `serve`
//! binary also runs [`AccountCache::sweep`] from a **periodic task**
//! (default every `idle_ttl / 4`; see `main.rs`). The cache itself owns no
//! task lifecycle; `sweep` is a plain method the composition schedules.
//!
//! Two rules, in order:
//!
//! 1. **Idle TTL** — entries untouched for longer than
//!    [`AccountCacheConfig::idle_ttl`] are dropped.
//! 2. **Hard cap** — if the cache is still at
//!    [`AccountCacheConfig::max_entries`], least-recently-touched entries
//!    are dropped until the new entry fits.
//!
//! Neither rule ever evicts an entry whose `event_tx` has live receivers
//! (decision 2026-06-09): a quiet-but-connected WebSocket client would
//! otherwise keep listening on a channel nothing publishes to. As a
//! consequence the hard cap is a target, not an invariant — if every entry
//! has a live subscriber the cache temporarily exceeds it rather than
//! orphaning a connection; growth stays bounded by the number of accounts
//! with open WebSockets.
//!
//! # Load coalescing
//!
//! Concurrent `get_or_load` calls for the same account must not build two
//! managers (each build opens a pool and replays the migration check). A
//! per-account async mutex serializes builders: the first caller takes the
//! account's build permit and loads; coalesced callers wait on the same
//! permit and find the finished entry when they acquire it. Failures are
//! not cached — each waiting caller retries the build itself.
//!
//! # Provider config (plan: "Plumbing — control plane → AtomicCore")
//!
//! The cache-miss build is where control-plane provider state reaches a
//! serving tenant: the account's active `provider_credentials` row is
//! loaded, decrypted through the [`KeyVault`], and turned into an explicit
//! [`ProviderConfig`] that the tenant manager is opened with — **always**
//! `Some`, never `None`. `None` would put atomic-core in settings-fallback
//! mode, letting the tenant database's own settings rows select providers;
//! the plan forbids that path in cloud. An account with no credentials row
//! gets a key-less config ([`keyless_provider_config`]) so provider calls
//! fail with a structured missing-key error rather than falling back.
//!
//! Live rotation ([`AccountCache::update_provider_config`]) swaps the config
//! on a cached entry **in place** — no eviction, so in-flight operations
//! finish on the config they started with while new operations pick up the
//! fresh one (plan: "Live rotation", steps 4-5).
//!
//! ## Generation-checked convergence
//!
//! The in-place swap only reaches the pod that handled the rotation, and it
//! can race a concurrent entry build (the build reads credentials, the
//! rotation lands, the swap misses, the build inserts the *old* config).
//! `accounts.provider_generation` bounds both: every provider mutation
//! bumps it transactionally ([`crate::provider_credentials`]), each cache
//! entry records the generation its config was built from — read in the
//! **same query** as the credentials, so the stamp is never newer than the
//! config — and CloudAuth's per-request account lookup (already a
//! per-request read; slice-1's no-auth-caching decision) carries the
//! current value into [`AccountCache::get_or_load_with_generation`]. A hit
//! whose entry lags the observed generation re-reads the control plane,
//! swaps the fresh config in place, and re-stamps the entry — under a
//! per-account refresh permit (the same keyed-lock idiom as the loading
//! map) so concurrent requests don't stampede the control plane. Any pod,
//! and any lost race, converges on the next authenticated request.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use atomic_core::{DatabaseManager, PgPoolConfig, ProviderConfig};
use atomic_server::state::ServerEvent;
use tokio::sync::{broadcast, Mutex};

use crate::control_plane::ControlPlane;
use crate::error::CloudError;
use crate::keyvault::KeyVault;
use crate::provider_config::{config_for_credentials, keyless_provider_config};
use crate::provider_credentials::{
    get_active_provider_state, touch_last_used, ActiveProviderState, ProviderCredentials,
};
use crate::provision::{is_tenant_db_name, ClusterConfig};

/// Capacity of each per-account event channel. Matches the sizing of
/// atomic-server's process-wide channel (`main.rs`); a tenant's event volume
/// is strictly a subset of a whole self-hosted server's.
const EVENT_CHANNEL_CAPACITY: usize = 4096;

/// Tuning knobs for [`AccountCache`]. Defaults are the plan's initial
/// guesses ("Open questions" → idle-TTL and hard-cap numbers); tune from
/// production data.
#[derive(Debug, Clone)]
pub struct AccountCacheConfig {
    /// Entries untouched this long are evicted (unless a WebSocket
    /// subscriber is live). Default 15 minutes.
    pub idle_ttl: Duration,
    /// Target ceiling on cached accounts. Exceeded only when every entry
    /// has live WebSocket subscribers. Default 1000.
    pub max_entries: usize,
    /// Max connections in each tenant's pool. Every cached account holds an
    /// open pool against the shared cluster, so each must stay small —
    /// default 5, the plan's per-tenant budget ("Tenant model"). The rest of
    /// the pool tuning (acquire timeout, slow-query logging) still comes
    /// from the `ATOMIC_PG_*` environment.
    pub tenant_pool_max_connections: u32,
    /// Close a tenant pool's connections after this long idle, so a
    /// quiet-but-cached account releases its connections back to the
    /// cluster well before the cache entry itself is evicted. Default
    /// 5 minutes.
    pub tenant_pool_idle_timeout: Duration,
}

impl Default for AccountCacheConfig {
    fn default() -> Self {
        Self {
            idle_ttl: Duration::from_secs(15 * 60),
            max_entries: 1000,
            tenant_pool_max_connections: 5,
            tenant_pool_idle_timeout: Duration::from_secs(5 * 60),
        }
    }
}

/// A resolved account's serving resources, handed to the auth middleware on
/// every request. Cheap to clone — both fields are reference-counted.
#[derive(Clone)]
pub struct TenantHandle {
    /// Manager whose pool points at the account's tenant database.
    pub manager: Arc<DatabaseManager>,
    /// The account's event channel: route handlers publish into it, the
    /// account's WebSocket sessions subscribe to it.
    pub event_tx: broadcast::Sender<ServerEvent>,
}

impl std::fmt::Debug for TenantHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // DatabaseManager has no Debug impl (and nothing in it is useful to
        // print); identify the handle by its channel's subscriber count.
        f.debug_struct("TenantHandle")
            .field("event_receivers", &self.event_tx.receiver_count())
            .finish_non_exhaustive()
    }
}

/// Cached state for one account (plan's `Entry` shape).
struct Entry {
    manager: Arc<DatabaseManager>,
    event_tx: broadcast::Sender<ServerEvent>,
    last_touched: Instant,
    /// `accounts.provider_generation` the entry's provider config was built
    /// from (module docs: "Generation-checked convergence"). Read in the
    /// same query as the credentials, so it is never newer than the config
    /// actually serving; refreshes re-stamp it.
    provider_generation: i64,
}

impl Entry {
    fn handle(&self) -> TenantHandle {
        TenantHandle {
            manager: Arc::clone(&self.manager),
            event_tx: self.event_tx.clone(),
        }
    }

    /// Whether eviction may take this entry: never while a WebSocket
    /// subscriber is live on its channel.
    fn evictable(&self) -> bool {
        self.event_tx.receiver_count() == 0
    }
}

/// All three maps live under one lock: `entries` is the cache proper,
/// `loading` holds the per-account build permits that coalesce concurrent
/// loads, and `refreshing` holds the per-account permits that serialize
/// generation refreshes (module docs: "Generation-checked convergence").
/// `refreshing` prunes by `Arc::strong_count` on acquire — an entry only
/// the map holds has no guard out and no waiter queued.
#[derive(Default)]
struct Inner {
    entries: HashMap<String, Entry>,
    loading: HashMap<String, Arc<Mutex<()>>>,
    refreshing: HashMap<String, Arc<Mutex<()>>>,
}

/// Cache of per-account tenant resources, keyed by account id.
///
/// On miss, the account's tenant database is looked up in the control
/// plane's `account_databases` and a [`DatabaseManager`] is opened against
/// it, alongside a fresh event channel. See the module docs for eviction
/// and coalescing semantics.
pub struct AccountCache {
    control: ControlPlane,
    cluster: ClusterConfig,
    /// Decrypts the account's stored provider credentials on the miss path
    /// (module docs: "Provider config").
    vault: Arc<dyn KeyVault>,
    config: AccountCacheConfig,
    inner: Mutex<Inner>,
}

impl AccountCache {
    /// Create an empty cache that resolves tenant databases through
    /// `control`, connects to them on `cluster`, and decrypts each account's
    /// provider credentials through `vault`.
    pub fn new(
        control: ControlPlane,
        cluster: ClusterConfig,
        vault: Arc<dyn KeyVault>,
        config: AccountCacheConfig,
    ) -> Self {
        Self {
            control,
            cluster,
            vault,
            config,
            inner: Mutex::new(Inner::default()),
        }
    }

    /// Resolve `account_id` to its serving resources, loading them on miss.
    ///
    /// A hit refreshes the entry's idle clock. Concurrent calls for the same
    /// account coalesce onto a single build (module docs); a failed build is
    /// returned to its caller and retried by any coalesced waiters.
    ///
    /// This form performs no provider-generation freshness check — callers
    /// on the authenticated request path, which have just read the accounts
    /// row anyway, should use
    /// [`get_or_load_with_generation`](Self::get_or_load_with_generation).
    pub async fn get_or_load(&self, account_id: &str) -> Result<TenantHandle, CloudError> {
        self.lookup_or_build(account_id).await
    }

    /// [`get_or_load`](Self::get_or_load), plus the per-request convergence
    /// check (module docs: "Generation-checked convergence"):
    /// `observed_generation` is the `accounts.provider_generation` the
    /// caller just read alongside authentication. When the cached entry's
    /// config was built from an older generation, the entry's provider
    /// config is refreshed from the control plane — in place, no eviction —
    /// before the handle is returned, so a rotation written by any pod (or
    /// one that raced this entry's build) is serving by the end of this
    /// call.
    pub async fn get_or_load_with_generation(
        &self,
        account_id: &str,
        observed_generation: i64,
    ) -> Result<TenantHandle, CloudError> {
        let handle = self.lookup_or_build(account_id).await?;
        self.refresh_stale_provider_config(account_id, observed_generation)
            .await?;
        Ok(handle)
    }

    async fn lookup_or_build(&self, account_id: &str) -> Result<TenantHandle, CloudError> {
        loop {
            // Fast path: cache hit. Otherwise pick up (or register) the
            // account's build permit while still under the map lock.
            let load_lock = {
                let mut inner = self.inner.lock().await;
                if let Some(entry) = inner.entries.get_mut(account_id) {
                    entry.last_touched = Instant::now();
                    return Ok(entry.handle());
                }
                Arc::clone(inner.loading.entry(account_id.to_string()).or_default())
            };

            let _build_permit = load_lock.lock().await;

            // Re-check under the permit: a coalesced builder may have
            // finished while we waited. And only proceed to build if our
            // permit is still the registered one — a stale permit (its
            // build cycle completed and was evicted, a new cycle began)
            // must rejoin the current cycle instead of double-building.
            {
                let mut inner = self.inner.lock().await;
                if let Some(entry) = inner.entries.get_mut(account_id) {
                    entry.last_touched = Instant::now();
                    return Ok(entry.handle());
                }
                let still_registered = inner
                    .loading
                    .get(account_id)
                    .is_some_and(|l| Arc::ptr_eq(l, &load_lock));
                if !still_registered {
                    continue;
                }
            }

            // We hold the live permit and there is no entry: build, outside
            // the map lock so other accounts aren't stalled behind this
            // load.
            let built = self.build_entry(account_id).await;

            let mut inner = self.inner.lock().await;
            if inner
                .loading
                .get(account_id)
                .is_some_and(|l| Arc::ptr_eq(l, &load_lock))
            {
                inner.loading.remove(account_id);
            }
            let entry = built?;
            let handle = entry.handle();
            self.sweep_locked(&mut inner);
            self.make_room_locked(&mut inner);
            inner.entries.insert(account_id.to_string(), entry);
            return Ok(handle);
        }
    }

    /// Bring `account_id`'s cached provider config up to (at least)
    /// `observed_generation`, re-reading the control plane when the entry
    /// lags. No-op when the entry is already current — the steady state,
    /// one map probe — or when no entry exists (a fresh build reads state
    /// at least as new as any prior observation).
    ///
    /// Concurrent stale requests serialize on a per-account refresh permit
    /// and re-check under it, so one control-plane read serves them all.
    /// Errors propagate: a request that *knows* the serving config is stale
    /// must not quietly proceed on credentials the account holder may just
    /// have revoked.
    async fn refresh_stale_provider_config(
        &self,
        account_id: &str,
        observed_generation: i64,
    ) -> Result<(), CloudError> {
        // Fast path under the map lock: entry current (or gone) → done.
        let permit = {
            let mut inner = self.inner.lock().await;
            match inner.entries.get(account_id) {
                Some(entry) if entry.provider_generation < observed_generation => {}
                _ => return Ok(()),
            }
            inner.refreshing.retain(|_, p| Arc::strong_count(p) > 1);
            Arc::clone(inner.refreshing.entry(account_id.to_string()).or_default())
        };
        let _guard = permit.lock().await;

        // Re-check under the permit: a coalesced refresh may have caught up
        // while we waited; the entry may also have been evicted (nothing to
        // refresh — the next build reads fresh state).
        let manager = {
            let inner = self.inner.lock().await;
            match inner.entries.get(account_id) {
                Some(entry) if entry.provider_generation < observed_generation => {
                    Arc::clone(&entry.manager)
                }
                _ => return Ok(()),
            }
        };

        // One snapshot read: generation + credentials together, so the
        // stamp below is never newer than the config it travels with.
        let state =
            get_active_provider_state(&self.control, self.vault.as_ref(), account_id).await?;
        let Some(state) = state else {
            // The accounts row vanished mid-request (concurrent deletion).
            // The deletion path evicts; nothing to converge here.
            tracing::debug!(account_id, "provider refresh found no accounts row");
            return Ok(());
        };
        let config = match &state.credentials {
            Some(credentials) => config_for_credentials(credentials),
            None => keyless_provider_config(),
        };
        let core = manager
            .active_core()
            .await
            .map_err(CloudError::core("resolving core for provider refresh"))?;
        core.update_provider_config(config);
        self.stamp_last_used(account_id, state.credentials.as_ref())
            .await;

        let mut inner = self.inner.lock().await;
        if let Some(entry) = inner.entries.get_mut(account_id) {
            // Only stamp the entry whose manager we actually updated — an
            // eviction + rebuild while we read would have built fresher
            // state than ours.
            if Arc::ptr_eq(&entry.manager, &manager)
                && entry.provider_generation < state.provider_generation
            {
                entry.provider_generation = state.provider_generation;
            }
        }
        tracing::info!(
            account_id,
            generation = state.provider_generation,
            "refreshed stale provider config from control plane"
        );
        Ok(())
    }

    /// Drop `account_id`'s entry immediately, returning whether one existed.
    ///
    /// This is the account-deletion path's eviction (the HTTP deletion
    /// route calls it right after `delete_account` returns — see
    /// [`crate::tenant_plane`] for why delete-then-evict is the safe order
    /// in-process). Eviction rules (live receivers, TTL) deliberately don't
    /// apply — deletion outranks an open WebSocket, and dropping the
    /// entry's `Sender` is exactly what severs those sessions.
    pub async fn evict(&self, account_id: &str) -> bool {
        self.inner.lock().await.entries.remove(account_id).is_some()
    }

    /// Run the idle-TTL sweep now. The same pass runs inline whenever a
    /// loaded entry is inserted; this is for explicit maintenance.
    pub async fn sweep(&self) {
        self.sweep_locked(&mut *self.inner.lock().await);
    }

    /// Number of cached accounts.
    pub async fn len(&self) -> usize {
        self.inner.lock().await.entries.len()
    }

    /// Whether the cache is empty.
    pub async fn is_empty(&self) -> bool {
        self.inner.lock().await.entries.is_empty()
    }

    /// Whether `account_id` currently has a cached entry.
    pub async fn contains(&self, account_id: &str) -> bool {
        self.inner.lock().await.entries.contains_key(account_id)
    }

    /// Idle-TTL pass: drop entries untouched past the TTL, keeping any with
    /// live WebSocket subscribers.
    fn sweep_locked(&self, inner: &mut Inner) {
        inner
            .entries
            .retain(|_, e| !e.evictable() || e.last_touched.elapsed() <= self.config.idle_ttl);
    }

    /// Hard-cap pass, run before inserting a new entry: evict
    /// least-recently-touched evictable entries until the newcomer fits.
    /// Stops short when only live-subscriber entries remain (module docs).
    fn make_room_locked(&self, inner: &mut Inner) {
        while inner.entries.len() >= self.config.max_entries.max(1) {
            let victim = inner
                .entries
                .iter()
                .filter(|(_, e)| e.evictable())
                .min_by_key(|(_, e)| e.last_touched)
                .map(|(id, _)| id.clone());
            match victim {
                Some(id) => {
                    inner.entries.remove(&id);
                }
                None => break,
            }
        }
    }

    /// Live provider rotation (plan: "Live rotation", step 4): swap the
    /// active [`ProviderConfig`] on `account_id`'s cached entry, if one is
    /// cached. Returns whether an entry was present.
    ///
    /// Deliberately **not** an eviction: the manager, its pool, and its
    /// event channel (with any live WebSocket subscribers) all stay put.
    /// Every core resolved from the manager shares one config slot, so the
    /// single call covers all of the account's knowledge bases; operations
    /// already in flight finish on the config they snapshotted at start. A
    /// miss needs no action — the next `get_or_load` builds from the
    /// control-plane state the rotation just wrote.
    pub async fn update_provider_config(
        &self,
        account_id: &str,
        config: ProviderConfig,
    ) -> Result<bool, CloudError> {
        // Clone the manager handle under the lock, swap outside it —
        // `active_core` does storage resolution that must not stall other
        // accounts. No `last_touched` refresh: a rotation is control-plane
        // traffic, not tenant activity.
        let manager = {
            let inner = self.inner.lock().await;
            match inner.entries.get(account_id) {
                Some(entry) => Arc::clone(&entry.manager),
                None => return Ok(false),
            }
        };
        let core = manager
            .active_core()
            .await
            .map_err(CloudError::core("resolving core for provider rotation"))?;
        core.update_provider_config(config);
        Ok(true)
    }

    /// Cache-miss path: control-plane lookup → tenant manager (opened with
    /// the account's explicit provider config; module docs) → fresh event
    /// channel.
    async fn build_entry(&self, account_id: &str) -> Result<Entry, CloudError> {
        let db_name: Option<String> = sqlx::query_scalar(
            "SELECT db_name FROM account_databases \
             WHERE account_id = $1 AND status = 'active' \
             ORDER BY created_at LIMIT 1",
        )
        .bind(account_id)
        .fetch_optional(self.control.pool())
        .await
        .map_err(CloudError::db("looking up account database"))?;
        let db_name =
            db_name.ok_or_else(|| CloudError::MissingTenantDatabase(account_id.to_string()))?;

        // The name feeds a connection URL, not DDL, but only the exact
        // generated shape is ever trusted — a corrupted control-plane row
        // must not direct a tenant at an arbitrary database.
        if !is_tenant_db_name(&db_name) {
            return Err(CloudError::InvalidDatabaseName(db_name));
        }

        let tenant_url = self.cluster.tenant_db_url(&db_name)?;

        // Resolve the account's provider config from the control plane —
        // ALWAYS an explicit Some (module docs: the settings-fallback path
        // is forbidden in cloud). No credentials row → key-less config →
        // structured missing-key errors downstream. The provider generation
        // arrives in the same query as the credentials, so the entry's
        // stamp below can never be newer than the config it describes —
        // which is what lets a rotation racing this build heal by
        // generation mismatch on the next request (module docs).
        let state =
            get_active_provider_state(&self.control, self.vault.as_ref(), account_id).await?;
        let state = state.unwrap_or_else(|| {
            // The accounts row vanished between the mapping lookup above and
            // this read (concurrent deletion). Build the key-less shape; the
            // deletion's eviction (or the idle TTL) reclaims the entry.
            tracing::warn!(account_id, "account row missing during entry build");
            ActiveProviderState {
                provider_generation: 0,
                credentials: None,
            }
        });
        let provider_config = match &state.credentials {
            Some(credentials) => config_for_credentials(credentials),
            None => keyless_provider_config(),
        };

        // Each cached account holds its own pool, so it must be small
        // (config docs); everything the cache config doesn't own still comes
        // from the `ATOMIC_PG_*` environment.
        let pool_config = PgPoolConfig {
            max_connections: self.config.tenant_pool_max_connections,
            idle_timeout: Some(self.config.tenant_pool_idle_timeout),
            ..PgPoolConfig::from_env()
        };
        // The manager open re-checks migrations and the default-KB seed on
        // every call; for an already-provisioned tenant both are no-op
        // reads. The data-dir argument is unused on the Postgres path.
        let manager = DatabaseManager::new_postgres_with_pool_and_provider(
            ".",
            &tenant_url,
            pool_config,
            Some(provider_config),
        )
        .await
        .map_err(CloudError::core("opening tenant database manager"))?;

        // Stamp the credential's last_used_at: handing the key to a serving
        // manager is the moment it goes into use (plan: "Audit /
        // visibility").
        self.stamp_last_used(account_id, state.credentials.as_ref())
            .await;

        let (event_tx, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        Ok(Entry {
            manager: Arc::new(manager),
            event_tx,
            last_touched: Instant::now(),
            provider_generation: state.provider_generation,
        })
    }

    /// Best-effort `last_used_at` stamp for the credentials a serving config
    /// was just built from — a stamp failure must never fail the tenant load
    /// or a refresh.
    async fn stamp_last_used(&self, account_id: &str, credentials: Option<&ProviderCredentials>) {
        let Some(credentials) = credentials else {
            return;
        };
        if let Err(e) = touch_last_used(
            &self.control,
            account_id,
            credentials.provider,
            credentials.origin,
        )
        .await
        {
            tracing::warn!(account_id, error = %e, "failed to stamp credential last_used_at");
        }
    }
}
