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
            }),
        }
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

    let return_url = format!("{}/billing", state.app_public_url);
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
    let success_url = format!("{}/billing?status=success", state.app_public_url);
    let cancel_url = format!("{}/billing?status=cancel", state.app_public_url);
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

    // Idempotency: claim the event id before applying any side effect. Stripe
    // redelivers until it sees a 2xx and does not guarantee at-most-once
    // delivery, so a verbatim replay must collapse to an ack with no repeated
    // money/quota/audit work (plan: "The webhook is the source of truth").
    // Only events with a real id participate; a malformed-but-verified event
    // (no `evt_…` id) falls through to apply, which is itself convergent.
    let event_id = event["id"].as_str().unwrap_or_default();
    let event_type = event["type"].as_str().unwrap_or_default();
    if !event_id.is_empty() {
        match dunning::claim_webhook_event(&state.control, event_id, event_type).await {
            Ok(true) => {}
            Ok(false) => {
                tracing::debug!(
                    event_id,
                    event_type,
                    "duplicate Stripe webhook; acking no-op"
                );
                return HttpResponse::Ok().json(serde_json::json!({ "received": true }));
            }
            Err(e) => {
                tracing::error!(error = %e, "claiming Stripe webhook event failed");
                return internal_error();
            }
        }
    }

    if let Err(e) = apply(&state, parsed).await {
        tracing::error!(error = %e, "applying Stripe webhook failed");
        // Release the claim so Stripe's retry re-processes this event rather
        // than being deduped into a permanent no-op — the apply failed, so
        // the side effects did NOT land. A failed release is logged but can't
        // change the 500 we owe Stripe (it will retry regardless).
        if !event_id.is_empty() {
            if let Err(e) = dunning::release_webhook_event(&state.control, event_id).await {
                tracing::error!(error = %e, event_id, "releasing failed webhook claim failed");
            }
        }
        return internal_error();
    }
    HttpResponse::Ok().json(serde_json::json!({ "received": true }))
}

/// Apply a projected webhook event to the control plane, resolving the
/// account by Stripe customer id (link it first on the upsert path).
async fn apply(state: &BillingState, event: WebhookEvent) -> Result<(), crate::error::CloudError> {
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
                match dunning::account_for_customer(&state.control, &sub.stripe_customer_id).await?
                {
                    Some(account_id) => Some(account_id),
                    None => link_from_subdomain(state, &sub).await?,
                };
            match account_id {
                Some(account_id) => {
                    dunning::apply_subscription_event(&state.control, &account_id, &sub).await
                }
                None => {
                    tracing::warn!(
                        customer = sub.stripe_customer_id,
                        "subscription event for an unknown Stripe customer with no \
                         resolvable subdomain metadata; ignoring"
                    );
                    Ok(())
                }
            }
        }
        WebhookEvent::SubscriptionDeleted { stripe_customer_id } => {
            apply_by_customer(state, &stripe_customer_id, |c, id| {
                Box::pin(dunning::apply_subscription_deleted(c, id))
            })
            .await
        }
        WebhookEvent::PaymentFailed { stripe_customer_id } => {
            apply_by_customer(state, &stripe_customer_id, |c, id| {
                Box::pin(dunning::apply_payment_failed(c, id))
            })
            .await
        }
        WebhookEvent::PaymentSucceeded { stripe_customer_id } => {
            apply_by_customer(state, &stripe_customer_id, |c, id| {
                Box::pin(dunning::apply_payment_succeeded(c, id))
            })
            .await
        }
        WebhookEvent::Ignored { event_type } => {
            tracing::debug!(event_type, "ignoring unhandled Stripe event");
            Ok(())
        }
    }
}

/// Establish the `stripe_customers` linkage for a brand-new subscription by
/// resolving its `metadata.subdomain` to an account, then return that account
/// id. `None` when the subscription carries no subdomain (out-of-band) or the
/// subdomain matches no account (a stale/forged subdomain — the verified
/// signature guarantees the event is from Stripe, but the metadata is
/// caller-supplied at checkout, so a mismatch is logged and ignored rather
/// than trusted). Linking is idempotent ([`dunning::link_stripe_customer`]
/// upserts), so a retry after a transient apply failure re-links harmlessly.
async fn link_from_subdomain(
    state: &BillingState,
    sub: &billing::SubscriptionState,
) -> Result<Option<String>, crate::error::CloudError> {
    let Some(subdomain) = sub.subdomain.as_deref() else {
        return Ok(None);
    };
    let Some(account_id) = state.control.account_id_by_subdomain(subdomain).await? else {
        tracing::warn!(
            customer = sub.stripe_customer_id,
            subdomain,
            "subscription metadata.subdomain matches no account; ignoring"
        );
        return Ok(None);
    };
    dunning::link_stripe_customer(&state.control, &account_id, &sub.stripe_customer_id).await?;
    tracing::info!(
        customer = sub.stripe_customer_id,
        account_id,
        subdomain,
        "linked Stripe customer to account from checkout subscription metadata"
    );
    Ok(Some(account_id))
}

/// Resolve the account for a Stripe customer and run `f` against it; a no-op
/// (logged) when the customer maps to no known account.
async fn apply_by_customer<F>(
    state: &BillingState,
    stripe_customer_id: &str,
    f: F,
) -> Result<(), crate::error::CloudError>
where
    F: for<'a> FnOnce(
        &'a ControlPlane,
        &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<(), crate::error::CloudError>> + 'a>,
    >,
{
    match dunning::account_for_customer(&state.control, stripe_customer_id).await? {
        Some(account_id) => f(&state.control, &account_id).await,
        None => {
            tracing::warn!(
                customer = stripe_customer_id,
                "billing event for an unknown Stripe customer; ignoring"
            );
            Ok(())
        }
    }
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
