//! The public demo instance (plan: `docs/plans/demo-instance.md`).
//!
//! One tenant — designated by deploy config (`--demo-subdomain` /
//! `ATOMIC_CLOUD_DEMO_SUBDOMAIN`), never by control-plane state — serves
//! anonymous read-only visitors. The design inverts the usual guard shape:
//! rather than wrapping a middleware around every authenticated surface
//! (and remembering to wrap the next one), the whitelist is enforced at
//! **principal-synthesis time** inside [`authenticate`]: a credential-less
//! request on the demo host either matches [`DEMO_ALLOWED`] and becomes a
//! [`CredentialSource::DemoVisitor`] principal, or is refused on the spot.
//! Every surface behind `CloudAuth` — `/api`, `/mcp`, `/ws`, the tenant
//! plane, the export plane, and anything added later — is therefore
//! demo-closed by construction. Default-deny with no wrap sites to forget.
//!
//! What the whitelist admits is the read surface the product frontend
//! renders (atoms, tags, canvas/graph, wiki, reports, boot/status reads)
//! plus exactly two POSTs: the search endpoints, which are semantically
//! reads but embed the query — the demo's only AI-spend surface, so they
//! carry a per-IP sliding-window limit here and the managed key's hard
//! credit cap as the backstop. Everything else 403s with a structured
//! `demo_forbidden` body carrying the signup URL, so the frontend can
//! render "get your own" instead of an error.
//!
//! Deliberately closed, with reasons:
//! - **Chat/conversations** — conversations are tenant-scoped; anonymous
//!   chat is a shared graffiti wall under our domain (and a free-LLM
//!   proxy). The frontend renders the chat panel as the signup CTA.
//! - **Exports** — the billing guard's egress exemption ("read-only is a
//!   serving state, not a hostage state") does NOT carry over: that
//!   principle protects a *user's* right to their data; an anonymous
//!   visitor has no data here, and full-corpus zips are a disk/CPU hole.
//! - **`/ws`** — visitors don't need live pipeline events (v1); closed
//!   until argued for.
//! - **`/mcp`** — an anonymous visitor gets `demo_forbidden`, not the
//!   OAuth challenge: there is no legitimate anonymous MCP flow on the
//!   demo host (consent requires a session the visitor cannot have).
//!
//! Credentialed requests on the demo host never reach this module's
//! checks — the operator's session/token authenticates normally (and only
//! the demo account's own credentials can, per the second-chokepoint
//! test), so seeding and feed management need no special path.
//!
//! [`authenticate`]: crate::auth
//! [`CredentialSource::DemoVisitor`]: crate::auth::CredentialSource::DemoVisitor

use std::time::Duration;

use actix_web::dev::ServiceRequest;
use actix_web::http::Method;
use actix_web::{web, HttpMessage, HttpResponse};

use crate::auth::{CloudAuth, CredentialSource, ResolvedTenant};
use crate::rate_limit::SlidingWindow;
use crate::server::cloud_plane_guard;

/// Per-IP admissions per minute on the search endpoints — the only
/// whitelisted routes that spend AI credits (one query embedding each).
/// Generous for a human exploring the demo, hostile to a scraping loop;
/// the managed key's credit cap bounds the worst case regardless.
const SEARCH_LIMIT_PER_MINUTE: u32 = 30;
const SEARCH_LIMIT_WINDOW: Duration = Duration::from_secs(60);

