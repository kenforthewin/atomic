//! Billing HTTP surface (plan: "Billing"): the authenticated portal/checkout
//! redirects on a tenant subdomain, and the single signed webhook on the app
//! host. The frontend (a later slice) only ever links to these; this slice
//! is the API + redirects.
//!
//! - `GET /api/billing/portal` (tenant, authenticated) — start a Stripe
//!   Customer Portal session for the account's existing customer and 302 the
//!   browser to it. 409 if the account has no Stripe customer yet (it must
//!   check out first).
//! - `GET /api/billing/checkout?plan=<id>` (tenant, authenticated) — start a
//!   Checkout Session for the named paid plan and 302 to it. The webhook
//!   lands the subscription afterward (we never write plan state from the
//!   redirect).
//! - `POST /billing/webhook` (app host, **unauthenticated** — Stripe is the
//!   caller; the signature is the auth) — verify the signature, project the
//!   event, and apply it to the control plane. A single URL, not
//!   per-subdomain (plan), correlated to an account by the Stripe customer id.
//!
//! All three are no-ops returning a structured `billing_not_configured` 503
//! when Stripe isn't configured (`--stripe-*` unset) — billing is optional
//! for self-hosted-style cloud deployments and dev clusters.

use std::collections::HashMap;
use std::sync::Arc;

use actix_web::guard::{Guard, GuardContext};
use actix_web::http::header;
use actix_web::{guard, web, HttpMessage, HttpRequest, HttpResponse};
use serde::Deserialize;

use crate::auth::ResolvedTenant;
use crate::billing::dunning;
use crate::billing::{self, BillingProvider, WebhookEvent};
use crate::control_plane::ControlPlane;
use crate::managed_keys::ManagedKeys;
use crate::tokens::TokenScope;

/// Everything the billing routes need. `None` provider means Stripe isn't
/// configured — every route degrades to a structured 503.
struct BillingState {
    control: ControlPlane,
    provider: Option<Arc<dyn BillingProvider>>,
    /// Stripe webhook signing secret (`whsec_…`). Empty when unconfigured.
    webhook_secret: String,
    /// Plan id → Stripe price id, for checkout. Built from `--stripe-price`
    /// flags; the reverse map drives webhook price→plan resolution.
    plan_to_price: HashMap<String, String>,
    price_to_plan: HashMap<String, String>,
    /// `https://app.<base>` (no trailing slash) for building return/success
    /// URLs.
    app_public_url: String,
    /// Normalized base domain, for the app-host guard on the webhook.
    base_domain: String,
    /// Managed runtime-key handle, for reconciling a tenant's managed-AI
    /// allowance to its plan after a subscription transition commits (MAI-1).
    /// `Disabled` (or a `with_provider` test that doesn't wire it) makes the
    /// post-commit reconcile a no-op. The reconcile runs OUTSIDE the webhook's
    /// claim+apply transaction — a provider PATCH must never extend or wedge
    /// that transaction.
    managed: ManagedKeys,
}

/// Public configuration for [`Billing::new`].
#[derive(Debug, Clone)]
pub struct BillingConfig {
    /// Stripe secret key (`sk_…`); `None` disables billing.
    pub stripe_secret_key: Option<String>,
    /// Stripe webhook signing secret (`whsec_…`).
    pub webhook_secret: Option<String>,
    /// `plan_id → stripe_price_id`.
    pub plan_prices: HashMap<String, String>,
    /// `https://app.<base>`; defaults to `https://app.<base_domain>`.
    pub app_public_url: Option<String>,
    pub base_domain: String,
    /// Override the Stripe API base URL (tests point it at wiremock).
    pub stripe_base_url: Option<String>,
}

/// The billing plane as a registrable unit, cheap to clone.
#[derive(Clone)]
pub struct Billing {
    state: web::Data<BillingState>,
}

