//! Plan-tier resource enforcement on the data plane (plan: "Observability,
//! quotas, billing" → "Quotas" → "Enforcement points").
//!
//! Cloud cannot touch atomic-server's route handlers (the one-way dependency
//! rule), so resource limits are enforced the way slice 2 marked dispatch
//! hints and slice 4 capped chat streams: a cloud middleware on `api_scope`
//! that path-matches the relevant mutating routes and checks the plan's
//! limit **before** delegating, returning the structured error itself. The
//! handler is never reached on a quota hit, so no atom is created and no
//! ledger work is enqueued.
//!
//! # What's enforced, and how the count is read
//!
//! | Route                                   | Limit       | Count source                        |
//! |-----------------------------------------|-------------|-------------------------------------|
//! | `POST /api/atoms`                       | `atom_limit`| account-wide live atom sum          |
//! | `POST /api/atoms/bulk`                  | `atom_limit`| account-wide sum + the request batch|
//! | `POST /api/ingest/url`                  | `atom_limit`| account-wide sum + 1                |
//! | `POST /api/ingest/urls`                 | `atom_limit`| account-wide sum + the request batch|
//! | `POST /api/import/obsidian`             | `atom_limit`| account-wide sum + 1 (per-note delta)|
//! | `POST /api/databases`                   | `kb_limit`  | `DatabaseManager::list_databases()` |
//!
//! URL ingestion and Obsidian import create atoms exactly like the
//! `/api/atoms` routes, so they count against `atom_limit` too — an account at
//! its ceiling can't slip past the gate by ingesting or importing instead of
//! creating (the plan's enforcement table lists "Atom create" generically; all
//! these surfaces grow the atom count).
//!
//! The atom count is **account-wide**: it sums `count_atoms()` across every
//! knowledge base in the tenant, not just the one a request targets, because
//! the limit is an account ceiling (plan). Counting only the targeted KB would
//! let a tenant on a finite-atom plan with `kb_limit > 1` evade the ceiling by
//! spreading atoms across KBs. This matches
//! [`account_over_plan_limits`]' semantics; the KB list is small, so it stays
//! cheap. The KB count is `DatabaseManager::list_databases()` length.
//!
//! Both counts are read **live** from the tenant database at enforcement
//! time — cheap, single-statement per KB, strongly consistent. There is no
//! stored atom/KB counter to drift (the `quota_usage` table is for metrics
//! that can't be counted cheaply live; see [`crate::plans`]). A `NULL` limit
//! means unlimited and the guard passes the request straight through —
//! the count is never even read.
//!
//! # Cost bound on the create hot path
//!
//! The atom gate adds tenant-database round-trips before the handler runs
//! (one `count_atoms()` per knowledge base). These ride the tenant pool's
//! connection acquire, which is bounded by the pool's `acquire_timeout`
//! (10s; see [`crate::account_cache`] / the cluster pool config) — there is
//! no statement-level timeout on the count itself, as that would require an
//! atomic-core change (the one-way dependency rule forbids teaching core
//! about cloud), and the count is a trivially-indexed aggregate. The acquire
//! timeout is the operative upper bound on how long the gate can delay a
//! create before failing it with an operational error.
//!
//! # The live-count gate is a soft ceiling, not a reservation
//!
//! The check reads the live count and admits if `current + delta <= limit`
//! with no row lock or pre-reservation — a deliberate trade (deviation log
//! below) for a cheap, drift-free counter over a contended `quota_usage`
//! UPSERT. The residual is a TOCTOU: two concurrent creates at
//! `current = limit - 1` can both observe the pre-write count and both admit,
//! landing the tenant one over the ceiling. This is acceptable for a soft
//! resource limit (the overshoot is bounded by concurrency, self-heals as
//! the count is re-read on the next write, and never affects money or
//! managed-key credits — those ride the AI ledger, not this gate). If a hard
//! cap is ever required, this is the seam to swap in a `SELECT … FOR UPDATE`
//! reservation against a stored counter.
//!
//! # The bulk batch delta
//!
//! A bulk create can push the tenant over the limit with a single request,
//! so the guard accounts for the batch: it admits only if
//! `current + batch_size <= limit`. Reading `batch_size` means reading the
//! request body in the middleware, which would consume the payload the
//! handler needs — so the guard buffers the body, counts the array's
//! elements, and **re-injects** the exact bytes before delegating
//! ([`peek_and_replay_json_array_len`]). The single-atom route needs no body
//! read (its delta is always 1).
//!
//! # Quota-exceeded response shape
//!
//! Exactly the plan's contract:
//!
//! ```json
//! { "error": "quota_exceeded",
//!   "metric": "atoms",
//!   "current": 100,
//!   "limit": 100,
//!   "resets_at": null,
//!   "upgrade_url": "https://app.<base>/account/billing" }
//! ```
//!
//! `resets_at` is `null` for resource limits — they don't reset on a clock,
//! they clear when the user deletes data or upgrades (plan: "Downgrade …
//! over-limit usage retained but writes blocked until under"). `upgrade_url`
//! is derived from the request host (`<sub>.<base>` → `https://app.<base>/account/billing`),
//! the same derivation the out-of-credits guard uses.

