//! Atomic-owned Actix transport for MCP Streamable HTTP.
//!
//! This keeps Atomic on the official `rmcp` protocol/service layer while
//! owning the HTTP boundary where clients are strict about status codes and
//! content types.

use super::{AtomicMcpServer, DbSelection, RequestManager};
use crate::db_extractor::RequestDatabaseManager;
use crate::state::ServerEvent;
use actix_web::{
    error::InternalError,
    http::{
        header::{self, CACHE_CONTROL},
        StatusCode,
    },
    middleware,
    web::{self, Bytes, Data},
    HttpMessage, HttpRequest, HttpResponse, Result, Scope,
};
use atomic_core::manager::DatabaseManager;
use futures::{Stream, StreamExt};
use rmcp::{
    model::{ClientJsonRpcMessage, ClientRequest, GetExtensions},
    serve_server,
    transport::{
        common::http_header::{
            EVENT_STREAM_MIME_TYPE, HEADER_LAST_EVENT_ID, HEADER_SESSION_ID, JSON_MIME_TYPE,
        },
        streamable_http_server::session::{local::LocalSessionManager, SessionManager},
        TransportAdapterIdentity,
    },
};
use std::{sync::Arc, time::Duration};
use tokio::sync::broadcast;

const HEADER_X_ACCEL_BUFFERING: &str = "X-Accel-Buffering";
const TEXT_MIME_TYPE: &str = "text/plain; charset=utf-8";

#[derive(Clone)]
pub struct AtomicMcpTransport {
    state: Data<TransportState>,
}

impl AtomicMcpTransport {
    pub fn new(
        manager: Arc<DatabaseManager>,
        event_tx: broadcast::Sender<ServerEvent>,
        sse_keep_alive: Duration,
    ) -> Self {
        Self {
            state: Data::new(TransportState {
                manager,
                event_tx,
                session_manager: Arc::new(LocalSessionManager::default()),
                sse_keep_alive,
            }),
        }
    }

    pub fn scope(
        self,
    ) -> Scope<
        impl actix_web::dev::ServiceFactory<
            actix_web::dev::ServiceRequest,
            Config = (),
            Response = actix_web::dev::ServiceResponse,
            Error = actix_web::Error,
            InitError = (),
        >,
    > {
        web::scope("")
            .app_data(self.state.clone())
            .wrap(middleware::NormalizePath::trim())
            .route("", web::get().to(handle_get))
            .route("", web::post().to(handle_post))
            .route("", web::delete().to(handle_delete))
    }
}

struct TransportState {
    manager: Arc<DatabaseManager>,
    event_tx: broadcast::Sender<ServerEvent>,
    session_manager: Arc<LocalSessionManager>,
    sse_keep_alive: Duration,
}

impl TransportState {
    fn server(&self) -> AtomicMcpServer {
        AtomicMcpServer::new(Arc::clone(&self.manager), self.event_tx.clone())
    }
}

fn text_error(status: StatusCode, message: &'static str) -> HttpResponse {
    HttpResponse::build(status)
        .content_type(TEXT_MIME_TYPE)
        .body(message)
}

fn accepts(req: &HttpRequest, media_type: &str) -> bool {
    req.headers()
        .get(header::ACCEPT)
        .and_then(|h| h.to_str().ok())
        .is_some_and(|value| {
            value.split(',').any(|part| {
                let item = part
                    .split(';')
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .to_ascii_lowercase();
                item == "*/*" || item == media_type || item == media_type.replace("text/", "*/")
            })
        })
}

fn accepts_all(req: &HttpRequest, media_types: &[&str]) -> bool {
    media_types
        .iter()
        .all(|media_type| accepts(req, media_type))
}

