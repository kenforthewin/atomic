//! Error type shared across atomic-cloud.

/// Errors produced by the cloud composition layer.
#[derive(Debug, thiserror::Error)]
pub enum CloudError {
    /// A configured URL (control plane, tenant cluster, or app public
    /// origin) failed to parse. The message says which.
    #[error("invalid URL: {0}")]
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

    /// A `magic_links.purpose` value didn't parse as a
    /// [`MagicLinkPurpose`] (`signup` | `login`).
    ///
    /// [`MagicLinkPurpose`]: crate::magic_links::MagicLinkPurpose
    #[error("unknown magic-link purpose {0:?}")]
    InvalidMagicLinkPurpose(String),

    /// Delivering a magic-link email failed (transport error or a
    /// non-success provider response). The message carries provider
    /// status/body text and **never** the link — the link is the
    /// credential (see [`crate::email`]).
    #[error("email send failed: {0}")]
    EmailSend(String),

    /// The provider-credential master key is missing or malformed. Raised
    /// at vault construction — i.e. at boot — never mid-request. The
    /// message names the environment variable and the expected shape; it
    /// **never** contains key material, valid or not.
    #[error("invalid provider-credential master key: {0}")]
    InvalidMasterKey(String),

    /// A stored `provider_credentials.encryption_version` is one this
    /// build's [`KeyVault`] doesn't know. Decrypting under the wrong key
    /// generation would fail confusingly (or worse, succeed by accident
    /// under a future scheme); reject typed instead.
    ///
    /// [`KeyVault`]: crate::keyvault::KeyVault
    #[error("unknown provider-credential encryption version {0}")]
    UnknownEncryptionVersion(i32),

    /// AES-256-GCM encryption failed. Practically unreachable with a valid
    /// key and fresh nonce (the cipher only rejects absurd plaintext
    /// lengths); kept typed so the vault never panics on provider input.
    #[error("provider-credential encryption failed")]
    CredentialEncrypt,

    /// AEAD authentication failed on decrypt: wrong master key, a
    /// ciphertext presented under a different (account, provider) binding
    /// than it was encrypted for, or a corrupt row. The message carries
    /// only that context — never key bytes or ciphertext.
    #[error("provider-credential decryption failed: {0}")]
    CredentialDecrypt(String),

    /// A `provider_credentials.provider` value didn't parse as a
    /// [`Provider`] (`openrouter` | `openai_compat`).
    ///
    /// [`Provider`]: crate::provider_credentials::Provider
    #[error("unknown provider {0:?}")]
    InvalidProvider(String),

    /// A `provider_credentials.origin` value didn't parse as a
    /// [`CredentialOrigin`] (`managed` | `user`).
    ///
    /// [`CredentialOrigin`]: crate::provider_credentials::CredentialOrigin
    #[error("unknown credential origin {0:?}")]
    InvalidCredentialOrigin(String),

    /// An `accounts.provider_pause_kind` value didn't parse as a
    /// [`PauseKind`] (`rate_limit` | `credits`).
    ///
    /// [`PauseKind`]: crate::backpressure::PauseKind
    #[error("unknown provider pause kind {0:?}")]
    InvalidPauseKind(String),

    /// [`set_active_provider`] was asked to point an account at a
    /// `(provider, origin)` with no stored credentials row (or the account
    /// itself doesn't exist). The flip is refused — an active pointer must
    /// always resolve to a decryptable row.
    ///
    /// [`set_active_provider`]: crate::provider_credentials::set_active_provider
    #[error("account {account_id} has no {provider}/{origin} provider credentials")]
    MissingProviderCredentials {
        account_id: String,
        provider: crate::provider_credentials::Provider,
        origin: crate::provider_credentials::CredentialOrigin,
    },

    /// The OpenRouter provisioning key is missing or empty. Raised at
    /// client construction — i.e. at boot — never mid-request. The message
    /// names the environment variable; it **never** contains key material.
    #[error("invalid provider-provisioning key: {0}")]
    InvalidProvisioningKey(String),

