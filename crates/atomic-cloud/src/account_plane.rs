//! The account plane: public (non-tenant) routes served on the **app host**
//! (plan: "Subdomain rules" — `app.atomic.cloud` for the marketing site /
//! signup; "Provisioning lifecycle" → "Signup" steps 1–2).
//!
//! # The host split
//!
//! The cloud composition serves two disjoint planes, split on the request
//! `Host`:
//!
//! - **Tenant subdomains** (`<slug>.<base>`) — atomic-server's API under
//!   [`CloudAuth`](crate::auth::CloudAuth). Unchanged by this module.
//! - **The app host** — the bare base domain (`atomic.cloud`) *and*
//!   `app.<base>` (`app.atomic.cloud`); both are accepted so the apex and
//!   the canonical `app.` name behave identically and a bare-domain visitor
//!   isn't met with a 404 (whichever one marketing doesn't use redirects to
//!   the other at the DNS/CDN layer, not here). No CloudAuth, no tenant
//!   state — these routes serve people who don't have an account yet.
//!
//! Both directions fail closed, each by its own mechanism:
//!
//! - Account-plane routes carry a [`Guard`] that matches only the app host,
//!   so on a tenant subdomain they don't exist (404 before any handler).
//! - Tenant routes need no guard: `CloudAuth` already 404s the bare base
//!   domain (no subdomain label to extract — see `subdomain_from_host` in
//!   `auth.rs`) and `app` is on the static blocklist
//!   ([`crate::reserved_subdomains`]) so its account lookup can never
//!   resolve. The e2e suite pins both directions against the live
//!   composition.
//!
//! # Routes
//!
//! - `POST /signup/request-link` `{email, subdomain}` — validate, issue a
//!   signup magic link, email it. Bad email/slug are honest 400s (they're
//!   client-fixable and leak nothing the public DNS namespace doesn't
//!   already leak); everything after validation — including email-send
//!   failure — answers the same neutral 200, because differential responses
//!   on the email axis are exactly the enumeration oracle the login route
//!   must not have, and the two routes should behave identically. One
//!   deliberate carve-out in the availability check: a subdomain whose only
//!   claim is a `'provisioning'` account for the *same* (lowercased) email
//!   is **not** "taken" — that claim is the requester's own crashed signup,
//!   and a fresh link is exactly how they resume it (`provision_account` is
//!   idempotent for the same email + subdomain).
//! - `POST /login/request-link` `{email}` — if an active account matches,
//!   email a login link. The response is byte-identical whether or not the
//!   account exists (no email enumeration; e2e-pinned) — **and arrives in
//!   uniform time**: both branches return right after the same synchronous
//!   work (the rate-limit charges plus one indexed account lookup). The
//!   issue+send on the exists branch is `tokio::spawn`ed fire-and-forget,
//!   because awaiting it (a DB insert plus an outbound Mailgun POST,
//!   hundreds of ms) would make response latency the very enumeration
//!   oracle the identical bodies exist to close. Spawn-side failures are
//!   logged, never reflected in the response — which is already the policy
//!   for send failures on both routes. The residual signal is the lookup
//!   itself: one indexed SELECT on both branches, far below network jitter.
//! - `GET /signup/complete?token=…` — consume the signup link and provision
//!   the account synchronously (plan: "Signup" steps 3–12, minus the
//!   deferred 7–8; step 9's managed key runs per the configured
//!   [`ManagedKeys`] mode), then establish a session and 302 to the new
//!   tenant. See "Completion semantics" below.
//! - `GET /login/complete?token=…` — consume the login link, find the
//!   active account by the link's email, establish a session, 302 to the
//!   account's subdomain.
//!
//! # Completion semantics
//!
//! The completion routes handle a **single-use credential**, so the order
//! of refusals is load-bearing. Signup completion admits in four steps —
//! shape, peek, permit, consume — each refusing as early (and as cheaply)
//! as the refusal can be made sound:
//!
//! - **Syntactic shape first** ([`magic_link_token_shape_ok`]): a token
//!   that can't possibly be real is refused before any database work.
//! - **Then a read-only eligibility peek** ([`peek_magic_link`]): a dead
//!   token (unknown, expired, spent, wrong purpose) is refused before the
//!   provision permit is touched. Together these keep junk requests from
//!   starving the semaphore — only plausibly-live tokens ever contend for a
//!   permit. The handler never awaits anything unbounded while
//!   unauthenticated; the peek is one indexed SELECT.
//! - **Capacity is checked before the token is consumed.** Synchronous
//!   provisioning is capped by a process-wide semaphore
//!   ([`AccountPlaneConfig::max_concurrent_provisions`]; plan: 4–8). A
//!   saturated process answers a structured 503 + `Retry-After` *without*
//!   touching the link — consume-then-refuse would burn the user's only
//!   credential on a condition that retrying cures. `try_acquire`, never
//!   wait.
//! - **Consumption is atomic and purpose-pinned** (one UPDATE; see
//!   [`crate::magic_links::consume_magic_link`]). Expired, reused,
//!   wrong-purpose, and unknown tokens are all the same honest
//!   `invalid_link` 400 — distinguishing WHY would hand out an oracle over
//!   the magic-link table. A double click is therefore one provision and
//!   one clean 400, never two provisions.
//! - **`SubdomainTaken` at consume time is a 409** telling the user to
//!   restart signup with a different name. The consumed token stays spent —
//!   un-consuming would reopen the replay window, and a fresh request-link
//!   is cheap and rate-limited, so the honest trade-off is "this click is
//!   spent, ask again".
//! - **A provision failure after consumption** leaves the accounts row in
//!   `status='provisioning'`; the response is a structured 500 advising
//!   retry-later. The safety-net reaper ([`crate::reaper`]) is what retries
//!   or rolls back such rows — and the request-link route's
//!   same-email carve-out (above) is what lets the user resume sooner by
//!   simply asking for a fresh link.
//!
//! On success the route creates a web session ([`crate::tokens`]) and sets
//! the [`SESSION_COOKIE`](crate::auth::SESSION_COOKIE) with
//! `Domain=.<base>; Secure; HttpOnly; SameSite=Lax; Max-Age=<session TTL>`
//! (plan: "Web sessions" — the leading dot makes one session work across
//! every subdomain; the account-scoped verification in `CloudAuth` is what
//! keeps that from crossing tenants). `Secure` is on by default and must
//! stay on in production. The one exception is a local/headless dev box
//! served over plain HTTP on a non-`localhost` host (e.g. reached over
//! Tailscale): browsers only exempt `localhost`/`*.localhost` from the
//! `Secure`-requires-HTTPS rule, so on such a host a `Secure` cookie is
//! silently dropped and the dashboard can never authenticate. The
//! [`AccountPlaneConfig::cookie_secure`] flag (CLI:
//! `--dangerously-insecure-cookies`, default off) drops `Secure` for exactly
//! that case and warns loudly at boot; never use it in production.
//! The redirect targets `<slug>.<base>` with the scheme/port of
//! [`AccountPlaneConfig::app_public_url`] — production defaults yield
//! `https://<slug>.<base>/`; dev setups keep their explicit port.
//!
//! # Anti-abuse limits (plan: "Quotas" table)
//!
//! - Request-link routes (signup + login combined): 5 per client IP per
//!   hour. The plan's table lists only "signup attempts per IP"; covering
//!   login with the same shared bucket is this implementation's choice —
//!   login request-link is the same probe surface (an enumerating client
//!   doesn't care which route answers), so it gets the same per-IP cost,
//!   and a shared bucket means switching routes doesn't mint fresh
//!   allowance. See [`crate::rate_limit`].
//! - Magic-link requests (signup + login combined): 3 per email per hour.
//!
//! Per-pod in-memory sliding windows ([`crate::rate_limit`]); refusals are
//! 429 with `Retry-After`. On both routes the IP limit is charged *before
//! everything else* — before validation (a validation-failing request is
//! still an attempt) and before any lookup. The email limit runs after
//! validation, keyed on the (lowercased) email, and is always charged
//! before the account lookup so the limiter's behavior cannot become an
//! enumeration side channel either.
//!
//! # Response headers
//!
//! Every account-plane response carries `Referrer-Policy: no-referrer`
//! (a `DefaultHeaders` wrap on both scopes). Completion URLs carry live
//! single-use credentials in their query string; without the policy, the
//! post-completion redirect (and any error page a browser renders) could
//! leak the token to the next origin via the `Referer` header.
//!
//! # Client IP derivation
//!
//! By default the connection's peer address is the client IP. That is
//! spoof-proof but wrong behind a reverse proxy: every request appears to
//! come from the proxy, so all clients share one bucket and a single abuser
//! exhausts signups for everyone. `trust_proxy_header` flips to reading
//! `X-Forwarded-For` — the **rightmost** entry, the one appended by the
//! trusted proxy itself; earlier entries are client-controlled. The
//! trade-off cuts both ways and is the operator's call: enabling the flag
//! without a header-sanitizing proxy in front lets clients spoof arbitrary
//! IPs and sidestep the per-IP limit entirely; leaving it off behind a
//! proxy collapses the limit to per-proxy granularity.

