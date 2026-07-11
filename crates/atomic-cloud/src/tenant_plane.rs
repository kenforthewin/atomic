//! Cloud-owned routes on the **tenant plane** (per-account subdomains).
//!
//! Most of the tenant plane *is* atomic-server's `api_scope()` under
//! [`CloudAuth`](crate::auth::CloudAuth); this module holds the routes that
//! have no self-hosted counterpart because they act on the **account** —
//! control-plane state — rather than on knowledge-base data. Three route
//! families: the dashboard overview, account deletion, and the provider
//! settings API.
//!
//! # `GET /api/account/overview` (cloud-only: the account dashboard's read)
//!
//! A single account-scope read that assembles everything the hosted
//! dashboard's landing view needs in one round-trip, so the SPA never
//! fans out across the plan/billing/provider/usage surfaces on mount:
//!
//! - **identity** — `subdomain`, `email` (the accounts row);
//! - **plan** — `{id, name}` plus the resource ceilings, resolved through
//!   the same `accounts.plan_id` FK the quota guard reads;
//! - **billing** — `billing_state` and `trial_ends_at`, the serving state
//!   the dashboard renders banners from (read off the same accounts row the
//!   [`CloudAuth`](crate::auth) lookup already resolved into
//!   [`ResolvedTenant`]);
//! - **usage** — live `atoms_used` (summed across the account's knowledge
//!   bases via [`AtomicCore::count_atoms`](atomic_core::AtomicCore::count_atoms))
//!   and `kb_count` (the [`DatabaseManager::list_databases`] length),
//!   counted straight from the tenant database the same way enforcement does
//!   ([`crate::plans`]) — never a stored counter that could skew;
//! - **provider** — the active credentials' *status only* (origin, provider,
//!   configured flag, the plaintext `model_config` summary, validation
//!   timestamps), reusing the provider status route's discipline:
//!   **no key material, ever**.
//!
//! Like every route here it is account-scope only (database/MCP tokens 403)
//! and read-only — it touches no credential secret and writes nothing.
//!
//! Every route here shares the same authorization posture: CloudAuth's host
//! routing binds the request to the subdomain's account (unreachable from
//! the app host; cross-tenant credentials fail the chokepoint), and only
//! **account-scope** credentials may act — database- and MCP-scoped tokens
//! get 403, because a KB-pinned integration must neither destroy the
//! account nor read/rotate its provider credentials.
//!
//! # The provider settings API (plan: "Provider management")
//!
//! Four routes implementing "BYOK entry & validation", "Live rotation",
//! "Model curation", and "Audit / visibility":
//!
//! - **`GET /api/account/provider`** — status only. Provider, origin,
//!   configured flag, model config, validation/usage timestamps, and (for
//!   managed keys) best-effort allowance usage from the provisioning API.
//!   **No key material, ever** — not even a prefix; the stored key is never
//!   displayed and rotation means replacing it.
//! - **`PUT /api/account/provider`** — BYOK save. The submitted
//!   `model_config` is vocabulary-checked
//!   ([`validate_byok_model_config`]) — the column is plaintext and echoed
//!   by status, so unknown keys (an `api_key` nested where it doesn't
//!   belong) are rejected, never stored. The candidate config's effective
//!   embedding dimension must equal the platform pin
//!   ([`PINNED_EMBEDDING_DIMENSION`]) — no cloud mechanism can recreate a
//!   tenant's vector index at another width, so a differing dimension is a
//!   structured 400 (`embedding_dimension_unsupported`), not a warning.
//!   Then the key is **validated against the provider before anything is
//!   stored** (OpenRouter: `GET {base}/auth/key`; OpenAI-compatible: a
//!   minimal embedding call through the same provider machinery the
//!   pipeline uses). Validation failure → 400 carrying the provider's
//!   error verbatim (scrubbed of the submitted key, should a hostile
//!   endpoint echo it — scrubbed *before* truncation, so a key cut by the
//!   length bound can't survive as a fragment) and nothing stored. Success
//!   → encrypt + UPSERT the `origin='user'` row, flip the active pointer,
//!   live-rotate.
//! - **`POST /api/account/provider/activate`** — the column flip between
//!   stored rows (managed ↔ BYOK, both directions). 404s when the target
//!   row doesn't exist; a missing managed row is **not** re-provisioned
//!   here — managed keys are minted at signup only.
//! - **`PUT /api/account/provider/models`** — model selection on the active
//!   row. Managed rows are curation-checked ([`crate::curated_models`]) and
//!   merged over the stored config so platform-seeded keys survive; BYOK
//!   rows choose freely within the vocabulary and replace wholesale. The
//!   same dimension pin applies — a write whose effective embedding
//!   dimension differs from the platform's is rejected before anything is
//!   stored. A same-dimension embedding-model change returns a loud
//!   `reembed_warning` — existing vectors were produced by the old model and
//!   are NOT re-embedded automatically, so search over existing content stays
//!   degraded until the operator manually re-embeds.
//!
//! **Live rotation** (plan steps 1-6): after any successful write the fresh
//! [`ProviderConfig`] is applied to the account's cached entry via
//! [`AccountCache::update_provider_config`] — an in-place swap, not an
//! eviction, so in-flight operations finish on the config they started with
//! and open WebSockets are untouched. Step 6 — clearing the circuit
//! breaker's pause (`accounts.provider_paused_until` + kind + streak) — is
//! not handled here at all: it rides the same statement/transaction as the
//! credential write's `provider_generation` bump (see
//! `crate::provider_credentials`), so a rotation and its fresh chance are
//! atomic by construction. The three mutating routes are serialized per
//! account ([`AccountLocks`]) so the stored and live configs cannot diverge
//! under concurrent writes.
//!
//! The pause clear alone is not a full recovery, though: ledger rows the
//! breaker's layer 1 pushed out — pipeline jobs re-enqueued with a
//! `not_before` horizon, `task_runs` deferred to the pause-recheck horizon
//! — would still sit dead for up to the *remaining* horizon (an hour, for a
//! credits pause) after the user fixed their key. So every successful
//! provider mutation also **re-arms the provider-held ledger work** in the
//! tenant database ([`rearm_provider_held_work`]): pipeline rows stamped
//! `provider-backoff` and task runs whose recorded failure classifies as a
//! provider failure get their horizons reset to now, and the next
//! dispatcher tick redispatches them against the configuration the user
//! just fixed. Best-effort by design — a re-arm failure leaves the rows to
//! retry at their stored horizons, which is degraded freshness, not lost
//! work.
//!
//! # `DELETE /api/account` (plan: "Provisioning lifecycle" → "Account deletion")
//!
//! Hard-deletes the authenticated account. It lives on the tenant subdomain
//! — deletion is an action on *this account* — and CloudAuth's host routing
//! is what binds the request to it: the route is unreachable from the app
//! host (no subdomain label → CloudAuth 404s before any handler) and
//! unreachable for other tenants (their credentials don't verify here — the
//! cross-tenant chokepoint). The e2e suite pins both directions anyway.
//!
//! Two refusals run before anything is touched, in order:
//!
//! - **Scope** — only account-scope credentials may destroy the account:
//!   account-scope tokens and web sessions (sessions always carry account
//!   scope; see [`crate::auth`]). Database- and MCP-scoped tokens get 403 —
//!   a KB-pinned integration must never be able to take the whole account
//!   down with it.
//! - **Confirmation** — the body must be `{"confirm": "<subdomain>"}`
//!   naming the account's own subdomain, so a stray DELETE (wrong host in a
//!   script, the shared base-domain cookie riding along) can't fire. A
//!   missing, malformed, or mismatched body is a 400.
//!
//! Then the sequence: [`delete_account`] → [`AccountCache::evict`] → the
//! WebSocket severing falls out of the eviction (below). A repeat DELETE
//! after success never reaches the handler: the accounts row is gone, so
//! CloudAuth answers 404 at step 2.
//!
//! # Cancellation-proofing: the work runs in a spawned task
//!
//! actix drops a handler's future the moment the client disconnects. The
//! deletion sequence destroys the account's credentials *first* (token
//! revocation, session deletion — deliberately, to close the
//! still-authenticated crash window) and then does multi-second work
//! (terminating backends, dropping the tenant database). A future dropped
//! between those would strand a zombie: `status = 'active'`, credentials
//! revoked, tenant database possibly gone — an account its owner can no
//! longer drive to completion with the credential they just used. So the
//! handler spawns the delete + evict sequence as a detached task
//! (`actix_web::rt::spawn` onto the worker's LocalSet) and awaits its
//! `JoinHandle`: a disconnect cancels only the *await*, while the spawned
//! task runs to completion regardless. Only a process death can interrupt
//! the sequence now — and that residue is exactly what the reaper's
//! interrupted-deletion arm detects (an active account with no
//! `account_databases` row) and completes.
//!
//! # Ordering: delete first, evict second
//!
//! The plan's deletion sequence lists cache eviction (step 5) before the
//! database drop (step 7); in-process the safe order is the reverse, and
//! [`delete_account`] makes it equivalent:
//!
//! - Evict-then-delete leaves a window where a concurrent authenticated
//!   request (tokens still verify until `delete_account`'s first step)
//!   re-warms the cache mid-drop — resurrecting a pool, and a fresh event
//!   channel nobody will ever sever, for an account that's about to be gone.
//! - Delete-then-evict closes that window: once `delete_account` returns,
//!   the accounts row is gone (CloudAuth 404s) and the `account_databases`
//!   rows are gone (a cache rebuild fails typed), so the eviction is final.
//!   The pooled connections an un-evicted entry held during the drop are
//!   handled inside `delete_account` — `pg_terminate_backend` plus
//!   `DROP DATABASE … WITH (FORCE)`.
//!
//! # WebSocket severing
//!
//! Eviction drops the cache entry, and with it the entry's broadcast
//! `Sender`. When the last remaining clone goes — typically this DELETE
//! request's own `RequestEventChannel` extension, dropped with the response
//! — every `/ws` session's `Receiver` yields `RecvError::Closed`;
//! `ws::start_event_session`'s forwarding loop breaks on exactly that,
//! drops its `actix_ws::Session`, and the connection terminates (the
//! session channel closing ends the response's streaming body). No
//! cloud-side reaping of sessions is needed. Deletion deliberately bypasses
//! the live-receiver eviction pinning — pinning protects *idle* entries
//! from TTL eviction; it must never protect a deleted account's resources
//! ([`AccountCache::evict`] is unconditional by contract, and the e2e suite
//! pins the combination).
//!
//! [`delete_account`]: crate::provision::delete_account
//! [`AccountCache::evict`]: crate::account_cache::AccountCache::evict

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use actix_web::middleware::from_fn;
use actix_web::{web, HttpMessage, HttpRequest, HttpResponse};
use atomic_core::providers::{create_embedding_provider, EmbeddingConfig, OpenRouterProvider};
use atomic_core::{ProviderConfig, ProviderType};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::account_cache::AccountCache;
use crate::auth::{CloudAuth, ResolvedTenant};
use crate::billing::BillingProvider;
use crate::control_plane::ControlPlane;
use crate::curated_models::{
    merge_managed_model_config, validate_managed_model_config, PINNED_EMBEDDING_DIMENSION,
};
use crate::error::CloudError;
use crate::keyvault::{KeyVault, SecretKey};
use crate::managed_keys::ManagedKeys;
use crate::provider_config::{
    build_provider_config, config_for_credentials, validate_byok_model_config,
};
use crate::provider_credentials::{
    get_active_credentials, get_credentials, record_validation, set_active_provider,
    update_model_config, upsert_credentials, CredentialOrigin, NewCredentials, Provider,
    ProviderCredentials,
};
use crate::provision::{delete_account, ClusterConfig};
use crate::server::cloud_plane_guard;
use crate::tokens::TokenScope;