impl Billing {
    /// Build the billing plane. A present secret key constructs the real
    /// [`billing::StripeClient`]; absent leaves the provider `None` (billing
    /// disabled). Fails only on a malformed secret key (empty), surfaced at
    /// boot.
    pub fn new(
        control: ControlPlane,
        config: BillingConfig,
    ) -> Result<Self, crate::error::CloudError> {
        let base_domain = config
            .base_domain
            .trim_start_matches('.')
            .to_ascii_lowercase();
        let app_public_url = config
            .app_public_url
            .unwrap_or_else(|| format!("https://app.{base_domain}"))
            .trim_end_matches('/')
            .to_string();
        let provider: Option<Arc<dyn BillingProvider>> = match config.stripe_secret_key {
            Some(key) => {
                let client = match config.stripe_base_url {
                    Some(base) => billing::StripeClient::with_base_url(key, base)?,
                    None => billing::StripeClient::new(key)?,
                };
                Some(Arc::new(client))
            }
            None => None,
        };
        let price_to_plan = config
            .plan_prices
            .iter()
            .map(|(plan, price)| (price.clone(), plan.clone()))
            .collect();
        Ok(Self {
            state: web::Data::new(BillingState {
                control,
                provider,
                webhook_secret: config.webhook_secret.unwrap_or_default(),
                plan_to_price: config.plan_prices,
                price_to_plan,
                app_public_url,
                base_domain,
                // Wired by `serve` via `with_managed_keys`; the constructor
                // can't take it without rippling through every test seam, and
                // `Disabled` is a correct no-op default for the webhook
                // reconcile.
                managed: ManagedKeys::Disabled,
            }),
        })
    }

    /// Build with an explicit provider — the test seam, so the lifecycle and
    /// route behavior can run against a scripted [`BillingProvider`] double
    /// and a real control plane without a Stripe key.
    pub fn with_provider(
        control: ControlPlane,
        provider: Option<Arc<dyn BillingProvider>>,
        webhook_secret: impl Into<String>,
        plan_prices: HashMap<String, String>,
        app_public_url: impl Into<String>,
        base_domain: impl Into<String>,
    ) -> Self {
        let base_domain = base_domain
            .into()
            .trim_start_matches('.')
            .to_ascii_lowercase();
        let price_to_plan = plan_prices
            .iter()
            .map(|(plan, price)| (price.clone(), plan.clone()))
            .collect();
        Self {
            state: web::Data::new(BillingState {
                control,
                provider,
                webhook_secret: webhook_secret.into(),
                plan_to_price: plan_prices,
                price_to_plan,
                app_public_url: app_public_url.into().trim_end_matches('/').to_string(),
                base_domain,
                managed: ManagedKeys::Disabled,
            }),
        }
    }

    /// Wire the managed runtime-key handle so the webhook reconciles a tenant's
    /// managed-AI allowance to its plan after a subscription transition commits
    /// (MAI-1). `serve` calls this; tests that don't exercise managed-key
    /// reconciliation leave the `Disabled` default (a no-op). Rebuilds the
    /// shared `web::Data`, so it must be called before the plane is shared with
    /// workers.
    pub fn with_managed_keys(mut self, managed: ManagedKeys) -> Self {
        let state = &*self.state;
        self.state = web::Data::new(BillingState {
            control: state.control.clone(),
            provider: state.provider.clone(),
            webhook_secret: state.webhook_secret.clone(),
            plan_to_price: state.plan_to_price.clone(),
            price_to_plan: state.price_to_plan.clone(),
            app_public_url: state.app_public_url.clone(),
            base_domain: state.base_domain.clone(),
            managed,
        });
        self
    }

    /// Whether Stripe is configured on this deployment (a secret key was
    /// supplied). When false, every billing route degrades to a structured
    /// `billing_not_configured` 503 — the dashboard reads this (surfaced in
    /// the account overview) to disable the portal/checkout actions with an
    /// explanatory note rather than navigating the browser onto a raw 503.
    pub fn is_configured(&self) -> bool {
        self.state.provider.is_some()
    }

    /// The configured billing provider, if any. Threaded into
    /// [`crate::provision::delete_account`] by the active-deletion HTTP route
    /// so it can fire a best-effort Stripe subscription cancel before the
    /// accounts-row CASCADE sweeps the `stripe_subscriptions` pointer (DEL-1).
    pub fn provider(&self) -> Option<&Arc<dyn BillingProvider>> {
        self.state.provider.as_ref()
    }

    /// Register the authenticated tenant-plane billing routes (portal,
    /// checkout). Behind the same `CloudAuth` as the rest of `/api/*`; the
    /// caller wires the auth wrap.
    ///
    /// These are configured INTO `atomic_server::app::api_scope()`, which is
    /// `web::scope("/api")` — so the paths registered here are relative to
    /// that scope (no `/api` prefix), yielding the public URLs
    /// `/api/billing/portal` and `/api/billing/checkout`.
    pub(crate) fn configure_tenant(&self, cfg: &mut web::ServiceConfig) {
        cfg.app_data(self.state.clone())
            .route("/billing/portal", web::get().to(portal))
            .route("/billing/checkout", web::get().to(checkout));
    }

