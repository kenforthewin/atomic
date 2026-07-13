//! AccountCache integration tests: load coalescing, idle-TTL and hard-cap
//! eviction (including the live-WebSocket-subscriber skip), and explicit
//! eviction.
//!
//! Postgres-gated; see `tests/support/mod.rs` for the skip/cleanup
//! conventions and the run command. Tenant databases provisioned here are
//! dropped by the support guard via the control database's references.

mod support;

use std::sync::Arc;
use std::time::Duration;

use atomic_cloud::{
    provision_account, AccountCache, AccountCacheConfig, CloudError, ClusterConfig, ControlPlane,
    ManagedKeys, NewAccount,
};
use support::with_control_db;

/// Migrated control plane + a cluster config pointing at the test cluster.
async fn setup(control_url: &str) -> (ControlPlane, ClusterConfig) {
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
    (control, cluster)
}

/// Provision an account and return its id.
async fn provision(control: &ControlPlane, cluster: &ClusterConfig, subdomain: &str) -> String {
    provision_account(
        control,
        cluster,
        &ManagedKeys::Disabled,
        NewAccount {
            email: format!("{subdomain}@example.com"),
            subdomain: subdomain.to_string(),
        },
    )
    .await
    .expect("provision account")
    .account_id
}

fn cache_with(
    control: &ControlPlane,
    cluster: &ClusterConfig,
    config: AccountCacheConfig,
) -> Arc<AccountCache> {
    Arc::new(AccountCache::new(
        control.clone(),
        cluster.clone(),
        support::test_vault(),
        config,
    ))
}

#[tokio::test]
async fn coalesced_loads_build_one_manager() {
    with_control_db("coalesced_loads_build_one_manager", |url| async move {
        let (control, cluster) = setup(&url).await;
        let account_id = provision(&control, &cluster, "solo").await;
        let cache = cache_with(&control, &cluster, AccountCacheConfig::default());

        // Eight concurrent loads for the same account must coalesce onto a
        // single build: every caller gets the *same* manager and channel.
        let loads = (0..8).map(|_| {
            let cache = Arc::clone(&cache);
            let account_id = account_id.clone();
            async move { cache.get_or_load(&account_id).await }
        });
        let handles: Vec<_> = futures::future::join_all(loads)
            .await
            .into_iter()
            .map(|r| r.expect("coalesced load succeeds"))
            .collect();

        let first = &handles[0];
        for handle in &handles[1..] {
            assert!(
                Arc::ptr_eq(&first.manager, &handle.manager),
                "coalesced loads must share one manager"
            );
        }
        assert_eq!(cache.len().await, 1);

        // A later call is a plain hit on the same entry.
        let again = cache.get_or_load(&account_id).await.expect("cache hit");
        assert!(Arc::ptr_eq(&first.manager, &again.manager));

        // An account the control plane doesn't know fails cleanly and
        // caches nothing.
        let err = cache
            .get_or_load(&uuid::Uuid::new_v4().to_string())
            .await
            .expect_err("unknown account must not load");
        assert!(
            matches!(err, CloudError::MissingTenantDatabase(_)),
            "expected MissingTenantDatabase, got {err:?}"
        );
        assert_eq!(cache.len().await, 1);
    })
    .await;
}

/// Issue: tenants on a shared cluster must get small pools (the plan
/// budgets ~5 connections per tenant), not atomic-core's env default of 50.
/// Asserts the cache's pool sizing reaches the tenant pool it builds — both
/// the recorded options and the real behavior at the cap.
#[tokio::test]
async fn tenant_pool_honors_configured_cap() {
    with_control_db("tenant_pool_honors_configured_cap", |url| async move {
        let (control, cluster) = setup(&url).await;
        let account_id = provision(&control, &cluster, "pooled").await;
        let idle_timeout = Duration::from_secs(45);
        let cache = cache_with(
            &control,
            &cluster,
            AccountCacheConfig {
                tenant_pool_max_connections: 1,
                tenant_pool_idle_timeout: idle_timeout,
                ..AccountCacheConfig::default()
            },
        );

        let handle = cache.get_or_load(&account_id).await.expect("load");
        let core = handle.manager.active_core().await.expect("active core");
        let atomic_core::storage::StorageBackend::Postgres(pg) = core.storage() else {
            panic!("tenant storage must be Postgres");
        };
        let options = pg.pool().options();
        assert_eq!(
            options.get_max_connections(),
            1,
            "cache config must bound the tenant pool"
        );
        assert_eq!(
            options.get_idle_timeout(),
            Some(idle_timeout),
            "cache config must set the tenant pool's idle timeout"
        );

        // The cap is real, not cosmetic: with the single permitted
        // connection checked out, a second acquire must block until the
        // first is returned.
        let held = pg.pool().acquire().await.expect("acquire at cap");
        let starved = tokio::time::timeout(Duration::from_millis(500), pg.pool().acquire()).await;
        assert!(
            starved.is_err(),
            "second acquire must block while the pool is saturated at cap 1"
        );
        drop(held);
        pg.pool()
            .acquire()
            .await
            .expect("released connection must be reusable");
    })
    .await;
}

