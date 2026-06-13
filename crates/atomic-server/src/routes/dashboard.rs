//! Dashboard configuration endpoints.
//!
//! Currently exposes the per-DB featured-report pointer the dashboard
//! widget reads. The pointer survives report deletion: `AtomicCore`'s
//! `delete_report` clears it automatically, and `get` self-heals stale
//! values by returning `None` if the referenced report no longer exists.
//!
//! Phase 4 will surface a UI chooser on top of these two endpoints.

use crate::db_extractor::Db;
use crate::error::{error_response, ApiErrorResponse};
use crate::event_channel::EventChannel;
use crate::state::ServerEvent;
use actix_web::{web, HttpResponse};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct FeaturedReportResponse {
    /// `None` when no report is currently featured. The dashboard widget
    /// renders its empty state in this case.
    pub report_id: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct SetFeaturedReportRequest {
    /// `None` clears the pointer; `Some(id)` validates the id and points
    /// the dashboard at it. Pointing at a non-existent id returns 400.
    pub report_id: Option<String>,
}

#[utoipa::path(
    get,
    path = "/api/dashboard/featured-report",
    responses(
        (status = 200, description = "Featured report id (or null)",
         body = FeaturedReportResponse)
    ),
    tag = "dashboard"
)]
pub async fn get_featured_report(db: Db) -> HttpResponse {
    match db.0.get_featured_report_id().await {
        Ok(report_id) => HttpResponse::Ok().json(FeaturedReportResponse { report_id }),
        Err(e) => error_response(e),
    }
}

#[utoipa::path(
    put,
    path = "/api/dashboard/featured-report",
    request_body = SetFeaturedReportRequest,
    responses(
        (status = 200, description = "Featured report updated",
         body = FeaturedReportResponse),
        (status = 400, description = "Referenced report does not exist",
         body = ApiErrorResponse)
    ),
    tag = "dashboard"
)]
pub async fn set_featured_report(
    db: Db,
    events: EventChannel,
    body: web::Json<SetFeaturedReportRequest>,
) -> HttpResponse {
    let id = body.report_id.as_deref();
    if let Err(e) = db.0.set_featured_report_id(id).await {
        return error_response(e);
    }
    // Broadcast so other clients (and the local BriefingWidget /
    // ReportDetailView star toggle) refetch without polling.
    let _ = events.0.send(ServerEvent::DashboardFeaturedChanged {
        report_id: body.report_id.clone(),
    });
    HttpResponse::Ok().json(FeaturedReportResponse {
        report_id: body.report_id.clone(),
    })
}