/// Ceiling on a BYOK validation round-trip. The save is synchronous with
/// the user's request, so a hung provider must produce a crisp 400, not a
/// stalled settings page (the OpenAI-compat provider's own default timeout
/// is minutes — built for long completions, not auth checks).
const VALIDATION_TIMEOUT: Duration = Duration::from_secs(10);

/// Ceiling on the managed-usage lookup inside `GET /api/account/provider`.
/// Usage is decoration on a status response; the response must never block
/// on the provisioning API (plan: "Audit / visibility" — best-effort).
const USAGE_TIMEOUT: Duration = Duration::from_secs(3);

/// The owned form of [`crate::BackupPolicy`] the tenant plane holds across
/// workers (adversarial-review issue 3). `serve` installs
/// [`Enabled`](Self::Enabled) with the configured store; tests and dev keep
/// the [`DisabledAcknowledged`](Self::DisabledAcknowledged) default, which the
/// route turns into a loud-warn drop. There is no third "forgot to wire it"
/// state — that's the whole point.
#[derive(Clone)]
enum DeletionBackupPolicy {
    /// Backups enabled: the deletion route takes a fail-closed final dump to
    /// this store before any drop.
    Enabled(Arc<dyn crate::backup_store::BackupStore>),
    /// Backups acknowledged-disabled: the route drops without a final dump,
    /// after a loud warning.
    DisabledAcknowledged,
}

impl DeletionBackupPolicy {
    /// Borrow as the call-time [`crate::BackupPolicy`] passed to
    /// [`delete_account`](crate::delete_account).
    fn as_policy(&self) -> crate::BackupPolicy<'_> {
        match self {
            DeletionBackupPolicy::Enabled(store) => crate::BackupPolicy::Required(store),
            DeletionBackupPolicy::DisabledAcknowledged => crate::BackupPolicy::DisabledAcknowledged,
        }
    }
}

/// Everything the tenant-plane handlers need, shared across workers.
struct PlaneState {
    control: ControlPlane,
    /// The shared tenant cluster, where `DELETE /api/account` drops the
    /// tenant database.
    cluster: ClusterConfig,
    /// Managed provider-key lifecycle: deletion step 3 deletes the
    /// account's managed runtime key via the provisioning API (plan:
    /// "Account deletion"); the provider status route reads allowance usage
    /// through it.
    managed: ManagedKeys,
    /// Encrypts/decrypts provider credentials at rest. Held directly (not
    /// through [`ManagedKeys`], which carries no vault in `Disabled` mode):
    /// BYOK entry works whether or not managed provisioning is enabled.
    vault: Arc<dyn KeyVault>,
    /// The serving cache this process resolves tenants through. Deletion
    /// must evict from *this* cache (or the dropped account's entry — and
    /// the WebSocket channel it owns — would linger until the idle TTL);
    /// provider writes live-rotate through it.
    cache: Arc<AccountCache>,
    /// The deletion backup policy for `DELETE /api/account` (plan: "Account
    /// deletion" step 4; adversarial-review issue 3). An **explicit** decision,
    /// not a fail-open `Option<store>`: either a store is wired (the final
    /// pre-drop dump is taken — the operator's only undo under hard-delete v1)
    /// or backups are acknowledged-disabled (dev), and forgetting to wire the
    /// store on an enabled deployment is impossible by construction. `serve`
    /// always installs the store via [`TenantPlane::with_backup_store`]; tests
    /// and dev leave the acknowledged-disabled default.
    backup_policy: DeletionBackupPolicy,
    /// Per-`pg_dump` budget for the final dump (adversarial-review issue 1);
    /// see [`BackupConfig::backup_timeout`](crate::BackupConfig::backup_timeout).
    backup_timeout: std::time::Duration,
    /// Serializes the provider-mutation routes per account; see
    /// [`AccountLocks`].
    provider_locks: AccountLocks,
    /// Whether Stripe is configured on this deployment (mirrors
    /// [`crate::billing_routes::Billing::is_configured`]). Surfaced in the
    /// account overview so the dashboard's billing page can disable the
    /// portal/checkout actions with an explanatory note when billing is off,
    /// rather than navigating the browser onto a raw `billing_not_configured`
    /// 503. Defaults to `false` (billing disabled) until `serve` flips it via
    /// [`TenantPlane::with_billing_configured`]; tests that don't exercise
    /// billing leave the default.
    billing_configured: bool,
    /// The configured Stripe billing provider, threaded so the active-deletion
    /// route (`DELETE /api/account`) can fire a best-effort subscription cancel
    /// before the accounts-row CASCADE sweeps the `stripe_subscriptions`
    /// pointer (DEL-1). `None` when billing is unconfigured (dev/tests, or a
    /// deployment with no Stripe key); `serve` wires it from
    /// [`crate::billing_routes::Billing::provider`].
    billing_provider: Option<Arc<dyn BillingProvider>>,
}

/// Per-account serialization of the provider-mutation routes (BYOK save,
/// activation, models write). Each of those is a multi-step,
/// non-transactional sequence — credential/pointer writes in the control
/// plane followed by the live config swap ([`apply_live_config`]) — so two
/// concurrent writes for the same account could interleave such that storage
/// holds one winner while the in-memory config matches the other, a
/// divergence nothing heals until the cache entry is rebuilt. Holding one
/// per-account lock across the whole sequence makes storage order and apply
/// order identical by construction; the rejected alternative — re-reading
/// the stored active credentials after the flip and applying *that* — only
/// shrinks the window, because the applies themselves can still invert.
/// Same in-process keyed-lock idiom as [`AccountCache`]'s loading map.
///
/// Writes for different accounts never contend, and the map prunes entries
/// nobody holds or awaits, so it stays bounded by in-flight writes.
#[derive(Default)]
struct AccountLocks {
    inner: std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl AccountLocks {
    /// Take the account's write lock, creating it on first use. The guard is
    /// owned, so it can be held across the route's await points.
    async fn acquire(&self, account_id: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = {
            let mut map = self.inner.lock().expect("account lock map poisoned");
            // Prune idle entries: strong_count == 1 means only the map holds
            // the Arc — no guard out, no waiter queued behind one.
            map.retain(|_, lock| Arc::strong_count(lock) > 1);
            Arc::clone(map.entry(account_id.to_string()).or_default())
        };
        lock.lock_owned().await
    }
}

/// The cloud-owned tenant-plane routes as a registrable unit: construct
/// once, hand a clone to every worker's `configure_cloud_app` call. Cheap to
/// clone.
#[derive(Clone)]
pub struct TenantPlane {
    state: web::Data<PlaneState>,
}

impl TenantPlane {
    /// The managed-key lifecycle handle — an accessor (like
    /// [`CloudAuth::base_domain`](crate::auth::CloudAuth)) so the admin
    /// plane composes from the same inputs without a new
    /// `configure_cloud_app` argument.
    pub(crate) fn managed(&self) -> ManagedKeys {
        self.state.managed.clone()
    }