/// The demo surface: `(method, pattern)` pairs, where a `*` segment matches
/// exactly one non-empty path segment. This is the **entire** anonymous
/// surface — a route absent here (including every route atomic-server or
/// the cloud planes grow in the future) is refused at authentication time.
///
/// Sources: the product frontend's command map (its read surface) plus
/// [`DEMO_CONFIG_PATH`]. When adding an entry, ask: is it a pure read of
/// demo corpus state (fine), an AI-spend read (needs a limiter), or
/// anything else (it doesn't belong here)?
const DEMO_ALLOWED: &[(&Method, &str)] = &[
    // Demo detection (this module's own endpoint).
    (&Method::GET, "/api/demo-config"),
    // Atoms — list, read, relations.
    (&Method::GET, "/api/atoms"),
    (&Method::GET, "/api/atoms/sources"),
    (&Method::GET, "/api/atoms/by-source-url"),
    (&Method::GET, "/api/atoms/*"),
    (&Method::GET, "/api/atoms/*/links"),
    (&Method::GET, "/api/atoms/*/similar"),
    (&Method::GET, "/api/atoms/*/embedding-status"),
    // Tags — tree + reads.
    (&Method::GET, "/api/tags"),
    (&Method::GET, "/api/tags/*"),
    (&Method::GET, "/api/tags/*/children"),
    (&Method::GET, "/api/tags/*/related"),
    // Canvas / graph / clustering.
    (&Method::GET, "/api/canvas/global"),
    (&Method::GET, "/api/canvas/positions"),
    (&Method::GET, "/api/canvas/atoms-with-embeddings"),
    (&Method::GET, "/api/graph/edges"),
    (&Method::GET, "/api/graph/neighborhood/*"),
    (&Method::GET, "/api/clustering"),
    (&Method::GET, "/api/clustering/connection-counts"),
    // Wiki.
    (&Method::GET, "/api/wiki"),
    (&Method::GET, "/api/wiki/suggestions"),
    (&Method::GET, "/api/wiki/*"),
    (&Method::GET, "/api/wiki/*/links"),
    (&Method::GET, "/api/wiki/*/related"),
    (&Method::GET, "/api/wiki/*/status"),
    (&Method::GET, "/api/wiki/*/versions"),
    (&Method::GET, "/api/wiki/versions/*"),
    // Reports (the weekly-digest showcase): the report, its findings list,
    // each finding atom, and the finding's citations back into the corpus.
    (&Method::GET, "/api/reports"),
    (&Method::GET, "/api/reports/*"),
    (&Method::GET, "/api/reports/*/findings"),
    (&Method::GET, "/api/dashboard/featured-report"),
    (&Method::GET, "/api/findings/*"),
    (&Method::GET, "/api/findings/*/citations"),
    // Boot/status reads the SPA needs to render.
    (&Method::GET, "/api/setup/status"),
    (&Method::GET, "/api/settings"),
    (&Method::GET, "/api/settings/models"),
    (&Method::GET, "/api/embeddings/status/all"),
    (&Method::GET, "/api/databases"),
    (&Method::GET, "/api/databases/*"),
    // Search — POSTs that are semantically reads; the AI-spend surface.
    (&Method::POST, "/api/search"),
    (&Method::POST, "/api/search/global"),
];

/// Whether `(method, path)` is admitted for anonymous demo visitors.
/// HEAD rides GET's entries (actix routes HEAD to GET handlers).
fn demo_allowed(method: &Method, path: &str) -> bool {
    let effective = if *method == Method::HEAD {
        &Method::GET
    } else {
        method
    };
    DEMO_ALLOWED
        .iter()
        .any(|(m, pattern)| *m == effective && pattern_matches(pattern, path))
}

/// Segment-wise match: `*` matches exactly one non-empty segment, anything
/// else matches literally. No prefix/suffix wildcards — the table stays
/// exact by construction. Query strings are the caller's problem (actix's
/// `req.path()` excludes them).
fn pattern_matches(pattern: &str, path: &str) -> bool {
    let mut pat = pattern.split('/');
    let mut got = path.split('/');
    loop {
        match (pat.next(), got.next()) {
            (None, None) => return true,
            (Some(p), Some(g)) => {
                if p == "*" {
                    if g.is_empty() {
                        return false;
                    }
                } else if p != g {
                    return false;
                }
            }
            _ => return false,
        }
    }
}

/// Whether `path` is one of the search endpoints (the per-IP limited pair).
fn is_search_path(path: &str) -> bool {
    path == "/api/search" || path == "/api/search/global"
}

/// The demo host's configuration, carried by `CloudAuth`'s context when a
/// demo subdomain is configured (absent → no demo behavior anywhere).
pub struct DemoPlane {
    subdomain: String,
    signup_url: String,
    trust_proxy_header: bool,
    search_limiter: SlidingWindow,
}

impl DemoPlane {
    /// `app_public_url` is the app host's public origin
    /// (`--app-public-url`), which serves the signup page; the CTA links
    /// there. `trust_proxy_header` follows the same flag the signup/login
    /// limiters use — behind Caddy it is true in production.
    pub fn new(
        subdomain: impl Into<String>,
        app_public_url: &str,
        trust_proxy_header: bool,
    ) -> Self {
        Self {
            subdomain: subdomain.into().to_ascii_lowercase(),
            signup_url: format!("{}/signup", app_public_url.trim_end_matches('/')),
            trust_proxy_header,
            search_limiter: SlidingWindow::new(SEARCH_LIMIT_PER_MINUTE, SEARCH_LIMIT_WINDOW),
        }
    }

    /// The subdomain this plane serves anonymously.
    pub fn subdomain(&self) -> &str {
        &self.subdomain
    }

    /// The signup CTA URL (also exposed via `GET /api/demo-config`).
    pub fn signup_url(&self) -> &str {
        &self.signup_url
    }