use std::sync::Arc;

use actix_web::cookie::{Cookie, SameSite};
use actix_web::guard::{Guard, GuardContext};
use actix_web::http::header;
use actix_web::middleware::DefaultHeaders;
use actix_web::{guard, web, HttpRequest, HttpResponse};
use serde::Deserialize;
use tokio::sync::Semaphore;

use crate::auth::SESSION_COOKIE;
use crate::billing::dunning::{start_trial, DEFAULT_TRIAL_DAYS};
use crate::control_plane::ControlPlane;
use crate::email::EmailSender;
use crate::error::CloudError;
use crate::magic_links::{
    consume_magic_link, issue_magic_link, magic_link_token_shape_ok, peek_magic_link,
    MagicLinkPurpose, MAGIC_LINK_TTL,
};
use crate::managed_keys::ManagedKeys;
use crate::provision::{
    email_format_ok, provision_account, subdomain_format_ok, ClusterConfig, NewAccount,
};
use crate::rate_limit::SlidingWindow;
use crate::reserved_subdomains;
use crate::tokens::create_session;

/// Web-session lifetime, which is also the cookie's `Max-Age`. Thirty days
/// balances "don't make magic-link users log in weekly" against bounded
/// exposure for a leaked cookie; sessions are server-stored, so revocation
/// (account deletion, future "sign out everywhere") is immediate regardless.
pub const SESSION_TTL: std::time::Duration = std::time::Duration::from_secs(30 * 24 * 60 * 60);

/// Default cap on concurrent in-flight provisions per process (plan:
/// "Signup" — synchronous, capped at 4–8).
pub const DEFAULT_MAX_CONCURRENT_PROVISIONS: usize = 4;

/// `Retry-After` seconds suggested when the provision semaphore is
/// saturated. The happy path is ~2–5 s per provision, so a short backoff is
/// honest.
const PROVISION_BUSY_RETRY_AFTER_SECS: u64 = 10;

