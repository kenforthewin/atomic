//! Stripe billing: customer portal, signed webhook, subscription lifecycle,
//! and the dunning state machine (plan: "Observability, quotas, billing" →
//! "Billing"; Decisions log 2026-06-09 "Billing v1 is subscription with
//! included AI credits", 2026-05-25 "Stripe via Customer Portal … Webhook at
//! app.<base>/billing/webhook", "Never auto-delete data for payment
//! failure").
//!
//! # v1 model
//!
//! Subscription with included AI credits. Each plan's monthly price includes
//! a managed-key allowance (`plans.ai_credits_monthly_cents`) that OpenRouter
//! enforces per key — no per-call metering on our side, no usage-based
//! invoicing. Stripe owns the entire payment UI (the Customer Portal: invoices,
//! payment methods, plan changes); cloud only ever redirects into it and
//! reacts to its webhooks.
//!
//! # The provider seam
//!
//! Every Stripe HTTP call goes through the [`BillingProvider`] trait, so the
//! lifecycle logic and the dunning state machine are testable without a real
//! Stripe account (a scripting test double drives them; see the tests). The
//! real implementation is [`StripeClient`]; its request shape is pinned by a
//! wiremock test and its webhook verification by a unit test over a
//! known-secret HMAC fixture.
//!
//! # The webhook is the source of truth
//!
//! Cloud never writes subscription state speculatively from a redirect.
//! `GET /billing/portal` / `GET /billing/checkout` only *start* a Stripe
//! session and 302 the browser into it; the authoritative state change lands
//! later via `POST /billing/webhook` (a single URL on the app host, not
//! per-subdomain — plan), which verifies the Stripe signature and updates
//! the control-plane rows. The events handled:
//! `customer.subscription.{created,updated,deleted}` and
//! `invoice.payment_{succeeded,failed}`.
//!
//! # The dunning state machine (`accounts.billing_state`)
//!
//! Orthogonal to `accounts.status` (which CloudAuth uses to gate
//! provisioning/active). A billing-delinquent account stays `status='active'`
//! but its `billing_state` restricts serving. **Data is never auto-deleted**
//! (plan + decisions log): the worst state retains everything.
//!
//! ```text
//!   payment_failed (Stripe dunning) ─▶ past_due  (full access; grace)
//!                          3 days past_due ─▶ read_only (writes blocked)
//!                         14 days past_due ─▶ suspended (serving blocked)
//!   payment_succeeded / checkout ───────────▶ active (cleared)
//! ```
//!
//! The time-driven `past_due → read_only → suspended` transitions are
//! advanced by [`dunning::advance_dunning`], a reaper-style sweep that reads
//! `past_due_since` and the elapsed thresholds. It takes an explicit `now`
//! so the transitions are testable by manufacturing a past `past_due_since`
//! via SQL — no real waits (the slice-2/5 reaper-test idiom).

pub mod dunning;