use actix_web::body::{BoxBody, MessageBody};
use actix_web::dev::{Payload, ServiceRequest, ServiceResponse};
use actix_web::http::{header, Method};
use actix_web::middleware::Next;
use actix_web::web::{self, Bytes};
use actix_web::{FromRequest, HttpMessage, HttpResponse};
use atomic_core::DatabaseManager;
use atomic_server::db_extractor::RequestDatabaseManager;

use crate::auth::ResolvedTenant;
use crate::plans::{Plan, PlanRegistry};

/// Which resource a mutating data-plane route consumes — the unit a quota
/// check is denominated in. `None` for routes the guard ignores.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QuotaTarget {
    /// `POST /api/atoms` or `POST /api/ingest/url` — one atom.
    Atom,
    /// `POST /api/atoms/bulk` — N atoms; the batch size is the top-level JSON
    /// array's length.
    AtomBulk,
    /// `POST /api/ingest/urls` — N atoms; the batch size is the length of the
    /// request body's `urls` array.
    IngestUrls,
    /// `POST /api/databases` — one knowledge base.
    Kb,
    /// `POST /mcp` — a JSON-RPC envelope whose resource cost can only be known
    /// from the body: a `tools/call` for the `create_atom` tool creates one
    /// atom, everything else (`initialize`, `tools/list`, a read tool, …)
    /// creates nothing. The body is peeked+replayed to classify, then it rides
    /// the single-atom delta like the other atom-creating routes. See
    /// [`mcp_creates_atom`].
    Mcp,
}

/// Classify a `(method, path)` into the resource it consumes, or `None` if
/// the guard doesn't enforce it. Exact-path matches: only the collection
/// `POST`s create resources (`/api/atoms/{id}` is an update, `/api/databases/{id}`
/// a rename — neither grows the count). The ingestion routes create atoms too,
/// so they count against `atom_limit` alongside the `/api/atoms` routes.
fn quota_target(method: &Method, path: &str) -> Option<QuotaTarget> {
    if *method != Method::POST {
        return None;
    }
    match path {
        // `/api/import/obsidian` creates one atom per vault note. Its batch
        // size lives on the server-side filesystem (`vault_path`), not in the
        // request body, so it can't be counted ahead of time the way the bulk
        // routes are — it rides the single-atom delta (admits only while the
        // tenant is at least one atom under the ceiling). Practical cloud
        // impact is currently low (the vault path is server-side and not a
        // surface a hosted tenant can drive a large vault through), but it is
        // a real atom-creating route and a bypass alias for `/api/atoms`, so
        // it is gated rather than left fallback-unbound.
        "/api/atoms" | "/api/ingest/url" | "/api/import/obsidian" => Some(QuotaTarget::Atom),
        "/api/atoms/bulk" => Some(QuotaTarget::AtomBulk),
        "/api/ingest/urls" => Some(QuotaTarget::IngestUrls),
        "/api/databases" => Some(QuotaTarget::Kb),
        // Manual-trigger routes carry a dynamic `{id}` segment, so they fall
        // through the exact-path arm. `POST /api/reports/{id}/run` writes a
        // finding atom and `POST /api/feeds/{id}/poll` ingests feed-entry
        // atoms — the same atom-creating work the dispatcher's atom-limit gate
        // defers for *scheduled* runs. Gating the manual trigger closes the
        // at-ceiling evasion (a user clicking "run"/"poll now" past the limit).
        // Single-atom delta: a poll's entry count isn't known ahead of time, so
        // these admit only while the tenant is at least one atom under the ceiling.
        _ if path.starts_with("/api/reports/") && path.ends_with("/run") => Some(QuotaTarget::Atom),
        _ if path.starts_with("/api/feeds/") && path.ends_with("/poll") => Some(QuotaTarget::Atom),
        // The per-tenant MCP Streamable HTTP surface (`POST /mcp`). Every
        // JSON-RPC call is a POST to this one path; only a `tools/call` for
        // `create_atom` actually creates a resource, so the body is inspected
        // in the guard to decide whether the atom delta applies (the path
        // alone can't tell a `create_atom` from an `initialize`). Without this,
        // an MCP client could create atoms past the plan ceiling that the REST
        // gate enforces — the same atom-creating work, a different transport.
        "/mcp" => Some(QuotaTarget::Mcp),
        _ => None,
    }
}

