//! The per-tenant streaming-chat semaphore (plan: "Worker fairness & job
//! queue" → "Streaming chat (not in a pool)").
//!
//! # Why chat is not pooled
//!
//! Every other AI work-type flows through a durable ledger and the
//! dispatcher's bounded pools ([`crate::dispatcher`]) because it is
//! background work: nobody is watching, so it can wait its fair turn.
//! Streaming chat is the opposite on every axis — request-driven (the user
//! just hit send), user-facing (tokens stream to an open UI), and
//! latency-critical (queueing the first token behind another tenant's wiki
//! synthesis would be product-breaking). Putting it in a pool would also
//! buy nothing durable: there is no ledger row to re-claim — a pod restart
//! simply terminates in-flight streams and the frontend retries (plan:
//! "Restart semantics").
//!
//! What chat still needs is an *abuse bound*: one account scripting dozens
//! of concurrent conversations must not monopolize the pod's connections
//! and the provider's goodwill. So the cap is a per-account semaphore at
//! the route, sized for humans (a person genuinely using the product holds
//! 1-2 streams; the default 3 leaves headroom for a retry racing its
//! predecessor). Provider rate limits do the actual throughput throttling
//! downstream.
//!
//! # The permit's lifetime
//!
//! The cap is only real if the permit spans the *stream*, not the routing.
//! [`chat_stream_guard`] acquires before calling into the handler and then
//! moves the permit **into the response body** ([`GuardedStreamBody`]), so
//! it releases when the body finishes streaming or is dropped (client
//! disconnect included) — never at headers-time. Today's chat handler
//! happens to stream over WebSocket while the HTTP response stays small,
//! which the held `next.call` await already covers; tying the permit to the
//! body as well makes the cap robust to the handler growing a streaming
//! (SSE) response body, where the handler future returns long before the
//! last byte.
//!
//! # Over-cap answer
//!
//! A structured 429 — `{ "error": "too_many_streams", "retry_after_seconds"
//! }` plus a `Retry-After` header. The wait hint is advisory (the real
//! signal is a stream finishing, which this process can't predict), small
//! enough that a legitimate client retries promptly.
//!
//! Like the worker-pool caps, the limit is **per pod**: an account's
//! effective fleet-wide cap is `cap × pod count`. Fine at small pod counts;
//! same scaling caveat as the plan's "Shape" section.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use actix_web::body::{BodySize, BoxBody, MessageBody};
use actix_web::dev::{ServiceRequest, ServiceResponse};
use actix_web::http::{header, Method};
use actix_web::middleware::Next;
use actix_web::web::Bytes;
use actix_web::{web, HttpMessage, HttpResponse};

use crate::auth::ResolvedTenant;

/// Default concurrent-stream cap per account (plan: "Streaming chat (not in
/// a pool)" — cap = 3). A `serve` CLI knob
/// (`--chat-streams-per-account`).
pub const DEFAULT_CHAT_STREAMS_PER_ACCOUNT: usize = 3;

/// Advisory retry hint on the over-cap 429, in seconds. The true release
/// signal is one of the account's streams finishing, which the server
/// cannot schedule — this is just a polite client pacing value.
const RETRY_AFTER_SECS: u64 = 5;

/// Whether `(method, path)` is a streaming-chat request: `POST
/// /api/conversations/{id}/messages` (atomic-server's `send_chat_message`
/// route — the one handler that holds a provider stream open for the whole
/// agent loop).
pub fn chat_stream_route(method: &Method, path: &str) -> bool {
    if *method != Method::POST {
        return false;
    }
    path.strip_prefix("/api/conversations/")
        .and_then(|rest| rest.split_once('/'))
        .is_some_and(|(id, tail)| !id.is_empty() && tail == "messages")
}

/// The per-account concurrent-stream counter. One instance per process —
/// construct once and clone into every worker's `configure_cloud_app` call
/// (a per-worker instance would multiply the cap by the worker count).
#[derive(Clone)]
pub struct ChatStreamLimiter {
    inner: Arc<LimiterInner>,
}

