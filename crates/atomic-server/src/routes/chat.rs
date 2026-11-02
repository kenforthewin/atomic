//! Chat / Conversation routes

use crate::db_extractor::{job_scope, Db};
use crate::error::{ok_or_error, ApiErrorResponse};
use crate::event_bridge::chat_event_callback;
use crate::event_channel::EventChannel;
use crate::state::AppState;
use actix_web::{web, HttpRequest, HttpResponse};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

#[derive(Deserialize, Serialize, ToSchema)]
pub struct CreateConversationBody {
    /// Tag IDs to scope the conversation
    #[serde(default)]
    pub tag_ids: Vec<String>,
    /// Optional conversation title
    pub title: Option<String>,
}

#[utoipa::path(post, path = "/api/conversations", request_body = CreateConversationBody, responses((status = 201, description = "Created conversation", body = atomic_core::ConversationWithTags)), tag = "chat")]
pub async fn create_conversation(db: Db, body: web::Json<CreateConversationBody>) -> HttpResponse {
    let req = body.into_inner();
    match db
        .0
        .create_conversation(&req.tag_ids, req.title.as_deref())
        .await
    {
        Ok(conv) => HttpResponse::Created().json(conv),
        Err(e) => crate::error::error_response(e),
    }
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct GetConversationsQuery {
    /// Filter by tag ID
    pub filter_tag_id: Option<String>,
    /// Max results (default: 50)
    pub limit: Option<i32>,
    /// Offset for pagination
    pub offset: Option<i32>,
    /// Include archived conversations (default: false)
    pub include_archived: Option<bool>,
}

#[utoipa::path(get, path = "/api/conversations", params(GetConversationsQuery), responses((status = 200, description = "List of conversations", body = Vec<atomic_core::ConversationWithTags>)), tag = "chat")]
pub async fn get_conversations(db: Db, query: web::Query<GetConversationsQuery>) -> HttpResponse {
    let limit = query.limit.unwrap_or(50);
    let offset = query.offset.unwrap_or(0);
    let include_archived = query.include_archived.unwrap_or(false);
    ok_or_error(
        db.0.get_conversations(
            query.filter_tag_id.as_deref(),
            limit,
            offset,
            include_archived,
        )
        .await,
    )
}

#[utoipa::path(get, path = "/api/conversations/{id}", params(("id" = String, Path, description = "Conversation ID")), responses((status = 200, description = "Conversation with messages", body = atomic_core::ConversationWithMessages), (status = 404, description = "Not found", body = ApiErrorResponse)), tag = "chat")]
pub async fn get_conversation(db: Db, path: web::Path<String>) -> HttpResponse {
    let id = path.into_inner();
    match db.0.get_conversation(&id).await {
        Ok(Some(conv)) => HttpResponse::Ok().json(conv),
        Ok(None) => {
            HttpResponse::NotFound().json(serde_json::json!({"error": "Conversation not found"}))
        }
        Err(e) => crate::error::error_response(e),
    }
}

#[derive(Deserialize, Serialize, ToSchema)]
pub struct UpdateConversationBody {
    /// Updated title
    pub title: Option<String>,
    /// Archive/unarchive
    pub is_archived: Option<bool>,
}

#[utoipa::path(put, path = "/api/conversations/{id}", params(("id" = String, Path, description = "Conversation ID")), request_body = UpdateConversationBody, responses((status = 200, description = "Updated conversation")), tag = "chat")]
pub async fn update_conversation(
    db: Db,
    path: web::Path<String>,
    body: web::Json<UpdateConversationBody>,
) -> HttpResponse {
    let id = path.into_inner();
    let req = body.into_inner();
    ok_or_error(
        db.0.update_conversation(&id, req.title.as_deref(), req.is_archived)
            .await,
    )
}

#[utoipa::path(delete, path = "/api/conversations/{id}", params(("id" = String, Path, description = "Conversation ID")), responses((status = 200, description = "Conversation deleted")), tag = "chat")]
pub async fn delete_conversation(db: Db, path: web::Path<String>) -> HttpResponse {
    let id = path.into_inner();
    ok_or_error(db.0.delete_conversation(&id).await)
}

#[derive(Deserialize, Serialize, ToSchema)]
pub struct SetScopeBody {
    /// The conversation's scope. Each entry is either `{"tag_id": "...",
    /// "mode": "include"|"require"|"exclude"}` or a bare tag id, which means
    /// include — the shape clients sent before modes existed.
    #[serde(default)]
    pub tag_ids: Vec<atomic_core::ScopeEntry>,
}

#[utoipa::path(put, path = "/api/conversations/{id}/scope", params(("id" = String, Path, description = "Conversation ID")), request_body = SetScopeBody, responses((status = 200, description = "Scope updated")), tag = "chat")]
pub async fn set_conversation_scope(
    db: Db,
    path: web::Path<String>,
    body: web::Json<SetScopeBody>,
) -> HttpResponse {
    let id = path.into_inner();
    let entries = body.into_inner().tag_ids;
    ok_or_error(db.0.set_conversation_scope(&id, &entries).await)
}

#[derive(Deserialize, Serialize, ToSchema)]
pub struct AddTagBody {
    /// Tag ID to add to scope
    pub tag_id: String,
    /// The role the tag plays: `include` (default), `require`, or `exclude`.
    /// Re-adding a tag already in scope changes its mode.
    #[serde(default)]
    pub mode: atomic_core::ScopeMode,
}

#[utoipa::path(post, path = "/api/conversations/{id}/scope/tags", params(("id" = String, Path, description = "Conversation ID")), request_body = AddTagBody, responses((status = 200, description = "Tag added to scope")), tag = "chat")]
pub async fn add_tag_to_scope(
    db: Db,
    path: web::Path<String>,
    body: web::Json<AddTagBody>,
) -> HttpResponse {
    let id = path.into_inner();
    let body = body.into_inner();
    ok_or_error(db.0.add_tag_to_scope(&id, &body.tag_id, body.mode).await)
}

#[utoipa::path(delete, path = "/api/conversations/{id}/scope/tags/{tag_id}", params(("id" = String, Path, description = "Conversation ID"), ("tag_id" = String, Path, description = "Tag ID")), responses((status = 200, description = "Tag removed from scope")), tag = "chat")]
pub async fn remove_tag_from_scope(db: Db, path: web::Path<(String, String)>) -> HttpResponse {
    let (id, tag_id) = path.into_inner();
    ok_or_error(db.0.remove_tag_from_scope(&id, &tag_id).await)
}

#[derive(Deserialize, Serialize, ToSchema)]
pub struct ChatSkillBody {
    pub name: String,
    pub instructions: String,
}

#[derive(Deserialize, Serialize, ToSchema)]
pub struct SendMessageBody {
    /// Message content
    pub content: String,
    /// Optional canvas context for canvas-aware chat tools
    #[serde(default)]
    pub canvas_context: Option<atomic_core::CanvasContext>,
    /// Optional current UI context for page-aware chat tools
    #[serde(default)]
    pub page_context: Option<atomic_core::PageContext>,
    /// User-loaded skills applied to this turn only
    #[serde(default)]
    pub skills: Vec<ChatSkillBody>,
}

#[utoipa::path(post, path = "/api/conversations/{id}/messages", params(("id" = String, Path, description = "Conversation ID")), request_body = SendMessageBody, responses((status = 200, description = "Assistant response (streaming events via WebSocket)", body = atomic_core::ChatMessageWithContext), (status = 400, description = "Invalid skill", body = ApiErrorResponse)), tag = "chat")]
pub async fn send_chat_message(
    req: HttpRequest,
    state: web::Data<AppState>,
    events: EventChannel,
    db: Db,
    path: web::Path<String>,
    body: web::Json<SendMessageBody>,
) -> HttpResponse {
    let conversation_id = path.into_inner();
    let body = body.into_inner();
    let events_tx = events.0.clone();
    let on_event = chat_event_callback(events_tx.clone());

    // Registered for the whole turn; the guard deregisters on every exit.
    let turn = state
        .chat_cancellations
        .begin(job_scope(&req), &conversation_id);

    let skills = body
        .skills
        .into_iter()
        .map(|skill| atomic_core::ChatSkill {
            name: skill.name,
            instructions: skill.instructions,
        })
        .collect::<Vec<_>>();
    if let Err(error) = atomic_core::agent::validate_chat_skills(&skills) {
        return HttpResponse::BadRequest().json(ApiErrorResponse { error });
    }
    let result =
        if body.canvas_context.is_some() || body.page_context.is_some() || !skills.is_empty() {
            db.0.send_chat_message_with_canvas_and_skills(
                &conversation_id,
                &body.content,
                on_event,
                body.canvas_context,
                body.page_context,
                skills,
                Some(turn.flag()),
            )
            .await
        } else {
            db.0.send_chat_message(&conversation_id, &body.content, on_event, Some(turn.flag()))
                .await
        };

    match result {
        Ok(message) => HttpResponse::Ok().json(message),
        Err(e) => {
            // The client watches WebSocket events during the turn and may
            // never read this body (navigation, disconnect, a proxy timing
            // out the long request) — put the failure on the event stream too
            // so the conversation surfaces it either way.
            let _ = events_tx.send(crate::state::ServerEvent::ChatError {
                conversation_id: conversation_id.clone(),
                error: e.to_string(),
            });
            crate::error::error_response(e)
        }
    }
}

#[utoipa::path(post, path = "/api/conversations/{id}/messages/cancel", params(("id" = String, Path, description = "Conversation ID")), responses((status = 202, description = "Cancellation requested")), tag = "chat")]
pub async fn cancel_chat_message(
    req: HttpRequest,
    state: web::Data<AppState>,
    path: web::Path<String>,
) -> HttpResponse {
    let conversation_id = path.into_inner();
    let scope = job_scope(&req);
    // Idempotent: nothing running is a valid state to ask for a stop from,
    // and the caller's next move (clear the streaming UI) is the same either
    // way. `cancelled` reports whether a turn was actually signalled.
    let cancelled = state
        .chat_cancellations
        .cancel(scope.as_deref(), &conversation_id);
    HttpResponse::Accepted().json(serde_json::json!({ "cancelled": cancelled }))
}

#[cfg(test)]
mod tests {
    //! The cancel route's ownership contract.
    //!
    //! Cancellation is the one chat operation that acts on a turn started by
    //! a *different* request, so it cannot authorize itself by loading a row
    //! — the registry is a process-global map and a conversation id is a
    //! guessable UUID. What keeps a multi-tenant composition honest is that
    //! registration and cancellation both key on
    //! [`crate::db_extractor::RequestJobScope`], the extension the composing
    //! layer stamps with the account id. These tests pin that: a scope that
    //! did not start a turn cannot stop it, however right it gets the id.

    use super::*;
    use crate::db_extractor::RequestJobScope;
    use crate::{
        export_jobs::ExportJobManager, log_buffer::LogBuffer,
        migration_jobs::MigrationJobManager, state::SetupClaimLimiter,
    };
    use actix_web::body::MessageBody;
    use actix_web::dev::{ServiceRequest, ServiceResponse};
    use actix_web::middleware::{from_fn, Next};
    use actix_web::{test as actix_test, App, HttpMessage};
    use std::sync::atomic::Ordering;
    use std::sync::Arc;
    use tokio::sync::{broadcast, Mutex};

    /// Header the test middleware reads to stand in for a composing layer's
    /// authenticated account. Absent = the standalone server, which installs
    /// no scope at all.
    const SCOPE_HEADER: &str = "x-test-scope";

    fn test_state() -> web::Data<AppState> {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let manager = Arc::new(atomic_core::DatabaseManager::new(temp.path()).expect("manager"));
        let (event_tx, _rx) = broadcast::channel(16);
        let state = web::Data::new(AppState {
            manager,
            event_tx,
            public_url: None,
            log_buffer: LogBuffer::new(16),
            export_jobs: ExportJobManager::for_tests(temp.path().join("exports")),
            migration_jobs: MigrationJobManager::for_tests(temp.path().join("migrations")),
            setup_token: None,
            dangerously_skip_setup_token: true,
            setup_claim_lock: Mutex::new(()),
            setup_claim_limiter: SetupClaimLimiter::new(),
            chat_cancellations: Default::default(),
        });
        // The SQLite files must outlive the state that has them open; the
        // process exits before anything would clean them up anyway.
        std::mem::forget(temp);
        state
    }

    /// Stand-in for the composing layer's auth middleware (atomic-cloud's
    /// `CloudAuth` inserts exactly this extension, with the account id).
    async fn install_scope(
        req: ServiceRequest,
        next: Next<impl MessageBody + 'static>,
    ) -> Result<ServiceResponse<impl MessageBody>, actix_web::Error> {
        let scope = req
            .headers()
            .get(SCOPE_HEADER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        if let Some(scope) = scope {
            req.extensions_mut().insert(RequestJobScope(scope));
        }
        next.call(req).await
    }

    fn cancel_request(conversation_id: &str, scope: Option<&str>) -> actix_http::Request {
        let mut req = actix_test::TestRequest::post().uri(&format!(
            "/api/conversations/{conversation_id}/messages/cancel"
        ));
        if let Some(scope) = scope {
            req = req.insert_header((SCOPE_HEADER, scope));
        }
        req.to_request()
    }

    macro_rules! cancel_app {
        ($state:expr) => {
            actix_test::init_service(
                App::new()
                    .app_data($state)
                    .route(
                        "/api/conversations/{id}/messages/cancel",
                        web::post().to(cancel_chat_message),
                    )
                    .wrap(from_fn(install_scope)),
            )
            .await
        };
    }

    /// Send a cancel and return its `cancelled` flag, asserting the route
    /// always accepts.
    async fn cancelled<S, B>(app: &S, request: actix_http::Request) -> bool
    where
        S: actix_web::dev::Service<actix_http::Request, Response = ServiceResponse<B>, Error = actix_web::Error>,
        B: MessageBody,
    {
        let res = actix_test::call_service(app, request).await;
        assert_eq!(res.status(), 202, "cancelling is always accepted");
        let body: serde_json::Value = actix_test::read_body_json(res).await;
        body["cancelled"].as_bool().expect("cancelled flag")
    }

    /// One tenant cannot stop another's turn by guessing the conversation
    /// id: the cancel reports nothing was running and the victim's flag stays
    /// down. The owning scope then stops it.
    #[actix_web::test]
    async fn a_foreign_scope_cannot_cancel_a_turn_it_did_not_start() {
        let state = test_state();
        let app = cancel_app!(state.clone());

        // Tenant B has a turn running on conversation `shared-id`.
        let turn = state
            .chat_cancellations
            .begin(Some("tenant-b".to_string()), "shared-id");
        let flag = turn.flag();

        // Tenant A knows the id and asks for it to stop.
        assert!(
            !cancelled(&app, cancel_request("shared-id", Some("tenant-a"))).await,
            "a foreign scope must find nothing to cancel"
        );
        assert!(
            !flag.load(Ordering::Relaxed),
            "tenant B's turn must still be running"
        );

        // The owning scope stops it.
        assert!(cancelled(&app, cancel_request("shared-id", Some("tenant-b"))).await);
        assert!(flag.load(Ordering::Relaxed), "the owner's cancel lands");
    }

    /// An unscoped request (the standalone server) is its own scope, not a
    /// wildcard: it cannot reach a scoped turn, and a scoped request cannot
    /// reach an unscoped one.
    #[actix_web::test]
    async fn the_absent_scope_is_a_scope_of_its_own() {
        let state = test_state();
        let app = cancel_app!(state.clone());

        let scoped = state
            .chat_cancellations
            .begin(Some("tenant-b".to_string()), "c1");
        let scoped_flag = scoped.flag();
        let unscoped = state.chat_cancellations.begin(None, "c2");
        let unscoped_flag = unscoped.flag();

        assert!(!cancelled(&app, cancel_request("c1", None)).await);
        assert!(!scoped_flag.load(Ordering::Relaxed));

        assert!(!cancelled(&app, cancel_request("c2", Some("tenant-b"))).await);
        assert!(!unscoped_flag.load(Ordering::Relaxed));

        // Each still reaches its own.
        assert!(cancelled(&app, cancel_request("c2", None)).await);
        assert!(unscoped_flag.load(Ordering::Relaxed));
    }

    /// Cancelling is idempotent and never 404s: nothing running is a valid
    /// state to ask for a stop from, and the client's next move (clear the
    /// streaming UI) is the same either way.
    #[actix_web::test]
    async fn cancelling_nothing_is_accepted() {
        let state = test_state();
        let app = cancel_app!(state);
        assert!(!cancelled(&app, cancel_request("never-existed", None)).await);
    }

    /// The turn guard deregisters on drop, so a later cancel finds nothing —
    /// and a superseding turn on the same conversation owns the entry, so
    /// the older guard's drop must not evict it.
    #[actix_web::test]
    async fn a_finished_turn_deregisters_and_a_superseding_turn_owns_the_entry() {
        let state = test_state();
        let registry = &state.chat_cancellations;

        {
            let _turn = registry.begin(None, "c1");
            assert!(registry.cancel(None, "c1"), "a live turn is cancellable");
        }
        assert!(
            !registry.cancel(None, "c1"),
            "a finished turn leaves nothing behind"
        );

        // A retry racing its predecessor: the second registration wins, and
        // the first guard dropping must not take it with it.
        let first = registry.begin(None, "c2");
        let second = registry.begin(None, "c2");
        let second_flag = second.flag();
        drop(first);
        assert!(
            registry.cancel(None, "c2"),
            "the superseding turn is still registered"
        );
        assert!(second_flag.load(Ordering::Relaxed));
    }

    /// Superseding a turn means stopping it. Once a second turn takes the
    /// key, no cancel can reach the first — so if `begin` doesn't raise its
    /// flag, the displaced turn streams to completion into a conversation
    /// whose next answer is already being written, with nothing able to stop
    /// it.
    #[actix_web::test]
    async fn a_superseded_turn_is_cancelled_by_the_one_replacing_it() {
        let state = test_state();
        let registry = &state.chat_cancellations;

        let first = registry.begin(None, "c1");
        let first_flag = first.flag();
        assert!(!first_flag.load(Ordering::Relaxed));

        let second = registry.begin(None, "c1");
        assert!(
            first_flag.load(Ordering::Relaxed),
            "the displaced turn is told to stop"
        );
        assert!(
            !second.flag().load(Ordering::Relaxed),
            "and the one replacing it is not"
        );

        // Tenancy still holds: a turn in another scope keys separately and is
        // untouched by a same-id registration here.
        let other = registry.begin(Some("tenant-a".to_string()), "c1");
        let other_flag = other.flag();
        let _third = registry.begin(None, "c1");
        assert!(
            !other_flag.load(Ordering::Relaxed),
            "another tenant's turn on the same conversation id is not displaced"
        );
    }
}
