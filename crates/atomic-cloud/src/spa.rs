//! Static serving for the cloud account-plane SPA (the signup/login pages and
//! the authenticated `/account/*` dashboard).
//!
//! The single-page app is built once (`crates/atomic-cloud/frontend`, `npm run
//! build` → `dist/`) and this module serves that `dist/` as the cloud server's
//! **fallback** route: every JSON/API/OAuth/MCP/WS route is an explicit
//! service registered ahead of it (see [`configure_cloud_app`](crate::server)),
//! so the SPA fallback only ever handles a path no real route matched — it can
//! never shadow `/api/*`, `/oauth/*`, `/mcp`, `/ws`, `/health`, or `/ready`.
//!
//! Two responsibilities beyond "send a file":
//!
//! 1. **Base-domain injection.** The SPA tells an app host from a tenant
//!    subdomain by reading a `<meta name="atomic-cloud-base-domain">` tag the
//!    build leaves as a placeholder. This module rewrites that placeholder to
//!    the deployment's real base domain **once at startup**, holding the
//!    rewritten `index.html` in memory — so the client never falls back to
//!    host-guessing in production.
//!
//! 2. **SPA fallback.** A client-side-routed app must serve `index.html` for
//!    deep links (`/account/provider`, `/login`) that have no file on disk.
//!    A request for an **existing** file under `dist/` (a hashed asset, the
//!    favicon, a logo) is served as that file with a long-lived cache header;
//!    anything else returns the cached `index.html` (200, no-cache) so the
//!    router can take over. Requests are path-traversal-guarded — a resolved
//!    path that escapes the dist root is refused — so the fallback can never
//!    read outside the build output.
//!
//! 3. **The tenant dashboard gate** ([`AccountGate`]). The `/account/*`
//!    dashboard is the one part of the SPA that must not be served to a
//!    browser that isn't logged in. Registered as an explicit tenant-host
//!    scope ahead of the fallback, the gate verifies the session cookie
//!    server-side: a valid session serves the same shell the fallback would,
//!    anything else `302`s to the app-host login — so an unauthenticated
//!    navigation never flashes the dashboard chrome before client-bouncing.
//!    The data plane is untouched: `/api/*` is matched earlier (by
//!    `CloudAuth`), so an unauthenticated API call still returns the
//!    structured JSON `401`, never this redirect.
//!
//! The whole thing is **optional**: a deployment (or a test) that wires no
//! dist directory simply doesn't register the fallback or the gate, and
//! unmatched paths 404 as before. `serve` points it at `frontend/dist`; the
//! README documents producing that build.

use std::io;
use std::path::{Component, Path, PathBuf};

use actix_web::http::header::{
    CacheControl, CacheDirective, ContentType, HeaderValue, CONTENT_TYPE, LOCATION,
};
use actix_web::{guard, web, HttpRequest, HttpResponse};

use crate::auth::{subdomain_from_host, SESSION_COOKIE};
use crate::control_plane::ControlPlane;
use crate::error::CloudError;
use crate::tokens;

/// The meta-tag placeholder the build leaves in `index.html` for the server
/// to rewrite with the deployment's base domain. Must match the value in
/// `frontend/index.html` (and `frontend/src/lib/host.ts`).
const BASE_DOMAIN_PLACEHOLDER: &str = "__ATOMIC_CLOUD_BASE_DOMAIN__";

/// Placeholder in the PRODUCT app's `index.html` (repo-root `index.html`)
/// replaced with `true` when the cloud server serves the product app at a
/// tenant root, signalling its client to authenticate by the same-origin
/// session cookie. Left unreplaced (so it reads as not-a-cloud-tenant) in
/// self-hosted and Tauri builds, which never serve through here.
const PRODUCT_CLOUD_PLACEHOLDER: &str = "__ATOMIC_CLOUD_TENANT__";

