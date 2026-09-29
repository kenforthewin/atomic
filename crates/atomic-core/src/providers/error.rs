//! Provider error types

use std::fmt;

/// Errors that can occur during provider operations
#[derive(Debug)]
pub enum ProviderError {
    /// Network/connection error
    Network(String),

    /// API error with status code
    Api { status: u16, message: String },

    /// Rate limited - may include retry-after hint
    RateLimited { retry_after_secs: Option<u64> },

    /// Model not found or unavailable
    ModelNotFound(String),

    /// Configuration error (missing API key, invalid settings, etc.)
    Configuration(String),

    /// Capability not supported by this provider
    CapabilityNotSupported(String),

    /// Failed to parse response
    ParseError(String),

    /// Provider not initialized
    NotInitialized,
}

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProviderError::Network(msg) => write!(f, "Network error: {}", msg),
            ProviderError::Api { status, message } => {
                write!(f, "API error ({}): {}", status, message)
            }
            ProviderError::RateLimited { retry_after_secs } => {
                if let Some(secs) = retry_after_secs {
                    write!(f, "Rate limited, retry after {} seconds", secs)
                } else {
                    write!(f, "Rate limited")
                }
            }
            ProviderError::ModelNotFound(model) => write!(f, "Model not found: {}", model),
            ProviderError::Configuration(msg) => write!(f, "Configuration error: {}", msg),
            ProviderError::CapabilityNotSupported(cap) => {
                write!(f, "Capability not supported: {}", cap)
            }
            ProviderError::ParseError(msg) => write!(f, "Parse error: {}", msg),
            ProviderError::NotInitialized => write!(f, "Provider not initialized"),
        }
    }
}

impl std::error::Error for ProviderError {}

impl ProviderError {
    /// Check if re-sending the same request may succeed: rate limits,
    /// network faults, and the transient HTTP statuses (408 timeout, 425/429
    /// throttling, 5xx server faults).
    ///
    /// Every other 4xx is a verdict on the request or the account, not the
    /// moment — above all 402 and 401/403, which is how a provider reports an
    /// exhausted credit limit or a revoked key. Retrying those in-call
    /// can't succeed and only multiplies the failed calls (and the time
    /// spent holding shared permits) until the caller finally sees the
    /// error; callers that need to wait them out do so at the scheduling
    /// layer via [`classify_provider_failure`].
    pub fn is_retryable(&self) -> bool {
        match self {
            ProviderError::RateLimited { .. } | ProviderError::Network(_) => true,
            ProviderError::Api { status, .. } => {
                matches!(status, 408 | 425 | 429 | 500..=599)
            }
            _ => false,
        }
    }

    /// Whether reducing batch size might resolve this error: a 400 may mean
    /// the provider's batch limit was exceeded, and a 413 that the payload
    /// was too large. Splitting the batch can succeed where retrying the
    /// same size won't.
    pub fn is_batch_reducible(&self) -> bool {
        matches!(self, ProviderError::Api { status: 400 | 413, .. })
    }

    /// Get suggested retry delay in seconds
    pub fn retry_after(&self) -> Option<u64> {
        match self {
            ProviderError::RateLimited { retry_after_secs } => *retry_after_secs,
            ProviderError::Network(_) => Some(1), // Default 1 second for network errors
            _ => None,
        }
    }
}

/// Coarse classification of a provider failure, recovered from an error
/// *message* — see [`classify_provider_failure`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderFailureClass {
    /// The provider rate-limited the call (HTTP 429 /
    /// [`ProviderError::RateLimited`]), with the `Retry-After` hint when the
    /// provider sent one.
    RateLimited { retry_after_secs: Option<u64> },
    /// The provider refused the call for billing reasons (HTTP 402 — e.g.
    /// an exhausted prepaid balance or per-key credit limit).
    PaymentRequired,
    /// The provider rejected the stored credentials (HTTP 401/403 — an
    /// expired, revoked, or mis-scoped API key). Like billing failures,
    /// these are environmental: no retry succeeds until the key is fixed.
    AuthFailed,
    /// The provider was transiently unavailable (HTTP 5xx, a connection or
    /// timeout error) — a server-side or network fault, not a permanent
    /// rejection of the request. Like a rate limit, the same call may
    /// succeed on a later attempt, so a scheduler should back off and retry
    /// rather than terminally fail the work. Mirrors the retryable set of
    /// [`ProviderError::is_retryable`] that isn't already a more specific
    /// class above.
    Transient,
    /// Anything else (including messages this classifier doesn't recognize).
    Other,
}