    /// Register the unauthenticated webhook on the app host (guarded so it
    /// 404s on tenant subdomains, like the account plane).
    pub(crate) fn configure_app(&self, cfg: &mut web::ServiceConfig) {
        cfg.service(
            web::resource("/billing/webhook")
                .guard(app_host_guard(self.state.base_domain.clone()))
                .app_data(self.state.clone())
                .route(web::post().to(webhook)),
        );
    }
}

#[derive(Deserialize)]
struct CheckoutQuery {
    plan: String,
}

/// `GET /api/billing/portal` — 302 into the Stripe Customer Portal.
async fn portal(state: web::Data<BillingState>, req: HttpRequest) -> HttpResponse {
    let Some(provider) = state.provider.as_ref() else {
        return billing_not_configured();
    };
    // Billing actions touch the whole account, not a single KB — only an
    // account-scope credential (or a web session) may start them, exactly
    // like `DELETE /api/account` (tenant_plane). A database- or MCP-scoped
    // token is pinned to a KB and gets 403.
    let account_id = match require_account_scope(&req) {
        Ok(id) => id,
        Err(resp) => return resp,
    };

    let customer: Result<Option<String>, _> =
        sqlx::query_scalar("SELECT stripe_customer_id FROM stripe_customers WHERE account_id = $1")
            .bind(&account_id)
            .fetch_optional(state.control.pool())
            .await
            .map(Option::flatten);
    let customer = match customer {
        Ok(Some(c)) => c,
        Ok(None) => {
            return HttpResponse::Conflict().json(serde_json::json!({
                "error": "no_billing_customer",
                "message": "This account has no billing customer yet. Start a checkout first.",
            }));
        }
        Err(e) => {
            tracing::error!(account_id, error = %e, "reading Stripe customer failed");
            return internal_error();
        }
    };

    let return_url = format!("{}/account/billing", state.app_public_url);
    match provider.create_portal_session(&customer, &return_url).await {
        Ok(session) => redirect(&session.url),
        Err(e) => {
            tracing::error!(account_id, error = %e, "creating portal session failed");
            billing_upstream_error()
        }
    }
}

/// `GET /api/billing/checkout?plan=<id>` — 302 into Stripe Checkout for the
/// named paid plan.
async fn checkout(
    state: web::Data<BillingState>,
    req: HttpRequest,
    query: web::Query<CheckoutQuery>,
) -> HttpResponse {
    let Some(provider) = state.provider.as_ref() else {
        return billing_not_configured();
    };
    let account_id = match require_account_scope(&req) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    let Some(price_id) = state.plan_to_price.get(&query.plan) else {
        return HttpResponse::BadRequest().json(serde_json::json!({
            "error": "unknown_plan",
            "message": "No such purchasable plan.",
        }));
    };

    let email: Result<Option<String>, _> =
        sqlx::query_scalar("SELECT email FROM accounts WHERE id = $1")
            .bind(&account_id)
            .fetch_optional(state.control.pool())
            .await;
    let email = match email {
        Ok(Some(e)) => e,
        Ok(None) => return unauthorized(),
        Err(e) => {
            tracing::error!(account_id, error = %e, "reading account email failed");
            return internal_error();
        }
    };

    let subdomain = req
        .extensions()
        .get::<ResolvedTenant>()
        .map(|t| t.subdomain.clone())
        .unwrap_or_default();
    let success_url = format!("{}/account/billing?status=success", state.app_public_url);
    let cancel_url = format!("{}/account/billing?status=cancel", state.app_public_url);
    match provider
        .create_checkout_session(price_id, &email, &subdomain, &success_url, &cancel_url)
        .await
    {
        Ok(session) => redirect(&session.url),
        Err(e) => {
            tracing::error!(account_id, error = %e, "creating checkout session failed");
            billing_upstream_error()
        }
    }
}