/// Cache lifetime for hashed build assets (`dist/assets/*`). The filenames
/// carry a content hash, so they are safe to cache effectively forever; a new
/// build emits new names. One year, the conventional "immutable" maximum.
const ASSET_MAX_AGE_SECS: u32 = 31_536_000;

/// The built SPA, ready to serve as the cloud server's fallback route.
///
/// Holds the dist root (for on-disk assets) and the base-domain-injected
/// `index.html` in memory (for the SPA fallback). Cheap to clone into each
/// worker — `index_html` is an `Arc<str>`, `root` an `Arc<Path>`.
#[derive(Clone)]
pub struct SpaServer {
    root: std::sync::Arc<Path>,
    index_html: std::sync::Arc<str>,
    /// Normalized base domain, so the fallback can tell a tenant host
    /// (`<slug>.<base>`) from the app host and pick the right shell.
    base_domain: std::sync::Arc<str>,
    /// The optional product knowledge-base app (the dark atoms/canvas SPA,
    /// `dist-web`). When attached, a tenant-host navigation that isn't a real
    /// file and isn't `/account/*` serves THIS app's shell at the tenant root,
    /// so the dashboard's "Open knowledge base" link lands on the product app.
    /// Absent in production (nginx serves the product app at the tenant root)
    /// and in pure-account dev; present when `serve --product-dir` points at a
    /// built `dist-web`. Its assets share the `/assets/` path with the account
    /// SPA, but Vite content-hashes filenames so the two never collide — the
    /// fallback resolves an asset by checking both roots.
    product: Option<ProductApp>,
}

/// The product knowledge-base app served at the tenant root (dev convenience;
/// see [`SpaServer::product`]).
#[derive(Clone)]
struct ProductApp {
    root: std::sync::Arc<Path>,
    /// The product app's `index.html`, served as its SPA shell. Unlike the
    /// account SPA there is no base-domain meta to inject — the product app
    /// talks to its same-origin tenant API and needs no cloud config.
    index_html: std::sync::Arc<str>,
}

impl SpaServer {
    /// Load the SPA from `dist_dir`, injecting `base_domain` into the
    /// `index.html` base-domain meta placeholder.
    ///
    /// Fails if the directory has no `index.html` — a misconfigured serve
    /// (pointed at a directory that was never built) should surface at boot,
    /// not as a 404 on the first browser hit.
    pub async fn load(dist_dir: impl AsRef<Path>, base_domain: &str) -> Result<Self, CloudError> {
        let root = dist_dir.as_ref().to_path_buf();
        let index_path = root.join("index.html");
        let raw = tokio::fs::read_to_string(&index_path)
            .await
            .map_err(|source| CloudError::Io {
                context: format!(
                    "reading SPA index.html at {} (was `npm run build` run in \
                     crates/atomic-cloud/frontend?)",
                    index_path.display()
                ),
                source,
            })?;
        // Normalize the base domain the same way the client does (lowercase,
        // no leading dot) so the meta value is canonical regardless of how the
        // operator spelled `--base-domain`.
        let normalized = base_domain.trim_start_matches('.').to_lowercase();
        let index_html = raw.replace(BASE_DOMAIN_PLACEHOLDER, &normalized);
        Ok(Self {
            root: root.into(),
            index_html: index_html.into(),
            base_domain: normalized.into(),
            product: None,
        })
    }

