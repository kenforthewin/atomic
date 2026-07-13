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
//! (default every `idle_ttl / 4`, capped at `background_idle_ttl` so
//! background faults actually leave near their short TTL; see `main.rs`).
//! The cache itself owns no task lifecycle; `sweep` is a plain method the
//! composition schedules.
//!
//! Two rules, in order:
//!
//! 1. **Idle TTL** — entries untouched for longer than their TTL are
//!    dropped. Entries built or promoted by serving traffic (or by the
//!    dispatcher's touch — see below) live on
//!    [`AccountCacheConfig::idle_ttl`]; entries a background-dispatch miss
//!    faulted in that nothing has since promoted live on the much shorter
//!    [`AccountCacheConfig::background_idle_ttl`]. The dispatcher's
//!    periodic full scan faults every active tenant through
//!    [`AccountCache::get_for_dispatch`]; if those faults earned the full
//!    serving TTL, any scan cadence at or under the TTL would keep the
//!    whole fleet resident — a hidden ordering constraint between two
//!    independently tuned knobs (scaling doc #2). On the short TTL a
//!    scan's faults drain by the next sweep instead, regardless of how the
//!    scan interval and the idle TTL compare.
//! 2. **Hard cap** — if the cache is still at
//!    [`AccountCacheConfig::max_entries`], the evictable entries closest
//!    to their idle deadline (`last_touched` plus their TTL, so unpromoted
//!    background faults go first even when fresher) are dropped until the
//!    new entry fits.
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
use std::sync::atomic::{AtomicU64, Ordering};
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
#[derive(Clone)]
pub struct AccountCacheConfig {
    /// Entries untouched this long are evicted (unless a WebSocket
    /// subscriber is live). Default 15 minutes.
    pub idle_ttl: Duration,
    /// Target ceiling on cached accounts. Exceeded only when every entry
    /// has live WebSocket subscribers. Default 1000.
    pub max_entries: usize,
    /// TTL for entries faulted in by a **background-dispatch miss**
    /// ([`AccountCache::get_for_dispatch`]) that no serving traffic or
    /// [`AccountCache::touch`] has since promoted. The dispatcher's slow
    /// scan resolves every active account through the dispatch path, so
    /// each scan faults the whole fleet in; on the full `idle_ttl` those
    /// faults would pin the fleet resident whenever scans recur at or
    /// under the TTL, silently coupling two knobs that are tuned
    /// independently (scaling doc #2). A background fault only needs to
    /// survive its own tick — the poll that faulted it promotes it within
    /// the same tick if it found live ledger state — so entries that
    /// showed no work drain at the first sweep past this TTL. Effectively
    /// capped at `idle_ttl`: a background fault never outlives a serving
    /// entry. Default 60 seconds.
    pub background_idle_ttl: Duration,
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
    /// Whether tenant managers execute the embedding/tagging pipeline jobs
    /// they enqueue in-process (`AtomicCore::set_inline_pipeline`). Default
    /// `true` — today's behavior, where a tenant's atom saves run their
    /// pipeline inside the serving process. The dispatcher composition
    /// (plan: "Worker fairness & job queue"; next phase) sets this `false`
    /// so request-path saves only write durable `atom_pipeline_jobs` rows
    /// and the per-pod dispatcher owns all execution. Never set `false`
    /// without a dispatcher attached: enqueued work would sit in the
    /// ledgers unexecuted.
    pub inline_pipeline: bool,
    /// Failure-disposition policy installed on every tenant manager built
    /// by this cache (`AtomicCore::set_failure_disposition_policy`).
    /// `None` (the default) keeps atomic-core's historical settle-by-fail
    /// behavior; the cloud serve composition installs
    /// [`crate::backpressure::provider_failure_policy`] so
    /// provider-classified `task_runs` failures defer without consuming
    /// retry budget (plan: jobs sit in the ledger, never fail).
    pub failure_disposition_policy:
        Option<atomic_core::scheduler::ledger::FailureDispositionPolicy>,
}

impl Default for AccountCacheConfig {
    fn default() -> Self {
        Self {
            idle_ttl: Duration::from_secs(15 * 60),
            max_entries: 1000,
            background_idle_ttl: Duration::from_secs(60),
            tenant_pool_max_connections: 5,
            tenant_pool_idle_timeout: Duration::from_secs(5 * 60),
            inline_pipeline: true,
            failure_disposition_policy: None,
        }
    }
}