    /// Build the plane over the same control plane, cluster, vault, and
    /// cache the rest of the composition serves from.
    pub fn new(
        control: ControlPlane,
        cluster: ClusterConfig,
        managed: ManagedKeys,
        vault: Arc<dyn KeyVault>,
        cache: Arc<AccountCache>,
    ) -> Self {
        Self {
            state: web::Data::new(PlaneState {
                control,
                cluster,
                managed,
                vault,
                cache,
                // Backups are acknowledged-disabled until `with_backup_store`
                // says otherwise (dev/tests). This is an explicit policy, not a
                // fail-open `None` (adversarial-review issue 3).
                backup_policy: DeletionBackupPolicy::DisabledAcknowledged,
                backup_timeout: crate::backup::DEFAULT_BACKUP_TIMEOUT,
                provider_locks: AccountLocks::default(),
                billing_configured: false,
                billing_provider: None,
            }),
        }
    }

    /// Record whether Stripe billing is configured on this deployment, so the
    /// account overview can tell the dashboard to enable or disable the
    /// portal/checkout actions (an unconfigured deployment 503s those routes).
    /// `serve` sets this from [`crate::billing_routes::Billing::is_configured`];
    /// tests leave the `false` default. Like [`with_backup_store`], this
    /// rebuilds the shared `web::Data`, so it must be called before the plane
    /// is shared with workers.
    ///
    /// [`with_backup_store`]: TenantPlane::with_backup_store
    pub fn with_billing_configured(mut self, configured: bool) -> Self {
        let state = &*self.state;
        self.state = web::Data::new(PlaneState {
            control: state.control.clone(),
            cluster: state.cluster.clone(),
            managed: state.managed.clone(),
            vault: state.vault.clone(),
            cache: Arc::clone(&state.cache),
            backup_policy: state.backup_policy.clone(),
            backup_timeout: state.backup_timeout,
            provider_locks: AccountLocks::default(),
            billing_configured: configured,
            billing_provider: state.billing_provider.clone(),
        });
        self
    }

    /// Wire the Stripe billing provider so the active-deletion route fires a
    /// best-effort subscription cancel before the accounts-row CASCADE destroys
    /// the `stripe_subscriptions` pointer (DEL-1). `serve` calls this from
    /// [`crate::billing_routes::Billing::provider`]; tests and unconfigured
    /// deployments leave the `None` default (the cancel step is then skipped).
    /// Like the other builders, this rebuilds the shared `web::Data`, so it must
    /// be called before the plane is shared with workers.
    pub fn with_billing_provider(mut self, provider: Option<Arc<dyn BillingProvider>>) -> Self {
        let state = &*self.state;
        self.state = web::Data::new(PlaneState {
            control: state.control.clone(),
            cluster: state.cluster.clone(),
            managed: state.managed.clone(),
            vault: state.vault.clone(),
            cache: Arc::clone(&state.cache),
            backup_policy: state.backup_policy.clone(),
            backup_timeout: state.backup_timeout,
            provider_locks: AccountLocks::default(),
            billing_configured: state.billing_configured,
            billing_provider: provider,
        });
        self
    }

    /// Wire the backup store the deletion route takes its final pre-drop dump
    /// to (plan: "Account deletion" step 4), with the per-dump `timeout`. This
    /// flips the deletion policy to [`DeletionBackupPolicy::Enabled`]: the
    /// route then takes a fail-closed final dump before any drop. `serve` calls
    /// this; tests that don't exercise deletion-backups leave the
    /// acknowledged-disabled default (deletions then take no final dump, with a
    /// loud warning). Must be called before the plane is shared with workers —
    /// it rebuilds the shared `web::Data`.
    pub fn with_backup_store(
        mut self,
        store: Arc<dyn crate::backup_store::BackupStore>,
        timeout: std::time::Duration,
    ) -> Self {
        let state = &*self.state;
        self.state = web::Data::new(PlaneState {
            control: state.control.clone(),
            cluster: state.cluster.clone(),
            managed: state.managed.clone(),
            vault: state.vault.clone(),
            cache: Arc::clone(&state.cache),
            backup_policy: DeletionBackupPolicy::Enabled(store),
            backup_timeout: timeout,
            provider_locks: AccountLocks::default(),
            billing_configured: state.billing_configured,
            billing_provider: state.billing_provider.clone(),
        });
        self
    }