    /// A provisioning-API call (create/update/delete/usage of a managed
    /// runtime key) failed: transport error, non-success status, or an
    /// unparseable response. `context` says which operation; `message`
    /// carries the status and a bounded slice of the provider's error body
    /// — never the provisioning key, and never a runtime-key plaintext
    /// (success bodies, the only ones that carry keys, are withheld from
    /// decode errors by construction; see [`crate::provisioning_api`]).
    #[error("provisioning API: {context}: {message}")]
    ProviderProvisioning { context: String, message: String },

    /// A control-plane invariant the code relies on was violated (e.g. an
    /// `accounts.id` that isn't a UUID). Indicates corruption or a bug, not
    /// a user error.
    #[error("control-plane invariant violated: {0}")]
    Invariant(String),

    /// A Stripe API call (checkout/portal/subscription/customer) failed:
    /// transport error, non-success status, or an unparseable response.
    /// `context` says which operation; `message` carries the status and a
    /// bounded slice of Stripe's error body — never the Stripe secret key.
    #[error("Stripe API: {context}: {message}")]
    Stripe { context: String, message: String },

    /// A Stripe webhook failed verification: a malformed `Stripe-Signature`
    /// header, a timestamp outside the tolerance window (replay defense), or
    /// a signature that didn't match the signing secret. The message names
    /// the failure mode; it never contains the signing secret or a valid
    /// signature. Maps to a 400 — a genuine Stripe delivery never produces
    /// it, so the only sender that sees it is a forger.
    #[error("Stripe webhook verification failed: {0}")]
    WebhookVerification(String),

    /// The Stripe configuration (secret key, webhook secret, or a plan's
    /// `stripe_price_id`) is missing or empty. Raised at client construction
    /// — i.e. at boot — or when a billing route runs without Stripe
    /// configured. The message names what's missing; it never contains key
    /// material.
    #[error("invalid Stripe configuration: {0}")]
    InvalidStripeConfig(String),

    /// A backup object-store operation failed (put/get/list/exists), or its
    /// configuration was invalid. `context` carries the operation and the
    /// underlying store error; it never contains S3 credentials (those live
    /// only in the process environment — see [`crate::backup_store`]).
    #[error("backup store: {0}")]
    BackupStore(String),

    /// A `pg_dump` / `pg_restore` invocation failed: the binary was missing,
    /// the process exited non-zero, or its output couldn't be captured. The
    /// message carries the operation, the exit status, and a **bounded** tail
    /// of the tool's stderr — never the connection password, which is passed
    /// to the child via `PGPASSWORD` in its environment and never appears in
    /// argv or in any error value (see [`crate::backup`]).
    #[error("backup runner: {0}")]
    Backup(String),

    /// A per-account operation couldn't take the account's advisory lock
    /// because a concurrent holder (a backup pass mid-dump, or a reaper pass)
    /// owns it past the retry budget. Surfaced by [`delete_account`] in
    /// [`DeleteLock::Acquire`] mode (adversarial-review issue 2): the caller
    /// retries rather than racing a `DROP DATABASE` against a live dump. The
    /// HTTP route maps this to a 503 (retryable).
    ///
    /// [`delete_account`]: crate::provision::delete_account
    /// [`DeleteLock::Acquire`]: crate::provision::DeleteLock::Acquire
    #[error("resource busy: {0}")]
    Busy(String),
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

    /// Wrap a filesystem [`std::io::Error`] from the local backup store with
    /// `context`, as a [`CloudError::BackupStore`]. Keeps the local store's
    /// `map_err` call sites to one line.
    pub(crate) fn backup_io(
        context: impl Into<String>,
    ) -> impl FnOnce(std::io::Error) -> CloudError {
        let context = context.into();
        move |source| CloudError::BackupStore(format!("{context}: {source}"))
    }
}