/// Classify a stringly provider failure by recognizing **this module's own
/// `Display` renderings** of [`ProviderError`].
///
/// Most failure paths flatten `ProviderError` into a `String` long before a
/// scheduler sees it (`EmbedError::message`, embedding-event payloads,
/// `task_runs.last_error`), usually wrapped in further context
/// (`"Embedding error: Provider error: Rate limited…"`). Hosts that drive
/// the durable ledgers need the rate-limit/billing signal back out of those
/// strings to schedule honest backoff, so the parser lives here — next to
/// the `Display` impl it must stay in sync with, pinned by the round-trip
/// tests below — rather than rotting in some caller.
///
/// Matching is substring-based and deliberately conservative: rate-limit
/// first (its rendering never embeds a response body), then the 402/401/403
/// status markers, and finally the transient 5xx / network markers. A
/// response body that *contains* one of these markers can misclassify, but
/// only on a call that already failed — the cost is a gentler retry
/// schedule, never a dropped result.
pub fn classify_provider_failure(message: &str) -> ProviderFailureClass {
    // `ProviderError::RateLimited` renders as "Rate limited" or
    // "Rate limited, retry after {N} seconds".
    if let Some(idx) = message.find("Rate limited") {
        let tail = &message[idx + "Rate limited".len()..];
        let retry_after_secs = tail.strip_prefix(", retry after ").and_then(|rest| {
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            digits.parse::<u64>().ok()
        });
        return ProviderFailureClass::RateLimited { retry_after_secs };
    }
    // `ProviderError::Api { status: 402, .. }` renders as "API error (402): …".
    if message.contains("API error (402)") {
        return ProviderFailureClass::PaymentRequired;
    }
    // `ProviderError::Api { status: 401 | 403, .. }` — credential rejections.
    if message.contains("API error (401)") || message.contains("API error (403)") {
        return ProviderFailureClass::AuthFailed;
    }
    // Transient server-side / network faults: a 5xx upstream error or a
    // connection/timeout failure. These are the retryable-but-not-yet-classified
    // members of `ProviderError::is_retryable` (everything `is_retryable`
    // covers that the rate-limit / payment / auth arms above don't already
    // claim). The same call may succeed on a later attempt, so a scheduler
    // backs off rather than terminally failing the work.
    if message.contains("API error (5") || message.contains("Network error:") {
        return ProviderFailureClass::Transient;
    }
    ProviderFailureClass::Other
}

impl From<reqwest::Error> for ProviderError {
    fn from(err: reqwest::Error) -> Self {
        ProviderError::Network(err.to_string())
    }
}

impl From<serde_json::Error> for ProviderError {
    fn from(err: serde_json::Error) -> Self {
        ProviderError::ParseError(err.to_string())
    }
}

// Allow converting to String for backward compatibility
impl From<ProviderError> for String {
    fn from(err: ProviderError) -> Self {
        err.to_string()
    }
}

#[cfg(test)]
mod classification_tests {
    use super::*;

    /// The classifier must recover what `Display` rendered — these
    /// round-trips are the contract that keeps the two in sync. A change to
    /// the renderings that forgets the parser fails here.
    #[test]
    fn classify_round_trips_display_renderings() {
        let with_hint = ProviderError::RateLimited {
            retry_after_secs: Some(30),
        };
        assert_eq!(
            classify_provider_failure(&with_hint.to_string()),
            ProviderFailureClass::RateLimited {
                retry_after_secs: Some(30)
            }
        );

        let without_hint = ProviderError::RateLimited {
            retry_after_secs: None,
        };
        assert_eq!(
            classify_provider_failure(&without_hint.to_string()),
            ProviderFailureClass::RateLimited {
                retry_after_secs: None
            }
        );

        let payment = ProviderError::Api {
            status: 402,
            message: "Insufficient credits".to_string(),
        };
        assert_eq!(
            classify_provider_failure(&payment.to_string()),
            ProviderFailureClass::PaymentRequired
        );

        for status in [401u16, 403] {
            let auth = ProviderError::Api {
                status,
                message: "key revoked".to_string(),
            };
            assert_eq!(
                classify_provider_failure(&auth.to_string()),
                ProviderFailureClass::AuthFailed,
                "{status} must classify as an auth failure"
            );
        }
    }