    /// Admit or refuse a credential-less request on the demo host. Called
    /// from `authenticate`'s no-credential branch — a `Ok(())` means the
    /// caller synthesizes the [`CredentialSource::DemoVisitor`] principal;
    /// an `Err` is the response to return (403 off-whitelist, 429 over the
    /// search limit).
    pub fn authorize(&self, req: &ServiceRequest) -> Result<(), HttpResponse> {
        if !demo_allowed(req.method(), req.path()) {
            return Err(self.forbidden());
        }
        if is_search_path(req.path()) {
            let ip = crate::account_plane::client_ip(req.request(), self.trust_proxy_header)
                .unwrap_or_else(|| "unknown".to_string());
            if let Err(retry_after) = self.search_limiter.check(&ip) {
                return Err(HttpResponse::TooManyRequests()
                    .insert_header(("Retry-After", retry_after.as_secs().max(1).to_string()))
                    .json(serde_json::json!({
                        "error": "demo_rate_limited",
                        "message": "Search is rate-limited on the public demo. \
                                    Sign up for an instance of your own.",
                        "signup_url": self.signup_url,
                    })));
            }
        }
        Ok(())
    }

    /// The structured refusal for anything off the whitelist: the frontend
    /// keys on `demo_forbidden` to render the signup CTA instead of an
    /// error state. 403 (not 401) so it is distinguishable from "you're
    /// logged out" by clients that treat 401 as a login redirect.
    fn forbidden(&self) -> HttpResponse {
        HttpResponse::Forbidden().json(serde_json::json!({
            "error": "demo_forbidden",
            "message": "This is a read-only public demo of Atomic.",
            "signup_url": self.signup_url,
        }))
    }
}

/// The path served by [`demo_config`]; whitelisted so the probe works
/// anonymously on the demo host.
const DEMO_CONFIG_PATH: &str = "/api/demo-config";

/// `GET /api/demo-config` — the SPA's demo-mode probe. A demo visitor gets
/// `{demo: true, signup_url}`; any authenticated principal (including the
/// demo account's operator, who should see the normal product) gets 404.
/// On non-demo hosts anonymous requests never reach this handler (CloudAuth
/// 401s them), which the probe likewise treats as "not a demo".
async fn demo_config(
    req: actix_web::HttpRequest,
    signup: web::Data<DemoConfigSignup>,
) -> HttpResponse {
    let is_visitor = req
        .extensions()
        .get::<ResolvedTenant>()
        .is_some_and(|t| t.principal.source == CredentialSource::DemoVisitor);
    if is_visitor {
        HttpResponse::Ok().json(serde_json::json!({
            "demo": true,
            "signup_url": signup.0,
        }))
    } else {
        HttpResponse::NotFound().json(serde_json::json!({ "error": "not_found" }))
    }
}

/// The signup URL captured into the `demo-config` resource's app data at
/// composition time (newtype so the `web::Data` is unambiguous).
struct DemoConfigSignup(String);