    /// Attach the product knowledge-base app (its built `dist-web`) so the
    /// tenant root serves it (see [`Self::product`]). No-ops with a single
    /// warn when the directory has no `index.html` — a dev run that didn't
    /// build the product web bundle still boots, the "Open knowledge base"
    /// link just falls back to the account dashboard as before. A directory
    /// that exists but is unreadable is a hard error.
    pub async fn with_product_dir(
        mut self,
        dist_dir: impl AsRef<Path>,
    ) -> Result<Self, CloudError> {
        let root = dist_dir.as_ref().to_path_buf();
        let index_path = root.join("index.html");
        match tokio::fs::read_to_string(&index_path).await {
            Ok(raw) => {
                // Mark the served product app as a cloud tenant so its client
                // authenticates by the same-origin session cookie instead of
                // prompting for a server URL + token. Self-hosted/Tauri builds
                // never go through here, so their placeholder stays unreplaced
                // (and reads as "not a cloud tenant").
                if !raw.contains(PRODUCT_CLOUD_PLACEHOLDER) {
                    // The marker the client reads (`isCloudTenant()`) is gone,
                    // so the `true` injection below is a no-op and every tenant
                    // will land on the self-hosted setup screen instead of
                    // authenticating by the session cookie — tenant login is
                    // silently broken. This is the same failure mode a
                    // misconfigured reverse proxy hits when it serves the
                    // product bundle without injecting the marker (see
                    // DEPLOY.md → "Product-app tenant marker"); shout about it
                    // at boot rather than 404-ing a confused user's login.
                    tracing::warn!(
                        product_dir = %root.display(),
                        placeholder = PRODUCT_CLOUD_PLACEHOLDER,
                        "product app index.html is missing the cloud-tenant \
                         marker placeholder: tenant auth will silently fall \
                         back to the self-hosted setup screen. The product \
                         bundle must carry `<meta name=\"atomic-cloud-tenant\" \
                         content=\"{PRODUCT_CLOUD_PLACEHOLDER}\">` for the \
                         server (or a reverse proxy) to rewrite to `true`."
                    );
                }
                let index_html = raw.replace(PRODUCT_CLOUD_PLACEHOLDER, "true");
                self.product = Some(ProductApp {
                    root: root.into(),
                    index_html: index_html.into(),
                });
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                tracing::warn!(
                    product_dir = %root.display(),
                    "no built product app found (run `npm run build:web`); \
                     the tenant root serves the account dashboard only"
                );
            }
            Err(source) => {
                return Err(CloudError::Io {
                    context: format!("reading product index.html at {}", index_path.display()),
                    source,
                });
            }
        }
        Ok(self)
    }

    /// Build the [`SpaServer`] only if `dist_dir` exists and carries an
    /// `index.html`; otherwise `Ok(None)`.
    ///
    /// `serve` uses this so a deployment without a built frontend (a
    /// pure-API pod, a dev run that hasn't run `npm run build`) boots cleanly
    /// with the SPA fallback simply absent, rather than failing. A directory
    /// that exists but is missing `index.html` is treated as "not built" — the
    /// same `None`, with a single loud log at the call site.
    pub async fn load_optional(
        dist_dir: impl AsRef<Path>,
        base_domain: &str,
    ) -> Result<Option<Self>, CloudError> {
        let index_path = dist_dir.as_ref().join("index.html");
        match tokio::fs::try_exists(&index_path).await {
            Ok(true) => Self::load(dist_dir, base_domain).await.map(Some),
            Ok(false) => Ok(None),
            Err(source) => Err(CloudError::Io {
                context: format!("probing for SPA index.html at {}", index_path.display()),
                source,
            }),
        }
    }

    /// The base-domain-injected `index.html`, for the fallback and tests.
    pub fn index_html(&self) -> &str {
        &self.index_html
    }

    /// Register this server as the app's fallback (`default_service`). Must be
    /// called **last** in `configure`, after every explicit route — actix
    /// matches explicit services first and only falls through to the default
    /// service when none matched.
    pub fn configure_fallback(self, cfg: &mut web::ServiceConfig) {
        cfg.app_data(web::Data::new(self))
            .default_service(web::route().to(serve_spa));
    }
}