fn has_json_content_type(req: &HttpRequest) -> bool {
    req.headers()
        .get(header::CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .is_some_and(|value| {
            value
                .split(';')
                .next()
                .unwrap_or_default()
                .trim()
                .eq_ignore_ascii_case(JSON_MIME_TYPE)
        })
}

fn request_session_id(req: &HttpRequest) -> Option<Arc<str>> {
    req.headers()
        .get(HEADER_SESSION_ID)
        .and_then(|v| v.to_str().ok())
        .map(|s| Arc::<str>::from(s.to_owned()))
}

/// Resolve which database this request selects, with the *same* precedence as
/// the data plane's [`resolve_core`](crate::db_extractor::resolve_core): the
/// `X-Atomic-Database` header first, then the `?db=` query parameter (and, on
/// the server side, the manager's active database when neither is present).
///
/// Honoring the header — not just `?db=` — is what keeps the MCP path in step
/// with the `Db` extractor: a composing layer that pre-resolves the manager
/// per request can pin a selection by injecting `X-Atomic-Database`, and that
/// pin is honored identically here and on the data plane. The standalone
/// server injects no such header, so behavior is unchanged unless something
/// installs it (the same contract the `Db` extractor exposes).
fn db_selection(req: &HttpRequest) -> DbSelection {
    let header_db = req
        .headers()
        .get("X-Atomic-Database")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    let db_id = header_db.or_else(|| {
        req.query_string().split('&').find_map(|pair| {
            let mut parts = pair.splitn(2, '=');
            if parts.next()? == "db" {
                parts.next().map(String::from)
            } else {
                None
            }
        })
    });
    DbSelection(db_id)
}

fn attach_request_extensions(req: &HttpRequest, message: &mut ClientJsonRpcMessage) {
    if let ClientJsonRpcMessage::Request(request_msg) = message {
        let extensions = request_msg.request.extensions_mut();
        extensions.insert(db_selection(req));
        // Mirror the data plane's manager override
        // (`db_extractor::request_manager`): when a composing layer installed
        // a per-request manager, carry it into the tool-call context so the
        // tools resolve against it rather than the manager baked in at
        // construction. Absent — the standalone server installs no such
        // middleware — the server falls back to the baked-in manager, so
        // self-hosted behavior is byte-identical.
        if let Some(manager) = req.extensions().get::<RequestDatabaseManager>() {
            extensions.insert(RequestManager(std::sync::Arc::clone(&manager.0)));
        }
    }
}

fn wrap_with_sse_keepalive<S>(
    stream: S,
    keep_alive: Duration,
) -> impl Stream<Item = Result<Bytes, actix_web::Error>>
where
    S: Stream<Item = Result<Bytes, actix_web::Error>> + Send + 'static,
{
    async_stream::stream! {
        let mut stream = Box::pin(stream);
        let mut keep_alive_timer = tokio::time::interval(keep_alive);
        keep_alive_timer.tick().await;

        loop {
            tokio::select! {
                result = stream.next() => {
                    match result {
                        Some(msg) => yield msg,
                        None => break,
                    }
                }
                _ = keep_alive_timer.tick() => {
                    yield Ok(Bytes::from(":ping\n\n"));
                }
            }
        }
    }
}

async fn handle_get(req: HttpRequest, state: Data<TransportState>) -> Result<HttpResponse> {
    if !accepts(&req, EVENT_STREAM_MIME_TYPE) {
        return Ok(text_error(
            StatusCode::NOT_ACCEPTABLE,
            "Not Acceptable: Client must accept text/event-stream",
        ));
    }

    let Some(session_id) = request_session_id(&req) else {
        return Ok(text_error(
            StatusCode::BAD_REQUEST,
            "Bad Request: Session ID is required",
        ));
    };

    let has_session = state
        .session_manager
        .has_session(&session_id)
        .await
        .map_err(|e| InternalError::new(e, StatusCode::INTERNAL_SERVER_ERROR))?;

    if !has_session {
        return Ok(text_error(
            StatusCode::NOT_FOUND,
            "Not Found: Session not found",
        ));
    }

    let last_event_id = req
        .headers()
        .get(HEADER_LAST_EVENT_ID)
        .and_then(|v| v.to_str().ok())
        .map(String::from);

    let sse_stream: std::pin::Pin<Box<dyn Stream<Item = _> + Send>> =
        if let Some(last_event_id) = last_event_id {
            Box::pin(
                state
                    .session_manager
                    .resume(&session_id, last_event_id)
                    .await
                    .map_err(|e| InternalError::new(e, StatusCode::INTERNAL_SERVER_ERROR))?,
            )
        } else {
            Box::pin(
                state
                    .session_manager
                    .create_standalone_stream(&session_id)
                    .await
                    .map_err(|e| InternalError::new(e, StatusCode::INTERNAL_SERVER_ERROR))?,
            )
        };

    let formatted_stream = sse_stream.map(|msg| {
        let mut output = String::new();
        if let Some(id) = msg.event_id {
            output.push_str(&format!("id: {id}\n"));
        }
        match msg.message {
            Some(message) => {
                let data = serde_json::to_string(message.as_ref()).unwrap_or_else(|_| "{}".into());
                output.push_str(&format!("data: {data}\n\n"));
            }
            None => output.push_str("data:\n\n"),
        }
        Ok::<_, actix_web::Error>(Bytes::from(output))
    });

    Ok(HttpResponse::Ok()
        .content_type(EVENT_STREAM_MIME_TYPE)
        .append_header((CACHE_CONTROL, "no-cache"))
        .append_header((HEADER_X_ACCEL_BUFFERING, "no"))
        .streaming(wrap_with_sse_keepalive(
            formatted_stream,
            state.sse_keep_alive,
        )))
}

async fn handle_post(
    req: HttpRequest,
    body: Bytes,
    state: Data<TransportState>,
) -> Result<HttpResponse> {
    if !accepts_all(&req, &[JSON_MIME_TYPE, EVENT_STREAM_MIME_TYPE]) {
        return Ok(text_error(
            StatusCode::NOT_ACCEPTABLE,
            "Not Acceptable: Client must accept both application/json and text/event-stream",
        ));
    }

    if !has_json_content_type(&req) {
        return Ok(text_error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Unsupported Media Type: Content-Type must be application/json",
        ));
    }

    let mut message: ClientJsonRpcMessage = match serde_json::from_slice(&body) {
        Ok(message) => message,
        Err(_) => {
            return Ok(text_error(
                StatusCode::BAD_REQUEST,
                "Bad Request: Body must be a valid MCP JSON-RPC message",
            ));
        }
    };

    attach_request_extensions(&req, &mut message);

    if let Some(session_id) = request_session_id(&req) {
        let has_session = state
            .session_manager
            .has_session(&session_id)
            .await
            .map_err(|e| InternalError::new(e, StatusCode::INTERNAL_SERVER_ERROR))?;

        if !has_session {
            return Ok(text_error(
                StatusCode::NOT_FOUND,
                "Not Found: Session not found",
            ));
        }

        return match message {
            ClientJsonRpcMessage::Request(request_msg) => {
                let stream = state
                    .session_manager
                    .create_stream(&session_id, ClientJsonRpcMessage::Request(request_msg))
                    .await
                    .map_err(|e| InternalError::new(e, StatusCode::INTERNAL_SERVER_ERROR))?;

                let formatted_stream = stream.map(|msg| {
                    let mut output = String::new();
                    if let Some(id) = msg.event_id {
                        output.push_str(&format!("id: {id}\n"));
                    }
                    match msg.message {
                        Some(message) => {
                            let data = serde_json::to_string(message.as_ref())
                                .unwrap_or_else(|_| "{}".into());
                            output.push_str(&format!("data: {data}\n\n"));
                        }
                        None => output.push_str("data:\n\n"),
                    }
                    Ok::<_, actix_web::Error>(Bytes::from(output))
                });

                Ok(HttpResponse::Ok()
                    .content_type(EVENT_STREAM_MIME_TYPE)
                    .append_header((CACHE_CONTROL, "no-cache"))
                    .append_header((HEADER_X_ACCEL_BUFFERING, "no"))
                    .streaming(wrap_with_sse_keepalive(
                        formatted_stream,
                        state.sse_keep_alive,
                    )))
            }
            ClientJsonRpcMessage::Notification(_)
            | ClientJsonRpcMessage::Response(_)
            | ClientJsonRpcMessage::Error(_) => {
                state
                    .session_manager
                    .accept_message(&session_id, message)
                    .await
                    .map_err(|e| InternalError::new(e, StatusCode::INTERNAL_SERVER_ERROR))?;

                Ok(HttpResponse::Accepted().finish())
            }
        };
    }

    let is_initialize = matches!(
        &message,
        ClientJsonRpcMessage::Request(request_msg)
            if matches!(request_msg.request, ClientRequest::InitializeRequest(_))
    );

    if !is_initialize {
        return Ok(text_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Unprocessable Entity: Expected initialize request",
        ));
    }

    let (session_id, transport) = state
        .session_manager
        .create_session()
        .await
        .map_err(|e| InternalError::new(e, StatusCode::INTERNAL_SERVER_ERROR))?;

    tokio::spawn({
        let session_manager = Arc::clone(&state.session_manager);
        let session_id = Arc::clone(&session_id);
        let service_instance = state.server();
        async move {
            let service = serve_server::<AtomicMcpServer, _, _, TransportAdapterIdentity>(
                service_instance,
                transport,
            )
            .await;

            match service {
                Ok(service) => {
                    let _ = service.waiting().await;
                }
                Err(e) => tracing::error!("Failed to create MCP service: {e}"),
            }

            let _ = session_manager
                .close_session(&session_id)
                .await
                .inspect_err(|e| {
                    tracing::error!("Failed to close MCP session {session_id}: {e}");
                });
        }
    });

    let response = state
        .session_manager
        .initialize_session(&session_id, message)
        .await
        .map_err(|e| InternalError::new(e, StatusCode::INTERNAL_SERVER_ERROR))?;

    let sse_stream = async_stream::stream! {
        let data = serde_json::to_string(&response).unwrap_or_else(|_| "{}".into());
        yield Ok::<_, actix_web::Error>(Bytes::from(format!("data: {data}\n\n")));
    };

    Ok(HttpResponse::Ok()
        .content_type(EVENT_STREAM_MIME_TYPE)
        .append_header((CACHE_CONTROL, "no-cache"))
        .append_header((HEADER_X_ACCEL_BUFFERING, "no"))
        .append_header((HEADER_SESSION_ID, session_id.as_ref()))
        .streaming(sse_stream))
}