/// Whether an MCP JSON-RPC request body is a `tools/call` invoking the
/// `create_atom` tool — the one MCP method that creates an atom and so must
/// charge the atom quota. Returns `false` for any other method, a batch
/// (JSON-RPC array) the SDK doesn't issue for tool calls, or an unparseable
/// body (the MCP transport will surface its own error; we simply don't charge
/// quota for something we can't confirm creates an atom).
///
/// The tool name mirrors `atomic-server`'s `create_atom` MCP tool. Counting is
/// shape-based, not field-materializing — the `arguments` object is ignored.
fn mcp_creates_atom(bytes: &[u8]) -> bool {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return false;
    };
    value.get("method").and_then(|m| m.as_str()) == Some("tools/call")
        && value
            .get("params")
            .and_then(|p| p.get("name"))
            .and_then(|n| n.as_str())
            == Some("create_atom")
}

/// Data-plane middleware enforcing plan-tier resource limits (module docs).
/// Wired inside CloudAuth and the plane guard, so [`ResolvedTenant`] and the
/// tenant manager are always installed; a missing extension is skipped
/// defensively (the plane guard already fails such requests closed). Runs
/// *outside* the dispatch-hint writer so a quota denial never marks a hint.
pub async fn quota_guard(
    registry: web::Data<PlanRegistry>,
    req: ServiceRequest,
    next: Next<impl MessageBody + 'static>,
) -> Result<ServiceResponse<BoxBody>, actix_web::Error> {
    let Some(target) = quota_target(req.method(), req.path()) else {
        return next.call(req).await.map(|res| res.map_into_boxed_body());
    };

    // Resolve the account + tenant manager off the extensions CloudAuth
    // installed. A request lacking them is a composition bug the plane guard
    // already fails closed — skip defensively rather than block. The
    // extensions `Ref` is fully scoped to this read so it is never held
    // across an await.
    let resolved = {
        let ext = req.extensions();
        let account_id = ext
            .get::<ResolvedTenant>()
            .map(|t| t.principal.account_id.clone());
        let manager = ext.get::<RequestDatabaseManager>().map(|m| m.0.clone());
        account_id.zip(manager)
    };
    let Some((account_id, manager)) = resolved else {
        return next.call(req).await.map(|res| res.map_into_boxed_body());
    };

    let plan = match registry.for_account(&account_id).await {
        Ok(plan) => plan,
        Err(e) => {
            tracing::error!(account_id, error = %e, "resolving plan for quota check failed");
            return Ok(req.into_response(internal_error()));
        }
    };

    // The bulk route's batch size must be read before the limit branch so
    // the body is buffered+replayed regardless of whether the plan is
    // unlimited (a one-shot read; replaying keeps the handler whole).
    let mut req = req;
    let delta: i64 = match target {
        QuotaTarget::Atom | QuotaTarget::Kb => 1,
        // The batch size is read from the body and the bytes are replayed so
        // the handler reads an untouched payload. `/api/atoms/bulk` is a
        // top-level array; `/api/ingest/urls` wraps its batch in a `urls`
        // field. An unreadable/wrong-shaped body isn't ours to reject — let
        // the handler return its own deserialization 400. A zero-length batch
        // creates nothing, so it can never exceed a limit.
        QuotaTarget::AtomBulk => match peek_and_replay_batch_len(&mut req, None).await {
            Ok(n) => n as i64,
            Err(()) => 0,
        },
        QuotaTarget::IngestUrls => match peek_and_replay_batch_len(&mut req, Some("urls")).await {
            Ok(n) => n as i64,
            Err(()) => 0,
        },
        // The MCP JSON-RPC body is peeked+replayed: a `create_atom` tool call
        // charges one atom; every other method (handshake, listing, a read
        // tool) charges nothing and passes straight through. A body we can't
        // read isn't ours to reject — the MCP transport surfaces its own
        // error.
        //
        // TODO: this charges the single-atom delta per `create_atom` call. If
        // the MCP surface ever grows a *batch* create tool (or per-tool
        // metering beyond the atom ceiling — e.g. managed-AI credits for a
        // tool that calls the LLM), this is the seam to read the per-tool
        // delta from `params.arguments` rather than assuming 1.
        QuotaTarget::Mcp => {
            if peek_and_replay_creates_atom(&mut req).await {
                1
            } else {
                0
            }
        }
    };

    // A zero delta creates nothing, so it can never exceed a limit — admit it
    // without reading the (account-wide) count. This keeps non-creating MCP
    // traffic (every handshake, `tools/list`, and read tool) and empty batches
    // off the count hot path, and means an already-over-limit tenant's reads
    // are never spuriously 402'd by `current > limit`.
    if delta == 0 {
        return next.call(req).await.map(|res| res.map_into_boxed_body());
    }

    match check_resource(target, &plan, &manager, &req, delta).await {
        Ok(None) => next.call(req).await.map(|res| res.map_into_boxed_body()),
        Ok(Some(denial)) => Ok(req.into_response(denial)),
        Err(e) => {
            tracing::error!(account_id, error = %e, "reading resource count for quota check failed");
            Ok(req.into_response(internal_error()))
        }
    }
}