/// The session gate in front of the tenant dashboard (`/account/*`).
///
/// The dashboard is the one part of the SPA that must not be served to a
/// browser that isn't logged in: it would flash an empty shell, fire an
/// `/api/account/overview` request, eat a 401, and only *then* client-bounce
/// to the login page. Instead this gate runs **server-side**, ahead of the
/// SPA fallback: a tenant-host `GET /account/*` with no valid session is a
/// `302` straight to the app-host login (`https://app.<base>/login`) — the
/// browser never renders the dashboard chrome for an unauthenticated user.
///
/// It guards only HTML *navigations* to `/account/*`. The data plane keeps
/// its own contract untouched: `/api/*` is matched first (by `CloudAuth`),
/// so an unauthenticated `/api/account/overview` still returns the structured
/// JSON `401`, never this redirect — exactly as an API client (or the
/// dashboard's own background fetch after a session expiry) expects.
///
/// A valid session serves the same SPA shell the fallback would: the gate
/// only decides *whether* to serve it, and delegates the bytes to the held
/// [`SpaServer`]. Resolution is fail-closed — an unknown subdomain, a missing
/// or expired cookie, or a control-plane hiccup all redirect to login rather
/// than reveal the dashboard or leak whether the subdomain names a real
/// account.
#[derive(Clone)]
pub struct AccountGate {
    spa: SpaServer,
    control: ControlPlane,
    /// Normalized base domain (`atomic.cloud`, or `localhost` in dev), for
    /// building the app-host login URL the unauthenticated case redirects to.
    base_domain: std::sync::Arc<str>,
    /// Scheme of the public origin (`https` in prod, `http` for local/dev) —
    /// the redirect target's scheme, matching what `CloudAuth` and the OAuth
    /// plane build their URLs with.
    public_scheme: std::sync::Arc<str>,
}

impl AccountGate {
    /// Build the gate from the loaded SPA plus the control plane and origin
    /// settings the redirect/lookup need. `base_domain` is normalized
    /// (lowercased, leading dot stripped) here so callers can pass the raw
    /// `--base-domain` value.
    pub fn new(
        spa: SpaServer,
        control: ControlPlane,
        base_domain: &str,
        public_scheme: &str,
    ) -> Self {
        Self {
            spa,
            control,
            base_domain: base_domain
                .trim_start_matches('.')
                .to_ascii_lowercase()
                .into(),
            public_scheme: public_scheme.to_ascii_lowercase().into(),
        }
    }

    /// Register the tenant-host `/account/*` scope ahead of the SPA fallback.
    ///
    /// Guarded to tenant subdomains (the dashboard never exists on the app
    /// host) and to `GET`/`HEAD` (a navigation, not an API call). Must be
    /// registered **before** [`SpaServer::configure_fallback`] so the gate
    /// wins the match for `/account/*`; everything else — assets, app-host
    /// pages — still falls through to the unguarded fallback.
    pub fn configure(self, cfg: &mut web::ServiceConfig) {
        let base_for_guard = self.base_domain.to_string();
        cfg.service(
            web::scope("/account")
                .guard(tenant_host_guard(base_for_guard))
                .guard(guard::Any(guard::Get()).or(guard::Head()))
                .app_data(web::Data::new(self))
                // `""` is `/account`, `"/{tail:.*}"` is every deeper path.
                .route("", web::route().to(serve_account))
                .route("/{tail:.*}", web::route().to(serve_account)),
        );
    }

    /// The app-host login URL an unauthenticated dashboard request bounces to.
    fn login_redirect_url(&self) -> String {
        login_redirect_url(&self.public_scheme, &self.base_domain)
    }