/// `POST /billing/webhook` (app host, Stripe-authenticated by signature).
/// Verify, project, apply. Always 200s a verified-but-unhandled event so
/// Stripe stops retrying; a verification failure is a 400 (only a forger
/// sees it).
async fn webhook(
    state: web::Data<BillingState>,
    req: HttpRequest,
    body: web::Bytes,
) -> HttpResponse {
    if state.provider.is_none() || state.webhook_secret.is_empty() {
        return billing_not_configured();
    }
    let signature = req
        .headers()
        .get("stripe-signature")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();

    let event =
        match billing::verify_webhook(&state.webhook_secret, &body, signature, billing::now_unix())
        {
            Ok(event) => event,
            Err(e) => {
                tracing::warn!(error = %e, "rejecting unverified Stripe webhook");
                return HttpResponse::BadRequest().json(serde_json::json!({
                    "error": "invalid_signature",
                }));
            }
        };

    let parsed = match billing::parse_event(&event, &state.price_to_plan) {
        Ok(parsed) => parsed,
        Err(e) => {
            // Verified but unprojectable: log and 400 so the misconfiguration
            // (e.g. an unmapped price) is visible rather than silently
            // acked.
            tracing::error!(error = %e, "verified Stripe event could not be projected");
            return HttpResponse::BadRequest().json(serde_json::json!({
                "error": "unprojectable_event",
            }));
        }
    };

    // Idempotency, atomic with the side effects. Stripe redelivers until it
    // sees a 2xx and does not guarantee at-most-once delivery, so a verbatim
    // replay must collapse to an ack with no repeated money/quota/audit work
    // (plan: "The webhook is the source of truth"). The claim INSERT and every
    // apply write run in ONE transaction so a crash between the claim and the
    // apply's side effects rolls BOTH back — Stripe's retry then re-processes
    // the event rather than seeing a committed-but-uneffected claim and acking
    // it as a permanent no-op (the adversarial finding). The `plan_transitions`
    // audit dedup is preserved: the claim row and the audit rows commit (or
    // abort) together. Only events with a real id participate in the claim; a
    // malformed-but-verified event (no `evt_…` id) still applies inside the
    // transaction (the apply is itself convergent), it just isn't deduped.
    let event_id = event["id"].as_str().unwrap_or_default();
    let event_type = event["type"].as_str().unwrap_or_default();

    let mut tx = match state.control.pool().begin().await {
        Ok(tx) => tx,
        Err(e) => {
            tracing::error!(error = %e, "opening webhook transaction failed");
            return internal_error();
        }
    };

    if !event_id.is_empty() {
        match dunning::claim_webhook_event_on_conn(&mut tx, event_id, event_type).await {
            Ok(true) => {}
            Ok(false) => {
                // Already processed: roll back the (no-op) claim attempt and
                // ack. Dropping the tx without commit rolls back too; the
                // explicit rollback is just clarity.
                let _ = tx.rollback().await;
                tracing::debug!(
                    event_id,
                    event_type,
                    "duplicate Stripe webhook; acking no-op"
                );
                return HttpResponse::Ok().json(serde_json::json!({ "received": true }));
            }
            Err(e) => {
                tracing::error!(error = %e, "claiming Stripe webhook event failed");
                let _ = tx.rollback().await;
                return internal_error();
            }
        }
    }

    let reconcile_account = match apply(&mut tx, parsed).await {
        Ok(account_id) => account_id,
        Err(e) => {
            tracing::error!(error = %e, "applying Stripe webhook failed");
            // Roll the transaction back — the claim and any partial side effects
            // are discarded, so Stripe's retry re-processes the event from a clean
            // slate (the side effects did NOT land). A failed rollback is logged
            // but can't change the 500 we owe Stripe (it will retry regardless).
            if let Err(e) = tx.rollback().await {
                tracing::error!(error = %e, event_id, "rolling back failed webhook transaction failed");
            }
            return internal_error();
        }
    };

    if let Err(e) = tx.commit().await {
        // The claim and side effects are still uncommitted (rolled back by the
        // failed commit), so Stripe's retry re-processes — correct, not a
        // permanent no-op.
        tracing::error!(error = %e, event_id, "committing webhook transaction failed");
        return internal_error();
    }

    // Reconcile the managed-key allowance to the now-committed plan, OUTSIDE the
    // claim+apply transaction — a provider PATCH must never extend or wedge that
    // transaction (MAI-1). Best-effort: a failure is logged inside
    // `reconcile_managed_key_limit` and the next plan transition (or an operator)
    // reconciles it; it must not turn a successfully-applied webhook into a 500
    // that makes Stripe redeliver. `Disabled` managed keys make this a no-op.
    if let Some(account_id) = reconcile_account {
        if let Err(e) =
            dunning::reconcile_managed_key_limit(&state.control, &state.managed, &account_id).await
        {
            tracing::error!(
                error = %e,
                account_id,
                "reconciling managed key limit after subscription transition failed; \
                 the next transition (or an operator) will reconcile it"
            );
        }
    }

    HttpResponse::Ok().json(serde_json::json!({ "received": true }))
}

