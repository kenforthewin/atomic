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

    /// A tenant-database operation through atomic-core failed (migrations,
    /// default-KB seeding). `context` says what was being attempted.
    #[error("{context}: {source}")]
    Core {
        context: String,
        #[source]
        source: atomic_core::AtomicCoreError,
    },

    /// A filesystem operation failed (e.g. creating the fallback scratch
    /// directory). `context` says what was being attempted.
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },

    /// The requested subdomain doesn't match the signup slug rule
    /// (`[a-z0-9-]{3,32}`).
    #[error("invalid subdomain {0:?}: must be 3-32 chars of [a-z0-9-]")]
    InvalidSubdomain(String),

    /// The requested subdomain is on the static platform blocklist or held
    /// in `subdomains_reserved` (post-deletion 90-day park).
    #[error("subdomain {0:?} is reserved")]
    SubdomainReserved(String),

    /// Another account already owns the requested subdomain. Provisioning
    /// claims subdomains via the `accounts.subdomain` UNIQUE constraint;
    /// the violation maps here, making "taken" a race-free signal.
    #[error("subdomain {0:?} is already taken")]
    SubdomainTaken(String),

    /// The signup email failed the basic shape check.
    #[error("invalid email address {0:?}")]
    InvalidEmail(String),

    /// No active `account_databases` row exists for an account the cache
    /// was asked to load — either provisioning never finished or the row
    /// was deleted out from under a live credential.
    #[error("account {0} has no active tenant database")]
    MissingTenantDatabase(String),

    /// The accounts row stopped being `status = 'provisioning'` partway
    /// through [`provision_account`](crate::provision::provision_account) —
    /// a concurrent [`delete_account`](crate::provision::delete_account)
    /// removed it (or a competing run changed it). The losing provision
    /// aborts, dropping any tenant database it just created so nothing is
    /// orphaned.
    #[error("account {0} is no longer provisioning; provision aborted")]
    AccountNoLongerProvisioning(String),

    /// A `cloud_tokens.scope` value didn't parse as a [`TokenScope`]
    /// (`account` | `database` | `mcp`).
    ///
    /// [`TokenScope`]: crate::tokens::TokenScope
    #[error("unknown token scope {0:?}")]
    InvalidTokenScope(String),

    /// A control-plane invariant the code relies on was violated (e.g. an
    /// `accounts.id` that isn't a UUID). Indicates corruption or a bug, not
    /// a user error.
    #[error("control-plane invariant violated: {0}")]
    Invariant(String),
}

impl CloudError {
    /// Build a closure that wraps an [`sqlx::Error`] with `context` —
    /// keeps `map_err` call sites to one line.
    pub(crate) fn db(context: impl Into<String>) -> impl FnOnce(sqlx::Error) -> CloudError {
        let context = context.into();
        move |source| CloudError::Database { context, source }
    }

    /// Like [`CloudError::db`], for errors crossing back from atomic-core.
    pub(crate) fn core(
        context: impl Into<String>,
    ) -> impl FnOnce(atomic_core::AtomicCoreError) -> CloudError {
        let context = context.into();
        move |source| CloudError::Core { context, source }
    }
}