    /// Register the tenant-plane routes on `cfg`, each behind `auth` (and
    /// the plane guard, mirroring the cloud `/ws` route). Called by
    /// `configure_cloud_app` **before** atomic-server's `api_scope()` so the
    /// exact-path resources here win the route match.
    pub(crate) fn configure(&self, cfg: &mut web::ServiceConfig, auth: CloudAuth) {
        // Later-registered wrap runs first on every resource below: auth
        // resolves the tenant, then the guard verifies the extensions exist.
        cfg.service(
            web::resource("/api/account")
                .app_data(self.state.clone())
                .route(web::delete().to(delete_account_route))
                .wrap(from_fn(cloud_plane_guard))
                .wrap(auth.clone()),
        );
        cfg.service(
            web::resource("/api/account/overview")
                .app_data(self.state.clone())
                .route(web::get().to(account_overview_route))
                .wrap(from_fn(cloud_plane_guard))
                .wrap(auth.clone()),
        );
        cfg.service(
            web::resource("/api/account/provider")
                .app_data(self.state.clone())
                .route(web::get().to(provider_status_route))
                .route(web::put().to(save_byok_provider_route))
                .wrap(from_fn(cloud_plane_guard))
                .wrap(auth.clone()),
        );
        cfg.service(
            web::resource("/api/account/provider/activate")
                .app_data(self.state.clone())
                .route(web::post().to(activate_provider_route))
                .wrap(from_fn(cloud_plane_guard))
                .wrap(auth.clone()),
        );
        cfg.service(
            web::resource("/api/account/provider/models")
                .app_data(self.state.clone())
                .route(web::put().to(update_models_route))
                .wrap(from_fn(cloud_plane_guard))
                .wrap(auth.clone()),
        );
        // The tenant token-management plane, at the SAME paths as
        // atomic-server's self-hosted token routes so the product app's
        // existing settings UI works unchanged. Registered here (ahead of
        // `api_scope`) these exact-path resources win the match; the
        // self-hosted handlers behind them stay unreachable, and the rest
        // of `/api/auth/*` stays unrouted by `fallback_bound_plane`.
        cfg.service(
            web::resource("/api/auth/tokens")
                .app_data(self.state.clone())
                .route(web::get().to(list_tokens_route))
                .route(web::post().to(create_token_route))
                .wrap(from_fn(cloud_plane_guard))
                .wrap(auth.clone()),
        );
        cfg.service(
            web::resource("/api/auth/tokens/{id}")
                .app_data(self.state.clone())
                .route(web::delete().to(revoke_token_route))
                .wrap(from_fn(cloud_plane_guard))
                .wrap(auth),
        );
    }
}

// --- Dashboard overview route (cloud-only) ----------------------------------

/// `GET /api/account/overview`: the hosted dashboard's single read (module
/// docs). Account-scope only; never returns key material.
///
/// The provider summary reuses the status route's exact discipline — the
/// active credentials' `origin`/`provider`/`model_config`/validation
/// timestamps, and nothing that could carry a secret. Usage is counted live
/// from the tenant database (atoms summed across knowledge bases, KB count
/// from the manager) so it cannot skew from a stored counter, the same
/// source the quota guard enforces against.
async fn account_overview_route(req: HttpRequest, state: web::Data<PlaneState>) -> HttpResponse {
    let tenant = match require_account_scope(&req) {
        Ok(tenant) => tenant,
        Err(denial) => return denial,
    };
    let account_id = &tenant.principal.account_id;

    // Identity + plan, joined off the one accounts row (and its plan FK). A
    // NULL plan_id falls back to the default plan id, matching
    // `PlanRegistry::for_account`; a resolved plan_id that names no row is a
    // fail-closed invariant rather than a silent unlimited grant.
    let account_row: Option<AccountOverviewRow> = match sqlx::query_as(
        "SELECT a.email, \
                a.trial_ends_at, \
                COALESCE(a.plan_id, 'free') AS plan_id, \
                p.name AS plan_name, \
                p.atom_limit, \
                p.kb_limit, \
                p.ai_credits_monthly_cents \
           FROM accounts a \
           LEFT JOIN plans p ON p.id = COALESCE(a.plan_id, 'free') \
          WHERE a.id = $1",
    )
    .bind(account_id)
    .fetch_optional(state.control.pool())
    .await
    {
        Ok(row) => row,
        Err(e) => {
            return provider_state_error(
                account_id,
                "reading account overview",
                CloudError::db("reading account overview")(e),
            )
        }
    };
    let Some(account_row) = account_row else {
        // CloudAuth resolved this request to the account moments ago, so the
        // row vanishing here is a concurrent deletion: 404, like a repeat
        // DELETE.
        return HttpResponse::NotFound().json(json!({
            "error": "account_not_found",
            "message": "This account no longer exists.",
        }));
    };
    if account_row.plan_name.is_none() {
        tracing::error!(
            account_id,
            plan_id = account_row.plan_id,
            "account references an unknown plan; overview cannot resolve it"
        );
        return HttpResponse::InternalServerError().json(json!({
            "error": "plan_unresolved",
            "message": "This account's plan could not be resolved.",
        }));
    }

    // Live usage from the tenant database: atoms summed across every
    // knowledge base, KB count from the manager (the same reads enforcement
    // uses; see crate::plans). A failure here is best-effort — usage renders
    // as unknown rather than failing the whole overview, which would leave the
    // dashboard blank for a transient tenant-DB blip.
    let (atoms_used, kb_count) = account_usage(&state, account_id).await;

    // Provider summary: status only, never the key (same contract as the
    // provider status route). A read failure degrades to `null`, not a 500 —
    // the dashboard shows "no provider summary" rather than nothing.
    let provider =
        match get_active_credentials(&state.control, state.vault.as_ref(), account_id).await {
            Ok(Some(credentials)) => json!({
                "configured": true,
                "origin": credentials.origin.as_str(),
                "provider": credentials.provider.as_str(),
                "model_config": credentials.model_config,
                "last_validated_at": credentials.last_validated_at.map(|t| t.to_rfc3339()),
                "last_validation_error": credentials.last_validation_error,
            }),
            Ok(None) => json!({
                "configured": false,
                "origin": Value::Null,
                "provider": Value::Null,
                "model_config": Value::Null,
                "last_validated_at": Value::Null,
                "last_validation_error": Value::Null,
            }),
            Err(e) => {
                tracing::warn!(account_id, error = %e, "overview: provider summary unavailable");
                Value::Null
            }
        };

    HttpResponse::Ok().json(json!({
        "subdomain": tenant.subdomain,
        "email": account_row.email,
        "plan": {
            "id": account_row.plan_id,
            "name": account_row.plan_name,
        },
        "billing_state": tenant.billing_state.as_str(),
        // Whether Stripe is wired on this deployment; the dashboard disables
        // the portal/checkout actions with an explanatory note when false
        // (those routes 503 `billing_not_configured` otherwise).
        "billing_configured": state.billing_configured,
        "trial_ends_at": account_row.trial_ends_at.map(|t| t.to_rfc3339()),
        "usage": {
            "atoms_used": atoms_used,
            "atom_limit": account_row.atom_limit,
            "kb_count": kb_count,
            "kb_limit": account_row.kb_limit,
            "ai_credits_monthly_cents": account_row.ai_credits_monthly_cents,
        },
        "provider": provider,
        "mcp_url": mcp_url(&req),
    }))
}

/// The accounts+plans join the overview reads. `plan_name` is `Option` so a
/// dangling `plan_id` (no matching `plans` row) surfaces as a fail-closed
/// error rather than decoding past it.
#[derive(sqlx::FromRow)]
struct AccountOverviewRow {
    email: String,
    trial_ends_at: Option<chrono::DateTime<chrono::Utc>>,
    plan_id: String,
    plan_name: Option<String>,
    atom_limit: Option<i32>,
    kb_limit: Option<i32>,
    ai_credits_monthly_cents: i32,
}

/// Live `(atoms_used, kb_count)` for the overview, summed across the
/// account's knowledge bases through the serving cache's manager (the same
/// path [`rearm_provider_held_work`] uses). Best-effort: a tenant-DB read
/// failure returns `(None, None)` and logs, so the dashboard renders "usage
/// unavailable" rather than the whole overview 500ing on a transient blip.
async fn account_usage(state: &PlaneState, account_id: &str) -> (Option<i64>, Option<i64>) {
    let handle = match state.cache.get_or_load(account_id).await {
        Ok(handle) => handle,
        Err(e) => {
            tracing::warn!(account_id, error = %e, "overview: tenant load failed; usage omitted");
            return (None, None);
        }
    };
    let databases = match handle.manager.list_databases().await {
        Ok((databases, _)) => databases,
        Err(e) => {
            tracing::warn!(account_id, error = %e, "overview: listing knowledge bases failed");
            return (None, None);
        }
    };
    let kb_count = databases.len() as i64;
    let mut atoms_used: i64 = 0;
    for db in &databases {
        let core = match handle.manager.get_core(&db.id).await {
            Ok(core) => core,
            Err(e) => {
                tracing::warn!(account_id, db_id = db.id, error = %e, "overview: core resolve failed");
                return (None, Some(kb_count));
            }
        };
        match core.count_atoms().await {
            Ok(count) => atoms_used += i64::from(count),
            Err(e) => {
                tracing::warn!(account_id, db_id = db.id, error = %e, "overview: count_atoms failed");
                return (None, Some(kb_count));
            }
        }
    }
    (Some(atoms_used), Some(kb_count))
}

/// The account's MCP endpoint URL, derived from the request's own
/// origin (scheme + the tenant `Host`) so it is correct per deployment
/// without threading the base domain in. The dashboard only *displays* this;
/// the endpoint itself is the `/mcp` route wired in `server.rs`.
fn mcp_url(req: &HttpRequest) -> String {
    let conn = req.connection_info().clone();
    format!("{}://{}/mcp", conn.scheme(), conn.host())
}

/// Confirmation body for `DELETE /api/account`. Extracted as
/// `Option<web::Json<…>>` so a missing or malformed body produces this
/// module's structured 400 instead of actix's default deserialization
/// error.
#[derive(Deserialize)]
struct DeleteAccountRequest {
    /// Must equal the subdomain the request arrived on.
    confirm: String,
}

/// `DELETE /api/account` (tenant subdomain only). See the module docs for
/// the refusal order and the delete→evict sequencing rationale.
async fn delete_account_route(
    req: HttpRequest,
    state: web::Data<PlaneState>,
    body: Option<web::Json<DeleteAccountRequest>>,
) -> HttpResponse {
    let tenant = match require_account_scope(&req) {
        Ok(tenant) => tenant,
        Err(denial) => return denial,
    };

    let confirmed = body
        .map(web::Json::into_inner)
        .is_some_and(|b| b.confirm == tenant.subdomain);
    if !confirmed {
        return confirmation_required(&tenant.subdomain);
    }

    // Spawn the destructive sequence and await the handle (module docs:
    // "Cancellation-proofing"): a client disconnect drops this handler
    // future, but the spawned task — which has already revoked the
    // account's credentials by its first step — keeps running to
    // completion. Evict happens inside the same task, after the delete
    // (module docs): the account rows are gone by then, so nothing can
    // rebuild the entry behind it; dropping the entry drops its event
    // channel's Sender, which is what severs the account's live WebSocket
    // sessions once the request-scoped clones unwind.
    // `actix_web::rt::spawn` (a `spawn_local` onto this worker's LocalSet)
    // rather than `tokio::spawn`: the detachment semantics are identical —
    // dropping this handler's future does not cancel the spawned task — and
    // it sidesteps rustc's overly-conservative `Send` analysis of sqlx
    // futures (the same limitation noted in tests/provisioning.rs).
    let task_state = state.clone();
    let account_id = tenant.principal.account_id.clone();
    let outcome = actix_web::rt::spawn(async move {
        // The HTTP route is the canonical active-account deletion path, so it
        // takes the per-account advisory lock itself (DeleteLock::Acquire) —
        // making this delete and a concurrent nightly backup pass of the same
        // tenant mutually exclusive (adversarial-review issue 2) — and states
        // an explicit backup policy (issue 3).
        delete_account(
            &task_state.control,
            &task_state.cluster,
            &task_state.managed,
            // This is the canonical active-deletion path, so it MUST fire the
            // best-effort Stripe subscription cancel before step 7's CASCADE
            // sweeps the `stripe_subscriptions` pointer (DEL-1). `delete_account`
            // treats `None` (unconfigured billing) and a missing subscription
            // row as no-ops, so this is correct whether or not Stripe is wired.
            task_state.billing_provider.as_ref(),
            task_state.backup_policy.as_policy(),
            crate::provision::DeleteLock::Acquire,
            &account_id,
            task_state.backup_timeout,
        )
        .await?;
        task_state.cache.evict(&account_id).await;
        Ok::<(), crate::error::CloudError>(())
    })
    .await;
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(join_error) => {
            tracing::error!(
                account_id = tenant.principal.account_id,
                error = %join_error,
                "account deletion task panicked"
            );
            return deletion_failed();
        }
    };
    if let Err(e) = outcome {
        // A Busy error (a backup pass holds the account's advisory lock past
        // the retry budget) is transient, not a failure: tell the caller to
        // retry (503 + Retry-After) rather than report the deletion broken
        // (adversarial-review issue 2).
        if matches!(e, crate::error::CloudError::Busy(_)) {
            tracing::info!(
                account_id = tenant.principal.account_id,
                subdomain = tenant.subdomain,
                "account deletion deferred: a backup pass holds the account lock; client should retry"
            );
            return deletion_busy();
        }
        tracing::error!(
            account_id = tenant.principal.account_id,
            subdomain = tenant.subdomain,
            error = %e,
            "account deletion failed"
        );
        return deletion_failed();
    }