/// The plan's anti-abuse rate-limit numbers ("Quotas" table), with the
/// windows exposed so tests can shrink them instead of sleeping through
/// real hours. Production callers use `Default`.
#[derive(Debug, Clone)]
pub struct RateLimits {
    /// Request-link admissions (signup + login combined, one shared bucket)
    /// per client IP per window. See the module docs for why login shares
    /// the plan's signup number.
    pub links_per_ip: u32,
    pub ip_window: std::time::Duration,
    /// Magic-link admissions (signup + login combined) per email per window.
    pub links_per_email: u32,
    pub email_window: std::time::Duration,
}

impl Default for RateLimits {
    fn default() -> Self {
        Self {
            links_per_ip: 5,
            ip_window: std::time::Duration::from_secs(3600),
            links_per_email: 3,
            email_window: std::time::Duration::from_secs(3600),
        }
    }
}

/// Configuration for [`AccountPlane::new`].
#[derive(Debug, Clone)]
pub struct AccountPlaneConfig {
    /// Base domain accounts are hosted under; the app host is this name
    /// itself plus `app.<base>`. Normalized like
    /// [`CloudAuth::new`](crate::auth::CloudAuth::new) (lowercase, leading
    /// dot tolerated).
    pub base_domain: String,
    /// Public origin used when building emailed links, e.g.
    /// `https://app.atomic.cloud`. `None` derives exactly that —
    /// `https://app.<base_domain>` — which is right for production;
    /// set it explicitly for local/dev deployments with ports or http.
    pub app_public_url: Option<String>,
    /// Derive the client IP from `X-Forwarded-For` (rightmost entry)
    /// instead of the connection peer address. See the module docs for the
    /// spoofing trade-off in both directions.
    pub trust_proxy_header: bool,
    pub rate_limits: RateLimits,
    /// Cap on concurrent in-flight provisions in this process (the
    /// signup-completion semaphore; plan says 4–8). Values below 1 are
    /// clamped to 1 — a zero-permit semaphore would refuse every signup
    /// forever.
    pub max_concurrent_provisions: usize,
    /// Web-session lifetime and cookie `Max-Age`. Production callers use
    /// the [`SESSION_TTL`] default; tests shrink it.
    pub session_ttl: std::time::Duration,
    /// Whether the session cookie carries the `Secure` attribute. Defaults to
    /// `true` and MUST stay `true` in production — a `Secure` cookie is only
    /// sent over HTTPS. Set `false` ONLY for a local/headless dev deployment
    /// served over plain HTTP on a non-`localhost` host (e.g. reached over
    /// Tailscale), where browsers would otherwise silently drop the session
    /// cookie and the dashboard could never authenticate. Boot warns loudly
    /// when this is `false`.
    pub cookie_secure: bool,
    /// Free-trial policy applied at signup completion (plan: "Trials"). The
    /// default grants the paid tier for [`DEFAULT_TRIAL_DAYS`] days with no
    /// card; [`TrialPolicy::disabled`] opts a deployment out (new accounts go
    /// straight to free).
    pub trial: TrialPolicy,
}

/// What free trial a freshly-provisioned account receives (plan: "Trials: 14
/// days of paid tier on signup, no card required").
#[derive(Debug, Clone)]
pub struct TrialPolicy {
    /// `false` disables trials: signup leaves the account on the free plan,
    /// `billing_state = 'active'`, no trial. (Provisioning already defaults a
    /// new account to free/active, so a disabled trial is a no-op at
    /// completion.)
    pub enabled: bool,
    /// The paid plan id the trial grants for its duration. Must name a row in
    /// the `plans` table; a misconfigured id is a fail-closed no-op (the
    /// trial UPDATE in [`start_trial`](crate::billing::dunning::start_trial)
    /// would set an unknown `plan_id`, so the FK on `accounts.plan_id`
    /// rejects it and the account stays free — the conservative default).
    pub plan_id: String,
    /// Trial length. The default is [`DEFAULT_TRIAL_DAYS`].
    pub duration: chrono::Duration,
}

impl TrialPolicy {
    /// The plan's default: 14 days of the `pro` tier, no card.
    pub fn default_enabled() -> Self {
        Self {
            enabled: true,
            plan_id: "pro".to_string(),
            duration: chrono::Duration::days(DEFAULT_TRIAL_DAYS),
        }
    }

    /// Trials off — every new account goes straight to free.
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            plan_id: "pro".to_string(),
            duration: chrono::Duration::days(DEFAULT_TRIAL_DAYS),
        }
    }
}

impl Default for TrialPolicy {
    fn default() -> Self {
        Self::default_enabled()
    }
}

impl AccountPlaneConfig {
    /// Production defaults under `base_domain`.
    pub fn new(base_domain: impl Into<String>) -> Self {
        Self {
            base_domain: base_domain.into(),
            app_public_url: None,
            trust_proxy_header: false,
            rate_limits: RateLimits::default(),
            max_concurrent_provisions: DEFAULT_MAX_CONCURRENT_PROVISIONS,
            session_ttl: SESSION_TTL,
            cookie_secure: true,
            trial: TrialPolicy::default_enabled(),
        }
    }
}