/// Whether an account is currently over `plan`'s resource limits, reading the
/// live atom and KB counts straight from the tenant `manager` — the same
/// strongly-consistent live reads the request-time guard uses, just summed
/// across the whole tenant rather than the one KB a request targets.
///
/// Used by the trial auto-downgrade and the subscription-deleted path (plan:
/// "Drops to free plan; if over free limits, read-only until under"): a
/// shrinking plan can't reject already-stored data (it is **retained, never
/// deleted**), so the account goes `read_only` until the user brings it back
/// under. A `NULL` limit on an axis is unlimited and never contributes to
/// over-limit. The atom count is summed over **every** knowledge base in the
/// tenant (the limit is an account-wide ceiling, not per-KB); the KB count is
/// the number of knowledge bases.
pub async fn account_over_plan_limits(
    plan: &Plan,
    manager: &DatabaseManager,
) -> Result<bool, atomic_core::AtomicCoreError> {
    if let Some(kb_limit) = plan.kb_limit {
        let kbs = manager.list_databases().await?.0;
        if kbs.len() as i64 > i64::from(kb_limit) {
            return Ok(true);
        }
    }
    account_over_atom_limit(plan, manager).await
}

/// Whether the account is over `plan`'s **atom** ceiling alone, summed across
/// every knowledge base — the atom axis of [`account_over_plan_limits`],
/// pulled out so the background dispatcher can gate atom-creating work without
/// re-checking the KB count (background work never creates a KB). A `NULL`
/// `atom_limit` is unlimited and never over. The limit is account-wide, so
/// the count is the sum over all KBs.
pub async fn account_over_atom_limit(
    plan: &Plan,
    manager: &DatabaseManager,
) -> Result<bool, atomic_core::AtomicCoreError> {
    let Some(atom_limit) = plan.atom_limit else {
        return Ok(false);
    };
    Ok(count_account_atoms(manager).await? > i64::from(atom_limit))
}