impl std::fmt::Debug for AccountCacheConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccountCacheConfig")
            .field("idle_ttl", &self.idle_ttl)
            .field("max_entries", &self.max_entries)
            .field("background_idle_ttl", &self.background_idle_ttl)
            .field(
                "tenant_pool_max_connections",
                &self.tenant_pool_max_connections,
            )
            .field("tenant_pool_idle_timeout", &self.tenant_pool_idle_timeout)
            .field("inline_pipeline", &self.inline_pipeline)
            .field(
                "failure_disposition_policy",
                &self.failure_disposition_policy.is_some(),
            )
            .finish()
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
    /// Whether the entry was faulted in by background dispatch and has not
    /// yet been promoted by serving traffic or [`AccountCache::touch`].
    /// Such entries idle out on the short
    /// [`AccountCacheConfig::background_idle_ttl`] (module docs: Eviction).
    background: bool,
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
    /// Entries evicted since construction — idle sweep, hard cap, and the
    /// deletion path's explicit [`Self::evict`] all count. Monotonic, read
    /// by the metrics scrape ([`Self::stats`]); relaxed ordering is enough
    /// for an independent statistic.
    evictions: AtomicU64,
}

/// One scrape's view of the cache (see [`AccountCache::stats`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheStats {
    /// Entries promoted to the serving TTL (real tenant traffic or work).
    pub serving: usize,
    /// Unpromoted background-dispatch faults on the short TTL.
    pub background: usize,
    /// Entries evicted since the cache was built.
    pub evictions: u64,
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
            evictions: AtomicU64::new(0),
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
        self.lookup_or_build(account_id, true).await
    }

    /// Resolve `account_id` for **background dispatch**, WITHOUT refreshing the
    /// entry's idle clock on a hit (DISP-3).
    ///
    /// The slow-scan poll resolves every active tenant, at whatever cadence
    /// the dispatcher is configured with; if each of those resolves went
    /// through [`get_or_load`](Self::get_or_load) it would bump
    /// `last_touched`, so with scans recurring inside the idle TTL no idle
    /// tenant would ever age out — the scan would keep the whole fleet
    /// warm, and above `max_entries` every miss would evict an LRU entry
    /// the next scan rebuilds, thrashing pool opens against the shared
    /// cluster (scaling doc #2). This accessor leaves
    /// `last_touched` untouched on a hit, so only genuine serving traffic
    /// (the request/WS path) — or live ledger state, which the dispatcher
    /// promotes via [`touch`](Self::touch) — keeps a tenant warm; polling
    /// alone can't pin the cache at cap. A miss still builds, so dispatch
    /// always sees fresh data — but the built entry starts as a
    /// **background fault** living on the short
    /// [`AccountCacheConfig::background_idle_ttl`], so a full scan's faults
    /// drain by the next sweep unless the poll finds work worth promoting.
    /// That bound is what lets the scan interval and the idle TTL be tuned
    /// independently: no ordering between them can pin the fleet resident.
    pub async fn get_for_dispatch(&self, account_id: &str) -> Result<TenantHandle, CloudError> {
        self.lookup_or_build(account_id, false).await
    }

    /// Refresh `account_id`'s idle clock iff an entry is cached — promoting
    /// a background fault to the full serving TTL — returning whether one
    /// was. Never loads.
    ///
    /// This is the dispatch path's promotion step, the counterpart to
    /// [`get_for_dispatch`](Self::get_for_dispatch)'s no-touch reads: a scan
    /// that merely *polls* a tenant and finds nothing must leave it
    /// idle-evictable, but the dispatcher calls this in two places where the
    /// tenant is demonstrably not idle. When a poll finds **live ledger
    /// state** — pending rows, due or backed off; either way the dispatch
    /// hint stays marked, so the fast path will fault this handle every tick
    /// regardless, and evicting it would reclaim nothing while costing a
    /// full rebuild (pool open + provider decrypt) once per idle TTL. And
    /// when a **claimed execution settles** — real work flowing. Both mean a
    /// tenant with a steady or backed-off background workload stays resident
    /// on one entry instead of churning an evict/refault cycle mid-workload.
    pub async fn touch(&self, account_id: &str) -> bool {
        match self.inner.lock().await.entries.get_mut(account_id) {
            Some(entry) => {
                entry.last_touched = Instant::now();
                entry.background = false;
                true
            }
            None => false,
        }
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
        let handle = self.lookup_or_build(account_id, true).await?;
        self.refresh_stale_provider_config(account_id, observed_generation)
            .await?;
        Ok(handle)
    }

    /// `serving` marks the caller as request-path traffic: a serving hit
    /// refreshes the entry's idle clock (and promotes a background fault to
    /// the full TTL). Background dispatch passes `false`, so its hits leave
    /// the clock alone (DISP-3 — see [`get_for_dispatch`]) and its misses
    /// build entries that live on the short
    /// [`AccountCacheConfig::background_idle_ttl`] until promoted (module
    /// docs: Eviction).
    ///
    /// [`get_for_dispatch`]: Self::get_for_dispatch
    async fn lookup_or_build(
        &self,
        account_id: &str,
        serving: bool,
    ) -> Result<TenantHandle, CloudError> {
        loop {
            // Fast path: cache hit. Otherwise pick up (or register) the
            // account's build permit while still under the map lock.
            let load_lock = {
                let mut inner = self.inner.lock().await;
                if let Some(entry) = inner.entries.get_mut(account_id) {
                    if serving {
                        entry.last_touched = Instant::now();
                        entry.background = false;
                    }
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
                    if serving {
                        entry.last_touched = Instant::now();
                        entry.background = false;
                    }
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
            let mut entry = built?;
            // A dispatch-path miss still builds — the poll needs fresh data
            // — but the entry starts as a background fault: it earns only
            // the short background TTL until real work or serving traffic
            // promotes it (module docs: Eviction).
            entry.background = !serving;
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
        let removed = self.inner.lock().await.entries.remove(account_id).is_some();
        if removed {
            self.evictions.fetch_add(1, Ordering::Relaxed);
        }
        removed
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

    /// One consistent snapshot of residency (by kind) and the eviction
    /// counter, for the metrics scrape (scaling doc #2's cache signals).
    pub async fn stats(&self) -> CacheStats {
        let inner = self.inner.lock().await;
        let background = inner.entries.values().filter(|e| e.background).count();
        CacheStats {
            serving: inner.entries.len() - background,
            background,
            evictions: self.evictions.load(Ordering::Relaxed),
        }
    }

    /// Open tenant-pool connections summed across every cached entry — the
    /// pod's share of the cluster's `max_connections` budget (scaling doc
    /// #1). Manager handles are cloned out under the lock and sampled
    /// outside it, so a scrape never stalls serving on pool internals.
    pub async fn tenant_pool_connections(&self) -> u64 {
        let managers: Vec<Arc<DatabaseManager>> = {
            let inner = self.inner.lock().await;
            inner
                .entries
                .values()
                .map(|e| Arc::clone(&e.manager))
                .collect()
        };
        managers
            .iter()
            .filter_map(|manager| manager.postgres_pool_status())
            .map(|(size, _idle)| u64::from(size))
            .sum()
    }

    /// The idle TTL `entry` is currently living on: the full serving TTL,
    /// or — while the entry is an unpromoted background fault — the short
    /// background TTL, capped by the serving TTL so a background fault
    /// never outlives a serving entry (module docs: Eviction).
    fn ttl_of(&self, entry: &Entry) -> Duration {
        if entry.background {
            self.config.background_idle_ttl.min(self.config.idle_ttl)
        } else {
            self.config.idle_ttl
        }
    }

    /// Idle-TTL pass: drop entries untouched past their TTL, keeping any
    /// with live WebSocket subscribers.
    fn sweep_locked(&self, inner: &mut Inner) {
        let before = inner.entries.len();
        inner
            .entries
            .retain(|_, e| !e.evictable() || e.last_touched.elapsed() <= self.ttl_of(e));
        self.evictions
            .fetch_add((before - inner.entries.len()) as u64, Ordering::Relaxed);
    }

    /// Hard-cap pass, run before inserting a new entry: evict the evictable
    /// entries closest to their idle deadline (`last_touched` plus their
    /// TTL) until the newcomer fits. Deadline order rather than plain LRU so
    /// unpromoted background faults — a full scan's artifacts — are
    /// sacrificed before serving entries even when the fault is fresher.
    /// Stops short when only live-subscriber entries remain (module docs).
    fn make_room_locked(&self, inner: &mut Inner) {
        while inner.entries.len() >= self.config.max_entries.max(1) {
            let victim = inner
                .entries
                .iter()
                .filter(|(_, e)| e.evictable())
                .min_by_key(|(_, e)| e.last_touched + self.ttl_of(e))
                .map(|(id, _)| id.clone());
            match victim {
                Some(id) => {
                    inner.entries.remove(&id);
                    self.evictions.fetch_add(1, Ordering::Relaxed);
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

        // Pipeline execution mode + failure-disposition policy (config
        // docs): both slots are shared by every core the manager resolves —
        // sibling construction clones them — so installing once on the
        // bootstrap core covers all of the account's knowledge bases,
        // current and future.
        if !self.config.inline_pipeline || self.config.failure_disposition_policy.is_some() {
            let bootstrap = manager
                .active_core()
                .await
                .map_err(CloudError::core("resolving core for tenant configuration"))?;
            if !self.config.inline_pipeline {
                bootstrap.set_inline_pipeline(false);
            }
            if let Some(policy) = &self.config.failure_disposition_policy {
                bootstrap.set_failure_disposition_policy(Some(Arc::clone(policy)));
            }
        }

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
            // The inserting path stamps the caller's flavor; a bare build
            // defaults to the serving shape.
            background: false,
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
