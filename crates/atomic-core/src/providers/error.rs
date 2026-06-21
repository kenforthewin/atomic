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
    /// Check if this error is retryable (same request or smaller batch).
    /// Only 400 (bad request) and 401 (auth) are permanent — everything
    /// else (404, 413, 5xx, etc.) may succeed with a smaller batch or on retry.
    pub fn is_retryable(&self) -> bool {
        match self {
            ProviderError::RateLimited { .. } | ProviderError::Network(_) => true,
            ProviderError::Api { status, .. } => !matches!(status, 400 | 401),
            _ => false,
        }
    }

    /// Whether reducing batch size might resolve this error.
    /// 400 errors may indicate the provider's batch limit was exceeded;
    /// splitting the batch can succeed where retrying the same size won't.
    pub fn is_batch_reducible(&self) -> bool {
        matches!(self, ProviderError::Api { status: 400, .. })
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