/// Everything the account-plane handlers need, shared across workers.
struct PlaneState {
    control: ControlPlane,
    /// The shared tenant cluster, for the synchronous provision in
    /// `/signup/complete`.
    cluster: ClusterConfig,
    /// Managed provider-key lifecycle for signup step 9 (plan: "Provider
    /// management"); `Disabled` provisions keyless accounts.
    managed: ManagedKeys,
    email: Arc<dyn EmailSender>,
    /// Normalized (lowercase, no leading dot, no port) base domain.
    base_domain: String,
    /// Link origin, no trailing slash.
    app_public_url: String,
    /// Scheme + explicit port of [`Self::app_public_url`], reused for the
    /// post-completion redirect to `<slug>.<base>` — production defaults
    /// yield `https://…/`; dev setups keep their `http` and port.
    tenant_scheme: String,
    tenant_port: Option<u16>,
    trust_proxy_header: bool,
    /// One shared per-IP bucket for both request-link routes (module docs:
    /// "Anti-abuse limits").
    ip_limiter: SlidingWindow,
    email_limiter: SlidingWindow,
    /// The synchronous-signup concurrency cap (module docs: "Completion
    /// semantics"). Checked with `try_acquire` *before* the token is
    /// consumed; never waited on — a saturated process answers 503.
    provision_permits: Arc<Semaphore>,
    session_ttl: std::time::Duration,
    /// `Secure` attribute on the session cookie (config; default true).
    cookie_secure: bool,
    /// Free-trial policy applied right after a first-time provision in
    /// `/signup/complete` (plan: "Trials").
    trial: TrialPolicy,
}

/// The account plane as a registrable unit: construct once, hand a clone to
/// every worker's `configure_cloud_app` call. Cheap to clone.
#[derive(Clone)]
pub struct AccountPlane {
    state: web::Data<PlaneState>,
}

impl AccountPlane {
    /// Build the plane. `cluster` is where `/signup/complete` provisions
    /// tenant databases. Fails when `app_public_url` (explicit or derived)
    /// doesn't parse — a misconfigured origin should fail at boot, not on
    /// the first signup's redirect.
    pub fn new(
        control: ControlPlane,
        cluster: ClusterConfig,
        managed: ManagedKeys,
        email: Arc<dyn EmailSender>,
        config: AccountPlaneConfig,
    ) -> Result<Self, CloudError> {
        let base_domain = config
            .base_domain
            .trim_start_matches('.')
            .to_ascii_lowercase();
        let app_public_url = config
            .app_public_url
            .unwrap_or_else(|| format!("https://app.{base_domain}"))
            .trim_end_matches('/')
            .to_string();
        let parsed = url::Url::parse(&app_public_url).map_err(|e| {
            CloudError::InvalidUrl(format!("app public URL {app_public_url:?}: {e}"))
        })?;
        let tenant_scheme = parsed.scheme().to_string();
        // `Url::port()` is None for the scheme's default port, which is
        // exactly right: default ports stay out of the redirect URL.
        let tenant_port = parsed.port();
        let limits = config.rate_limits;
        Ok(Self {
            state: web::Data::new(PlaneState {
                control,
                cluster,
                managed,
                email,
                base_domain,
                app_public_url,
                tenant_scheme,
                tenant_port,
                trust_proxy_header: config.trust_proxy_header,
                ip_limiter: SlidingWindow::new(limits.links_per_ip, limits.ip_window),
                email_limiter: SlidingWindow::new(limits.links_per_email, limits.email_window),
                provision_permits: Arc::new(Semaphore::new(
                    config.max_concurrent_provisions.max(1),
                )),
                session_ttl: config.session_ttl,
                cookie_secure: config.cookie_secure,
                trial: config.trial,
            }),
        })
    }

    /// The signup-completion provision semaphore. Public so the saturation
    /// test can hold a permit (standing in for a slow in-flight provision)
    /// and prove a concurrent completion gets 503 without consuming its
    /// token; production code only touches this through `signup_complete`.
    pub fn provision_permits(&self) -> Arc<Semaphore> {
        Arc::clone(&self.state.provision_permits)
    }

    /// Register the account-plane routes on `cfg`, each guarded to the app
    /// host. Called by `configure_cloud_app`; the guard is what makes these
    /// routes not exist on tenant subdomains (fail-closed direction one in
    /// the module docs). Every response carries
    /// `Referrer-Policy: no-referrer` (module docs: "Response headers") —
    /// completion URLs hold live single-use tokens that must never leak via
    /// `Referer`.
    pub(crate) fn configure(&self, cfg: &mut web::ServiceConfig) {
        let no_referrer = || DefaultHeaders::new().add((header::REFERRER_POLICY, "no-referrer"));
        cfg.service(
            web::scope("/signup")
                .guard(app_host_guard(self.state.base_domain.clone()))
                .app_data(self.state.clone())
                .wrap(no_referrer())
                .route("/request-link", web::post().to(signup_request_link))
                .route("/complete", web::get().to(signup_complete)),
        );
        cfg.service(
            web::scope("/login")
                .guard(app_host_guard(self.state.base_domain.clone()))
                .app_data(self.state.clone())
                .wrap(no_referrer())
                .route("/request-link", web::post().to(login_request_link))
                .route("/complete", web::get().to(login_complete)),
        );
    }
}

/// Whether `host` (as sent by the client, possibly with a port) addresses
/// the app host: the bare base domain or `app.<base>`. Mirrors the parsing
/// edge cases of `auth::subdomain_from_host` — port stripped, matching
/// case-insensitive, lookalike suffixes rejected by exact comparison.
fn is_app_host(host: &str, base_domain: &str) -> bool {
    // Strip any port. IPv6 literals contain colons too, but they can never
    // equal `<base>` or `app.<base>`, so mangling them is harmless.
    let host = host.split(':').next().unwrap_or("").to_ascii_lowercase();
    host == base_domain
        || host
            .strip_prefix("app.")
            .is_some_and(|rest| rest == base_domain)
}