    /// Billing and credential rejections must fail fast: an exhausted
    /// per-key limit (OpenRouter's 403 "Key limit exceeded") retried in-call
    /// turned every embedding batch into a storm of doomed requests.
    #[test]
    fn only_transient_api_statuses_are_retryable() {
        let api = |status| ProviderError::Api {
            status,
            message: "boom".to_string(),
        };
        for status in [408u16, 425, 429, 500, 502, 503, 504] {
            assert!(api(status).is_retryable(), "{status} should be retryable");
        }
        for status in [400u16, 401, 402, 403, 404, 413, 422] {
            assert!(!api(status).is_retryable(), "{status} must not be retryable");
        }
    }

    #[test]
    fn oversized_batches_are_reducible() {
        for status in [400u16, 413] {
            let err = ProviderError::Api {
                status,
                message: "too big".to_string(),
            };
            assert!(err.is_batch_reducible(), "{status} should split the batch");
        }
        let limit = ProviderError::Api {
            status: 403,
            message: "Key limit exceeded".to_string(),
        };
        assert!(!limit.is_batch_reducible());
    }

    /// Real failure strings arrive wrapped in caller context; the substring
    /// match must survive the wrapping.
    #[test]
    fn classify_survives_error_wrapping() {
        assert_eq!(
            classify_provider_failure(
                "Embedding error: Provider error: Rate limited, retry after 120 seconds"
            ),
            ProviderFailureClass::RateLimited {
                retry_after_secs: Some(120)
            }
        );
        assert_eq!(
            classify_provider_failure("Wiki error: API error (402): out of credits"),
            ProviderFailureClass::PaymentRequired
        );
    }

    /// Server-side and network faults classify as `Transient` so a
    /// scheduler backs off and retries rather than terminally failing —
    /// these mirror the retryable members of [`ProviderError::is_retryable`]
    /// not already claimed by the rate-limit / payment / auth arms.
    #[test]
    fn classify_recognizes_transient_failures() {
        for message in [
            "API error (500): upstream exploded",
            "API error (502): bad gateway",
            "API error (503): service unavailable",
            "Embedding error: Provider error: Network error: connection refused",
            "Network error: timed out",
        ] {
            assert_eq!(
                classify_provider_failure(message),
                ProviderFailureClass::Transient,
                "{message:?} must classify as Transient"
            );
        }
    }

    /// Genuinely unrelated failures stay `Other` — the conservative default.
    #[test]
    fn classify_leaves_unrelated_errors_alone() {
        for message in [
            "Parse error: bad JSON",
            "Model not found: gpt-nonexistent",
            "",
        ] {
            assert_eq!(
                classify_provider_failure(message),
                ProviderFailureClass::Other,
                "{message:?} must classify as Other"
            );
        }
    }

    /// A malformed retry-after tail degrades to "rate limited, no hint",
    /// never to a parse failure.
    #[test]
    fn classify_tolerates_malformed_retry_after() {
        assert_eq!(
            classify_provider_failure("Rate limited, retry after soon-ish"),
            ProviderFailureClass::RateLimited {
                retry_after_secs: None
            }
        );
    }
}

