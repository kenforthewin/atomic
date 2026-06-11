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
//! # Routes (this slice)
//!
//! - `POST /signup/request-link` `{email, subdomain}` — validate, issue a
//!   signup magic link, email it. Bad email/slug are honest 400s (they're
//!   client-fixable and leak nothing the public DNS namespace doesn't
//!   already leak); everything after validation — including email-send
//!   failure — answers the same neutral 200, because differential responses
//!   on the email axis are exactly the enumeration oracle the login route
//!   must not have, and the two routes should behave identically.
//! - `POST /login/request-link` `{email}` — if an active account matches,
//!   email a login link. The response is byte-identical whether or not the
//!   account exists (no email enumeration; e2e-pinned).
//!
//! The consume routes (`/signup/complete`, `/login/complete`) arrive with
//! the rest of the signup slice; until then issued links are inert rows.
//!
//! # Anti-abuse limits (plan: "Quotas" table)
//!
//! - Signup request-link: 5 per client IP per hour.
//! - Magic-link requests (signup + login combined): 3 per email per hour.
//!
//! Per-pod in-memory sliding windows ([`crate::rate_limit`]); refusals are
//! 429 with `Retry-After`. The IP limit runs *before* validation — a
//! validation-failing request is still a signup attempt — and the email
//! limit runs after it, keyed on the (lowercased) validated email, and is
//! always charged before the account lookup so the limiter's behavior
//! cannot become an enumeration side channel either.
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

use actix_web::guard::{Guard, GuardContext};
use actix_web::http::header;
use actix_web::{guard, web, HttpRequest, HttpResponse};
use serde::Deserialize;

use crate::control_plane::ControlPlane;
use crate::email::EmailSender;
use crate::magic_links::{issue_magic_link, MagicLinkPurpose, MAGIC_LINK_TTL};
use crate::provision::{email_format_ok, subdomain_format_ok};
use crate::rate_limit::SlidingWindow;
use crate::reserved_subdomains;

/// The plan's anti-abuse rate-limit numbers ("Quotas" table), with the
/// windows exposed so tests can shrink them instead of sleeping through
/// real hours. Production callers use `Default`.
#[derive(Debug, Clone)]
pub struct RateLimits {
    /// Signup request-link admissions per client IP per window.
    pub signup_links_per_ip: u32,
    pub signup_ip_window: std::time::Duration,
    /// Magic-link admissions (signup + login combined) per email per window.
    pub links_per_email: u32,
    pub email_window: std::time::Duration,
}

impl Default for RateLimits {
    fn default() -> Self {
        Self {
            signup_links_per_ip: 5,
            signup_ip_window: std::time::Duration::from_secs(3600),
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
}

impl AccountPlaneConfig {
    /// Production defaults under `base_domain`.
    pub fn new(base_domain: impl Into<String>) -> Self {
        Self {
            base_domain: base_domain.into(),
            app_public_url: None,
            trust_proxy_header: false,
            rate_limits: RateLimits::default(),
        }
    }
}

/// Everything the account-plane handlers need, shared across workers.
struct PlaneState {
    control: ControlPlane,
    email: Arc<dyn EmailSender>,
    /// Normalized (lowercase, no leading dot, no port) base domain.
    base_domain: String,
    /// Link origin, no trailing slash.
    app_public_url: String,
    trust_proxy_header: bool,
    signup_ip_limiter: SlidingWindow,
    email_limiter: SlidingWindow,
}

/// The account plane as a registrable unit: construct once, hand a clone to
/// every worker's `configure_cloud_app` call. Cheap to clone.
#[derive(Clone)]
pub struct AccountPlane {
    state: web::Data<PlaneState>,
}

impl AccountPlane {
    pub fn new(
        control: ControlPlane,
        email: Arc<dyn EmailSender>,
        config: AccountPlaneConfig,
    ) -> Self {
        let base_domain = config
            .base_domain
            .trim_start_matches('.')
            .to_ascii_lowercase();
        let app_public_url = config
            .app_public_url
            .unwrap_or_else(|| format!("https://app.{base_domain}"))
            .trim_end_matches('/')
            .to_string();
        let limits = config.rate_limits;
        Self {
            state: web::Data::new(PlaneState {
                control,
                email,
                base_domain,
                app_public_url,
                trust_proxy_header: config.trust_proxy_header,
                signup_ip_limiter: SlidingWindow::new(
                    limits.signup_links_per_ip,
                    limits.signup_ip_window,
                ),
                email_limiter: SlidingWindow::new(limits.links_per_email, limits.email_window),
            }),
        }
    }

    /// Register the account-plane routes on `cfg`, each guarded to the app
    /// host. Called by `configure_cloud_app`; the guard is what makes these
    /// routes not exist on tenant subdomains (fail-closed direction one in
    /// the module docs).
    pub(crate) fn configure(&self, cfg: &mut web::ServiceConfig) {
        cfg.service(
            web::scope("/signup")
                .guard(app_host_guard(self.state.base_domain.clone()))
                .app_data(self.state.clone())
                .route("/request-link", web::post().to(signup_request_link)),
        );
        cfg.service(
            web::scope("/login")
                .guard(app_host_guard(self.state.base_domain.clone()))
                .app_data(self.state.clone())
                .route("/request-link", web::post().to(login_request_link)),
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
    if let Err(retry_after) = state
        .signup_ip_limiter
        .check(ip.as_deref().unwrap_or("unknown"))
    {
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
    let taken: Result<bool, sqlx::Error> =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM accounts WHERE subdomain = $1)")
            .bind(&subdomain)
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
/// way (no email enumeration — e2e-pinned).
async fn login_request_link(
    state: web::Data<PlaneState>,
    req: HttpRequest,
    body: web::Json<LoginRequest>,
) -> HttpResponse {
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
            let ip = client_ip(&req, state.trust_proxy_header);
            issue_and_send(&state, &email, MagicLinkPurpose::Login, None, ip.as_deref()).await;
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