/// Route guard matching only app-host requests. Reads the same host source
/// as `CloudAuth` (the `Host` header, falling back to the URI authority for
/// HTTP/2 `:authority` requests); a request with neither matches nothing.
fn app_host_guard(base_domain: String) -> impl Guard {
    guard::fn_guard(move |ctx: &GuardContext<'_>| {
        let head = ctx.head();
        head.headers()
            .get(header::HOST)
            .and_then(|v| v.to_str().ok())
            .or_else(|| head.uri.host())
            .is_some_and(|host| is_app_host(host, &base_domain))
    })
}

/// The client IP for rate limiting and the `request_ip` breadcrumb. See the
/// module docs for the proxy-header trade-off.
fn client_ip(req: &HttpRequest, trust_proxy_header: bool) -> Option<String> {
    if trust_proxy_header {
        if let Some(ip) = req
            .headers()
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(rightmost_forwarded_ip)
        {
            return Some(ip);
        }
    }
    req.peer_addr().map(|addr| addr.ip().to_string())
}

/// The rightmost entry of an `X-Forwarded-For` value — the one appended by
/// the trusted proxy itself. Everything to its left arrived *in* the
/// client's request and is attacker-controlled.
fn rightmost_forwarded_ip(value: &str) -> Option<String> {
    value
        .rsplit(',')
        .map(str::trim)
        .find(|entry| !entry.is_empty())
        .map(String::from)
}

#[derive(Deserialize)]
struct SignupRequest {
    email: String,
    subdomain: String,
}

#[derive(Deserialize)]
struct LoginRequest {
    email: String,
}

/// `POST /signup/request-link` (app host only). Signup steps 1–2: validate,
/// issue, email. See the module docs for the response policy.
async fn signup_request_link(
    state: web::Data<PlaneState>,
    req: HttpRequest,
    body: web::Json<SignupRequest>,
) -> HttpResponse {
    // Rate-limit by IP before anything else — a validation-failing request
    // is still a signup attempt (the plan's "signup attempts per IP").
    let ip = client_ip(&req, state.trust_proxy_header);
    if let Err(retry_after) = state.ip_limiter.check(ip.as_deref().unwrap_or("unknown")) {
        return rate_limited(retry_after);
    }

    // Step 1 — validation, with honest 400s. The subdomain checks mirror
    // provisioning's (same helpers, same queries) but are best-effort UX:
    // the authoritative claim is the accounts UNIQUE constraint at consume
    // time, so a race here just means the eventual click fails cleanly.
    let SignupRequest { email, subdomain } = body.into_inner();
    if !email_format_ok(&email) {
        return validation_error("invalid_email", "That email address doesn't look valid.");
    }
    if !subdomain_format_ok(&subdomain) {
        return validation_error(
            "invalid_subdomain",
            "Subdomains are 3-32 characters of a-z, 0-9, and hyphens.",
        );
    }
    if reserved_subdomains::is_reserved(&subdomain) {
        return validation_error("subdomain_reserved", "That subdomain is reserved.");
    }
    let actively_reserved: Result<bool, sqlx::Error> = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM subdomains_reserved \
         WHERE subdomain = $1 AND expires_at > NOW())",
    )
    .bind(&subdomain)
    .fetch_one(state.control.pool())
    .await;
    match actively_reserved {
        Ok(true) => {
            return validation_error("subdomain_reserved", "That subdomain is reserved.");
        }
        Ok(false) => {}
        Err(e) => {
            tracing::error!(error = %e, "subdomain reservation check failed");
            return internal_error();
        }
    }
    // "Taken" exempts the requester's own stuck claim: a `'provisioning'`
    // row for the same (lowercased) email is a crashed earlier signup, and
    // re-requesting a link is the documented way to resume it — the
    // eventual completion re-runs `provision_account`, which resumes that
    // exact claim idempotently. Any other claim (active, or another email's
    // in-flight provision) is honestly taken.
    let taken: Result<bool, sqlx::Error> = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM accounts \
         WHERE subdomain = $1 \
           AND NOT (status = 'provisioning' AND LOWER(email) = LOWER($2)))",
    )
    .bind(&subdomain)
    .bind(&email)
    .fetch_one(state.control.pool())
    .await;
    match taken {
        Ok(true) => {
            return validation_error("subdomain_taken", "That subdomain is already taken.");
        }
        Ok(false) => {}
        Err(e) => {
            tracing::error!(error = %e, "subdomain availability check failed");
            return internal_error();
        }
    }

    // Per-email limit, after validation (no point charging garbage strings)
    // and before issuance.
    if let Err(retry_after) = state.email_limiter.check(&email.to_ascii_lowercase()) {
        return rate_limited(retry_after);
    }

    // Step 2 — issue and send. From here on the answer is the neutral 200
    // no matter what: the requester can't act on an issuance or delivery
    // failure, and differential responses are the enumeration shape the
    // login route forbids — keep the routes identical.
    issue_and_send(
        &state,
        &email,
        MagicLinkPurpose::Signup,
        Some(&subdomain),
        ip.as_deref(),
    )
    .await;
    link_requested()
}

