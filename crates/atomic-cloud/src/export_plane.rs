//! Tenant-scoped markdown export jobs — the cloud counterpart of
//! atomic-server's export plane.
//!
//! Self-hosted atomic-server runs one process-global [`ExportJobManager`]:
//! one namespace of job ids, one artifact directory. Under cloud that shape
//! is a tenant-isolation hole (any tenant could poll or delete another
//! tenant's job by id), which is why the composition guard originally left
//! the whole export family unrouted (`fallback_bound_plane`). This module
//! restores the plane the tenant-safe way: **one [`ExportJobManager`] per
//! account**, created lazily under a per-account artifact directory, with
//! every route resolving the job id inside the requesting account's manager
//! only — a leaked job id from another tenant is a plain 404 here.
//!
//! The routes mirror atomic-server's paths exactly so the product app's
//! existing export UI works unchanged on a tenant subdomain (the same
//! shadowing trick as the `/api/auth/tokens*` plane — registered ahead of
//! `api_scope`, the exact-path resources win the match):
//!
//! - `POST /api/databases/{id}/exports/markdown` — start (or return the
//!   in-flight) export of one of the tenant's knowledge bases. The db id is
//!   resolved through the request's [`RequestDatabaseManager`], so a foreign
//!   db id NotFounds without touching another tenant.
//! - `GET /api/exports/{id}` — job status; issues the short-lived download
//!   token when complete.
//! - `DELETE /api/exports/{id}` — cancel a running job or delete a finished
//!   one (and its artifacts).
//! - `GET /api/exports/{id}/download` — stream the zip, gated on the token.
//!
//! Egress is part of the product's non-payment promise ("read-only, but
//! always exportable"), so the start and delete routes are exempted from the
//! dunning write-block (`billing_guard::is_write_block_exempt`) — they
//! mutate scratch state, never tenant content.
//!
//! Artifacts live under a process-owned temp dir (per-account subdirs) and
//! are bounded by the manager's own retention (24h complete / 1h failed,
//! plus delete-on-restart via `TempDir`).

use std::collections::HashMap;
use std::sync::Mutex;

use actix_files::NamedFile;
use actix_web::http::header::{
    ContentDisposition, DispositionParam, DispositionType, HeaderValue, REFERRER_POLICY,
};
use actix_web::middleware::from_fn;
use actix_web::{web, HttpMessage, HttpRequest, HttpResponse};
use atomic_core::AtomicCoreError;
use atomic_server::db_extractor::RequestDatabaseManager;
use atomic_server::error::error_response;
use atomic_server::export_jobs::ExportJobManager;
use serde::Deserialize;

use crate::auth::CloudAuth;
use crate::server::cloud_plane_guard;
use crate::tenant_plane::require_account_scope;

/// Per-account export-job managers behind the cloud-owned export routes.
/// One instance per process (owned by `TenantPlane`), shared across workers.
pub(crate) struct TenantExportPlane {
    /// Owns the artifact root for the process lifetime; the directory (and
    /// any stray artifacts) is removed when the process exits cleanly.
    root: tempfile::TempDir,
    /// Lazily-created manager per account id. An account's manager owns
    /// `<root>/<account_id>/` — job ids resolve only within it.
    managers: Mutex<HashMap<String, ExportJobManager>>,
}

impl TenantExportPlane {
    pub(crate) fn new() -> Result<Self, std::io::Error> {
        Ok(Self {
            root: tempfile::Builder::new()
                .prefix("atomic-cloud-exports-")
                .tempdir()?,
            managers: Mutex::new(HashMap::new()),
        })
    }

    /// Jobs currently queued or running across every account's manager —
    /// the metrics scrape's exports gauge (scaling doc #5: export
    /// artifacts share the data volume). Poisoned locks read as 0.
    pub(crate) fn active_job_count(&self) -> usize {
        self.managers
            .lock()
            .map(|managers| managers.values().map(ExportJobManager::active_jobs).sum())
            .unwrap_or(0)
    }

    /// The requesting account's manager, created on first use.
    fn for_account(&self, account_id: &str) -> Result<ExportJobManager, AtomicCoreError> {
        let mut managers = self
            .managers
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        if let Some(manager) = managers.get(account_id) {
            return Ok(manager.clone());
        }
        let manager = ExportJobManager::new(self.root.path().join(account_id))?;
        managers.insert(account_id.to_string(), manager.clone());
        Ok(manager)
    }

