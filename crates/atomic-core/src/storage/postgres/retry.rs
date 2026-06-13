//! Retry wrapper for transient Postgres errors.
//!
//! Postgres can fail a transaction with `40P01` (deadlock_detected) or `40001`
//! (serialization_failure) under contention. Both are transient by definition
//! — the application is expected to retry the offending transaction from
//! scratch. sqlx does not retry on its own, so high-contention call sites
//! (concurrent claim/lease updates, ledger inserts) wrap their work in
//! [`with_retry`] to absorb these errors invisibly.
//!
//! Tuning: 4 total attempts with exponential backoff starting at 10ms
//! (10, 50, 250, 1250ms). That covers the typical "two workers raced on the
//! same row set" case without adding noticeable latency to the happy path.
//! Non-transient errors bypass the loop and surface immediately.

use std::future::Future;
use std::time::Duration;

use sqlx::Error as SqlxError;

const MAX_ATTEMPTS: u32 = 4;
const INITIAL_BACKOFF: Duration = Duration::from_millis(10);
const BACKOFF_FACTOR: u32 = 5;

/// Run an async closure with retry on transient Postgres errors.
///
/// The closure is invoked from scratch on each attempt — pass a reference to
/// the pool or transaction-builder, not a half-built future, so the retry can
/// re-execute the full statement against a fresh connection.
pub(crate) async fn with_retry<F, Fut, T>(mut f: F) -> Result<T, SqlxError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, SqlxError>>,
{
    let mut backoff = INITIAL_BACKOFF;
    for attempt in 1..=MAX_ATTEMPTS {
        match f().await {
            Ok(value) => return Ok(value),
            Err(e) if attempt < MAX_ATTEMPTS && is_transient(&e) => {
                tracing::debug!(
                    attempt,
                    backoff_ms = backoff.as_millis() as u64,
                    error = %e,
                    "Retrying transient Postgres error"
                );
                tokio::time::sleep(backoff).await;
                backoff *= BACKOFF_FACTOR;
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!("loop exits via return in the final iteration")
}

/// Returns true for Postgres SQLSTATE codes that indicate a transient,
/// retryable failure: deadlock_detected (40P01) and serialization_failure
/// (40001). Anything else — including connection errors, unique violations,
/// syntax errors — surfaces immediately.
fn is_transient(err: &SqlxError) -> bool {
    if let SqlxError::Database(db_err) = err {
        if let Some(code) = db_err.code() {
            return matches!(code.as_ref(), "40P01" | "40001");
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::borrow::Cow;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    /// Minimal `DatabaseError` so tests can construct `SqlxError::Database`
    /// with a chosen SQLSTATE — sqlx's real `PgDatabaseError` is only built
    /// from the wire protocol and cannot be instantiated directly.
    #[derive(Debug)]
    struct MockDbError {
        code: &'static str,
    }

    impl std::fmt::Display for MockDbError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "mock db error {}", self.code)
        }
    }

    impl std::error::Error for MockDbError {}

    impl sqlx::error::DatabaseError for MockDbError {
        fn message(&self) -> &str {
            "mock"
        }
        fn code(&self) -> Option<Cow<'_, str>> {
            Some(Cow::Borrowed(self.code))
        }
        fn as_error(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
            self
        }
        fn as_error_mut(&mut self) -> &mut (dyn std::error::Error + Send + Sync + 'static) {
            self
        }
        fn into_error(self: Box<Self>) -> Box<dyn std::error::Error + Send + Sync + 'static> {
            self
        }
        fn kind(&self) -> sqlx::error::ErrorKind {
            sqlx::error::ErrorKind::Other
        }
    }

    fn transient(code: &'static str) -> SqlxError {
        SqlxError::Database(Box::new(MockDbError { code }))
    }

    #[tokio::test(start_paused = true)]
    async fn returns_immediately_on_success() {
        let attempts = Arc::new(AtomicU32::new(0));
        let attempts_clone = attempts.clone();
        let result: Result<i32, SqlxError> = with_retry(move || {
            let attempts = attempts_clone.clone();
            async move {
                attempts.fetch_add(1, Ordering::SeqCst);
                Ok(42)
            }
        })
        .await;

        assert_eq!(result.unwrap(), 42);
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn surfaces_non_transient_immediately() {
        let attempts = Arc::new(AtomicU32::new(0));
        let attempts_clone = attempts.clone();
        let result: Result<(), SqlxError> = with_retry(move || {
            let attempts = attempts_clone.clone();
            async move {
                attempts.fetch_add(1, Ordering::SeqCst);
                Err(SqlxError::RowNotFound)
            }
        })
        .await;

        assert!(matches!(result, Err(SqlxError::RowNotFound)));
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            1,
            "non-transient errors should not be retried"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn retries_on_deadlock_then_succeeds() {
        let attempts = Arc::new(AtomicU32::new(0));
        let attempts_clone = attempts.clone();
        let result: Result<i32, SqlxError> = with_retry(move || {
            let attempts = attempts_clone.clone();
            async move {
                let n = attempts.fetch_add(1, Ordering::SeqCst) + 1;
                if n < 3 {
                    Err(transient("40P01"))
                } else {
                    Ok(n as i32)
                }
            }
        })
        .await;

        assert_eq!(result.unwrap(), 3);
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            3,
            "should retry through the first two transient failures"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn gives_up_after_max_attempts() {
        let attempts = Arc::new(AtomicU32::new(0));
        let attempts_clone = attempts.clone();
        let result: Result<(), SqlxError> = with_retry(move || {
            let attempts = attempts_clone.clone();
            async move {
                attempts.fetch_add(1, Ordering::SeqCst);
                Err(transient("40001"))
            }
        })
        .await;

        assert!(matches!(result, Err(SqlxError::Database(_))));
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            MAX_ATTEMPTS,
            "should exhaust the full retry budget before giving up"
        );
    }
}