/// `POST /login/request-link` (app host only). Sends a login link when an
/// active account matches the email; the response is byte-identical either
/// way (no email enumeration — e2e-pinned) **and returns without awaiting
/// the issue+send** (module docs: uniform time — awaiting only on the
/// exists branch would be a timing oracle over account existence).
async fn login_request_link(
    state: web::Data<PlaneState>,
    req: HttpRequest,
    body: web::Json<LoginRequest>,
) -> HttpResponse {
    // Same shared per-IP bucket as signup, charged before everything else —
    // a probing request is an attempt whether or not it validates, and the
    // limit is what keeps the (already timing-uniform) route from being
    // freely probed across many distinct emails.
    let ip = client_ip(&req, state.trust_proxy_header);
    if let Err(retry_after) = state.ip_limiter.check(ip.as_deref().unwrap_or("unknown")) {
        return rate_limited(retry_after);
    }

    let LoginRequest { email } = body.into_inner();
    if !email_format_ok(&email) {
        return validation_error("invalid_email", "That email address doesn't look valid.");
    }

    // Charge the per-email limit before the account lookup, uniformly, so
    // neither the limiter's count nor its 429s depend on whether the
    // account exists.
    if let Err(retry_after) = state.email_limiter.check(&email.to_ascii_lowercase()) {
        return rate_limited(retry_after);
    }

    let exists: Result<bool, sqlx::Error> = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM accounts \
         WHERE LOWER(email) = LOWER($1) AND status = 'active')",
    )
    .bind(&email)
    .fetch_one(state.control.pool())
    .await;
    match exists {
        Ok(true) => {
            // Fire-and-forget: both branches return immediately after the
            // same synchronous work above. The spawned task's failures are
            // logged inside issue_and_send, never surfaced — exactly the
            // existing policy for send failures, now also keeping the
            // response timing identical to the not-exists branch.
            let state = state.clone();
            tokio::spawn(async move {
                issue_and_send(&state, &email, MagicLinkPurpose::Login, None, ip.as_deref()).await;
            });
        }
        Ok(false) => {
            // No account: do nothing, answer exactly like the happy path.
        }
        Err(e) => {
            // A database error is email-independent, so a 500 here is not
            // an enumeration signal — and hiding a dead control plane
            // behind a 200 would be worse.
            tracing::error!(error = %e, "account lookup for login link failed");
            return internal_error();
        }
    }
    link_requested()
}

/// Issue a magic link and email it, logging — never surfacing — failures.
/// Both request-link routes answer the neutral 200 regardless of this
/// function's outcome; see the module docs for why.
async fn issue_and_send(
    state: &PlaneState,
    email: &str,
    purpose: MagicLinkPurpose,
    requested_subdomain: Option<&str>,
    request_ip: Option<&str>,
) {
    let plaintext = match issue_magic_link(
        &state.control,
        email,
        purpose,
        requested_subdomain,
        request_ip,
        MAGIC_LINK_TTL,
    )
    .await
    {
        Ok(plaintext) => plaintext,
        Err(e) => {
            tracing::error!(purpose = purpose.as_str(), error = %e, "magic link issuance failed");
            return;
        }
    };
    let link = format!(
        "{}/{}/complete?token={plaintext}",
        state.app_public_url,
        purpose.as_str()
    );
    if let Err(e) = state.email.send_magic_link(email, &link, purpose).await {
        // The error (and this log line) carries provider detail but never
        // the link; see crate::email.
        tracing::error!(purpose = purpose.as_str(), error = %e, "magic link email failed");
    }
}

/// Query shape of both completion routes. `Option` so a missing parameter
/// produces this module's structured `invalid_link` response instead of
/// actix's default deserialization 400.
#[derive(Deserialize)]
struct CompleteQuery {
    token: Option<String>,
}

