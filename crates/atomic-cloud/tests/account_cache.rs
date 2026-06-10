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
    NewAccount,
};
use support::with_control_db;

/// Migrated control plane + a cluster config pointing at the test cluster.
async fn setup(control_url: &str) -> (ControlPlane, ClusterConfig) {
    let control = ControlPlane::connect(control_url)
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
    Arc::new(AccountCache::new(control.clone(), cluster.clone(), config))
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