#[tokio::test]
async fn ttl_eviction_skips_live_receivers() {
    with_control_db("ttl_eviction_skips_live_receivers", |url| async move {
        let (control, cluster) = setup(&url).await;
        let account_id = provision(&control, &cluster, "idle").await;
        let cache = cache_with(
            &control,
            &cluster,
            AccountCacheConfig {
                idle_ttl: Duration::from_millis(100),
                max_entries: 1000,
                ..AccountCacheConfig::default()
            },
        );

        let handle = cache.get_or_load(&account_id).await.expect("load");

        // Idle past the TTL but with a live subscriber: must survive —
        // evicting it would orphan the WebSocket's channel.
        let rx = handle.event_tx.subscribe();
        tokio::time::sleep(Duration::from_millis(250)).await;
        cache.sweep().await;
        assert!(
            cache.contains(&account_id).await,
            "live receiver must block TTL eviction"
        );

        // Subscriber gone: the same idle entry is now evictable.
        drop(rx);
        cache.sweep().await;
        assert!(
            !cache.contains(&account_id).await,
            "idle entry without receivers must be evicted"
        );

        // The next request rebuilds from scratch.
        let rebuilt = cache.get_or_load(&account_id).await.expect("reload");
        assert!(
            !Arc::ptr_eq(&handle.manager, &rebuilt.manager),
            "post-eviction load must build a fresh manager"
        );
        assert_eq!(cache.len().await, 1);
    })
    .await;
}

/// Scaling doc #2 (DISP-3): dispatch-path resolves must not renew the idle
/// TTL, or the periodic full scan — which polls every active account more
/// often than the TTL elapses — keeps the whole fleet resident and no
/// tenant ever idle-evicts.
///
/// Three phases over the same account, all on one timeline shape
/// (load, hit at TTL/2, sweep at 1.25×TTL):
///
/// 1. **Scan-shaped traffic** (`get_for_dispatch`): the mid-window hit is a
///    genuine hit (same manager) but does NOT reset the idle clock, so the
///    sweep — running past the TTL as measured from the LOAD, though within
///    it as measured from the hit — evicts. An implementation that touched
///    on dispatch hits would keep the entry and fail here.
/// 2. **Serving traffic** (`get_or_load`): the identical timeline keeps the
///    entry warm — today's interactive semantics, unchanged.
/// 3. **Promoted touch** (`touch`, the dispatcher's executed-work path):
///    renews exactly like serving traffic, and reports whether an entry
///    existed.
#[tokio::test]
async fn dispatch_hits_do_not_renew_idle_ttl_but_serving_hits_do() {
    with_control_db("dispatch_hits_do_not_renew_idle_ttl", |url| async move {
        let (control, cluster) = setup(&url).await;
        let account_id = provision(&control, &cluster, "scanned").await;
        // Generous margins (500ms on the overshoot-sensitive side): the
        // retention assertions require the sleeps to stay UNDER the TTL,
        // which a loaded CI box can violate with tight timings.
        let ttl = Duration::from_millis(2000);
        let half = ttl / 2;
        let three_quarters = ttl * 3 / 4;
        let cache = cache_with(
            &control,
            &cluster,
            AccountCacheConfig {
                idle_ttl: ttl,
                ..AccountCacheConfig::default()
            },
        );

        // Phase 1 — invariant A. Load once (the last "user" traffic), then
        // only scan-shaped hits.
        let loaded = cache.get_or_load(&account_id).await.expect("load");
        tokio::time::sleep(half).await;
        let polled = cache
            .get_for_dispatch(&account_id)
            .await
            .expect("dispatch resolve");
        assert!(
            Arc::ptr_eq(&loaded.manager, &polled.manager),
            "get_for_dispatch on a cached account must hit, not rebuild"
        );
        tokio::time::sleep(three_quarters).await;
        cache.sweep().await;
        assert!(
            !cache.contains(&account_id).await,
            "a tenant whose only recent traffic is dispatch polling must idle-evict"
        );

        // Phase 2 — invariant C: same timeline through the serving path.
        let reloaded = cache.get_or_load(&account_id).await.expect("reload");
        assert!(
            !Arc::ptr_eq(&loaded.manager, &reloaded.manager),
            "phase 1's eviction must have been real (fresh manager on reload)"
        );
        tokio::time::sleep(half).await;
        cache.get_or_load(&account_id).await.expect("serving hit");
        tokio::time::sleep(three_quarters).await;
        cache.sweep().await;
        assert!(
            cache.contains(&account_id).await,
            "a serving hit inside the TTL must keep the entry warm"
        );

        // Phase 3 — the promotion: an explicit touch renews like serving
        // traffic…
        tokio::time::sleep(half).await;
        assert!(
            cache.touch(&account_id).await,
            "touch reports the entry it renewed"
        );
        tokio::time::sleep(three_quarters).await;
        cache.sweep().await;
        assert!(
            cache.contains(&account_id).await,
            "a promoted touch inside the TTL must keep the entry warm"
        );

        // …and the TTL still applies afterward; touch on a gone entry is a
        // reported no-op, never a load.
        tokio::time::sleep(ttl + half).await;
        cache.sweep().await;
        assert!(
            !cache.contains(&account_id).await,
            "the idle TTL still evicts once touches stop"
        );
        assert!(
            !cache.touch(&account_id).await,
            "touch never loads a missing entry"
        );
        assert!(cache.is_empty().await);
    })
    .await;
}