    /// Whether `req` carries a session cookie that verifies against the
    /// account named by the request's `Host` subdomain. Fail-closed: any
    /// missing piece (no host, unknown/inactive subdomain, no cookie, expired
    /// session, lookup error) is `false`.
    async fn has_valid_session(&self, req: &HttpRequest) -> bool {
        let Some(host) = req
            .headers()
            .get(actix_web::http::header::HOST)
            .and_then(|v| v.to_str().ok())
            .or_else(|| req.uri().host())
        else {
            return false;
        };
        let Some(subdomain) = subdomain_from_host(host, &self.base_domain) else {
            return false;
        };
        let Some(cookie) = req.cookie(SESSION_COOKIE) else {
            return false;
        };

        // Resolve the subdomain to an active account, then verify the session
        // against it — the same `account_id`-scoped check `CloudAuth` makes,
        // so a session for another tenant (the cookie crosses subdomains)
        // resolves nothing here either.
        let account_id: Option<(String,)> = match sqlx::query_as(
            "SELECT id FROM accounts WHERE subdomain = $1 AND status = 'active'",
        )
        .bind(&subdomain)
        .fetch_optional(self.control.pool())
        .await
        {
            Ok(row) => row,
            Err(e) => {
                tracing::error!(error = %e, subdomain, "account-gate lookup failed");
                return false;
            }
        };
        let Some((account_id,)) = account_id else {
            return false;
        };

        match tokens::verify_session(&self.control, &account_id, cookie.value()).await {
            Ok(Some(_)) => true,
            Ok(None) => false,
            Err(e) => {
                tracing::error!(error = %e, account_id, "account-gate session verify failed");
                false
            }
        }
    }
}

/// The `/account/*` handler: serve the SPA shell for a logged-in browser, or
/// `302` to the app-host login for everyone else.
async fn serve_account(req: HttpRequest, gate: web::Data<AccountGate>) -> HttpResponse {
    if gate.has_valid_session(&req).await {
        return spa_shell_response(gate.spa.index_html());
    }
    // No session → bounce to the app-host login. `302` (not `307`) because the
    // browser must re-issue as a plain `GET` of the login page, and the
    // dashboard navigation that triggered this is always itself a `GET`.
    HttpResponse::Found()
        .insert_header((LOCATION, gate.login_redirect_url()))
        .insert_header(CacheControl(vec![CacheDirective::NoCache]))
        .finish()
}

/// The app-host login URL the gate redirects an unauthenticated dashboard
/// navigation to: `<scheme>://app.<base>/login`. Free function so the
/// construction is unit-testable without standing up a control plane.
fn login_redirect_url(public_scheme: &str, base_domain: &str) -> String {
    format!("{public_scheme}://app.{base_domain}/login")
}

/// Whether `host` is a tenant subdomain under `base_domain` — the predicate
/// the gate's route guard applies. A host that resolves no single-label
/// subdomain below the base (the bare base, a deep name, a foreign host) is
/// not a tenant. The reserved `app` label — the public app host — is excluded
/// explicitly so the gate never fires on `app.<base>` (matching the frontend's
/// host split and CloudAuth's `app`-is-never-an-account invariant); an
/// `app.<base>/account/*` request thus falls through to the SPA fallback
/// rather than the dashboard gate.
fn host_is_tenant(host: &str, base_domain: &str) -> bool {
    subdomain_from_host(host, base_domain).is_some_and(|sub| sub != "app")
}

/// Route guard matching only **tenant** subdomains (`<slug>.<base>`), the
/// mirror of the account plane's app-host guard. Reads the same host source
/// as `CloudAuth` (the `Host` header, falling back to the URI authority).
fn tenant_host_guard(base_domain: String) -> impl guard::Guard {
    guard::fn_guard(move |ctx: &guard::GuardContext<'_>| {
        let head = ctx.head();
        head.headers()
            .get(actix_web::http::header::HOST)
            .and_then(|v| v.to_str().ok())
            .or_else(|| head.uri.host())
            .is_some_and(|host| host_is_tenant(host, &base_domain))
    })
}