/// Apply a projected webhook event to the control plane, resolving the
/// account by Stripe customer id (link it first on the upsert path).
///
/// Runs on the webhook's claim+apply transaction (`conn`), so every write here
/// commits or rolls back atomically with the event-id claim.
///
/// Returns the account id whose managed-AI allowance must be reconciled to its
/// (now-updated) plan AFTER the transaction commits — `Some` for the plan-moving
/// subscription arms (upserted → trial/pro allowance; deleted → free allowance),
/// `None` for events that don't change the plan tier (payment failed/succeeded,
/// ignored, or an unresolved customer). The caller runs the reconcile outside
/// this transaction so a provider PATCH never extends or wedges it (MAI-1).
async fn apply(
    conn: &mut sqlx::PgConnection,
    event: WebhookEvent,
) -> Result<Option<String>, crate::error::CloudError> {
    match event {
        WebhookEvent::SubscriptionUpserted(sub) => {
            // Resolve (and, on the very first subscription, ESTABLISH) the
            // account↔customer linkage. A real Stripe checkout's
            // `customer.subscription.created` is the first time cloud learns
            // the `cus_…` id — the redirect path deliberately writes no state
            // (the webhook is the source of truth), so no `stripe_customers`
            // row exists yet. The subscription carries the account's subdomain
            // in its metadata (stamped at checkout via
            // `subscription_data[metadata][subdomain]`); we map it to the
            // account and link the customer before applying. Without this, a
            // genuine first subscription would resolve to no account and be
            // silently dropped — the happy path would never complete. (Plan:
            // "Key events: customer.subscription.{created,updated,deleted}" —
            // we link off the subscription itself rather than handling a
            // separate `checkout.session.completed`, keeping the handled-event
            // set exactly as the plan specifies.)
            let account_id =
                match dunning::account_for_customer_on_conn(conn, &sub.stripe_customer_id).await? {
                    Some(account_id) => Some(account_id),
                    None => link_from_subdomain(conn, &sub).await?,
                };
            match account_id {
                Some(account_id) => {
                    dunning::apply_subscription_event_on_conn(conn, &account_id, &sub).await?;
                    // Plan may have moved (active/trialing on a paid price) —
                    // reconcile the managed-key allowance post-commit.
                    Ok(Some(account_id))
                }
                None => {
                    tracing::warn!(
                        customer = sub.stripe_customer_id,
                        "subscription event for an unknown Stripe customer with no \
                         resolvable subdomain metadata; ignoring"
                    );
                    Ok(None)
                }
            }
        }
        WebhookEvent::SubscriptionDeleted { stripe_customer_id } => {
            match dunning::account_for_customer_on_conn(conn, &stripe_customer_id).await? {
                Some(account_id) => {
                    dunning::apply_subscription_deleted_on_conn(conn, &account_id).await?;
                    // Dropped to free — reconcile the managed-key allowance down
                    // to the free cap post-commit.
                    Ok(Some(account_id))
                }
                None => unknown_customer(&stripe_customer_id).map(|()| None),
            }
        }
        WebhookEvent::PaymentFailed { stripe_customer_id } => {
            match dunning::account_for_customer_on_conn(conn, &stripe_customer_id).await? {
                Some(account_id) => dunning::apply_payment_failed_on_conn(conn, &account_id)
                    .await
                    .map(|()| None),
                None => unknown_customer(&stripe_customer_id).map(|()| None),
            }
        }
        WebhookEvent::PaymentSucceeded { stripe_customer_id } => {
            match dunning::account_for_customer_on_conn(conn, &stripe_customer_id).await? {
                Some(account_id) => dunning::apply_payment_succeeded_on_conn(conn, &account_id)
                    .await
                    .map(|()| None),
                None => unknown_customer(&stripe_customer_id).map(|()| None),
            }
        }
        WebhookEvent::Ignored { event_type } => {
            tracing::debug!(event_type, "ignoring unhandled Stripe event");
            Ok(None)
        }
    }
}

/// Log-and-ignore a billing event whose Stripe customer maps to no known
/// account (a customer created out-of-band) — a verified-but-irrelevant event.
fn unknown_customer(stripe_customer_id: &str) -> Result<(), crate::error::CloudError> {
    tracing::warn!(
        customer = stripe_customer_id,
        "billing event for an unknown Stripe customer; ignoring"
    );
    Ok(())
}