/// Scaling doc #2, the knob-decoupling half: an entry built by a
/// **dispatch-path miss** (`get_for_dispatch` on a cold account — exactly
/// what the periodic full scan does to every unhinted tenant) lives on the
/// short `background_idle_ttl`, not the serving `idle_ttl`, until serving
/// traffic or a touch promotes it. This is what frees the slow-scan
/// interval and the idle TTL to be tuned independently: no ordering
/// between them can pin the fleet resident, because a scan fault that
/// shows no work drains at the first sweep past the background TTL.
#[tokio::test]
async fn background_faults_live_on_short_ttl_until_promoted() {
    with_control_db("background_faults_short_ttl", |url| async move {
        let (control, cluster) = setup(&url).await;
        let account_id = provision(&control, &cluster, "bg-fault").await;
        // Every sleep sits at least 1s from each boundary it must not
        // cross, so a loaded CI box has slack on both sides.
        let background = Duration::from_millis(1000);
        let serving = Duration::from_millis(4000);
        let cache = cache_with(
            &control,
            &cluster,
            AccountCacheConfig {
                idle_ttl: serving,
                background_idle_ttl: background,
                ..AccountCacheConfig::default()
            },
        );

        // A dispatch miss builds (the poll needs fresh data) but earns only
        // the background TTL: past it — though far inside the serving TTL —
        // the sweep drops the entry.
        cache
            .get_for_dispatch(&account_id)
            .await
            .expect("dispatch fault");
        tokio::time::sleep(background * 2).await;
        cache.sweep().await;
        assert!(
            !cache.contains(&account_id).await,
            "an unpromoted background fault must evict on the background TTL"
        );

        // Serving traffic promotes: a get_or_load hit on a background fault
        // upgrades it to the full serving TTL.
        cache
            .get_for_dispatch(&account_id)
            .await
            .expect("dispatch refault");
        cache.get_or_load(&account_id).await.expect("serving hit");
        tokio::time::sleep(background * 2).await;
        cache.sweep().await;
        assert!(
            cache.contains(&account_id).await,
            "a serving hit must promote a background fault to the full TTL"
        );

        // The promotion granted the serving TTL, nothing more: idle long
        // enough and the entry still leaves.
        tokio::time::sleep(serving).await;
        cache.sweep().await;
        assert!(
            !cache.contains(&account_id).await,
            "a promoted entry still idles out on the serving TTL"
        );

        // The dispatcher's touch — its poll-found-work / executed-claim
        // promotion — upgrades a background fault the same way.
        cache
            .get_for_dispatch(&account_id)
            .await
            .expect("dispatch refault");
        assert!(cache.touch(&account_id).await, "touch reports the entry");
        tokio::time::sleep(background * 2).await;
        cache.sweep().await;
        assert!(
            cache.contains(&account_id).await,
            "a touch must promote a background fault to the full TTL"
        );
    })
    .await;
}