/// Truncate a string to at most `max_bytes` bytes without splitting a UTF-8 character.
pub fn truncate_utf8(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    // Find the largest char boundary <= max_bytes
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// The gateway's id for a request. Lives in the headers, which arrive before
/// the body — so it survives the failures where the body doesn't, and resolves
/// via `GET /api/v1/generation?id=…` to provider, timings, finish reason and
/// cost. Often the only thread back to a call that delivered nothing.
pub fn gateway_trace_id(headers: &reqwest::header::HeaderMap) -> Option<String> {
    ["x-generation-id", "x-request-id"]
        .iter()
        .find_map(|name| headers.get(*name))
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

/// Render a response body for a log line. Never raw: a padded body (see
/// [`decode_error`]) prints as an empty field plus a few hundred newlines
/// dumped into the log, so the diagnostic goes blank exactly when the body is
/// what you need. Whitespace bodies are described; everything else is escaped.
pub fn body_for_log(body: &str, max_bytes: usize) -> String {
    if body.is_empty() {
        return "<empty body>".to_string();
    }
    if body.trim().is_empty() {
        return format!(
            "<{} bytes of gateway padding, {} newlines, no payload>",
            body.len(),
            body.matches('\n').count()
        );
    }
    format!("{:?}", truncate_utf8(body, max_bytes))
}

/// Classify a 2xx body that would not deserialize.
///
/// OpenRouter commits `200 OK` before the upstream produces anything, then
/// holds the connection open with JSON whitespace (invisible to a parser, per
/// RFC 8259). The status can't be retracted afterwards, so a late failure ends
/// the body instead — leaving a complete, well-formed 200 containing only
/// padding. That's a transport failure, not a malformed answer, and the
/// difference decides whether the work is retried or dropped.
///
/// `serde_json` draws the line: `Category::Eof` means the input ran out (empty,
/// all padding, or cut mid-JSON) — always a truncated transfer. `Syntax` and
/// `Data` mean bytes arrived and were wrong, which no retry fixes.
pub fn decode_error(
    what: &str,
    body: &str,
    err: &serde_json::Error,
    trace_id: Option<&str>,
) -> ProviderError {
    let trace = trace_id
        .map(|id| format!(" [generation {id}]"))
        .unwrap_or_default();

    if err.classify() != serde_json::error::Category::Eof {
        return ProviderError::ParseError(format!("Failed to parse {what}: {err}{trace}"));
    }

    let detail = if body.is_empty() {
        "the gateway committed 200 and sent no body at all".to_string()
    } else if body.trim().is_empty() {
        format!(
            "the gateway committed 200, padded for {} bytes ({} newlines), and ended the \
             body without ever sending a payload",
            body.len(),
            body.matches('\n').count()
        )
    } else {
        format!("the body ended mid-JSON after {} bytes ({err})", body.len())
    };
    // Rendered as "Network error: …", which `is_retryable` and
    // `classify_provider_failure` both already read as transient.
    ProviderError::Network(format!("truncated {what}: {detail}{trace}"))
}

/// Streaming counterpart of [`decode_error`]: a committed 200 whose stream
/// closed without carrying a payload. Distinct from "the model returned empty
/// content", which is a real answer; this is the absence of one.
pub fn stream_delivered_nothing(what: &str, trace_id: Option<&str>) -> ProviderError {
    let trace = trace_id
        .map(|id| format!(" [generation {id}]"))
        .unwrap_or_default();
    ProviderError::Network(format!(
        "truncated {what}: the gateway committed 200 and closed the stream without \
         delivering any payload{trace}"
    ))
}

#[cfg(test)]
mod transport_tests {
    use super::*;

    fn eof_err(body: &str) -> serde_json::Error {
        serde_json::from_str::<serde_json::Value>(body).unwrap_err()
    }

    /// The reported failure: a complete 200 whose body is nothing but
    /// keepalive padding must be retryable, and must read as `Transient` to a
    /// scheduler.
    #[test]
    fn padded_body_is_a_retryable_transient_failure() {
        let body = "\n         \n".repeat(116);
        let err = decode_error("chat response", &body, &eof_err(&body), Some("gen-abc"));

        assert!(matches!(err, ProviderError::Network(_)), "got {err:?}");
        assert!(err.is_retryable());
        assert_eq!(
            classify_provider_failure(&err.to_string()),
            ProviderFailureClass::Transient
        );
        let rendered = err.to_string();
        assert!(rendered.contains("232 newlines"), "{rendered}");
        assert!(rendered.contains("gen-abc"), "{rendered}");
    }

    /// A body cut mid-JSON is the same transfer failure.
    #[test]
    fn truncated_json_is_transient_too() {
        let body = r#"{"id":"gen-1","choices":["#;
        let err = decode_error("chat response", body, &eof_err(body), None);
        assert!(matches!(err, ProviderError::Network(_)), "got {err:?}");
        assert!(err.is_retryable());
    }

    /// Bytes that genuinely arrived and were wrong stay a parse error — no
    /// retry fixes a schema mismatch or malformed JSON.
    #[test]
    fn real_parse_failures_stay_permanent() {
        let syntax = "{not json";
        let err = decode_error("chat response", syntax, &eof_err(syntax), None);
        assert!(matches!(err, ProviderError::ParseError(_)), "got {err:?}");
        assert!(!err.is_retryable());

        // Valid JSON, wrong shape: serde reports Data, not Eof.
        let data_err = serde_json::from_str::<Vec<u32>>("{\"a\":1}").unwrap_err();
        let err = decode_error("chat response", "{\"a\":1}", &data_err, None);
        assert!(matches!(err, ProviderError::ParseError(_)), "got {err:?}");
    }

    /// A padded body must never be logged raw — that is why the field came
    /// back blank in the original report.
    #[test]
    fn padding_is_described_not_dumped() {
        let body = "\n         \n".repeat(116);
        let rendered = body_for_log(&body, 500);
        assert!(!rendered.contains('\n'), "log line must stay one line");
        assert!(rendered.contains("232 newlines"), "{rendered}");

        assert_eq!(body_for_log("", 500), "<empty body>");
        assert_eq!(body_for_log("{\"a\":1}", 500), "\"{\\\"a\\\":1}\"");
    }
}