/// `GET /signup/complete?token=…` (app host only). Signup steps 3–12 from
/// the plan, minus the deferred 7–8 (cloud-curated per-DB settings, default
/// report — later slices); step 9 (managed provider key) runs inside
/// `provision_account` per the configured [`ManagedKeys`] mode. See the
/// module docs ("Completion semantics") for the refusal ordering this
/// implements.
async fn signup_complete(
    state: web::Data<PlaneState>,
    req: HttpRequest,
    query: web::Query<CompleteQuery>,
) -> HttpResponse {
    let Some(token) = query.into_inner().token else {
        return invalid_link();
    };

    // Admission order (module docs: "Completion semantics"): shape, peek,
    // permit, consume. The two read-only checks refuse dead tokens before
    // the permit is touched, so junk requests can never starve the
    // semaphore; the permit still precedes consumption, so saturation
    // never burns a live token.
    if !magic_link_token_shape_ok(&token) {
        return invalid_link();
    }
    match peek_magic_link(&state.control, &token, MagicLinkPurpose::Signup).await {
        Ok(true) => {}
        Ok(false) => return invalid_link(),
        Err(e) => {
            tracing::error!(error = %e, "peeking signup link failed");
            return internal_error();
        }
    }

    // Capacity BEFORE consumption: a saturated process must refuse without
    // spending the single-use token, so the same link succeeds on retry.
    // `try_acquire` — never wait — because each waiter would pin an HTTP
    // connection behind multi-second provisions.
    let Ok(_permit) = Arc::clone(&state.provision_permits).try_acquire_owned() else {
        return provisioning_busy();
    };

    // The atomic consume remains the only authority: a token that died
    // between peek and consume (a double click racing this request) is
    // refused here exactly as before.
    let record = match consume_magic_link(&state.control, &token, MagicLinkPurpose::Signup).await {
        Ok(Some(record)) => record,
        Ok(None) => return invalid_link(),
        Err(e) => {
            tracing::error!(error = %e, "consuming signup link failed");
            return internal_error();
        }
    };
    let Some(subdomain) = record.requested_subdomain else {
        // Issuance always stores a subdomain on signup links; a row without
        // one is corruption, not user error.
        tracing::error!(
            email = record.email,
            "signup link row has no requested_subdomain"
        );
        return internal_error();
    };

    // The token is spent from here on. provision_account re-validates,
    // claims the subdomain via the UNIQUE constraint, and is idempotent
    // under resume; SubdomainTaken/SubdomainReserved at this point means
    // the name was claimed (or parked by a deletion) while the link sat in
    // the inbox.
    let provisioned = provision_account(
        &state.control,
        &state.cluster,
        &state.managed,
        NewAccount {
            email: record.email.clone(),
            subdomain,
        },
    )
    .await;
    match provisioned {
        Ok(account) => {
            // Start the free trial (plan: "Trials: 14 days of paid tier on
            // signup, no card required"). First-time-only and idempotent
            // (see `start_trial`), so a resume of an already-trialing signup
            // re-runs harmlessly; disabled deployments skip it and leave the
            // account on free. A trial-start failure must NOT fail signup —
            // the account is fully provisioned and serving; the worst case is
            // it sits on free instead of the paid trial, which the user can
            // remedy by upgrading. Log and continue.
            if state.trial.enabled {
                if let Err(e) = start_trial(
                    &state.control,
                    &account.account_id,
                    &state.trial.plan_id,
                    state.trial.duration,
                )
                .await
                {
                    tracing::error!(
                        account_id = account.account_id,
                        error = %e,
                        "starting free trial failed; account remains on free"
                    );
                }
            }
            tracing::info!(
                account_id = account.account_id,
                subdomain = account.subdomain,
                "signup completed"
            );
            session_redirect(&state, &req, &account.account_id, &account.subdomain).await
        }
        // The consumed token stays spent on a name conflict (module docs):
        // un-consuming would reopen the replay window, and a fresh
        // request-link is cheap and rate-limited.
        Err(CloudError::SubdomainTaken(s)) => subdomain_conflict("subdomain_taken", &s),
        Err(CloudError::SubdomainReserved(s)) => subdomain_conflict("subdomain_reserved", &s),
        Err(e) => {
            // The claim may have landed before the failure, leaving the
            // accounts row in 'provisioning'. That is deliberate: the
            // safety-net reaper (plan: "Failure recovery & the reaper")
            // retries or rolls back stuck rows; a user retry can also
            // resume it (provision_account is idempotent for the same
            // email + subdomain).
            tracing::error!(error = %e, email = record.email, "synchronous provision failed");
            provision_failed()
        }
    }
}

/// `GET /login/complete?token=…` (app host only). Consume the login link
/// (purpose-pinned — a signup link refuses here without being spent), find
/// the active account by the link's email, establish a session, redirect.
async fn login_complete(
    state: web::Data<PlaneState>,
    req: HttpRequest,
    query: web::Query<CompleteQuery>,
) -> HttpResponse {
    let Some(token) = query.into_inner().token else {
        return invalid_link();
    };
    // Same syntactic gate as signup completion (there's no permit to
    // protect here, but garbage shouldn't cost a database round-trip
    // either, and the refusal is byte-identical).
    if !magic_link_token_shape_ok(&token) {
        return invalid_link();
    }
    let record = match consume_magic_link(&state.control, &token, MagicLinkPurpose::Login).await {
        Ok(Some(record)) => record,
        Ok(None) => return invalid_link(),
        Err(e) => {
            tracing::error!(error = %e, "consuming login link failed");
            return internal_error();
        }
    };

    // The newest active account wins if an email somehow has several
    // (accounts.email is not unique); the request-link route only verified
    // existence. An account deleted between request and click answers the
    // same `invalid_link` as a stale token — account state is not an oracle
    // this route hands out.
    let account: Option<(String, String)> = match sqlx::query_as(
        "SELECT id, subdomain FROM accounts \
         WHERE LOWER(email) = LOWER($1) AND status = 'active' \
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(&record.email)
    .fetch_optional(state.control.pool())
    .await
    {
        Ok(account) => account,
        Err(e) => {
            tracing::error!(error = %e, "account lookup for login completion failed");
            return internal_error();
        }
    };
    let Some((account_id, subdomain)) = account else {
        return invalid_link();
    };
    session_redirect(&state, &req, &account_id, &subdomain).await
}

/// Step 12: create the web session and answer the 302 that lands the
/// browser on the account's subdomain with the session cookie set.
///
/// Cookie attributes per the plan ("Web sessions"): `Domain=.<base>` (the
/// leading dot — one session works on every subdomain; `CloudAuth`'s
/// account-scoped verification is what keeps it from crossing tenants),
/// `Secure; HttpOnly; SameSite=Lax`, `Max-Age` = the session TTL so browser
/// and server expire together. `Secure` is unconditional — see module docs.
async fn session_redirect(
    state: &PlaneState,
    req: &HttpRequest,
    account_id: &str,
    subdomain: &str,
) -> HttpResponse {
    let ip = client_ip(req, state.trust_proxy_header);
    let ua = req
        .headers()
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok());
    let session = match create_session(
        &state.control,
        account_id,
        state.session_ttl,
        ip.as_deref(),
        ua,
    )
    .await
    {
        Ok(session) => session,
        Err(e) => {
            tracing::error!(error = %e, account_id, "creating session failed");
            return internal_error();
        }
    };

    let cookie = Cookie::build(SESSION_COOKIE, session)
        .domain(format!(".{}", state.base_domain))
        .path("/")
        .secure(state.cookie_secure)
        .http_only(true)
        .same_site(SameSite::Lax)
        .max_age(
            actix_web::cookie::time::Duration::try_from(state.session_ttl)
                .unwrap_or(actix_web::cookie::time::Duration::MAX),
        )
        .finish();

    let port = state
        .tenant_port
        .map(|p| format!(":{p}"))
        .unwrap_or_default();
    let location = format!(
        "{}://{subdomain}.{}{port}/",
        state.tenant_scheme, state.base_domain
    );
    HttpResponse::Found()
        .insert_header((header::LOCATION, location))
        .cookie(cookie)
        .finish()
}