/// Hard-cap companion to the background TTL: when the cap forces an
/// eviction, victims are picked by idle *deadline* (`last_touched` plus the
/// entry's TTL), not raw recency — so a full scan hitting a saturated cache
/// sacrifices its own freshly faulted artifacts before anyone's serving
/// entry.
#[tokio::test]
async fn hard_cap_prefers_background_faults_over_serving_entries() {
    with_control_db("hard_cap_prefers_background", |url| async move {
        let (control, cluster) = setup(&url).await;
        let served = provision(&control, &cluster, "cap-served").await;
        let scanned = provision(&control, &cluster, "cap-scanned").await;
        let incoming = provision(&control, &cluster, "cap-incoming").await;
        let cache = cache_with(
            &control,
            &cluster,
            AccountCacheConfig {
                idle_ttl: Duration::from_secs(3600),
                background_idle_ttl: Duration::from_secs(60),
                max_entries: 2,
                ..AccountCacheConfig::default()
            },
        );

        // Serving entry first, background fault second: the fault is the
        // FRESHER of the two by last_touched — plain LRU would evict the
        // serving entry — but its idle deadline is the earlier by far.
        cache.get_or_load(&served).await.expect("load served");
        cache
            .get_for_dispatch(&scanned)
            .await
            .expect("background fault");
        cache.get_or_load(&incoming).await.expect("overflow");
        assert_eq!(cache.len().await, 2, "hard cap must hold");
        assert!(
            !cache.contains(&scanned).await,
            "the background fault is the victim despite being fresher"
        );
        assert!(
            cache.contains(&served).await,
            "the serving entry survives the cap pass"
        );
        assert!(cache.contains(&incoming).await);
    })
    .await;
}

#[tokio::test]
async fn hard_cap_evicts_least_recently_touched() {
    with_control_db("hard_cap_evicts_least_recently_touched", |url| async move {
        let (control, cluster) = setup(&url).await;
        let a = provision(&control, &cluster, "cap-a").await;
        let b = provision(&control, &cluster, "cap-b").await;
        let c = provision(&control, &cluster, "cap-c").await;
        let cache = cache_with(
            &control,
            &cluster,
            AccountCacheConfig {
                idle_ttl: Duration::from_secs(3600),
                max_entries: 2,
                ..AccountCacheConfig::default()
            },
        );

        cache.get_or_load(&a).await.expect("load a");
        cache.get_or_load(&b).await.expect("load b");
        assert_eq!(cache.len().await, 2);

        // Touch `a` so `b` becomes the least-recently-touched entry, then
        // overflow the cap: `b` must be the victim.
        cache.get_or_load(&a).await.expect("touch a");
        cache.get_or_load(&c).await.expect("load c");
        assert_eq!(cache.len().await, 2, "hard cap must hold");
        assert!(cache.contains(&a).await, "recently touched entry survives");
        assert!(!cache.contains(&b).await, "LRU entry is evicted");
        assert!(cache.contains(&c).await, "new entry is inserted");
    })
    .await;
}

#[tokio::test]
async fn hard_cap_never_evicts_live_receivers_and_evict_removes() {
    with_control_db(
        "hard_cap_never_evicts_live_receivers_and_evict_removes",
        |url| async move {
            let (control, cluster) = setup(&url).await;
            let a = provision(&control, &cluster, "live-a").await;
            let b = provision(&control, &cluster, "live-b").await;
            let cache = cache_with(
                &control,
                &cluster,
                AccountCacheConfig {
                    idle_ttl: Duration::from_secs(3600),
                    max_entries: 1,
                    ..AccountCacheConfig::default()
                },
            );

            // `a` is over the cap but has a live subscriber: the cap pass
            // must skip it and let the cache temporarily exceed the target
            // rather than orphan the connection.
            let handle_a = cache.get_or_load(&a).await.expect("load a");
            let _rx_a = handle_a.event_tx.subscribe();
            cache.get_or_load(&b).await.expect("load b");
            assert!(
                cache.contains(&a).await,
                "live receiver must block cap eviction"
            );
            assert!(cache.contains(&b).await);
            assert_eq!(cache.len().await, 2, "cap is exceeded, never orphaned");

            // Explicit eviction (the account-deletion path) outranks
            // everything, live subscribers included.
            assert!(cache.evict(&a).await, "evict reports removal");
            assert!(!cache.contains(&a).await);
            assert!(!cache.evict(&a).await, "second evict is a no-op");

            // With `a` gone the cap holds again for the next insert.
            cache.get_or_load(&a).await.expect("reload a");
            assert_eq!(cache.len().await, 1, "cap enforced once evictable");
        },
    )
    .await;
}