/// Establish the `stripe_customers` linkage for a brand-new subscription by
/// resolving its `metadata.subdomain` to an account, then return that account
/// id. `None` when the subscription carries no subdomain (out-of-band) or the
/// subdomain matches no account (a stale/forged subdomain — the verified
/// signature guarantees the event is from Stripe, but the metadata is
/// caller-supplied at checkout, so a mismatch is logged and ignored rather
/// than trusted). Linking is idempotent ([`dunning::link_stripe_customer_on_conn`]
/// upserts), so a retry after a transient apply failure re-links harmlessly.
///
/// The subdomain lookup and the link both run on the webhook's transaction
/// (`conn`) so they share the claim's atomicity.
async fn link_from_subdomain(
    conn: &mut sqlx::PgConnection,
    sub: &billing::SubscriptionState,
) -> Result<Option<String>, crate::error::CloudError> {
    let Some(subdomain) = sub.subdomain.as_deref() else {
        return Ok(None);
    };
    let account_id: Option<String> =
        sqlx::query_scalar("SELECT id FROM accounts WHERE subdomain = $1")
            .bind(subdomain)
            .fetch_optional(&mut *conn)
            .await
            .map_err(crate::error::CloudError::db(
                "looking up account by subdomain",
            ))?;
    let Some(account_id) = account_id else {
        tracing::warn!(
            customer = sub.stripe_customer_id,
            subdomain,
            "subscription metadata.subdomain matches no account; ignoring"
        );
        return Ok(None);
    };
    dunning::link_stripe_customer_on_conn(conn, &account_id, &sub.stripe_customer_id).await?;
    tracing::info!(
        customer = sub.stripe_customer_id,
        account_id,
        subdomain,
        "linked Stripe customer to account from checkout subscription metadata"
    );
    Ok(Some(account_id))
}

/// Resolve the request's account id, requiring an **account-scope** credential
/// — the billing routes' authorization prologue, mirroring
/// [`crate::tenant_plane`]'s `require_account_scope`. CloudAuth installs the
/// extension on every request it passes; its absence is a composition bug and
/// fails closed (500) rather than guessing an identity. A real-but-KB-pinned
/// credential (database/MCP scope) gets a structured 403.
fn require_account_scope(req: &HttpRequest) -> Result<String, HttpResponse> {
    let extensions = req.extensions();
    let Some(tenant) = extensions.get::<ResolvedTenant>() else {
        tracing::error!(
            path = req.path(),
            "billing route reached without a resolved tenant"
        );
        return Err(internal_error());
    };
    if tenant.principal.scope != TokenScope::Account {
        return Err(account_scope_required());
    }
    Ok(tenant.principal.account_id.clone())
}

fn account_scope_required() -> HttpResponse {
    HttpResponse::Forbidden().json(serde_json::json!({
        "error": "account_scope_required",
        "message": "This action requires an account-scope token or a web session.",
    }))
}

fn redirect(url: &str) -> HttpResponse {
    HttpResponse::Found()
        .insert_header((header::LOCATION, url))
        .finish()
}

fn billing_not_configured() -> HttpResponse {
    HttpResponse::ServiceUnavailable().json(serde_json::json!({
        "error": "billing_not_configured",
        "message": "Billing is not enabled on this deployment.",
    }))
}

fn billing_upstream_error() -> HttpResponse {
    HttpResponse::BadGateway().json(serde_json::json!({
        "error": "billing_upstream_error",
        "message": "The billing provider could not be reached. Try again shortly.",
    }))
}

fn unauthorized() -> HttpResponse {
    HttpResponse::Unauthorized().json(serde_json::json!({ "error": "unauthorized" }))
}

fn internal_error() -> HttpResponse {
    HttpResponse::InternalServerError().json(serde_json::json!({ "error": "internal_error" }))
}

/// App-host guard for the webhook, mirroring the account plane's.
fn app_host_guard(base_domain: String) -> impl Guard {
    guard::fn_guard(move |ctx: &GuardContext<'_>| {
        let head = ctx.head();
        head.headers()
            .get(header::HOST)
            .and_then(|v| v.to_str().ok())
            .or_else(|| head.uri.host())
            .is_some_and(|host| {
                let host = host.split(':').next().unwrap_or("").to_ascii_lowercase();
                host == base_domain
                    || host
                        .strip_prefix("app.")
                        .is_some_and(|rest| rest == base_domain)
            })
    })
}