use std::time::{SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::error::CloudError;

type HmacSha256 = Hmac<Sha256>;

/// Maximum age of a webhook timestamp, in seconds (Stripe's documented
/// recommendation). Rejecting older signatures bounds the replay window for a
/// leaked-but-stale signed payload.
pub const WEBHOOK_TOLERANCE_SECS: i64 = 300;

/// Default name of the environment variable holding the Stripe secret key
/// (`sk_…`). Like [`crate::MASTER_KEY_ENV`] and
/// [`crate::PROVISIONING_KEY_ENV`], the secret VALUE is only ever read from
/// the environment — `serve` takes the variable NAME on argv, never the key
/// itself, so it can't leak into process listings (`ps`, `/proc/<pid>/cmdline`).
pub const STRIPE_SECRET_KEY_ENV: &str = "ATOMIC_CLOUD_STRIPE_SECRET_KEY";

/// Default name of the environment variable holding the Stripe webhook signing
/// secret (`whsec_…`). Read from the environment by NAME for the same custody
/// reason as [`STRIPE_SECRET_KEY_ENV`].
pub const STRIPE_WEBHOOK_SECRET_ENV: &str = "ATOMIC_CLOUD_STRIPE_WEBHOOK_SECRET";

/// A Stripe Checkout / Customer-Portal session: just the redirect URL cloud
/// 302s the browser to.
#[derive(Debug, Clone)]
pub struct StripeSession {
    pub url: String,
}

/// The subscription fields cloud persists, extracted from a verified webhook
/// event by [`parse_subscription_event`] (and returned by the provider's
/// retrieve calls). Stripe owns the canonical record; this is the projection
/// cloud needs to widen quotas and drive dunning.
#[derive(Debug, Clone)]
pub struct SubscriptionState {
    pub stripe_customer_id: String,
    pub stripe_subscription_id: String,
    /// The plan the subscription's price maps to (resolved from the price's
    /// metadata or the configured price→plan map; see the webhook handler).
    pub plan_id: String,
    /// Stripe's subscription status: `active`, `past_due`, `canceled`, …
    pub status: String,
    pub current_period_start: chrono::DateTime<chrono::Utc>,
    pub current_period_end: chrono::DateTime<chrono::Utc>,
    pub cancel_at_period_end: bool,
}

/// The Stripe operations cloud needs, behind a trait so the billing routes
/// and lifecycle are testable without a real Stripe account. Object-safe
/// (`async_trait`) so it can live behind `Arc<dyn BillingProvider>`.
#[async_trait::async_trait]
pub trait BillingProvider: Send + Sync {
    /// Create (or reuse) a Checkout Session subscribing `customer_email` to
    /// `price_id`, tagging it with the account's `subdomain` for the webhook
    /// to correlate. Returns the session's hosted URL.
    async fn create_checkout_session(
        &self,
        price_id: &str,
        customer_email: &str,
        subdomain: &str,
        success_url: &str,
        cancel_url: &str,
    ) -> Result<StripeSession, CloudError>;

    /// Create a Customer Portal session for an existing Stripe customer and
    /// return its hosted URL. The portal is where the user manages invoices,
    /// payment methods, and plan changes.
    async fn create_portal_session(
        &self,
        stripe_customer_id: &str,
        return_url: &str,
    ) -> Result<StripeSession, CloudError>;
}

/// The real Stripe REST client. Adapted from the parts-bin (commit 4b44c51)
/// to the current idiom: typed [`CloudError`] instead of stringly errors, a
/// bounded error-body slice that never leaks the secret key, and constant-
/// time webhook signature comparison.
#[derive(Clone)]
pub struct StripeClient {
    secret_key: String,
    base_url: String,
    http: reqwest::Client,
}

impl StripeClient {
    /// Production client against `https://api.stripe.com`. The secret key is
    /// rejected empty at construction — a misconfigured billing deployment
    /// should fail at boot, not on the first checkout.
    pub fn new(secret_key: impl Into<String>) -> Result<Self, CloudError> {
        Self::with_base_url(secret_key, "https://api.stripe.com")
    }

    /// Like [`new`](Self::new) with an overridable base URL — the wiremock
    /// request-shape test points it at a local server speaking the same API.
    pub fn with_base_url(
        secret_key: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Result<Self, CloudError> {
        let secret_key = secret_key.into();
        if secret_key.is_empty() {
            return Err(CloudError::InvalidStripeConfig(
                "Stripe secret key is empty".to_string(),
            ));
        }
        Ok(Self {
            secret_key,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            http: reqwest::Client::new(),
        })
    }

    /// POST a form to a Stripe endpoint, mapping a non-success status to a
    /// typed [`CloudError::Stripe`] carrying a bounded slice of the error
    /// body. The secret key rides in the Basic-auth username (Stripe's
    /// convention) and never appears in any error.
    async fn post_form(
        &self,
        path: &str,
        context: &'static str,
        params: &[(&str, &str)],
    ) -> Result<serde_json::Value, CloudError> {
        let resp = self
            .http
            .post(format!("{}{path}", self.base_url))
            .basic_auth(&self.secret_key, None::<&str>)
            .form(params)
            .send()
            .await
            .map_err(|e| CloudError::Stripe {
                context: context.to_string(),
                message: e.to_string(),
            })?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(CloudError::Stripe {
                context: context.to_string(),
                message: format!("HTTP {}: {}", status.as_u16(), bounded(&body)),
            });
        }
        serde_json::from_str(&body).map_err(|e| CloudError::Stripe {
            context: context.to_string(),
            message: format!("unparseable response: {e}"),
        })
    }
}

#[async_trait::async_trait]
impl BillingProvider for StripeClient {
    async fn create_checkout_session(
        &self,
        price_id: &str,
        customer_email: &str,
        subdomain: &str,
        success_url: &str,
        cancel_url: &str,
    ) -> Result<StripeSession, CloudError> {
        let json = self
            .post_form(
                "/v1/checkout/sessions",
                "creating checkout session",
                &[
                    ("mode", "subscription"),
                    ("line_items[0][price]", price_id),
                    ("line_items[0][quantity]", "1"),
                    ("customer_email", customer_email),
                    ("metadata[subdomain]", subdomain),
                    ("subscription_data[metadata][subdomain]", subdomain),
                    ("success_url", success_url),
                    ("cancel_url", cancel_url),
                ],
            )
            .await?;
        session_url(json, "checkout")
    }

    async fn create_portal_session(
        &self,
        stripe_customer_id: &str,
        return_url: &str,
    ) -> Result<StripeSession, CloudError> {
        let json = self
            .post_form(
                "/v1/billing_portal/sessions",
                "creating portal session",
                &[("customer", stripe_customer_id), ("return_url", return_url)],
            )
            .await?;
        session_url(json, "portal")
    }
}

/// Pull the `url` field out of a Stripe session response.
fn session_url(json: serde_json::Value, kind: &'static str) -> Result<StripeSession, CloudError> {
    json["url"]
        .as_str()
        .map(|url| StripeSession {
            url: url.to_string(),
        })
        .ok_or_else(|| CloudError::Stripe {
            context: format!("reading {kind} session URL"),
            message: "no `url` in Stripe response".to_string(),
        })
}

/// Bound an error body to a sane length for logs/messages.
fn bounded(s: &str) -> String {
    const MAX: usize = 500;
    if s.len() <= MAX {
        s.to_string()
    } else {
        format!("{}…", &s[..MAX])
    }
}

/// Verify a Stripe webhook signature and parse the event payload (plan:
/// "Verifies Stripe signature"; Stripe's documented scheme:
/// <https://docs.stripe.com/webhooks/signatures>).
///
/// The `Stripe-Signature` header is `t=<unix>,v1=<hex-hmac>[,v1=<hex-hmac>…]`.
/// The signed payload is `"{t}.{body}"`, HMAC-SHA256'd under the endpoint's
/// signing secret. This verifies, in order: the header parses and carries a
/// timestamp + at least one `v1` signature; the timestamp is within
/// [`WEBHOOK_TOLERANCE_SECS`] of `now` (replay defense); and at least one
/// provided signature equals the expected one under **constant-time**
/// comparison (`Mac::verify_slice`, not `==` — a timing oracle on the MAC
/// would let an attacker forge it byte by byte). Only then is the body
/// parsed as JSON.
///
/// `now_unix` is injected so the tolerance check is testable with a
/// manufactured timestamp; production passes the wall clock.
pub fn verify_webhook(
    signing_secret: &str,
    payload: &[u8],
    signature_header: &str,
    now_unix: i64,
) -> Result<serde_json::Value, CloudError> {
    if signing_secret.is_empty() {
        return Err(CloudError::InvalidStripeConfig(
            "Stripe webhook signing secret is empty".to_string(),
        ));
    }

    let mut timestamp: Option<i64> = None;
    let mut signatures: Vec<&str> = Vec::new();
    for part in signature_header.split(',') {
        match part.split_once('=') {
            Some(("t", t)) => timestamp = t.parse().ok(),
            Some(("v1", sig)) => signatures.push(sig),
            _ => {}
        }
    }

    let timestamp = timestamp.ok_or_else(|| {
        CloudError::WebhookVerification("missing or invalid timestamp".to_string())
    })?;
    if signatures.is_empty() {
        return Err(CloudError::WebhookVerification(
            "no v1 signature in header".to_string(),
        ));
    }
    if (now_unix - timestamp).abs() > WEBHOOK_TOLERANCE_SECS {
        return Err(CloudError::WebhookVerification(
            "timestamp outside tolerance window".to_string(),
        ));
    }

    // Expected MAC over "{t}.{body}". The payload bytes are appended raw —
    // they need not be valid UTF-8, and Stripe signs the exact bytes.
    let mut signed = Vec::with_capacity(payload.len() + 16);
    signed.extend_from_slice(timestamp.to_string().as_bytes());
    signed.push(b'.');
    signed.extend_from_slice(payload);

    let matched = signatures.iter().any(|sig| {
        let Ok(expected_bytes) = data_encoding::HEXLOWER_PERMISSIVE.decode(sig.as_bytes()) else {
            return false;
        };
        // Fresh MAC per candidate: `verify_slice` consumes it, and
        // constant-time comparison is the whole point.
        let mut mac = HmacSha256::new_from_slice(signing_secret.as_bytes())
            .expect("HMAC accepts any key length");
        mac.update(&signed);
        mac.verify_slice(&expected_bytes).is_ok()
    });
    if !matched {
        return Err(CloudError::WebhookVerification(
            "no signature matched the signing secret".to_string(),
        ));
    }

    serde_json::from_slice(payload).map_err(|e| {
        CloudError::WebhookVerification(format!("verified signature but unparseable body: {e}"))
    })
}

/// A parsed, verified Stripe webhook event reduced to the cases cloud acts
/// on (plan: "Key events"). Everything else is [`WebhookEvent::Ignored`].
#[derive(Debug, Clone)]
pub enum WebhookEvent {
    /// `customer.subscription.{created,updated}` — persist + apply the
    /// projection (plan changes, past_due from Stripe's own status).
    SubscriptionUpserted(SubscriptionState),
    /// `customer.subscription.deleted` — drop to free, retain data.
    SubscriptionDeleted { stripe_customer_id: String },
    /// `invoice.payment_failed` — enter past_due.
    PaymentFailed { stripe_customer_id: String },
    /// `invoice.payment_succeeded` — clear dunning.
    PaymentSucceeded { stripe_customer_id: String },
    /// An event type cloud doesn't act on (acknowledged with 200 so Stripe
    /// stops retrying).
    Ignored { event_type: String },
}

/// Project a verified event JSON into the [`WebhookEvent`] cloud acts on.
///
/// `price_to_plan` maps a Stripe price id to a cloud plan id — the
/// subscription's price is the authoritative plan signal (a price's
/// `metadata.plan_id`, if present, takes precedence so test fixtures and
/// out-of-band prices resolve without the map). An unmappable price is an
/// error rather than a silent free-tier grant: persisting a subscription
/// under the wrong plan would widen or narrow quotas incorrectly.
pub fn parse_event(
    event: &serde_json::Value,
    price_to_plan: &std::collections::HashMap<String, String>,
) -> Result<WebhookEvent, CloudError> {
    let event_type = event["type"].as_str().unwrap_or_default().to_string();
    let object = &event["data"]["object"];

    match event_type.as_str() {
        "customer.subscription.created" | "customer.subscription.updated" => Ok(
            WebhookEvent::SubscriptionUpserted(parse_subscription(object, price_to_plan)?),
        ),
        "customer.subscription.deleted" => Ok(WebhookEvent::SubscriptionDeleted {
            stripe_customer_id: customer_id(object)?,
        }),
        "invoice.payment_failed" => Ok(WebhookEvent::PaymentFailed {
            stripe_customer_id: customer_id(object)?,
        }),
        "invoice.payment_succeeded" => Ok(WebhookEvent::PaymentSucceeded {
            stripe_customer_id: customer_id(object)?,
        }),
        _ => Ok(WebhookEvent::Ignored { event_type }),
    }
}

/// Extract a `customer` id from an event object (`"cus_…"`), where Stripe
/// represents it as a bare string id.
fn customer_id(object: &serde_json::Value) -> Result<String, CloudError> {
    object["customer"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| {
            CloudError::WebhookVerification("event object has no customer id".to_string())
        })
}

/// Project a Stripe subscription object into [`SubscriptionState`]. The plan
/// is resolved from the first line item's price: its `metadata.plan_id` if
/// present, else `price_to_plan[price_id]`.
fn parse_subscription(
    sub: &serde_json::Value,
    price_to_plan: &std::collections::HashMap<String, String>,
) -> Result<SubscriptionState, CloudError> {
    let err = |msg: &str| CloudError::WebhookVerification(format!("subscription event: {msg}"));

    let stripe_subscription_id = sub["id"].as_str().ok_or_else(|| err("no id"))?.to_string();
    let stripe_customer_id = customer_id(sub)?;
    let status = sub["status"]
        .as_str()
        .ok_or_else(|| err("no status"))?
        .to_string();
    let cancel_at_period_end = sub["cancel_at_period_end"].as_bool().unwrap_or(false);
    let current_period_start =
        parse_epoch(&sub["current_period_start"]).ok_or_else(|| err("no current_period_start"))?;
    let current_period_end =
        parse_epoch(&sub["current_period_end"]).ok_or_else(|| err("no current_period_end"))?;

    // First line item's price → plan.
    let price = &sub["items"]["data"][0]["price"];
    let plan_id = price["metadata"]["plan_id"]
        .as_str()
        .map(str::to_string)
        .or_else(|| {
            price["id"]
                .as_str()
                .and_then(|id| price_to_plan.get(id).cloned())
        })
        .ok_or_else(|| {
            err("could not resolve a plan from the subscription's price (no metadata.plan_id and no price→plan mapping)")
        })?;

    Ok(SubscriptionState {
        stripe_customer_id,
        stripe_subscription_id,
        plan_id,
        status,
        current_period_start,
        current_period_end,
        cancel_at_period_end,
    })
}

/// Parse a Stripe Unix-epoch field (seconds) into a UTC datetime.
fn parse_epoch(value: &serde_json::Value) -> Option<chrono::DateTime<chrono::Utc>> {
    value
        .as_i64()
        .and_then(|secs| chrono::DateTime::from_timestamp(secs, 0))
}

/// The current Unix time in seconds, for [`verify_webhook`]'s production
/// call site.
pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a valid `Stripe-Signature` header for `payload` under
    /// `secret` at time `t` — the fixture the verification tests sign with.
    fn sign(secret: &str, payload: &[u8], t: i64) -> String {
        let mut signed = Vec::new();
        signed.extend_from_slice(t.to_string().as_bytes());
        signed.push(b'.');
        signed.extend_from_slice(payload);
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(&signed);
        let hex = data_encoding::HEXLOWER.encode(&mac.finalize().into_bytes());
        format!("t={t},v1={hex}")
    }

    const SECRET: &str = "whsec_test_secret";

    #[test]
    fn verify_accepts_a_correctly_signed_payload() {
        let payload = br#"{"id":"evt_1","type":"invoice.payment_failed"}"#;
        let t = 1_700_000_000;
        let header = sign(SECRET, payload, t);
        let event = verify_webhook(SECRET, payload, &header, t).expect("valid signature");
        assert_eq!(event["id"], "evt_1");
        assert_eq!(event["type"], "invoice.payment_failed");
    }

    #[test]
    fn verify_rejects_a_tampered_payload() {
        let payload = br#"{"amount":100}"#;
        let t = 1_700_000_000;
        let header = sign(SECRET, payload, t);
        // The signature was computed over the original bytes; a different
        // body must not verify against it.
        let tampered = br#"{"amount":999}"#;
        let err = verify_webhook(SECRET, tampered, &header, t).expect_err("tampered body");
        assert!(matches!(err, CloudError::WebhookVerification(_)));
    }

    #[test]
    fn verify_rejects_wrong_secret() {
        let payload = br#"{"id":"evt_2"}"#;
        let t = 1_700_000_000;
        let header = sign(SECRET, payload, t);
        let err = verify_webhook("whsec_other", payload, &header, t).expect_err("wrong secret");
        assert!(matches!(err, CloudError::WebhookVerification(_)));
    }

    #[test]
    fn verify_rejects_a_stale_timestamp() {
        let payload = br#"{"id":"evt_3"}"#;
        let signed_at = 1_700_000_000;
        let header = sign(SECRET, payload, signed_at);
        // "now" is more than the tolerance past the signed timestamp.
        let now = signed_at + WEBHOOK_TOLERANCE_SECS + 1;
        let err = verify_webhook(SECRET, payload, &header, now).expect_err("stale");
        assert!(matches!(err, CloudError::WebhookVerification(_)));
        // Exactly at the boundary still verifies.
        let header = sign(SECRET, payload, now - WEBHOOK_TOLERANCE_SECS);
        assert!(verify_webhook(SECRET, payload, &header, now).is_ok());
    }

    #[test]
    fn verify_rejects_a_malformed_header() {
        let payload = br#"{}"#;
        for bad in ["", "v1=abc", "t=123", "garbage", "t=notanumber,v1=ff"] {
            let err = verify_webhook(SECRET, payload, bad, 123).expect_err(bad);
            assert!(matches!(err, CloudError::WebhookVerification(_)), "{bad}");
        }
    }

    #[test]
    fn verify_picks_the_matching_signature_among_several() {
        // Stripe can roll secrets and send multiple v1 signatures; matching
        // any one is sufficient.
        let payload = br#"{"id":"evt_4"}"#;
        let t = 1_700_000_000;
        let good = sign(SECRET, payload, t);
        let good_v1 = good.rsplit("v1=").next().unwrap();
        let header = format!("t={t},v1=deadbeef,v1={good_v1}");
        assert!(verify_webhook(SECRET, payload, &header, t).is_ok());
    }

    #[test]
    fn empty_secret_is_config_error() {
        let err = verify_webhook("", b"{}", "t=1,v1=ff", 1).expect_err("empty secret");
        assert!(matches!(err, CloudError::InvalidStripeConfig(_)));
    }

    #[test]
    fn parse_event_projects_the_acted_on_types() {
        use std::collections::HashMap;
        let mut map = HashMap::new();
        map.insert("price_pro".to_string(), "pro".to_string());

        // Subscription with the plan in the price metadata (no map needed).
        let sub = serde_json::json!({
            "type": "customer.subscription.created",
            "data": { "object": {
                "id": "sub_1",
                "customer": "cus_1",
                "status": "active",
                "cancel_at_period_end": false,
                "current_period_start": 1_700_000_000_i64,
                "current_period_end": 1_702_592_000_i64,
                "items": { "data": [ { "price": {
                    "id": "price_pro",
                    "metadata": { "plan_id": "pro" }
                } } ] }
            }}
        });
        match parse_event(&sub, &map).unwrap() {
            WebhookEvent::SubscriptionUpserted(s) => {
                assert_eq!(s.plan_id, "pro");
                assert_eq!(s.status, "active");
                assert_eq!(s.stripe_customer_id, "cus_1");
            }
            other => panic!("expected upsert, got {other:?}"),
        }

        // Price → plan resolved through the map when metadata is absent.
        let sub_via_map = serde_json::json!({
            "type": "customer.subscription.updated",
            "data": { "object": {
                "id": "sub_2", "customer": "cus_2", "status": "past_due",
                "current_period_start": 1, "current_period_end": 2,
                "items": { "data": [ { "price": { "id": "price_pro", "metadata": {} } } ] }
            }}
        });
        match parse_event(&sub_via_map, &map).unwrap() {
            WebhookEvent::SubscriptionUpserted(s) => assert_eq!(s.plan_id, "pro"),
            other => panic!("expected upsert, got {other:?}"),
        }

        // An unmappable price is an error, not a silent free grant.
        let sub_bad = serde_json::json!({
            "type": "customer.subscription.created",
            "data": { "object": {
                "id": "sub_3", "customer": "cus_3", "status": "active",
                "current_period_start": 1, "current_period_end": 2,
                "items": { "data": [ { "price": { "id": "price_unknown", "metadata": {} } } ] }
            }}
        });
        assert!(matches!(
            parse_event(&sub_bad, &map),
            Err(CloudError::WebhookVerification(_))
        ));

        for (etype, ctor) in [
            ("customer.subscription.deleted", "deleted"),
            ("invoice.payment_failed", "failed"),
            ("invoice.payment_succeeded", "succeeded"),
        ] {
            let ev = serde_json::json!({
                "type": etype,
                "data": { "object": { "customer": "cus_9" } }
            });
            let parsed = parse_event(&ev, &map).unwrap();
            match (ctor, parsed) {
                ("deleted", WebhookEvent::SubscriptionDeleted { stripe_customer_id })
                | ("failed", WebhookEvent::PaymentFailed { stripe_customer_id })
                | ("succeeded", WebhookEvent::PaymentSucceeded { stripe_customer_id }) => {
                    assert_eq!(stripe_customer_id, "cus_9");
                }
                (_, other) => panic!("unexpected projection for {etype}: {other:?}"),
            }
        }

        // Unhandled type acknowledged, not errored.
        let other = serde_json::json!({ "type": "charge.refunded", "data": { "object": {} } });
        assert!(matches!(
            parse_event(&other, &map).unwrap(),
            WebhookEvent::Ignored { .. }
        ));
    }

    #[test]
    fn stripe_client_rejects_empty_key() {
        assert!(matches!(
            StripeClient::new(""),
            Err(CloudError::InvalidStripeConfig(_))
        ));
        assert!(StripeClient::new("sk_test_x").is_ok());
    }
}