async fn handle_delete(req: HttpRequest, state: Data<TransportState>) -> Result<HttpResponse> {
    let Some(session_id) = request_session_id(&req) else {
        return Ok(text_error(
            StatusCode::BAD_REQUEST,
            "Bad Request: Session ID is required",
        ));
    };

    let has_session = state
        .session_manager
        .has_session(&session_id)
        .await
        .map_err(|e| InternalError::new(e, StatusCode::INTERNAL_SERVER_ERROR))?;

    if !has_session {
        return Ok(text_error(
            StatusCode::NOT_FOUND,
            "Not Found: Session not found",
        ));
    }

    state
        .session_manager
        .close_session(&session_id)
        .await
        .map_err(|e| InternalError::new(e, StatusCode::INTERNAL_SERVER_ERROR))?;

    Ok(HttpResponse::NoContent().finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::ServerEvent;
    use actix_web::{body::to_bytes, http::header, test as actix_test, App};
    use serde_json::json;

    fn test_transport() -> (AtomicMcpTransport, tempfile::TempDir) {
        let temp = tempfile::TempDir::new().unwrap();
        let manager = Arc::new(DatabaseManager::new(temp.path()).unwrap());
        let (event_tx, _) = broadcast::channel::<ServerEvent>(16);
        let transport = AtomicMcpTransport::new(manager, event_tx, Duration::from_secs(30));

        (transport, temp)
    }

    fn initialize_request() -> serde_json::Value {
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {
                    "name": "atomic-test",
                    "version": "0.0.0"
                }
            }
        })
    }

    fn tools_list_request(id: i32) -> serde_json::Value {
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/list",
            "params": {}
        })
    }

    macro_rules! initialize_session {
        ($app:expr) => {{
            let req = actix_test::TestRequest::post()
                .uri("/mcp")
                .insert_header((
                    header::ACCEPT,
                    format!("{JSON_MIME_TYPE}, {EVENT_STREAM_MIME_TYPE}"),
                ))
                .insert_header((header::CONTENT_TYPE, JSON_MIME_TYPE))
                .set_payload(initialize_request().to_string())
                .to_request();
            let resp = actix_test::call_service(&$app, req).await;
            assert_eq!(resp.status(), StatusCode::OK);
            assert_content_type(&resp, EVENT_STREAM_MIME_TYPE);
            let session_id = resp
                .headers()
                .get(HEADER_SESSION_ID)
                .and_then(|value| value.to_str().ok())
                .expect("initialize response should include session id")
                .to_string();
            let body = to_bytes(resp.into_body()).await.unwrap();
            let body = String::from_utf8(body.to_vec()).unwrap();
            assert!(
                body.contains("\"result\""),
                "initialize should return a JSON-RPC result: {body}"
            );
            session_id
        }};
    }

    macro_rules! send_initialized {
        ($app:expr, $session_id:expr) => {{
            let req = actix_test::TestRequest::post()
                .uri("/mcp")
                .insert_header((
                    header::ACCEPT,
                    format!("{JSON_MIME_TYPE}, {EVENT_STREAM_MIME_TYPE}"),
                ))
                .insert_header((header::CONTENT_TYPE, JSON_MIME_TYPE))
                .insert_header((HEADER_SESSION_ID, $session_id.clone()))
                .set_payload(
                    json!({
                        "jsonrpc": "2.0",
                        "method": "notifications/initialized"
                    })
                    .to_string(),
                )
                .to_request();
            let resp = actix_test::call_service(&$app, req).await;
            assert_eq!(resp.status(), StatusCode::ACCEPTED);
        }};
    }

    fn assert_content_type(resp: &actix_web::dev::ServiceResponse, expected: &str) {
        let content_type = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .expect("response should include content-type");
        assert!(
            content_type.starts_with(expected),
            "expected content-type {expected}, got {content_type}"
        );
    }

    #[actix_web::test]
    async fn get_missing_session_returns_400_with_content_type() {
        let (transport, _temp) = test_transport();
        let app = actix_test::init_service(
            App::new().service(web::scope("/mcp").service(transport.scope())),
        )
        .await;

        let req = actix_test::TestRequest::get()
            .uri("/mcp")
            .insert_header((header::ACCEPT, EVENT_STREAM_MIME_TYPE))
            .to_request();
        let resp = actix_test::call_service(&app, req).await;

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_content_type(&resp, "text/plain");
    }

    #[actix_web::test]
    async fn get_unknown_session_returns_404_with_content_type() {
        let (transport, _temp) = test_transport();
        let app = actix_test::init_service(
            App::new().service(web::scope("/mcp").service(transport.scope())),
        )
        .await;

        let req = actix_test::TestRequest::get()
            .uri("/mcp")
            .insert_header((header::ACCEPT, EVENT_STREAM_MIME_TYPE))
            .insert_header((HEADER_SESSION_ID, "stale-session"))
            .to_request();
        let resp = actix_test::call_service(&app, req).await;

        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert_content_type(&resp, "text/plain");
    }

    #[actix_web::test]
    async fn post_unknown_session_returns_404_with_content_type() {
        let (transport, _temp) = test_transport();
        let app = actix_test::init_service(
            App::new().service(web::scope("/mcp").service(transport.scope())),
        )
        .await;

        let req = actix_test::TestRequest::post()
            .uri("/mcp")
            .insert_header((
                header::ACCEPT,
                format!("{JSON_MIME_TYPE}, {EVENT_STREAM_MIME_TYPE}"),
            ))
            .insert_header((header::CONTENT_TYPE, JSON_MIME_TYPE))
            .insert_header((HEADER_SESSION_ID, "stale-session"))
            .set_payload(tools_list_request(2).to_string())
            .to_request();
        let resp = actix_test::call_service(&app, req).await;

        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert_content_type(&resp, "text/plain");
    }

    #[actix_web::test]
    async fn post_without_session_requires_initialize_with_content_type() {
        let (transport, _temp) = test_transport();
        let app = actix_test::init_service(
            App::new().service(web::scope("/mcp").service(transport.scope())),
        )
        .await;

        let req = actix_test::TestRequest::post()
            .uri("/mcp")
            .insert_header((
                header::ACCEPT,
                format!("{JSON_MIME_TYPE}, {EVENT_STREAM_MIME_TYPE}"),
            ))
            .insert_header((header::CONTENT_TYPE, JSON_MIME_TYPE))
            .set_payload(tools_list_request(2).to_string())
            .to_request();
        let resp = actix_test::call_service(&app, req).await;

        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_content_type(&resp, "text/plain");
    }

    #[actix_web::test]
    async fn initialize_returns_sse_and_session_header() {
        let (transport, _temp) = test_transport();
        let app = actix_test::init_service(
            App::new().service(web::scope("/mcp").service(transport.scope())),
        )
        .await;
        let session_id = initialize_session!(app);

        assert!(!session_id.is_empty());
    }

    #[actix_web::test]
    async fn notification_with_session_returns_accepted() {
        let (transport, _temp) = test_transport();
        let app = actix_test::init_service(
            App::new().service(web::scope("/mcp").service(transport.scope())),
        )
        .await;
        let session_id = initialize_session!(app);
        send_initialized!(app, session_id);

        let req = actix_test::TestRequest::post()
            .uri("/mcp")
            .insert_header((
                header::ACCEPT,
                format!("{JSON_MIME_TYPE}, {EVENT_STREAM_MIME_TYPE}"),
            ))
            .insert_header((header::CONTENT_TYPE, JSON_MIME_TYPE))
            .insert_header((HEADER_SESSION_ID, session_id))
            .set_payload(
                json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/initialized"
                })
                .to_string(),
            )
            .to_request();
        let resp = actix_test::call_service(&app, req).await;

        assert_eq!(resp.status(), StatusCode::ACCEPTED);
    }

    #[actix_web::test]
    async fn request_with_session_returns_sse_stream() {
        let (transport, _temp) = test_transport();
        let app = actix_test::init_service(
            App::new().service(web::scope("/mcp").service(transport.scope())),
        )
        .await;
        let session_id = initialize_session!(app);

        let req = actix_test::TestRequest::post()
            .uri("/mcp")
            .insert_header((
                header::ACCEPT,
                format!("{JSON_MIME_TYPE}, {EVENT_STREAM_MIME_TYPE}"),
            ))
            .insert_header((header::CONTENT_TYPE, JSON_MIME_TYPE))
            .insert_header((HEADER_SESSION_ID, session_id))
            .set_payload(tools_list_request(2).to_string())
            .to_request();
        let resp = actix_test::call_service(&app, req).await;

        assert_eq!(resp.status(), StatusCode::OK);
        assert_content_type(&resp, EVENT_STREAM_MIME_TYPE);

        let _ = resp.into_body();
    }

    #[actix_web::test]
    async fn delete_unknown_session_returns_404_with_content_type() {
        let (transport, _temp) = test_transport();
        let app = actix_test::init_service(
            App::new().service(web::scope("/mcp").service(transport.scope())),
        )
        .await;

        let req = actix_test::TestRequest::delete()
            .uri("/mcp")
            .insert_header((HEADER_SESSION_ID, "stale-session"))
            .to_request();
        let resp = actix_test::call_service(&app, req).await;

        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert_content_type(&resp, "text/plain");
    }

    // --- Per-request manager resolution ------------------------------------
    //
    // The transport copies a `RequestDatabaseManager` extension (installed by
    // a composing layer's middleware) into the tool-call context, so the
    // tools resolve against that manager rather than the one baked in at
    // construction. With no such middleware (every test above) the baked-in
    // manager is used, exactly as the standalone server runs — these two
    // tests pin both sides of that fallback.

    use actix_web::dev::{Service, ServiceRequest, ServiceResponse, Transform};
    use actix_web::{Error, HttpMessage};
    use futures::future::{ready, LocalBoxFuture, Ready};
    use std::task::{Context, Poll};

    /// Minimal middleware that installs a `RequestDatabaseManager` on every
    /// request — standing in for a composing layer's middleware that
    /// pre-resolves the manager per request.
    #[derive(Clone)]
    struct InjectManager(Arc<DatabaseManager>);

    impl<S> Transform<S, ServiceRequest> for InjectManager
    where
        S: Service<ServiceRequest, Response = ServiceResponse, Error = Error> + 'static,
    {
        type Response = ServiceResponse;
        type Error = Error;
        type Transform = InjectManagerMw<S>;
        type InitError = ();
        type Future = Ready<Result<Self::Transform, Self::InitError>>;

        fn new_transform(&self, service: S) -> Self::Future {
            ready(Ok(InjectManagerMw {
                service: Arc::new(service),
                manager: Arc::clone(&self.0),
            }))
        }
    }

    struct InjectManagerMw<S> {
        service: Arc<S>,
        manager: Arc<DatabaseManager>,
    }

    impl<S> Service<ServiceRequest> for InjectManagerMw<S>
    where
        S: Service<ServiceRequest, Response = ServiceResponse, Error = Error> + 'static,
    {
        type Response = ServiceResponse;
        type Error = Error;
        type Future = LocalBoxFuture<'static, Result<Self::Response, Self::Error>>;

        fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            self.service.poll_ready(cx)
        }

        fn call(&self, req: ServiceRequest) -> Self::Future {
            req.extensions_mut()
                .insert(RequestDatabaseManager(Arc::clone(&self.manager)));
            let fut = self.service.call(req);
            Box::pin(fut)
        }
    }

    /// Read the JSON-RPC result text from a tool-call SSE response body.
    async fn read_tool_text(resp: ServiceResponse) -> String {
        let body = to_bytes(resp.into_body()).await.unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        // The streamed body is `data: {json}\n\n`; pull the JSON line and dig
        // out the tool result's text content.
        let data_line = body
            .lines()
            .find_map(|l| l.strip_prefix("data: "))
            .expect("SSE body should carry a data line");
        let value: serde_json::Value = serde_json::from_str(data_line).unwrap();
        value["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }

    /// Build a transport over an empty baked-in manager, plus a *separate*
    /// override manager whose active database holds one atom. Returns the
    /// override manager and the created atom id.
    async fn override_manager_with_atom() -> (Arc<DatabaseManager>, tempfile::TempDir, String) {
        let temp = tempfile::TempDir::new().unwrap();
        let manager = Arc::new(DatabaseManager::new(temp.path()).unwrap());
        let core = manager.active_core().await.unwrap();
        let atom = core
            .create_atom(
                atomic_core::CreateAtomRequest {
                    content: "override-manager-atom-body".to_string(),
                    source_url: None,
                    published_at: None,
                    tag_ids: vec![],
                    skip_if_source_exists: false,
                },
                |_| {},
            )
            .await
            .unwrap()
            .unwrap();
        (manager, temp, atom.atom.id)
    }

    fn read_atom_request(id: i32, atom_id: &str) -> serde_json::Value {
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": { "name": "read_atom", "arguments": { "atom_id": atom_id } }
        })
    }

    #[actix_web::test]
    async fn tool_call_resolves_request_manager_override() {
        // Baked-in manager is empty; the override manager holds the atom.
        let (transport, _baked) = test_transport();
        let (override_mgr, _override_temp, atom_id) = override_manager_with_atom().await;

        let app = actix_test::init_service(
            App::new().service(
                web::scope("/mcp")
                    .service(transport.scope())
                    .wrap(InjectManager(override_mgr)),
            ),
        )
        .await;
        let session_id = initialize_session!(app);
        send_initialized!(app, session_id);

        let req = actix_test::TestRequest::post()
            .uri("/mcp")
            .insert_header((
                header::ACCEPT,
                format!("{JSON_MIME_TYPE}, {EVENT_STREAM_MIME_TYPE}"),
            ))
            .insert_header((header::CONTENT_TYPE, JSON_MIME_TYPE))
            .insert_header((HEADER_SESSION_ID, session_id))
            .set_payload(read_atom_request(2, &atom_id).to_string())
            .to_request();
        let resp = actix_test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::OK);

        // The atom lives only in the override manager, so resolving against it
        // returns the body — proving the per-request override won over the
        // empty baked-in manager.
        let text = read_tool_text(resp).await;
        assert!(
            text.contains("override-manager-atom-body"),
            "tool must resolve against the injected per-request manager: {text}"
        );
    }

    #[actix_web::test]
    async fn tool_call_falls_back_to_baked_in_manager_without_override() {
        // No InjectManager middleware: exactly how the standalone server runs.
        // The atom lives in a *different* manager the transport never sees, so
        // the baked-in (empty) manager reports it missing — byte-identical to
        // self-hosted behavior.
        let (transport, _baked) = test_transport();
        let (_override_mgr, _override_temp, atom_id) = override_manager_with_atom().await;

        let app = actix_test::init_service(
            App::new().service(web::scope("/mcp").service(transport.scope())),
        )
        .await;
        let session_id = initialize_session!(app);
        send_initialized!(app, session_id);

        let req = actix_test::TestRequest::post()
            .uri("/mcp")
            .insert_header((
                header::ACCEPT,
                format!("{JSON_MIME_TYPE}, {EVENT_STREAM_MIME_TYPE}"),
            ))
            .insert_header((header::CONTENT_TYPE, JSON_MIME_TYPE))
            .insert_header((HEADER_SESSION_ID, session_id))
            .set_payload(read_atom_request(2, &atom_id).to_string())
            .to_request();
        let resp = actix_test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::OK);

        let text = read_tool_text(resp).await;
        assert!(
            text.contains("Atom not found"),
            "without an override the baked-in manager is used: {text}"
        );
    }

    /// Build a manager with two databases: the ACTIVE (default) one holds
    /// `active-kb-atom-body`, a SECOND non-active one holds
    /// `second-kb-atom-body`. Returns the manager, the second db's id, and the
    /// two atom ids `(active_atom, second_atom)`.
    async fn manager_with_two_kbs() -> (
        Arc<DatabaseManager>,
        tempfile::TempDir,
        String,
        String,
        String,
    ) {
        let temp = tempfile::TempDir::new().unwrap();
        let manager = Arc::new(DatabaseManager::new(temp.path()).unwrap());

        let create = |core: atomic_core::AtomicCore, body: &'static str| async move {
            core.create_atom(
                atomic_core::CreateAtomRequest {
                    content: body.to_string(),
                    source_url: None,
                    published_at: None,
                    tag_ids: vec![],
                    skip_if_source_exists: false,
                },
                |_| {},
            )
            .await
            .unwrap()
            .unwrap()
            .atom
            .id
        };

        let active_core = manager.active_core().await.unwrap();
        let active_atom = create(active_core, "active-kb-atom-body").await;

        let second = manager.create_database("Second KB").await.unwrap();
        let second_core = manager.get_core(&second.id).await.unwrap();
        let second_atom = create(second_core, "second-kb-atom-body").await;

        (manager, temp, second.id, active_atom, second_atom)
    }

    #[actix_web::test]
    async fn tool_call_db_selection_honors_x_atomic_database_header_over_active() {
        // The transport selects the request's database with the SAME precedence
        // as the `Db` extractor: `X-Atomic-Database` header first, then `?db=`,
        // then the manager's active db. Here the active db is a *different* KB
        // than the one the header names, so a tool call carrying only the header
        // (no `?db=`) must resolve against the header's KB — not the active one.
        let (transport, _baked) = test_transport();
        let (mgr, _temp, second_db, active_atom, second_atom) = manager_with_two_kbs().await;

        let app = actix_test::init_service(
            App::new().service(
                web::scope("/mcp")
                    .service(transport.scope())
                    .wrap(InjectManager(mgr)),
            ),
        )
        .await;
        let session_id = initialize_session!(app);
        send_initialized!(app, session_id);

        // read_atom on the SECOND KB's atom, selecting that KB via the header
        // only — resolves there, returning its body.
        let req = actix_test::TestRequest::post()
            .uri("/mcp")
            .insert_header((
                header::ACCEPT,
                format!("{JSON_MIME_TYPE}, {EVENT_STREAM_MIME_TYPE}"),
            ))
            .insert_header((header::CONTENT_TYPE, JSON_MIME_TYPE))
            .insert_header((HEADER_SESSION_ID, session_id.clone()))
            .insert_header(("X-Atomic-Database", second_db.clone()))
            .set_payload(read_atom_request(2, &second_atom).to_string())
            .to_request();
        let resp = actix_test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let text = read_tool_text(resp).await;
        assert!(
            text.contains("second-kb-atom-body"),
            "the header selects the second KB: {text}"
        );

        // The ACTIVE KB's atom is NOT reachable through the header-selected KB —
        // proving the header won over the active-db fallback (not the reverse).
        let req = actix_test::TestRequest::post()
            .uri("/mcp")
            .insert_header((
                header::ACCEPT,
                format!("{JSON_MIME_TYPE}, {EVENT_STREAM_MIME_TYPE}"),
            ))
            .insert_header((header::CONTENT_TYPE, JSON_MIME_TYPE))
            .insert_header((HEADER_SESSION_ID, session_id))
            .insert_header(("X-Atomic-Database", second_db))
            .set_payload(read_atom_request(3, &active_atom).to_string())
            .to_request();
        let resp = actix_test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let text = read_tool_text(resp).await;
        assert!(
            text.contains("Atom not found"),
            "header selection does not fall through to the active KB: {text}"
        );
    }
}