    tracing::info!(
        account_id = tenant.principal.account_id,
        subdomain = tenant.subdomain,
        source = tenant.principal.source.as_str(),
        "account deleted via HTTP route"
    );
    HttpResponse::Ok().json(serde_json::json!({
        "status": "deleted",
        "subdomain": tenant.subdomain,
    }))
}

// --- Provider settings routes (plan: "Provider management") ----------------

/// `GET /api/account/provider`: status only — never the key, never a prefix
/// (module docs). Managed usage is best-effort decoration: the lookup is
/// capped at [`USAGE_TIMEOUT`] and any failure renders as `null`, never as
/// an error response.
async fn provider_status_route(req: HttpRequest, state: web::Data<PlaneState>) -> HttpResponse {
    let tenant = match require_account_scope(&req) {
        Ok(tenant) => tenant,
        Err(denial) => return denial,
    };
    let account_id = &tenant.principal.account_id;

    let credentials =
        match get_active_credentials(&state.control, state.vault.as_ref(), account_id).await {
            Ok(credentials) => credentials,
            Err(e) => return provider_state_error(account_id, "reading provider credentials", e),
        };
    let Some(credentials) = credentials else {
        return HttpResponse::Ok().json(json!({
            "configured": false,
            "provider": Value::Null,
            "origin": Value::Null,
            "model_config": Value::Null,
            "created_at": Value::Null,
            "rotated_at": Value::Null,
            "last_used_at": Value::Null,
            "last_validated_at": Value::Null,
            "last_validation_error": Value::Null,
            "usage": Value::Null,
        }));
    };

    let usage = managed_key_usage(&state.managed, &credentials).await;
    HttpResponse::Ok().json(json!({
        "configured": true,
        "provider": credentials.provider.as_str(),
        "origin": credentials.origin.as_str(),
        "model_config": credentials.model_config,
        "created_at": credentials.created_at.to_rfc3339(),
        "rotated_at": credentials.rotated_at.map(|t| t.to_rfc3339()),
        "last_used_at": credentials.last_used_at.map(|t| t.to_rfc3339()),
        "last_validated_at": credentials.last_validated_at.map(|t| t.to_rfc3339()),
        "last_validation_error": credentials.last_validation_error,
        "usage": usage,
    }))
}

/// BYOK save body. `model_config` follows the vocabulary documented in
/// [`crate::provider_config`]; omitted, the provider's defaults apply.
#[derive(Deserialize)]
struct SaveByokRequest {
    provider: String,
    api_key: String,
    #[serde(default)]
    model_config: Option<Value>,
}

/// `PUT /api/account/provider`: BYOK entry/rotation (plan: "BYOK entry &
/// validation"). The submitted key is validated against the provider
/// **before** anything is stored; a validation failure is a 400 carrying
/// the provider's error verbatim and leaves the account's stored state —
/// including a previously saved BYOK key — completely untouched. The
/// stored key is never displayed, so rotation is simply this same route
/// with a new key.
async fn save_byok_provider_route(
    req: HttpRequest,
    state: web::Data<PlaneState>,
    body: Option<web::Json<SaveByokRequest>>,
) -> HttpResponse {
    let tenant = match require_account_scope(&req) {
        Ok(tenant) => tenant,
        Err(denial) => return denial,
    };
    let account_id = &tenant.principal.account_id;

    let Some(body) = body.map(web::Json::into_inner) else {
        return bad_request(
            "invalid_request",
            "Body must be {\"provider\": \"openrouter\"|\"openai_compat\", \
             \"api_key\": \"...\", \"model_config\"?: {...}}.",
        );
    };
    let provider: Provider = match body.provider.parse() {
        Ok(provider) => provider,
        Err(_) => {
            return bad_request(
                "invalid_provider",
                "provider must be \"openrouter\" or \"openai_compat\"; \
                 other providers are not available in cloud.",
            )
        }
    };
    let api_key = body.api_key.trim();
    if api_key.is_empty() {
        return bad_request("invalid_api_key", "api_key must not be empty.");
    }
    let api_key = SecretKey::new(api_key.to_string());
    let model_config = body.model_config.unwrap_or_else(|| json!({}));
    // Vocabulary check (module docs): the column is plaintext and echoed by
    // the status route, so anything outside the documented keys — most
    // dangerously a nested api_key — is rejected before it can be stored.
    if let Err(violation) = validate_byok_model_config(&model_config) {
        return bad_request("invalid_model_config", &violation);
    }

    // One provider write per account at a time, from here through the live
    // swap (see [`AccountLocks`]). Taken before validation as well — a save
    // racing another save/activate/models write has no defined winner
    // anyway, and serializing the whole sequence is what keeps the stored
    // and live configs convergent.
    let _write_guard = state.provider_locks.acquire(account_id).await;

    // Build the candidate config; refuse any config whose effective
    // embedding dimension differs from the platform pin BEFORE the network
    // round-trip — the tenant's vector column cannot be recreated at
    // another width (module docs), so storing such a config would wedge
    // every future embed.
    let candidate = build_provider_config(provider, Some(&api_key), &model_config);
    if candidate.embedding_dimension() != PINNED_EMBEDDING_DIMENSION {
        return embedding_dimension_unsupported(candidate.embedding_dimension());
    }

    // Validate the key against the provider BEFORE storing anything (plan:
    // validation on save).
    if let Err(provider_error) = validate_provider_key(&candidate).await {
        // Scrub the submitted key out of the message before it goes
        // anywhere — a hostile or misconfigured endpoint could echo it.
        let message = provider_error.replace(api_key.expose(), "[redacted]");
        return HttpResponse::BadRequest().json(json!({
            "error": "provider_validation_failed",
            "message": message,
        }));
    }

    // Re-embed warning: compare against the config the account is actually
    // using right now (whatever its origin).
    let previous =
        match get_active_credentials(&state.control, state.vault.as_ref(), account_id).await {
            Ok(previous) => previous,
            Err(e) => return provider_state_error(account_id, "reading provider credentials", e),
        };
    let warning = reembed_warning(
        previous.as_ref().map(config_for_credentials).as_ref(),
        &candidate,
    );

    // Store: encrypt + UPSERT the user row (the upsert stamps rotated_at on
    // replacement and resets validation state), record the validation that
    // just succeeded, flip the active pointer.
    let new = NewCredentials {
        provider,
        origin: CredentialOrigin::User,
        api_key,
        external_key_id: None,
        model_config: model_config.clone(),
    };
    if let Err(e) = upsert_credentials(&state.control, state.vault.as_ref(), account_id, new).await
    {
        return provider_state_error(account_id, "storing provider credentials", e);
    }
    // Audit metadata only — a failed stamp must not fail a save that
    // already happened.
    if let Err(e) = record_validation(
        &state.control,
        account_id,
        provider,
        CredentialOrigin::User,
        None,
    )
    .await
    {
        tracing::warn!(account_id, error = %e, "failed to record BYOK validation success");
    }
    if let Err(e) = set_active_provider(
        &state.control,
        account_id,
        Some((provider, CredentialOrigin::User)),
    )
    .await
    {
        return provider_state_error(account_id, "activating provider credentials", e);
    }

    // Live rotation (plan steps 4-5): swap the cached entry's config in
    // place — no eviction, in-flight operations unaffected.
    if let Err(denial) = apply_live_config(&state, account_id, candidate).await {
        return denial;
    }

    tracing::info!(
        account_id,
        provider = provider.as_str(),
        "BYOK provider credentials saved and activated"
    );
    HttpResponse::Ok().json(json!({
        "status": "saved",
        "provider": provider.as_str(),
        "origin": CredentialOrigin::User.as_str(),
        "validated": true,
        "reembed_warning": warning,
    }))
}

/// Activation body: which stored credentials row to make active.
#[derive(Deserialize)]
struct ActivateProviderRequest {
    provider: String,
    origin: String,
}