/// Whether the account has **no room for another atom** under `plan` — its
/// account-wide atom count is at or above the ceiling (`count >= limit`), so a
/// single new atom would overshoot. A `NULL` `atom_limit` is unlimited and
/// always has room.
///
/// This is the *background-dispatch* gate (distinct from
/// [`account_over_atom_limit`]'s strictly-over "already exceeds" semantics,
/// which drives the downgrade read-only decision): the dispatcher must defer
/// atom-creating work the moment the next atom would breach the ceiling, the
/// same boundary the request-time guard enforces with `current + 1 <= limit`.
pub async fn account_atom_limit_reached(
    plan: &Plan,
    manager: &DatabaseManager,
) -> Result<bool, atomic_core::AtomicCoreError> {
    let Some(atom_limit) = plan.atom_limit else {
        return Ok(false);
    };
    Ok(count_account_atoms(manager).await? >= i64::from(atom_limit))
}

/// Sum the live atom count across every knowledge base in the tenant — the
/// account-wide atom total both the request-time gate and the sweep/dispatcher
/// over-limit checks denominate against. Strongly consistent (each KB count is
/// a single live statement); the KB list is small.
async fn count_account_atoms(
    manager: &DatabaseManager,
) -> Result<i64, atomic_core::AtomicCoreError> {
    let kbs = manager.list_databases().await?.0;
    let mut total: i64 = 0;
    for db in kbs {
        let core = manager.get_core(&db.id).await?;
        total += i64::from(core.count_atoms().await?);
    }
    Ok(total)
}

/// Run the resource check for `target`. `Ok(None)` admits; `Ok(Some(resp))`
/// is the 402 to return; `Err` is an operational fault reading the count.
async fn check_resource(
    target: QuotaTarget,
    plan: &Plan,
    manager: &DatabaseManager,
    req: &ServiceRequest,
    delta: i64,
) -> Result<Option<HttpResponse>, atomic_core::AtomicCoreError> {
    let (metric, limit) = match target {
        QuotaTarget::Atom | QuotaTarget::AtomBulk | QuotaTarget::IngestUrls | QuotaTarget::Mcp => {
            ("atoms", plan.atom_limit)
        }
        QuotaTarget::Kb => ("knowledge_bases", plan.kb_limit),
    };
    // NULL limit = unlimited: never read the count, never block.
    let Some(limit) = limit else {
        return Ok(None);
    };
    let limit = i64::from(limit);

    let current: i64 = match target {
        QuotaTarget::Atom | QuotaTarget::AtomBulk | QuotaTarget::IngestUrls | QuotaTarget::Mcp => {
            // The atom limit is an account-wide ceiling, so the gate sums
            // atoms across EVERY knowledge base — not just the one this
            // request targets. Counting a single KB would let a tenant on a
            // finite-atom plan with `kb_limit > 1` evade the ceiling by
            // spreading atoms across KBs (each KB stays under, the account
            // doesn't). This matches [`account_over_plan_limits`]' semantics;
            // the account's KB list is small, so it's the same cheap work the
            // trial/downgrade sweep does.
            count_account_atoms(manager).await?
        }
        QuotaTarget::Kb => manager.list_databases().await?.0.len() as i64,
    };

    // Admit only if the request keeps the tenant at-or-under the limit.
    // `current + delta` is the count the create would land on; `delta` is 1
    // for the single routes and the batch size for bulk.
    if current + delta > limit {
        return Ok(Some(quota_exceeded(metric, current, limit, req)));
    }
    Ok(None)
}

/// Buffer the request body, count the batch's elements, and **re-inject the
/// exact bytes** so the downstream handler reads an untouched payload.
///
/// `field = None` counts a top-level JSON array (`/api/atoms/bulk`);
/// `field = Some("urls")` counts the named array field inside a top-level JSON
/// object (`/api/ingest/urls`). `Err(())` for a body that doesn't match the
/// expected shape (or can't be read) — the caller treats that as a zero delta
/// and lets the handler surface its own deserialization error. The bytes are
/// always replayed first, including on the error path.
async fn peek_and_replay_batch_len(
    req: &mut ServiceRequest,
    field: Option<&str>,
) -> Result<usize, ()> {
    // `web::Bytes::from_request` drains the payload into memory. Clone the
    // (cheap, Arc-backed) HttpRequest so the immutable `request()` borrow
    // doesn't overlap the mutable `take_payload()` borrow.
    let http_req = req.request().clone();
    let bytes = Bytes::from_request(&http_req, &mut req.take_payload())
        .await
        .map_err(|_| ())?;
    // Always replay the exact bytes before returning so the handler reads an
    // untouched payload regardless of the count outcome.
    let len = batch_len(&bytes, field);
    req.set_payload(Payload::from(bytes));
    len
}