// --- Responses --------------------------------------------------------------

/// The shared neutral 200 — byte-identical across both routes and every
/// post-validation outcome.
fn link_requested() -> HttpResponse {
    HttpResponse::Ok().json(serde_json::json!({
        "status": "ok",
        "message": "If the request was valid, a link is on its way. Check your email.",
    }))
}

fn validation_error(code: &str, message: &str) -> HttpResponse {
    HttpResponse::BadRequest().json(serde_json::json!({
        "error": code,
        "message": message,
    }))
}

/// The one refusal for every dead token — missing, unknown, expired,
/// already consumed, or presented to the wrong endpoint. Deliberately
/// undifferentiated (module docs: no oracle over the magic-link table).
fn invalid_link() -> HttpResponse {
    HttpResponse::BadRequest().json(serde_json::json!({
        "error": "invalid_link",
        "message": "This link is invalid or expired. Request a new one.",
    }))
}

/// The provision semaphore is saturated. Answered *before* the token is
/// consumed, so the same link works once capacity frees up.
fn provisioning_busy() -> HttpResponse {
    HttpResponse::ServiceUnavailable()
        .insert_header((
            header::RETRY_AFTER,
            PROVISION_BUSY_RETRY_AFTER_SECS.to_string(),
        ))
        .json(serde_json::json!({
            "error": "provisioning_busy",
            "message": "Too many accounts are being set up right now. \
                        Open your link again in a few seconds.",
            "retry_after_seconds": PROVISION_BUSY_RETRY_AFTER_SECS,
        }))
}

/// The requested name was claimed (`subdomain_taken`) or parked by a
/// deletion (`subdomain_reserved`) while the link sat unconsumed. The spent
/// token is not refunded — see the module docs for the trade-off.
fn subdomain_conflict(code: &str, subdomain: &str) -> HttpResponse {
    HttpResponse::Conflict().json(serde_json::json!({
        "error": code,
        "message": format!(
            "The subdomain {subdomain:?} is no longer available. \
             Start signup again with a different name."
        ),
    }))
}

/// Provisioning failed after the token was consumed. The accounts row may
/// be parked in 'provisioning'; the reaper owns its recovery (plan:
/// "Failure recovery & the reaper").
fn provision_failed() -> HttpResponse {
    HttpResponse::InternalServerError().json(serde_json::json!({
        "error": "provision_failed",
        "message": "Something went wrong setting up your account. \
                    Try again in a few minutes.",
    }))
}

fn rate_limited(retry_after: std::time::Duration) -> HttpResponse {
    // Round up: telling a client to retry a second early guarantees a
    // second 429.
    let seconds = retry_after.as_secs() + u64::from(retry_after.subsec_nanos() > 0);
    HttpResponse::TooManyRequests()
        .insert_header((header::RETRY_AFTER, seconds.to_string()))
        .json(serde_json::json!({
            "error": "rate_limited",
            "message": "Too many requests. Try again later.",
            "retry_after_seconds": seconds,
        }))
}

fn internal_error() -> HttpResponse {
    HttpResponse::InternalServerError().json(serde_json::json!({ "error": "internal_error" }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_host_matching() {
        let base = "atomic.cloud";
        for ok in [
            "atomic.cloud",
            "app.atomic.cloud",
            "Atomic.Cloud:443",
            "APP.atomic.cloud:8080",
        ] {
            assert!(is_app_host(ok, base), "{ok:?} must match the app host");
        }
        for bad in [
            "kenny.atomic.cloud",
            "app.atomic.cloud.evil.com",
            "xapp.atomic.cloud",
            "app.app.atomic.cloud",
            "appatomic.cloud",
            "app.",
            "atomic.cloud.evil.com",
            "evil-atomic.cloud",
            "",
            "[::1]:8080",
        ] {
            assert!(!is_app_host(bad, base), "{bad:?} must not match");
        }
        // localhost-style base for dev/tests.
        assert!(is_app_host("localhost:8080", "localhost"));
        assert!(is_app_host("app.localhost:8080", "localhost"));
        assert!(!is_app_host("kenny.localhost:8080", "localhost"));
    }

    #[test]
    fn forwarded_ip_takes_the_rightmost_entry() {
        // The rightmost entry is the proxy-appended one; spoofed entries
        // arrive on the left.
        assert_eq!(
            rightmost_forwarded_ip("1.2.3.4, 5.6.7.8").as_deref(),
            Some("5.6.7.8")
        );
        assert_eq!(
            rightmost_forwarded_ip("203.0.113.7").as_deref(),
            Some("203.0.113.7")
        );
        // Trailing commas / whitespace don't yield empty keys.
        assert_eq!(
            rightmost_forwarded_ip("1.2.3.4, ").as_deref(),
            Some("1.2.3.4")
        );
        assert_eq!(rightmost_forwarded_ip(""), None);
        assert_eq!(rightmost_forwarded_ip(" , "), None);
    }
}