/// `POST /api/account/provider/activate`: the column flip between stored
/// rows (plan: "Managed key lifecycle" — switching back is a column flip,
/// not a re-provision). Works in both directions; 404 when the target row
/// doesn't exist. A missing managed row is NOT re-provisioned here —
/// managed keys are minted at signup only.
async fn activate_provider_route(
    req: HttpRequest,
    state: web::Data<PlaneState>,
    body: Option<web::Json<ActivateProviderRequest>>,
) -> HttpResponse {
    let tenant = match require_account_scope(&req) {
        Ok(tenant) => tenant,
        Err(denial) => return denial,
    };
    let account_id = &tenant.principal.account_id;

    let Some(body) = body.map(web::Json::into_inner) else {
        return bad_request(
            "invalid_request",
            "Body must be {\"provider\": \"openrouter\"|\"openai_compat\", \
             \"origin\": \"managed\"|\"user\"}.",
        );
    };
    let provider: Provider = match body.provider.parse() {
        Ok(provider) => provider,
        Err(_) => {
            return bad_request(
                "invalid_provider",
                "provider must be \"openrouter\" or \"openai_compat\".",
            )
        }
    };
    let origin: CredentialOrigin = match body.origin.parse() {
        Ok(origin) => origin,
        Err(_) => return bad_request("invalid_origin", "origin must be \"managed\" or \"user\"."),
    };

    // One provider write per account at a time (see [`AccountLocks`]).
    let _write_guard = state.provider_locks.acquire(account_id).await;

    let target = match get_credentials(
        &state.control,
        state.vault.as_ref(),
        account_id,
        provider,
        origin,
    )
    .await
    {
        Ok(target) => target,
        Err(e) => return provider_state_error(account_id, "reading provider credentials", e),
    };
    let Some(target) = target else {
        return credentials_not_found(provider, origin);
    };

    let previous =
        match get_active_credentials(&state.control, state.vault.as_ref(), account_id).await {
            Ok(previous) => previous,
            Err(e) => return provider_state_error(account_id, "reading provider credentials", e),
        };
    let config = config_for_credentials(&target);
    let warning = reembed_warning(
        previous.as_ref().map(config_for_credentials).as_ref(),
        &config,
    );

    match set_active_provider(&state.control, account_id, Some((provider, origin))).await {
        Ok(()) => {}
        // The row vanished between the read above and the flip (concurrent
        // deletion): same outcome as never existing.
        Err(CloudError::MissingProviderCredentials { .. }) => {
            return credentials_not_found(provider, origin)
        }
        Err(e) => return provider_state_error(account_id, "activating provider credentials", e),
    }

    if let Err(denial) = apply_live_config(&state, account_id, config).await {
        return denial;
    }

    tracing::info!(
        account_id,
        provider = provider.as_str(),
        origin = origin.as_str(),
        "active provider switched"
    );
    HttpResponse::Ok().json(json!({
        "status": "activated",
        "provider": provider.as_str(),
        "origin": origin.as_str(),
        "reembed_warning": warning,
    }))
}

/// Model-selection body. On a BYOK row the full `model_config` replaces the
/// stored one — the vocabulary is small enough that read-modify-write on
/// the client is the honest contract. On a managed row the submitted keys
/// are merged over the stored config instead (route docs below).
#[derive(Deserialize)]
struct UpdateModelsRequest {
    model_config: Value,
}

/// `PUT /api/account/provider/models`: model selection on the **active**
/// credentials row (plan: "Model curation"). Managed rows are
/// curation-checked — pinned embedding model, curated LLM list, no base-URL
/// overrides — and the write merges over the stored config so
/// platform-owned keys survive ([`merge_managed_model_config`]); BYOK rows
/// choose freely within the documented vocabulary and replace wholesale.
/// Every write must keep the effective embedding dimension at the platform
/// pin (rejected otherwise, before storing — module docs); a same-dimension
/// embedding-model change carries the loud `reembed_warning`.
/// Whether the account's plan unlocks the premium agentic model list — the
/// `premium_models` flag on its `plans.feature_flags` (mirrors
/// [`crate::plans::Plan::feature_enabled`]). A NULL `plan_id`, an unknown plan,
/// or any read error resolves to `false` (free tier), so the fuller list is
/// never handed out by accident. Read only on the rare provider-model write.
async fn account_has_premium_models(control: &ControlPlane, account_id: &str) -> bool {
    sqlx::query_scalar::<_, bool>(
        "SELECT COALESCE((p.feature_flags->>'premium_models')::boolean, false) \
           FROM accounts a \
           LEFT JOIN plans p ON p.id = COALESCE(a.plan_id, 'free') \
          WHERE a.id = $1",
    )
    .bind(account_id)
    .fetch_optional(control.pool())
    .await
    .ok()
    .flatten()
    .unwrap_or(false)
}

async fn update_models_route(
    req: HttpRequest,
    state: web::Data<PlaneState>,
    body: Option<web::Json<UpdateModelsRequest>>,
) -> HttpResponse {
    let tenant = match require_account_scope(&req) {
        Ok(tenant) => tenant,
        Err(denial) => return denial,
    };
    let account_id = &tenant.principal.account_id;

    let Some(body) = body.map(web::Json::into_inner) else {
        return bad_request("invalid_request", "Body must be {\"model_config\": {...}}.");
    };
    let submitted = body.model_config;
    if !submitted.is_object() {
        return bad_request(
            "invalid_model_config",
            "model_config must be a JSON object.",
        );
    }

    // One provider write per account at a time (see [`AccountLocks`]).
    let _write_guard = state.provider_locks.acquire(account_id).await;

    let active =
        match get_active_credentials(&state.control, state.vault.as_ref(), account_id).await {
            Ok(active) => active,
            Err(e) => return provider_state_error(account_id, "reading provider credentials", e),
        };
    let Some(active) = active else {
        return HttpResponse::NotFound().json(json!({
            "error": "no_active_provider",
            "message": "No provider is configured for this account; save one first.",
        }));
    };

    // Curation is per-origin: managed model choices spend the platform's
    // money (and the platform's key), BYOK choices are the user's own. On
    // managed rows the validated write is then MERGED over the stored
    // config: curation means the user can only ever submit the user-writable
    // model keys, so a wholesale replace would silently drop the
    // platform-owned keys seeded at provision (notably a base-URL override
    // routing managed traffic through a proxy). BYOK rows keep the
    // documented read-modify-write contract — the user owns every key,
    // having seeded them all at save time — but are still vocabulary-checked
    // (the plaintext-column rule; see the save route) before anything lands.
    let model_config = if active.origin == CredentialOrigin::Managed {
        // The agentic model list is plan-gated: premium plans unlock the fuller
        // set. Resolve the account's tier so curation checks the right list.
        let premium = account_has_premium_models(&state.control, account_id).await;
        if let Err(violation) = validate_managed_model_config(&submitted, premium) {
            return HttpResponse::BadRequest().json(json!({
                "error": "model_not_curated",
                "message": violation,
            }));
        }
        merge_managed_model_config(&active.model_config, &submitted)
    } else {
        if let Err(violation) = validate_byok_model_config(&submitted) {
            return bad_request("invalid_model_config", &violation);
        }
        submitted
    };

    // The dimension pin applies to model selection too — and must reject
    // BEFORE the write lands, or a stored-but-unservable config would wedge
    // the account (module docs).
    let new_config = build_provider_config(active.provider, Some(&active.api_key), &model_config);
    if new_config.embedding_dimension() != PINNED_EMBEDDING_DIMENSION {
        return embedding_dimension_unsupported(new_config.embedding_dimension());
    }

    let old_config = config_for_credentials(&active);
    let updated = match update_model_config(
        &state.control,
        account_id,
        active.provider,
        active.origin,
        &model_config,
    )
    .await
    {
        Ok(updated) => updated,
        Err(e) => return provider_state_error(account_id, "updating model config", e),
    };
    if !updated {
        // The active row vanished between the read and the write
        // (concurrent deletion).
        return credentials_not_found(active.provider, active.origin);
    }

    let warning = reembed_warning(Some(&old_config), &new_config);

    if let Err(denial) = apply_live_config(&state, account_id, new_config).await {
        return denial;
    }

    tracing::info!(
        account_id,
        provider = active.provider.as_str(),
        origin = active.origin.as_str(),
        "provider model config updated"
    );
    HttpResponse::Ok().json(json!({
        "status": "updated",
        "provider": active.provider.as_str(),
        "origin": active.origin.as_str(),
        "model_config": model_config,
        "reembed_warning": warning,
    }))
}

// --- Provider route helpers --------------------------------------------------

/// Validate a candidate provider config against the provider itself, before
/// anything is stored (plan: "BYOK entry & validation"). `Err` carries the
/// provider's error text — surfaced verbatim in the 400 body per the plan —
/// bounded in length and scrubbed of the key by the caller.
///
/// - **OpenRouter**: `GET {base}/auth/key`, the documented key-introspection
///   endpoint. The request reuses [`OpenRouterProvider`]'s own client and
///   base-URL normalization so what we validate is exactly what the
///   pipeline will call.
/// - **OpenAI-compatible**: there is no standard auth-check endpoint, so a
///   minimal embedding call through the same provider machinery the
///   pipeline uses (`create_embedding_provider` → `embed_batch`).
async fn validate_provider_key(config: &ProviderConfig) -> Result<(), String> {
    let validation = async {
        match config.provider_type {
            ProviderType::OpenRouter => validate_openrouter_key(config).await,
            ProviderType::OpenAICompat => validate_openai_compat_key(config).await,
            // Unreachable from these routes (`Provider` has no Ollama
            // variant); typed refusal rather than a panic if that changes.
            ProviderType::Ollama => Err("Ollama is not available in cloud".to_string()),
        }
    };
    match tokio::time::timeout(VALIDATION_TIMEOUT, validation).await {
        Ok(outcome) => outcome,
        Err(_) => Err(format!(
            "provider validation timed out after {}s",
            VALIDATION_TIMEOUT.as_secs()
        )),
    }
}

async fn validate_openrouter_key(config: &ProviderConfig) -> Result<(), String> {
    let Some(api_key) = config.openrouter_api_key.clone() else {
        return Err("OpenRouter API key not configured".to_string());
    };
    let provider = OpenRouterProvider::with_base_url(api_key, config.openrouter_base_url.clone());
    let url = format!("{}/auth/key", provider.base_url());
    let response = provider
        .client()
        .get(&url)
        .bearer_auth(provider.api_key())
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = response.status();
    if status.is_success() {
        return Ok(());
    }
    // Do NOT echo the upstream response body: this validation drives an
    // outbound request to a tenant-supplied base URL, so reflecting the body
    // turns a non-2xx into a read-oracle for whatever the URL points at
    // (SEC-1). Report the status code only — enough for the tenant to fix a
    // bad key, nothing about the upstream content.
    Err(format!(
        "the provider rejected the request (HTTP {})",
        status.as_u16()
    ))
}