/// The fallback handler: serve an existing build file, or the SPA shell.
///
/// Only `GET`/`HEAD` are served as the SPA; any other method on an unmatched
/// path is a genuine 404 (a `POST` to a nonexistent API route is a client
/// error, not a page navigation). This keeps the fallback from masking a
/// mistyped API call as an HTML 200.
async fn serve_spa(req: HttpRequest, spa: web::Data<SpaServer>) -> HttpResponse {
    if !matches!(
        *req.method(),
        actix_web::http::Method::GET | actix_web::http::Method::HEAD
    ) {
        return HttpResponse::NotFound().json(serde_json::json!({ "error": "not_found" }));
    }

    // `index.html` is NEVER served from disk. Both shells carry boot-time
    // rewrites — the account SPA's base-domain meta, the product app's
    // cloud-tenant marker — so the raw build file is always wrong. Concretely:
    // the product PWA's service worker precaches `/index.html` as its
    // navigation fallback, and serving the account dist's raw file here (it
    // matched first) poisoned every tenant navigation with the dashboard
    // document. An explicit `index.html` request falls through to the same
    // host-appropriate in-memory shell as any deep link.
    let wants_index = req.path().ends_with("/index.html");

    // Try to serve an existing build file. Check the account dist first, then
    // the product dist (when attached): the two share the `/assets/` path but
    // Vite content-hashes filenames, so a given asset lives in exactly one of
    // them and "check both" resolves it without collision. Anything that isn't
    // a real file (a deep link) falls through to a shell below.
    let product_root = spa.product.as_ref().map(|p| p.root.as_ref());
    for root in std::iter::once(spa.root.as_ref()).chain(product_root) {
        if wants_index {
            break;
        }
        if let Some(resolved) = resolve_asset_path(root, req.path()) {
            match tokio::fs::read(&resolved).await {
                Ok(bytes) => return asset_response(&resolved, bytes),
                // Resolved inside a dist root but isn't a file (a directory, or
                // a deep link that coincidentally matches a real subdir name):
                // keep looking, then fall through to the shell.
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => {
                    tracing::error!(path = req.path(), error = %e, "serving SPA asset failed");
                    return HttpResponse::InternalServerError()
                        .json(serde_json::json!({ "error": "asset_read_failed" }));
                }
            }
        }
    }

    // No file matched → serve a SPA shell. On a tenant host with the product
    // app attached, the tenant root belongs to the product knowledge base
    // (the dashboard lives behind the `/account/*` gate, matched earlier), so
    // serve the product shell. The app host — and any tenant host without a
    // product app — gets the account SPA shell.
    if let Some(product) = &spa.product {
        if request_host(&req).is_some_and(|h| host_is_tenant(h, &spa.base_domain)) {
            return spa_shell_response(&product.index_html);
        }
    }
    spa_shell_response(spa.index_html())
}