/// Buffer the MCP JSON-RPC body, classify whether it is a `create_atom` tool
/// call ([`mcp_creates_atom`]), and **re-inject the exact bytes** so the MCP
/// transport reads an untouched payload. A body that can't be read replays as
/// empty and classifies as non-creating — the transport will surface its own
/// error; we don't charge quota for something we can't confirm creates an atom.
async fn peek_and_replay_creates_atom(req: &mut ServiceRequest) -> bool {
    // Clone the (cheap, Arc-backed) HttpRequest so the immutable `request()`
    // borrow doesn't overlap the mutable `take_payload()` borrow, exactly like
    // the batch peek above.
    let http_req = req.request().clone();
    let bytes = match Bytes::from_request(&http_req, &mut req.take_payload()).await {
        Ok(bytes) => bytes,
        Err(_) => return false,
    };
    let creates = mcp_creates_atom(&bytes);
    // Always replay the exact bytes so the handler reads an untouched payload.
    req.set_payload(Payload::from(bytes));
    creates
}

/// Count the batch length in `bytes`: a top-level array when `field` is
/// `None`, or the named array field of a top-level object otherwise. Returns
/// `Err(())` for any other shape. Counts without materializing element values.
fn batch_len(bytes: &[u8], field: Option<&str>) -> Result<usize, ()> {
    match field {
        None => serde_json::from_slice::<Vec<serde::de::IgnoredAny>>(bytes)
            .map(|items| items.len())
            .map_err(|_| ()),
        Some(name) => {
            // The array can be large, so avoid counting it twice: deserialize
            // the whole object once, then read the named field's length.
            let obj: serde_json::Map<String, serde_json::Value> =
                serde_json::from_slice(bytes).map_err(|_| ())?;
            match obj.get(name) {
                Some(serde_json::Value::Array(items)) => Ok(items.len()),
                _ => Err(()),
            }
        }
    }
}

/// Placeholder upgrade link, derived from the request host
/// (`<sub>.<base>` → `https://app.<base>/account/billing`) — the same derivation the
/// out-of-credits guard uses (plan: `upgrade_url` =
/// `https://app.<base-domain>/billing`).
fn upgrade_url(req: &ServiceRequest) -> String {
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .or_else(|| req.uri().host())
        .unwrap_or_default();
    let host = host.split(':').next().unwrap_or(host);
    let base = host.split_once('.').map(|(_, base)| base).unwrap_or(host);
    format!("https://app.{base}/account/billing")
}

/// The plan's quota-exceeded response shape, verbatim. `resets_at` is always
/// `null` for resource limits (module docs).
fn quota_exceeded(metric: &str, current: i64, limit: i64, req: &ServiceRequest) -> HttpResponse {
    HttpResponse::PaymentRequired().json(serde_json::json!({
        "error": "quota_exceeded",
        "metric": metric,
        "current": current,
        "limit": limit,
        "resets_at": serde_json::Value::Null,
        "upgrade_url": upgrade_url(req),
    }))
}