async fn validate_openai_compat_key(config: &ProviderConfig) -> Result<(), String> {
    // `create_embedding_provider` also enforces the config shape (a missing
    // base URL is its typed error, surfaced here as the 400 message).
    let provider = create_embedding_provider(config).map_err(|e| e.to_string())?;
    let embedding_config = EmbeddingConfig::new(config.embedding_model().to_string());
    provider
        .embed_batch(
            &["Atomic provider validation".to_string()],
            &embedding_config,
        )
        .await
        .map(|_| ())
        .map_err(|e| {
            // Do NOT surface the provider error to the tenant: it carries the
            // upstream response body verbatim, and this validation drives an
            // outbound request to a tenant-supplied base URL — reflecting it
            // turns the 400 into a read-oracle for whatever the URL points at
            // (SEC-1). The verbatim error is logged server-side for the
            // operator; the tenant gets a generic message, enough to fix a bad
            // config.
            tracing::warn!(error = %e, "openai-compatible BYOK validation failed");
            "the provider rejected the request".to_string()
        })
}

/// Best-effort managed-key allowance usage for the status route. `null`
/// unless the credentials are managed, carry an external id, and the
/// provisioning API answers within [`USAGE_TIMEOUT`].
async fn managed_key_usage(managed: &ManagedKeys, credentials: &ProviderCredentials) -> Value {
    if credentials.origin != CredentialOrigin::Managed {
        return Value::Null;
    }
    let ManagedKeys::Enabled { api, .. } = managed else {
        return Value::Null;
    };
    let Some(external_key_id) = &credentials.external_key_id else {
        return Value::Null;
    };
    match tokio::time::timeout(USAGE_TIMEOUT, api.get_key_usage(external_key_id)).await {
        Ok(Ok(usage)) => json!({
            "usage_usd": usage.usage_usd,
            "limit_usd": usage.limit_usd,
            "limit_remaining_usd": usage.limit_remaining_usd,
            "disabled": usage.disabled,
        }),
        Ok(Err(e)) => {
            tracing::warn!(
                account_id = credentials.account_id,
                error = %e,
                "managed key usage lookup failed; status served without it"
            );
            Value::Null
        }
        Err(_) => {
            tracing::warn!(
                account_id = credentials.account_id,
                "managed key usage lookup timed out; status served without it"
            );
            Value::Null
        }
    }
}

/// The loud re-embed warning (plan: "Model curation" — warn loudly on
/// embedding-model switches). `Null` when the embedding model is unchanged
/// or there was no previous config to change from.
///
/// Changing the embedding model does **not** trigger an automatic re-embed:
/// the write only swaps the active config (see [`apply_live_config`]). Every
/// already-stored vector was produced by the old model and now lives in a
/// different vector space from anything embedded by the new one, so search
/// quality degrades and stays degraded until the operator manually re-embeds
/// the affected atoms. The copy says exactly that — it must not promise a
/// re-embed nothing here enqueues.
fn reembed_warning(previous: Option<&ProviderConfig>, next: &ProviderConfig) -> Value {
    match previous {
        Some(previous) if previous.embedding_model() != next.embedding_model() => json!(format!(
            "Embedding model changed from {:?} to {:?}. Existing atoms are NOT re-embedded \
             automatically: their vectors were produced by the old model and now occupy a \
             different vector space from anything embedded by the new one. Semantic search, \
             related atoms, and tag suggestions over existing content will be degraded until \
             you manually re-embed the affected atoms.",
            previous.embedding_model(),
            next.embedding_model(),
        )),
        _ => Value::Null,
    }
}

/// Live rotation, step 4: apply the fresh config to the cached entry. A
/// miss is success (the next cache load reads the control-plane state just
/// written); an *error* means the control plane and the serving config have
/// diverged with no eviction to heal them, so the request fails loudly and
/// the honest instruction is to retry (the retry re-runs the whole write,
/// idempotently, and re-attempts the swap).
///
/// On success this also re-arms the tenant's provider-held ledger work
/// (module docs) — the mutation is the user's recovery action, and the rows
/// the old configuration's failures pushed out should redispatch on the
/// next tick, not after their stale horizons.
async fn apply_live_config(
    state: &PlaneState,
    account_id: &str,
    config: ProviderConfig,
) -> Result<(), HttpResponse> {
    match state.cache.update_provider_config(account_id, config).await {
        Ok(_was_cached) => {
            rearm_provider_held_work(state, account_id).await;
            Ok(())
        }
        Err(e) => {
            tracing::error!(
                account_id,
                error = %e,
                "provider config stored but live rotation failed"
            );
            Err(HttpResponse::InternalServerError().json(json!({
                "error": "provider_rotation_incomplete",
                "message": "The provider configuration was saved but could not be applied \
                            to the running session. Retry the request.",
            })))
        }
    }
}

/// Reset the horizons on ledger work held back by provider failures, in
/// every knowledge base of the account's tenant database: pipeline jobs the
/// dispatcher re-enqueued with `reason = 'provider-backoff'`
/// (`AtomicCore::rearm_pipeline_jobs`) and pending task runs whose recorded
/// `last_error` classifies as a provider failure — deferred rows and
/// provider-classified backoffs alike
/// (`AtomicCore::rearm_provider_blocked_task_runs`). Best-effort: failures
/// log loudly and the rows simply wait out their stored horizons instead.
async fn rearm_provider_held_work(state: &PlaneState, account_id: &str) {
    let handle = match state.cache.get_or_load(account_id).await {
        Ok(handle) => handle,
        Err(e) => {
            tracing::warn!(account_id, error = %e, "re-arm: tenant load failed");
            return;
        }
    };
    let databases = match handle.manager.list_databases().await {
        Ok((databases, _)) => databases,
        Err(e) => {
            tracing::warn!(account_id, error = %e, "re-arm: listing knowledge bases failed");
            return;
        }
    };
    for db in &databases {
        let core = match handle.manager.get_core(&db.id).await {
            Ok(core) => core,
            Err(e) => {
                tracing::warn!(account_id, db_id = db.id, error = %e, "re-arm: core resolve failed");
                continue;
            }
        };
        let pipeline = match core
            .rearm_pipeline_jobs(crate::dispatcher::PROVIDER_BACKOFF_REASON)
            .await
        {
            Ok(count) => count,
            Err(e) => {
                tracing::warn!(account_id, db_id = db.id, error = %e, "re-arm: pipeline jobs failed");
                0
            }
        };
        let task_runs = match core.rearm_provider_blocked_task_runs().await {
            Ok(count) => count,
            Err(e) => {
                tracing::warn!(account_id, db_id = db.id, error = %e, "re-arm: task runs failed");
                0
            }
        };
        if pipeline > 0 || task_runs > 0 {
            tracing::info!(
                account_id,
                db_id = db.id,
                pipeline_jobs = pipeline,
                task_runs,
                "provider mutation re-armed held ledger work"
            );
        }
    }
}

// --- Denial responses -------------------------------------------------------

/// Resolve the request's tenant and require an account-scope credential —
/// the shared prologue of every route in this module (module docs: the
/// authorization posture). CloudAuth installs the extension on every
/// request it passes; like `cloud_plane_guard`, its absence is a
/// composition bug and fails closed rather than guessing at an identity.
// --- Tenant token management (`/api/auth/tokens*`) ---------------------------
//
// Cloud's per-tenant implementation of the self-hosted token plane: same
// paths, same response shapes (the product settings UI consumes both), but
// backed by the account's control-plane `cloud_tokens` rows — the store
// CloudAuth actually verifies against. Account-scope only, like every route
// in this module: a db-pinned or MCP-scoped credential minting an
// account-scope token would be privilege escalation.

/// Body of `POST /api/auth/tokens` — mirrors atomic-server's `CreateTokenBody`.
#[derive(Deserialize)]
struct CreateCloudTokenBody {
    name: String,
}

/// `POST /api/auth/tokens`: mint an account-scope token, returning its
/// plaintext exactly once (only the hash is stored). Response shape matches
/// the self-hosted route: `{id, name, token, prefix, created_at}`.
async fn create_token_route(
    req: HttpRequest,
    state: web::Data<PlaneState>,
    body: web::Json<CreateCloudTokenBody>,
) -> HttpResponse {
    let tenant = match require_account_scope(&req) {
        Ok(tenant) => tenant,
        Err(denial) => return denial,
    };
    let name = body.into_inner().name;
    let name = name.trim();
    if name.is_empty() || name.len() > 100 {
        return bad_request(
            "invalid_token_name",
            "Token name must be 1-100 characters.",
        );
    }

    match crate::tokens::issue_token(
        &state.control,
        &tenant.principal.account_id,
        TokenScope::Account,
        None,
        name,
    )
    .await
    {
        Ok(plaintext) => HttpResponse::Created().json(json!({
            "id": crate::tokens::sha256_hex(&plaintext),
            "name": name,
            "token": plaintext,
            "prefix": crate::tokens::display_prefix(&plaintext),
            "created_at": chrono::Utc::now().to_rfc3339(),
            "scope": "account",
        })),
        Err(e) => {
            tracing::error!(error = %e, "minting tenant token failed");
            HttpResponse::InternalServerError().json(json!({
                "error": "token_create_failed",
                "message": "Could not create the token.",
            }))
        }
    }
}