    /// Register the export family, shadowing atomic-server's same-path
    /// self-hosted handlers (see module docs). Mirrors the token plane's
    /// registration: auth resolves the tenant, the guard fails closed.
    pub(crate) fn configure(
        plane: web::Data<TenantExportPlane>,
        cfg: &mut web::ServiceConfig,
        auth: CloudAuth,
    ) {
        cfg.service(
            web::resource("/api/databases/{id}/exports/markdown")
                .app_data(plane.clone())
                .route(web::post().to(start_export_route))
                .wrap(from_fn(cloud_plane_guard))
                .wrap(auth.clone()),
        );
        cfg.service(
            web::resource("/api/exports/{id}")
                .app_data(plane.clone())
                .route(web::get().to(export_status_route))
                .route(web::delete().to(cancel_or_delete_export_route))
                .wrap(from_fn(cloud_plane_guard))
                .wrap(auth.clone()),
        );
        cfg.service(
            web::resource("/api/exports/{id}/download")
                .app_data(plane)
                .route(web::get().to(download_export_route))
                .wrap(from_fn(cloud_plane_guard))
                .wrap(auth),
        );
    }
}

/// Resolve the pieces every handler needs: the account (scope-checked, like
/// the token plane) and its export-job manager.
fn account_manager(
    req: &HttpRequest,
    plane: &TenantExportPlane,
) -> Result<ExportJobManager, HttpResponse> {
    let tenant = require_account_scope(req)?;
    plane
        .for_account(&tenant.principal.account_id)
        .map_err(error_response)
}

async fn start_export_route(
    req: HttpRequest,
    plane: web::Data<TenantExportPlane>,
    path: web::Path<String>,
) -> HttpResponse {
    let jobs = match account_manager(&req, &plane) {
        Ok(jobs) => jobs,
        Err(denial) => return denial,
    };
    // The tenant's own DatabaseManager (installed by CloudAuth, verified by
    // the plane guard): the db id resolves inside this tenant or NotFounds.
    let Some(manager) = req
        .extensions()
        .get::<RequestDatabaseManager>()
        .map(|m| m.0.clone())
    else {
        tracing::error!(
            path = req.path(),
            "export route reached without a resolved tenant manager"
        );
        return HttpResponse::InternalServerError().json(serde_json::json!({
            "error": "tenant_not_resolved",
            "message": "The request was not resolved to an account.",
        }));
    };
    match jobs.start_markdown_export(manager, path.into_inner()).await {
        Ok(job) => HttpResponse::Accepted().json(job),
        Err(e) => error_response(e),
    }
}

async fn export_status_route(
    req: HttpRequest,
    plane: web::Data<TenantExportPlane>,
    path: web::Path<String>,
) -> HttpResponse {
    let jobs = match account_manager(&req, &plane) {
        Ok(jobs) => jobs,
        Err(denial) => return denial,
    };
    match jobs.status(&path.into_inner(), true) {
        Ok(job) => HttpResponse::Ok().json(job),
        Err(e) => error_response(e),
    }
}

async fn cancel_or_delete_export_route(
    req: HttpRequest,
    plane: web::Data<TenantExportPlane>,
    path: web::Path<String>,
) -> HttpResponse {
    let jobs = match account_manager(&req, &plane) {
        Ok(jobs) => jobs,
        Err(denial) => return denial,
    };
    match jobs.cancel_or_delete(&path.into_inner()) {
        Ok(job) => HttpResponse::Ok().json(job),
        Err(e) => error_response(e),
    }
}

#[derive(Deserialize)]
struct DownloadQuery {
    token: String,
}

async fn download_export_route(
    req: HttpRequest,
    plane: web::Data<TenantExportPlane>,
    path: web::Path<String>,
    query: web::Query<DownloadQuery>,
) -> HttpResponse {
    let jobs = match account_manager(&req, &plane) {
        Ok(jobs) => jobs,
        Err(denial) => return denial,
    };
    let artifact = match jobs.validate_download(&path.into_inner(), &query.token) {
        Ok(artifact) => artifact,
        Err(e) => return error_response(e),
    };

    // Same response shape as atomic-server's download handler: attachment
    // disposition with the db-derived filename, and no referrer leakage of
    // the tokened URL.
    let file = match NamedFile::open_async(&artifact.path).await {
        Ok(file) => file.set_content_disposition(ContentDisposition {
            disposition: DispositionType::Attachment,
            parameters: vec![DispositionParam::Filename(artifact.filename)],
        }),
        Err(e) => return error_response(AtomicCoreError::Io(e)),
    };
    let mut response = file.into_response(&req);
    response
        .headers_mut()
        .insert(REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    response
}