/// Register `GET /api/demo-config` ahead of `api_scope` (the tenant-plane
/// pattern), wrapped in the same auth + plane guard so [`ResolvedTenant`]
/// is installed. Registered unconditionally — on deployments with no demo
/// subdomain there are no DemoVisitor principals, so it uniformly 404s.
pub fn configure(cfg: &mut web::ServiceConfig, auth: CloudAuth) {
    let signup_url = auth
        .demo_signup_url()
        .unwrap_or_default();
    cfg.service(
        web::resource(DEMO_CONFIG_PATH)
            .app_data(web::Data::new(DemoConfigSignup(signup_url)))
            .route(web::get().to(demo_config))
            .wrap(actix_web::middleware::from_fn(cloud_plane_guard))
            .wrap(auth),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whitelist_admits_the_read_surface() {
        for (method, path) in [
            (&Method::GET, "/api/atoms"),
            (&Method::GET, "/api/atoms/abc-123"),
            (&Method::GET, "/api/atoms/abc-123/links"),
            (&Method::GET, "/api/atoms/abc-123/similar"),
            (&Method::GET, "/api/tags"),
            (&Method::GET, "/api/tags/t-1/children"),
            (&Method::GET, "/api/canvas/atoms-with-embeddings"),
            (&Method::GET, "/api/graph/neighborhood/abc-123"),
            (&Method::GET, "/api/wiki/tag-9/status"),
            (&Method::GET, "/api/wiki/versions/v-2"),
            (&Method::GET, "/api/reports/r-1"),
            (&Method::GET, "/api/reports/r-1/findings"),
            (&Method::GET, "/api/findings/f-1/citations"),
            (&Method::GET, "/api/settings"),
            (&Method::GET, "/api/databases"),
            (&Method::GET, "/api/demo-config"),
            (&Method::HEAD, "/api/atoms"),
            (&Method::POST, "/api/search"),
            (&Method::POST, "/api/search/global"),
        ] {
            assert!(demo_allowed(method, path), "{method} {path} must be admitted");
        }
    }

    #[test]
    fn whitelist_refuses_everything_else() {
        for (method, path) in [
            // Mutations, including on whitelisted read paths.
            (&Method::POST, "/api/atoms"),
            (&Method::DELETE, "/api/atoms/abc-123"),
            (&Method::PUT, "/api/settings/onboarding_completed"),
            (&Method::POST, "/api/tags"),
            // Chat / conversations — closed by decision.
            (&Method::GET, "/api/conversations"),
            (&Method::POST, "/api/conversations"),
            (&Method::GET, "/api/conversations/c-1"),
            // Exports — the billing guard's egress exemption must NOT
            // carry over to anonymous visitors.
            (&Method::POST, "/api/databases/default/exports/markdown"),
            (&Method::GET, "/api/exports/job-1/download"),
            (&Method::DELETE, "/api/exports/job-1"),
            // Feeds config, tokens, account plane, provider probes.
            (&Method::GET, "/api/feeds"),
            (&Method::GET, "/api/auth/tokens"),
            (&Method::POST, "/api/auth/tokens"),
            (&Method::GET, "/api/account/overview"),
            (&Method::DELETE, "/api/account"),
            (&Method::GET, "/api/provider/verify"),
            (&Method::GET, "/api/logs"),
            // Ingestion and pipeline pokes.
            (&Method::POST, "/api/ingest/url"),
            (&Method::POST, "/api/import/obsidian"),
            (&Method::POST, "/api/embeddings/process-pending"),
            (&Method::POST, "/api/clustering/compute"),
            (&Method::POST, "/api/graph/rebuild-edges"),
            // Report mutations and run-now (an agentic run = AI spend).
            (&Method::POST, "/api/reports"),
            (&Method::POST, "/api/reports/r-1/run"),
            (&Method::PUT, "/api/reports/r-1"),
            (&Method::DELETE, "/api/reports/r-1"),
            (&Method::PUT, "/api/dashboard/featured-report"),
            // Non-API surfaces behind CloudAuth.
            (&Method::GET, "/ws"),
            (&Method::POST, "/mcp"),
            (&Method::GET, "/mcp"),
            // Path-shape probes: wildcards match exactly one segment.
            (&Method::GET, "/api/atoms/a/b/c"),
            (&Method::GET, "/api/atoms//links"),
            (&Method::GET, "/api"),
            (&Method::GET, "/"),
        ] {
            assert!(
                !demo_allowed(method, path),
                "{method} {path} must be refused"
            );
        }
    }

    #[test]
    fn pattern_matching_is_segment_exact() {
        assert!(pattern_matches("/api/atoms/*", "/api/atoms/x"));
        assert!(!pattern_matches("/api/atoms/*", "/api/atoms"));
        assert!(!pattern_matches("/api/atoms/*", "/api/atoms/x/y"));
        assert!(!pattern_matches("/api/atoms/*", "/api/atoms/"));
        assert!(!pattern_matches("/api/atoms", "/api/atoms/x"));
    }

    #[test]
    fn search_limiter_admits_then_refuses() {
        let plane = DemoPlane::new("demo", "https://app.example.test", false);
        let req = actix_web::test::TestRequest::post()
            .uri("/api/search")
            .peer_addr("203.0.113.9:443".parse().unwrap())
            .to_srv_request();
        for _ in 0..SEARCH_LIMIT_PER_MINUTE {
            assert!(plane.authorize(&req).is_ok());
        }
        let refused = plane.authorize(&req).unwrap_err();
        assert_eq!(refused.status(), actix_web::http::StatusCode::TOO_MANY_REQUESTS);
        // A different IP is unaffected (the window is per-IP, not global).
        let other = actix_web::test::TestRequest::post()
            .uri("/api/search")
            .peer_addr("198.51.100.7:443".parse().unwrap())
            .to_srv_request();
        assert!(plane.authorize(&other).is_ok());
    }

    #[test]
    fn off_whitelist_is_403_with_signup_url() {
        let plane = DemoPlane::new("demo", "https://app.example.test/", false);
        let req = actix_web::test::TestRequest::post()
            .uri("/api/atoms")
            .to_srv_request();
        let refused = plane.authorize(&req).unwrap_err();
        assert_eq!(refused.status(), actix_web::http::StatusCode::FORBIDDEN);
        assert_eq!(plane.signup_url(), "https://app.example.test/signup");
    }
}