/// `GET /api/auth/tokens`: the account's live tokens, metadata only —
/// shaped like atomic-core's `ApiTokenInfo` so the settings UI renders it
/// unchanged. Includes OAuth-minted MCP tokens: listing (and revoking) them
/// is how a user audits and severs MCP-client access.
async fn list_tokens_route(req: HttpRequest, state: web::Data<PlaneState>) -> HttpResponse {
    let tenant = match require_account_scope(&req) {
        Ok(tenant) => tenant,
        Err(denial) => return denial,
    };
    match crate::tokens::list_account_tokens(&state.control, &tenant.principal.account_id).await {
        Ok(tokens) => HttpResponse::Ok().json(
            tokens
                .into_iter()
                .map(|t| {
                    json!({
                        "id": t.id,
                        "name": t.name,
                        "token_prefix": t.token_prefix.unwrap_or_default(),
                        "created_at": t.created_at.to_rfc3339(),
                        "last_used_at": t.last_used_at.map(|ts| ts.to_rfc3339()),
                        "is_revoked": false,
                        "scope": t.scope,
                    })
                })
                .collect::<Vec<_>>(),
        ),
        Err(e) => {
            tracing::error!(error = %e, "listing tenant tokens failed");
            HttpResponse::InternalServerError().json(json!({
                "error": "token_list_failed",
                "message": "Could not list tokens.",
            }))
        }
    }
}

/// `DELETE /api/auth/tokens/{id}`: revoke one of the account's tokens.
/// The id is the stored hash; the account scoping inside the UPDATE is the
/// cross-tenant chokepoint, so an id from another tenant reads as 404.
async fn revoke_token_route(
    req: HttpRequest,
    state: web::Data<PlaneState>,
    path: web::Path<String>,
) -> HttpResponse {
    let tenant = match require_account_scope(&req) {
        Ok(tenant) => tenant,
        Err(denial) => return denial,
    };
    let token_id = path.into_inner();
    match crate::tokens::revoke_account_token(
        &state.control,
        &tenant.principal.account_id,
        &token_id,
    )
    .await
    {
        Ok(true) => HttpResponse::Ok().json(json!({ "revoked": true })),
        Ok(false) => HttpResponse::NotFound().json(json!({
            "error": "not_found",
            "message": "No such token.",
        })),
        Err(e) => {
            tracing::error!(error = %e, "revoking tenant token failed");
            HttpResponse::InternalServerError().json(json!({
                "error": "token_revoke_failed",
                "message": "Could not revoke the token.",
            }))
        }
    }
}

fn require_account_scope(req: &HttpRequest) -> Result<ResolvedTenant, HttpResponse> {
    let Some(tenant) = req.extensions().get::<ResolvedTenant>().cloned() else {
        tracing::error!(
            path = req.path(),
            "tenant-plane route reached without a resolved tenant"
        );
        return Err(HttpResponse::InternalServerError().json(json!({
            "error": "tenant_not_resolved",
            "message": "The request was not resolved to an account.",
        })));
    };
    if tenant.principal.scope != TokenScope::Account {
        return Err(account_scope_required());
    }
    Ok(tenant)
}

/// The credential is real but not allowed to act on the account: database-
/// and MCP-scoped tokens are pinned to a knowledge base, and account
/// deletion and provider credentials are strictly above their station.
fn account_scope_required() -> HttpResponse {
    HttpResponse::Forbidden().json(serde_json::json!({
        "error": "account_scope_required",
        "message": "This action requires an account-scope token or a web session.",
    }))
}

/// Structured 400 for malformed provider-route requests.
fn bad_request(error: &str, message: &str) -> HttpResponse {
    HttpResponse::BadRequest().json(json!({
        "error": error,
        "message": message,
    }))
}

/// Structured 400 for a provider config whose effective embedding dimension
/// differs from the platform pin (module docs; the plan's "warn loudly" is
/// deliberately hardened to a rejection here — the warning promised a
/// re-embed no cloud mechanism can deliver at a different width).
fn embedding_dimension_unsupported(requested: usize) -> HttpResponse {
    HttpResponse::BadRequest().json(json!({
        "error": "embedding_dimension_unsupported",
        "message": format!(
            "This deployment's vector index is fixed at \
             {PINNED_EMBEDDING_DIMENSION} dimensions; the submitted \
             configuration produces {requested}-dimensional embeddings. \
             Changing the embedding dimension is not supported in cloud — \
             choose an embedding model (or embedding_dimension) that \
             produces {PINNED_EMBEDDING_DIMENSION}-dimensional vectors."
        ),
        "required_dimension": PINNED_EMBEDDING_DIMENSION,
        "requested_dimension": requested,
    }))
}

/// 404 for a `(provider, origin)` with no stored credentials row.
fn credentials_not_found(provider: Provider, origin: CredentialOrigin) -> HttpResponse {
    HttpResponse::NotFound().json(json!({
        "error": "provider_credentials_not_found",
        "message": format!(
            "No {provider}/{origin} credentials are stored for this account."
        ),
    }))
}

/// Control-plane failure during a provider operation. The body is generic
/// by design — [`CloudError`] messages never carry key material, but a
/// secret-handling surface earns belt-and-braces; the detail goes to the
/// log.
fn provider_state_error(account_id: &str, context: &str, e: CloudError) -> HttpResponse {
    tracing::error!(account_id, error = %e, "{context} failed");
    HttpResponse::InternalServerError().json(json!({
        "error": "provider_state_error",
        "message": "Something went wrong reading or writing provider state; try again.",
    }))
}

/// Missing, malformed, or mismatched confirmation body. The message names
/// the expected value — the caller already proved they control the account,
/// so this leaks nothing; it just makes a deliberate retry easy and an
/// accidental one impossible.
fn confirmation_required(subdomain: &str) -> HttpResponse {
    HttpResponse::BadRequest().json(serde_json::json!({
        "error": "confirmation_mismatch",
        "message": format!(
            "Deleting this account is permanent. \
             Send {{\"confirm\": {subdomain:?}}} to proceed."
        ),
    }))
}

/// `delete_account` failed partway. The credential that authenticated this
/// request is likely already revoked (revocation is deletion's first step),
/// so the body must NOT advise retrying with it. The honest message:
/// recovery is automatic — anything left half-deleted is reaper territory
/// (the interrupted-deletion arm; see [`crate::reaper`]) — and if the
/// account is still reachable, a fresh login link mints a fresh credential
/// to retry with.
fn deletion_failed() -> HttpResponse {
    HttpResponse::InternalServerError().json(serde_json::json!({
        "error": "deletion_failed",
        "message": "Something went wrong deleting the account. Cleanup \
                    completes automatically in the background; if the \
                    account is still reachable, request a fresh login link \
                    to sign in and try again.",
    }))
}

/// A deletion couldn't take the account's advisory lock because a backup pass
/// holds it (adversarial-review issue 2). Transient: a 503 with a short
/// `Retry-After` so the client retries once the dump completes. Nothing was
/// destroyed — the lock guards the destructive window, so a Busy result means
/// the delete never started dropping.
fn deletion_busy() -> HttpResponse {
    HttpResponse::ServiceUnavailable()
        .insert_header(("Retry-After", "15"))
        .json(serde_json::json!({
            "error": "deletion_busy",
            "message": "A backup of this account is in progress. No data was \
                        changed; retry the deletion in a few seconds.",
        }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The provider-write ordering guard: same-account acquisitions
    /// serialize, different accounts never contend, and the map prunes
    /// entries once nobody holds or awaits them. A true HTTP-level race test
    /// would be timing-dependent flake bait; this pins the seam the routes
    /// rely on instead.
    #[actix_web::test]
    async fn account_locks_serialize_per_account() {
        let locks = AccountLocks::default();

        let guard_a = locks.acquire("acct-a").await;

        // Same account: a second acquire must park behind the held guard.
        let blocked =
            tokio::time::timeout(Duration::from_millis(50), locks.acquire("acct-a")).await;
        assert!(
            blocked.is_err(),
            "same-account acquire must wait for the held guard"
        );

        // Different account: no contention.
        let guard_b = tokio::time::timeout(Duration::from_millis(50), locks.acquire("acct-b"))
            .await
            .expect("different accounts must not contend");

        // Release A: the next same-account acquire proceeds.
        drop(guard_a);
        let guard_a2 = tokio::time::timeout(Duration::from_millis(50), locks.acquire("acct-a"))
            .await
            .expect("released lock must be reacquirable");

        // Pruning: once every guard is dropped, the next acquire sweeps the
        // idle entries — the map holds exactly the key being acquired.
        drop(guard_a2);
        drop(guard_b);
        let guard_c = locks.acquire("acct-c").await;
        let live_keys: Vec<String> = locks
            .inner
            .lock()
            .expect("lock map")
            .keys()
            .cloned()
            .collect();
        assert_eq!(
            live_keys,
            vec!["acct-c".to_string()],
            "idle entries must be pruned on acquire"
        );
        drop(guard_c);
    }
}
