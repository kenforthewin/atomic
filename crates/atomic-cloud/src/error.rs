//! Error type shared across atomic-cloud.

/// Errors produced by the cloud composition layer.
#[derive(Debug, thiserror::Error)]
pub enum CloudError {
    /// The configured control-plane connection URL failed to parse.
    #[error("invalid control-plane database URL: {0}")]
    InvalidUrl(String),

    /// A Postgres database name contained characters outside
    /// `[A-Za-z0-9_-]`. Database names are interpolated into DDL as quoted
    /// identifiers (they cannot be bound as parameters), so anything more
    /// exotic is rejected outright.
    #[error("invalid database name {0:?}: only [A-Za-z0-9_-] is permitted")]
    InvalidDatabaseName(String),

    /// A control-plane database operation failed. `context` says what was
    /// being attempted; `source` is the underlying sqlx error.
    #[error("{context}: {source}")]
    Database {
        context: String,
        #[source]
        source: sqlx::Error,
    },
}

impl CloudError {
    /// Build a closure that wraps an [`sqlx::Error`] with `context` —
    /// keeps `map_err` call sites to one line.
    pub(crate) fn db(context: impl Into<String>) -> impl FnOnce(sqlx::Error) -> CloudError {
        let context = context.into();
        move |source| CloudError::Database { context, source }
    }
}