struct LimiterInner {
    max_per_account: usize,
    /// `account_id → streams in flight`. Entries are removed at zero, so
    /// the map's size is bounded by accounts *currently* streaming.
    counts: Mutex<HashMap<String, usize>>,
}

impl ChatStreamLimiter {
    /// `max_per_account` is clamped to at least 1 — a zero cap would brick
    /// chat entirely, which is never the intent of a tuning knob.
    pub fn new(max_per_account: usize) -> Self {
        Self {
            inner: Arc::new(LimiterInner {
                max_per_account: max_per_account.max(1),
                counts: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Try to start a stream for `account_id`. `None` when the account is
    /// at its cap; the returned permit releases the slot on drop.
    pub fn try_begin(&self, account_id: &str) -> Option<ChatStreamPermit> {
        let mut counts = self.inner.counts.lock().expect("stream counts poisoned");
        let count = counts.entry(account_id.to_string()).or_insert(0);
        if *count >= self.inner.max_per_account {
            return None;
        }
        *count += 1;
        Some(ChatStreamPermit {
            account_id: account_id.to_string(),
            inner: Arc::clone(&self.inner),
        })
    }

    /// Streams currently in flight for `account_id`. Test/metrics
    /// instrumentation.
    pub fn in_flight(&self, account_id: &str) -> usize {
        self.inner
            .counts
            .lock()
            .expect("stream counts poisoned")
            .get(account_id)
            .copied()
            .unwrap_or(0)
    }
}

/// One admitted stream's slot. Released on drop — hold it for exactly the
/// stream's lifetime (the guard moves it into the response body).
pub struct ChatStreamPermit {
    account_id: String,
    inner: Arc<LimiterInner>,
}

impl Drop for ChatStreamPermit {
    fn drop(&mut self) {
        let mut counts = self.inner.counts.lock().expect("stream counts poisoned");
        if let Some(count) = counts.get_mut(&self.account_id) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                counts.remove(&self.account_id);
            }
        }
    }
}

/// Response body that owns a [`ChatStreamPermit`]: byte-for-byte the inner
/// body, releasing the permit when the body is dropped — after the last
/// byte streams out, or early on client disconnect (module docs: the
/// permit's lifetime).
struct GuardedStreamBody {
    inner: BoxBody,
    _permit: ChatStreamPermit,
}

impl MessageBody for GuardedStreamBody {
    type Error = Box<dyn std::error::Error>;

    fn size(&self) -> BodySize {
        self.inner.size()
    }

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Bytes, Self::Error>>> {
        // `BoxBody` is `Unpin` (its stream variant is already boxed), so
        // plain projection is sound.
        Pin::new(&mut self.get_mut().inner).poll_next(cx)
    }
}

/// Data-plane middleware: cap concurrent chat streams per account. Wired
/// inside CloudAuth and the plane guard (so [`ResolvedTenant`] is always
/// installed; a missing extension is skipped defensively — the plane guard
/// already fails such requests closed) and *outside* the hint writer, so an
/// over-cap denial never reaches a handler and never marks a dispatch hint.
///
/// Non-chat routes pass through untouched. For chat, the permit is acquired
/// before the handler runs and travels with the response body until the
/// stream is done (module docs: the permit's lifetime).
pub async fn chat_stream_guard(
    limiter: web::Data<ChatStreamLimiter>,
    req: ServiceRequest,
    next: Next<impl MessageBody + 'static>,
) -> Result<ServiceResponse<BoxBody>, actix_web::Error> {
    if !chat_stream_route(req.method(), req.path()) {
        return next.call(req).await.map(|res| res.map_into_boxed_body());
    }
    let account_id = req
        .extensions()
        .get::<ResolvedTenant>()
        .map(|tenant| tenant.principal.account_id.clone());
    let Some(account_id) = account_id else {
        return next.call(req).await.map(|res| res.map_into_boxed_body());
    };

    let Some(permit) = limiter.try_begin(&account_id) else {
        let denial = HttpResponse::TooManyRequests()
            .insert_header((header::RETRY_AFTER, RETRY_AFTER_SECS))
            .json(serde_json::json!({
                "error": "too_many_streams",
                "message": "Too many chat streams are already running for this \
                            account. Wait for one to finish and retry.",
                "retry_after_seconds": RETRY_AFTER_SECS,
            }));
        return Ok(req.into_response(denial));
    };

    let res = next.call(req).await?;
    Ok(res
        .map_into_boxed_body()
        .map_body(|_, inner| GuardedStreamBody {
            inner,
            _permit: permit,
        })
        .map_into_boxed_body())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use actix_web::http::StatusCode;
    use actix_web::middleware::from_fn;
    use actix_web::test as actix_test;
    use actix_web::App;

    use super::*;
    use crate::auth::{AuthPrincipal, CredentialSource};
    use crate::tokens::TokenScope;

    #[test]
    fn chat_route_matcher() {
        let post = Method::POST;
        assert!(chat_stream_route(&post, "/api/conversations/abc/messages"));
        for open in [
            "/api/conversations",
            "/api/conversations/abc",
            "/api/conversations//messages",
            "/api/conversations/abc/scope",
            "/api/atoms",
        ] {
            assert!(!chat_stream_route(&post, open), "{open} must stay open");
        }
        // Reads on the messages path are not streams.
        assert!(!chat_stream_route(
            &Method::GET,
            "/api/conversations/abc/messages"
        ));
    }

    /// The counting semantics, deterministically: cap admits exactly
    /// `max_per_account`, accounts are isolated, a dropped permit reopens
    /// the slot, and the map prunes to nothing.
    #[test]
    fn limiter_caps_and_releases() {
        let limiter = ChatStreamLimiter::new(3);
        let p1 = limiter.try_begin("a").expect("first stream");
        let _p2 = limiter.try_begin("a").expect("second stream");
        let _p3 = limiter.try_begin("a").expect("third stream");
        assert!(limiter.try_begin("a").is_none(), "fourth must be refused");
        assert_eq!(limiter.in_flight("a"), 3);

        // Another account is unaffected by a's saturation.
        let pb = limiter.try_begin("b").expect("other account streams");
        drop(pb);
        assert_eq!(limiter.in_flight("b"), 0, "entries prune at zero");

        // Releasing one of a's permits reopens exactly one slot.
        drop(p1);
        assert_eq!(limiter.in_flight("a"), 2);
        let _p4 = limiter.try_begin("a").expect("freed slot admits again");
        assert!(limiter.try_begin("a").is_none());
    }

    #[test]
    fn zero_cap_clamps_to_one() {
        let limiter = ChatStreamLimiter::new(0);
        assert!(
            limiter.try_begin("a").is_some(),
            "a zero cap must degrade to serial, not brick chat"
        );
    }

    fn fake_tenant() -> ResolvedTenant {
        ResolvedTenant {
            principal: AuthPrincipal {
                account_id: "acct-1".to_string(),
                scope: TokenScope::Account,
                allowed_db_id: None,
                source: CredentialSource::Token,
            },
            subdomain: "alpha".to_string(),
            provider_pause: None,
        }
    }

    /// Test stand-in for CloudAuth: installs the [`ResolvedTenant`]
    /// extension the guard reads. Wrapped outermost (registered last) so it
    /// runs before the guard.
    async fn install_tenant(
        req: ServiceRequest,
        next: Next<impl MessageBody + 'static>,
    ) -> Result<ServiceResponse<impl MessageBody>, actix_web::Error> {
        req.extensions_mut().insert(fake_tenant());
        next.call(req).await
    }

    /// The load-bearing property (module docs: the permit's lifetime): with
    /// a handler that returns a *streaming* response body, the permit is
    /// still held after the handler future completes (headers out, body
    /// pending) and releases only when the body is consumed. A permit
    /// released at headers-time would show `in_flight == 0` at the first
    /// assertion and make the cap a no-op for SSE-shaped responses.
    #[actix_web::test]
    async fn permit_spans_response_body_not_just_handler() {
        async fn streaming_handler() -> HttpResponse {
            // Two chunks with a beat between them, so the body is provably
            // pending after the handler returns.
            let stream = futures::stream::unfold(0u8, |n| async move {
                match n {
                    0 => Some((
                        Ok::<Bytes, std::io::Error>(Bytes::from_static(b"first ")),
                        1,
                    )),
                    1 => {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        Some((Ok(Bytes::from_static(b"second")), 2))
                    }
                    _ => None,
                }
            });
            HttpResponse::Ok().streaming(stream)
        }

        let limiter = ChatStreamLimiter::new(3);
        let app = actix_test::init_service(
            App::new()
                .app_data(web::Data::new(limiter.clone()))
                .route(
                    "/api/conversations/{id}/messages",
                    web::post().to(streaming_handler),
                )
                .wrap(from_fn(chat_stream_guard))
                .wrap(from_fn(install_tenant)),
        )
        .await;

        let req = actix_test::TestRequest::post()
            .uri("/api/conversations/c1/messages")
            .to_request();
        let res = actix_test::call_service(&app, req).await;
        assert_eq!(res.status(), StatusCode::OK);

        // Handler future is done (we hold the response), body is not: the
        // permit must still be held.
        assert_eq!(
            limiter.in_flight("acct-1"),
            1,
            "permit must outlive the handler future"
        );

        // Drain the body; the permit releases with it.
        let collected = actix_test::read_body(res).await;
        assert_eq!(&collected[..], b"first second");
        assert_eq!(
            limiter.in_flight("acct-1"),
            0,
            "permit must release when the body finishes"
        );
    }

    /// Over-cap requests get the structured 429 without invoking the
    /// handler, and a dropped (not consumed) response body also releases —
    /// the client-disconnect path.
    #[actix_web::test]
    async fn over_cap_denies_and_dropped_body_releases() {
        let limiter = ChatStreamLimiter::new(1);
        let app = actix_test::init_service(
            App::new()
                .app_data(web::Data::new(limiter.clone()))
                .route(
                    "/api/conversations/{id}/messages",
                    web::post().to(|| async { HttpResponse::Ok().body("reply") }),
                )
                .wrap(from_fn(chat_stream_guard))
                .wrap(from_fn(install_tenant)),
        )
        .await;

        // Hold the only slot by keeping an unconsumed response around.
        let held = actix_test::call_service(
            &app,
            actix_test::TestRequest::post()
                .uri("/api/conversations/c1/messages")
                .to_request(),
        )
        .await;
        assert_eq!(held.status(), StatusCode::OK);
        assert_eq!(limiter.in_flight("acct-1"), 1);

        // Second stream: structured 429 with the retry hint, header + body.
        let denied = actix_test::call_service(
            &app,
            actix_test::TestRequest::post()
                .uri("/api/conversations/c2/messages")
                .to_request(),
        )
        .await;
        assert_eq!(denied.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            denied
                .headers()
                .get(header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok()),
            Some(RETRY_AFTER_SECS.to_string().as_str())
        );
        let body: serde_json::Value = actix_test::read_body_json(denied).await;
        assert_eq!(body["error"], "too_many_streams");
        assert_eq!(body["retry_after_seconds"], RETRY_AFTER_SECS);

        // GETs on the same path are not streams and pass while saturated
        // (404 here — no such route — but crucially not 429).
        let read = actix_test::call_service(
            &app,
            actix_test::TestRequest::get()
                .uri("/api/conversations/c1/messages")
                .to_request(),
        )
        .await;
        assert_ne!(read.status(), StatusCode::TOO_MANY_REQUESTS);

        // Dropping the held response without reading its body releases the
        // slot — the disconnected-client path.
        drop(held);
        assert_eq!(limiter.in_flight("acct-1"), 0);
        let reopened = actix_test::call_service(
            &app,
            actix_test::TestRequest::post()
                .uri("/api/conversations/c3/messages")
                .to_request(),
        )
        .await;
        assert_eq!(reopened.status(), StatusCode::OK);
    }
}