/// The request's host (`Host` header, falling back to the URI authority),
/// the same source [`AccountGate`] and `CloudAuth` read.
fn request_host(req: &HttpRequest) -> Option<&str> {
    req.headers()
        .get(actix_web::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .or_else(|| req.uri().host())
}

/// Resolve a request path to a candidate file under `root`, or `None` when the
/// path is the SPA root (`/`) or escapes the dist directory.
///
/// Traversal-guarded: the path is split into components, `..` and absolute
/// roots are rejected outright (rather than popped), so no request can address
/// a file outside the build output even through an encoded or layered `..`.
fn resolve_asset_path(root: &Path, request_path: &str) -> Option<PathBuf> {
    let trimmed = request_path.trim_start_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    let rel = Path::new(trimmed);
    let mut safe = PathBuf::new();
    for component in rel.components() {
        match component {
            Component::Normal(part) => safe.push(part),
            // Anything that isn't a plain path segment — `..`, a root, a
            // Windows prefix, a bare `.` — disqualifies the whole path: a
            // request that tries to climb out of dist gets the shell, never a
            // file outside it.
            _ => return None,
        }
    }
    Some(root.join(safe))
}

/// A 200 for an existing build file, with a content type from its extension
/// and a cache header keyed on whether it's a hashed asset.
fn asset_response(path: &Path, bytes: Vec<u8>) -> HttpResponse {
    let mut builder = HttpResponse::Ok();
    if let Some(content_type) = content_type_for(path) {
        builder.insert_header((CONTENT_TYPE, content_type));
    }
    // Files under `assets/` carry a content hash in their name → immutable,
    // cache hard. Everything else (favicon, logos) gets a short cache.
    let is_hashed_asset = path
        .components()
        .any(|c| matches!(c, Component::Normal(part) if part == "assets"));
    if is_hashed_asset {
        builder.insert_header(CacheControl(vec![
            CacheDirective::Public,
            CacheDirective::MaxAge(ASSET_MAX_AGE_SECS),
            CacheDirective::Extension("immutable".to_string(), None),
        ]));
    } else {
        builder.insert_header(CacheControl(vec![
            CacheDirective::Public,
            CacheDirective::MaxAge(3600),
        ]));
    }
    builder.body(bytes)
}

/// The SPA shell (`index.html`) response: 200, HTML, explicitly **not**
/// cached, so a deploy's new asset hashes are always picked up (the shell is
/// the one file whose name doesn't change between builds).
fn spa_shell_response(index_html: &str) -> HttpResponse {
    HttpResponse::Ok()
        .insert_header(ContentType::html())
        .insert_header(CacheControl(vec![CacheDirective::NoCache]))
        .body(index_html.to_string())
}

/// A minimal extension → `Content-Type` map for the file kinds a Vite build
/// emits. Unknown extensions get no header (actix/browsers sniff), which is
/// fine for the SPA's narrow output set.
fn content_type_for(path: &Path) -> Option<HeaderValue> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())?
        .to_ascii_lowercase();
    let value = match ext.as_str() {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ttf" => "font/ttf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "txt" => "text/plain; charset=utf-8",
        "map" => "application/json",
        _ => return None,
    };
    HeaderValue::from_static(value).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_rejects_traversal_and_root() {
        let root = Path::new("/srv/dist");
        // The SPA root has no file — it's the shell.
        assert_eq!(resolve_asset_path(root, "/"), None);
        assert_eq!(resolve_asset_path(root, ""), None);
        // Traversal in any form is refused (→ shell, never a file outside).
        assert_eq!(resolve_asset_path(root, "/../secret"), None);
        assert_eq!(resolve_asset_path(root, "/assets/../../etc/passwd"), None);
        assert_eq!(resolve_asset_path(root, "/a/../../b"), None);
        // A normal asset path resolves under the root.
        assert_eq!(
            resolve_asset_path(root, "/assets/index-abc123.js"),
            Some(PathBuf::from("/srv/dist/assets/index-abc123.js"))
        );
        assert_eq!(
            resolve_asset_path(root, "/favicon.svg"),
            Some(PathBuf::from("/srv/dist/favicon.svg"))
        );
    }

    #[test]
    fn content_types_cover_vite_output() {
        for (file, expected) in [
            ("index.html", "text/html; charset=utf-8"),
            ("assets/index-abc.js", "text/javascript; charset=utf-8"),
            ("assets/index-abc.css", "text/css; charset=utf-8"),
            ("logo.svg", "image/svg+xml"),
            ("font.woff2", "font/woff2"),
        ] {
            let ct = content_type_for(Path::new(file)).expect("known type");
            assert_eq!(ct.to_str().unwrap(), expected, "{file}");
        }
        assert!(content_type_for(Path::new("weird.xyz")).is_none());
    }

    #[tokio::test]
    async fn load_injects_base_domain_into_meta() {
        let dir = tempfile::tempdir().expect("tempdir");
        tokio::fs::write(
            dir.path().join("index.html"),
            format!(
                r#"<meta name="atomic-cloud-base-domain" content="{BASE_DOMAIN_PLACEHOLDER}" />"#
            ),
        )
        .await
        .expect("write index");

        let spa = SpaServer::load(dir.path(), ".Atomic.Cloud")
            .await
            .expect("load spa");
        assert!(
            spa.index_html().contains(r#"content="atomic.cloud""#),
            "base domain normalized + injected: {}",
            spa.index_html()
        );
        assert!(
            !spa.index_html().contains(BASE_DOMAIN_PLACEHOLDER),
            "placeholder fully replaced"
        );
    }

    #[test]
    fn account_gate_host_split_and_login_url() {
        // Only a single-label subdomain under the base is a tenant; the bare
        // base, `app.<base>`, and foreign hosts are not (so the gate never
        // fires on the app host). Ports are tolerated.
        assert!(host_is_tenant("alpha.atomic.cloud", "atomic.cloud"));
        assert!(host_is_tenant("alpha.atomic.cloud:8080", "atomic.cloud"));
        assert!(!host_is_tenant("atomic.cloud", "atomic.cloud"));
        assert!(!host_is_tenant("app.atomic.cloud", "atomic.cloud"));
        assert!(!host_is_tenant("a.b.atomic.cloud", "atomic.cloud"));
        assert!(!host_is_tenant("evil.com", "atomic.cloud"));
        // Local/dev base: `kenny.localhost` is a tenant, bare `localhost` not.
        assert!(host_is_tenant("kenny.localhost", "localhost"));
        assert!(!host_is_tenant("localhost", "localhost"));

        // The redirect target is the app-host login over the public scheme.
        assert_eq!(
            login_redirect_url("https", "atomic.cloud"),
            "https://app.atomic.cloud/login"
        );
        assert_eq!(
            login_redirect_url("http", "localhost"),
            "http://app.localhost/login"
        );
    }

    #[tokio::test]
    async fn load_optional_absent_when_unbuilt() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Directory exists but has no index.html → treated as "not built".
        let spa = SpaServer::load_optional(dir.path(), "atomic.cloud")
            .await
            .expect("probe");
        assert!(spa.is_none(), "no index.html → no SPA server");
    }

    /// Build a minimal account SPA in a temp dir, to attach a product app to.
    async fn account_spa() -> (tempfile::TempDir, SpaServer) {
        let dir = tempfile::tempdir().expect("tempdir");
        tokio::fs::write(dir.path().join("index.html"), "<html>account</html>")
            .await
            .expect("write account index");
        let spa = SpaServer::load(dir.path(), "atomic.cloud")
            .await
            .expect("load account spa");
        (dir, spa)
    }

    #[tokio::test]
    async fn with_product_dir_injects_tenant_marker() {
        let (_account_dir, spa) = account_spa().await;
        let product = tempfile::tempdir().expect("tempdir");
        tokio::fs::write(
            product.path().join("index.html"),
            format!(r#"<meta name="atomic-cloud-tenant" content="{PRODUCT_CLOUD_PLACEHOLDER}" />"#),
        )
        .await
        .expect("write product index");

        let spa = spa
            .with_product_dir(product.path())
            .await
            .expect("attach product");
        let product_html = &spa.product.expect("product attached").index_html;
        assert!(
            product_html.contains(r#"content="true""#),
            "tenant marker injected: {product_html}"
        );
        assert!(
            !product_html.contains(PRODUCT_CLOUD_PLACEHOLDER),
            "placeholder fully replaced"
        );
    }

    #[tokio::test]
    async fn with_product_dir_attaches_even_without_marker() {
        // A product bundle whose marker placeholder was stripped (e.g. a proxy
        // that already rewrote it, or a stale build) still attaches — the
        // server warns at serve time (OPS-2) rather than refusing to boot, so
        // the operator sees the misconfiguration instead of a hard crash.
        let (_account_dir, spa) = account_spa().await;
        let product = tempfile::tempdir().expect("tempdir");
        tokio::fs::write(product.path().join("index.html"), "<html>no marker</html>")
            .await
            .expect("write product index");

        let spa = spa
            .with_product_dir(product.path())
            .await
            .expect("attach product despite missing marker");
        assert!(spa.product.is_some(), "product still attached");
    }
}