fn internal_error() -> HttpResponse {
    HttpResponse::InternalServerError().json(serde_json::json!({ "error": "internal_error" }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quota_target_matches_only_creating_posts() {
        let post = Method::POST;
        assert_eq!(quota_target(&post, "/api/atoms"), Some(QuotaTarget::Atom));
        assert_eq!(
            quota_target(&post, "/api/atoms/bulk"),
            Some(QuotaTarget::AtomBulk)
        );
        // URL ingestion creates atoms too — counts against atom_limit.
        assert_eq!(
            quota_target(&post, "/api/ingest/url"),
            Some(QuotaTarget::Atom)
        );
        // Obsidian import creates one atom per note — a bypass alias for
        // /api/atoms, gated on the single-atom delta.
        assert_eq!(
            quota_target(&post, "/api/import/obsidian"),
            Some(QuotaTarget::Atom)
        );
        assert_eq!(
            quota_target(&post, "/api/ingest/urls"),
            Some(QuotaTarget::IngestUrls)
        );
        assert_eq!(quota_target(&post, "/api/databases"), Some(QuotaTarget::Kb));
        // Manual-trigger atom-creating routes (dynamic {id} segment) are gated
        // as single-atom deltas, closing the at-ceiling evasion.
        assert_eq!(
            quota_target(&post, "/api/reports/abc/run"),
            Some(QuotaTarget::Atom)
        );
        assert_eq!(
            quota_target(&post, "/api/feeds/abc/poll"),
            Some(QuotaTarget::Atom)
        );
        // The per-tenant MCP surface is a single POST path; the body decides
        // whether it actually creates an atom (classified in the guard).
        assert_eq!(quota_target(&post, "/mcp"), Some(QuotaTarget::Mcp));
        // Updates, reads, and nested paths are not resource creates.
        for ignored in [
            "/api/atoms/abc",
            "/api/atoms/abc/process",
            "/api/databases/default",
            "/api/databases/default/activate",
            "/api/tags",
            // Reads/other report+feed routes are not atom creates.
            "/api/reports/abc",
            "/api/reports",
            "/api/feeds/abc",
            "/api/feeds",
        ] {
            assert_eq!(quota_target(&post, ignored), None, "{ignored} ignored");
        }
        // Reads on the create paths are not creates.
        assert_eq!(quota_target(&Method::GET, "/api/atoms"), None);
        assert_eq!(quota_target(&Method::PUT, "/api/atoms"), None);
        assert_eq!(quota_target(&Method::GET, "/api/ingest/urls"), None);
        // The MCP GET (SSE stream) and DELETE (session teardown) aren't POSTs,
        // so they never reach the body classifier.
        assert_eq!(quota_target(&Method::GET, "/mcp"), None);
        assert_eq!(quota_target(&Method::DELETE, "/mcp"), None);
    }

    #[test]
    fn mcp_creates_atom_only_for_create_atom_tool_call() {
        // A `tools/call` for `create_atom` charges one atom.
        assert!(mcp_creates_atom(
            br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"create_atom","arguments":{"content":"hi"}}}"#
        ));
        // Other tool calls create no atom.
        assert!(!mcp_creates_atom(
            br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"search_atoms","arguments":{"query":"x"}}}"#
        ));
        // Non-tool methods (handshake, listing) create nothing.
        assert!(!mcp_creates_atom(
            br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#
        ));
        assert!(!mcp_creates_atom(
            br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#
        ));
        // An unparseable / wrong-shaped body isn't ours to charge.
        assert!(!mcp_creates_atom(b"not json"));
        assert!(!mcp_creates_atom(br#"{"method":"tools/call"}"#));
    }

    #[test]
    fn batch_len_counts_top_level_array() {
        assert_eq!(batch_len(b"[]", None), Ok(0));
        assert_eq!(
            batch_len(br#"[{"content":"a"},{"content":"b"}]"#, None),
            Ok(2)
        );
        // Non-array bodies are not ours to count.
        assert_eq!(batch_len(br#"{"content":"a"}"#, None), Err(()));
        assert_eq!(batch_len(b"not json", None), Err(()));
    }

    #[test]
    fn batch_len_counts_named_object_field() {
        assert_eq!(batch_len(br#"{"urls":[]}"#, Some("urls")), Ok(0));
        assert_eq!(
            batch_len(
                br#"{"urls":[{"url":"a"},{"url":"b"},{"url":"c"}]}"#,
                Some("urls")
            ),
            Ok(3)
        );
        // A top-level array is the wrong shape for the field form.
        assert_eq!(batch_len(br#"[{"url":"a"}]"#, Some("urls")), Err(()));
        // Missing or non-array field is not ours to count.
        assert_eq!(batch_len(br#"{"other":[]}"#, Some("urls")), Err(()));
        assert_eq!(batch_len(br#"{"urls":"x"}"#, Some("urls")), Err(()));
        assert_eq!(batch_len(b"not json", Some("urls")), Err(()));
    }
}
