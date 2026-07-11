//! API tokens and web sessions in the control plane.
//!
//! `cloud_tokens` is the single source of truth for every API token in
//! cloud — account-scope, KB-scope, MCP-scope — there is no per-tenant
//! `api_tokens` table (plan: "Auth & tenant routing" → "Token model").
//! Sessions live in their own table because their lifetime and revocation
//! UX differ.
//!
//! Both secrets follow the same discipline: an opaque random plaintext is
//! returned to the caller exactly once, only its SHA-256 hash is persisted,
//! and verification is always scoped by `account_id` — a valid credential
//! for account A presented in account B's context must not verify, which is
//! what enforces cross-tenant isolation at the lowest layer.
//!
//! This slice covers issuance and verification only. The OAuth flow that
//! mints MCP-scoped tokens, cookie handling, and token-management routes
//! arrive with the signup and OAuth slices (plan: "OAuth", "MCP token UX").

use std::str::FromStr;
use std::time::Duration;

use chrono::{DateTime, Utc};
use rand::RngCore;
use sha2::{Digest, Sha256};

use crate::control_plane::ControlPlane;
use crate::error::CloudError;

/// Prefix on API-token plaintexts (plan: opaque `atm_<random>`).
pub const TOKEN_PREFIX: &str = "atm_";

/// Prefix on session plaintexts. Distinct from [`TOKEN_PREFIX`] so a leaked
/// credential is immediately classifiable and a session cookie pasted where
/// a token belongs fails loudly rather than confusingly.
pub const SESSION_PREFIX: &str = "ats_";

/// What a token may reach (plan: "Token model"). Serialized to text in
/// `cloud_tokens.scope`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenScope {
    /// Full access to everything under the account.
    Account,
    /// Pinned to a single knowledge base (`cloud_tokens.allowed_db_id`).
    Database,
    /// Issued through the MCP OAuth flow; typically database-scoped.
    Mcp,
}

impl TokenScope {
    /// The text stored in `cloud_tokens.scope`.
    pub fn as_str(self) -> &'static str {
        match self {
            TokenScope::Account => "account",
            TokenScope::Database => "database",
            TokenScope::Mcp => "mcp",
        }
    }
}

impl std::fmt::Display for TokenScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for TokenScope {
    type Err = CloudError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "account" => Ok(TokenScope::Account),
            "database" => Ok(TokenScope::Database),
            "mcp" => Ok(TokenScope::Mcp),
            other => Err(CloudError::InvalidTokenScope(other.to_string())),
        }
    }
}

/// A verified `cloud_tokens` row.
#[derive(Debug, Clone)]
pub struct TokenRecord {
    /// SHA-256 hex of the plaintext — the row's primary key.
    pub hash: String,
    pub account_id: String,
    pub scope: TokenScope,
    /// For [`TokenScope::Database`] (and typically [`TokenScope::Mcp`])
    /// tokens: the one `db_id` the token may touch. The auth middleware's
    /// chokepoint check compares this against the resolved database.
    pub allowed_db_id: Option<String>,
    /// Human-readable label chosen at issuance ("my-laptop").
    pub name: String,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    pub revoked_at: Option<DateTime<Utc>>,
}

/// Raw row shape; `scope` parses into [`TokenScope`] in `TryFrom`.
#[derive(sqlx::FromRow)]
struct TokenRow {
    hash: String,
    account_id: String,
    scope: String,
    allowed_db_id: Option<String>,
    name: String,
    created_at: DateTime<Utc>,
    last_used_at: Option<DateTime<Utc>>,
    expires_at: Option<DateTime<Utc>>,
    revoked_at: Option<DateTime<Utc>>,
}

impl TryFrom<TokenRow> for TokenRecord {
    type Error = CloudError;

    fn try_from(row: TokenRow) -> Result<Self, Self::Error> {
        Ok(TokenRecord {
            scope: row.scope.parse()?,
            hash: row.hash,
            account_id: row.account_id,
            allowed_db_id: row.allowed_db_id,
            name: row.name,
            created_at: row.created_at,
            last_used_at: row.last_used_at,
            expires_at: row.expires_at,
            revoked_at: row.revoked_at,
        })
    }
}

/// A verified `sessions` row.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SessionRecord {
    /// SHA-256 hex of the plaintext — the row's primary key.
    pub hash: String,
    pub account_id: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub ip_first_seen: Option<String>,
    pub ua_first_seen: Option<String>,
}

/// Generate an opaque secret: `prefix` + 32 bytes from the OS RNG, base32
/// (RFC 4648, no padding, lowercased). Returns `(plaintext, sha256_hex)` —
/// only the hash is ever persisted. Shared with [`crate::magic_links`],
/// which follows the same discipline under its own prefix.
pub(crate) fn generate_secret(prefix: &str) -> (String, String) {
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    let plaintext = format!(
        "{prefix}{}",
        data_encoding::BASE32_NOPAD
            .encode(&bytes)
            .to_ascii_lowercase()
    );
    let hash = sha256_hex(&plaintext);
    (plaintext, hash)
}

/// SHA-256 of the full plaintext (prefix included), lowercase hex.
pub(crate) fn sha256_hex(plaintext: &str) -> String {
    data_encoding::HEXLOWER.encode(&Sha256::digest(plaintext.as_bytes()))
}

/// Issue a new API token for `account_id` and return its plaintext —
/// the only time the plaintext ever exists outside the caller's hands.
///
/// Tokens issued here don't expire (`expires_at` stays NULL); revocation is
/// the lifecycle tool. Expiring issuance arrives with the OAuth/MCP slice,
/// which needs it for authorization-code-derived tokens.
pub async fn issue_token(
    control: &ControlPlane,
    account_id: &str,
    scope: TokenScope,
    allowed_db_id: Option<&str>,
    name: &str,
) -> Result<String, CloudError> {
    let (plaintext, hash) = generate_secret(TOKEN_PREFIX);
    sqlx::query(
        "INSERT INTO cloud_tokens (hash, account_id, scope, allowed_db_id, name, token_prefix) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(&hash)
    .bind(account_id)
    .bind(scope.as_str())
    .bind(allowed_db_id)
    .bind(name)
    .bind(display_prefix(&plaintext))
    .execute(control.pool())
    .await
    .map_err(CloudError::db("inserting cloud token"))?;
    Ok(plaintext)
}

/// The stored display prefix: the plaintext's first 10 chars
/// (`atm_` + 6). Enough to tell tokens apart in a list — and to match the
/// self-hosted UI's "is this the token I'm using?" comparison — without
/// meaningfully narrowing the 32-random-byte search space.
pub(crate) fn display_prefix(plaintext: &str) -> &str {
    &plaintext[..10.min(plaintext.len())]
}

/// Token metadata for the tenant token-management surface: everything a
/// list view needs, never the hash's preimage. `id` is the hash itself —
/// it identifies the row for revocation and is useless for authentication
/// (verifying requires the plaintext preimage).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TokenMeta {
    #[sqlx(rename = "hash")]
    pub id: String,
    pub name: String,
    pub scope: String,
    pub token_prefix: Option<String>,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
}

/// The account's live tokens (not revoked, not expired), oldest first.
/// Includes OAuth-minted MCP tokens — surfacing them is how a user audits
/// and severs MCP-client access.
pub async fn list_account_tokens(
    control: &ControlPlane,
    account_id: &str,
) -> Result<Vec<TokenMeta>, CloudError> {
    sqlx::query_as(
        "SELECT hash, name, scope, token_prefix, created_at, last_used_at \
         FROM cloud_tokens \
         WHERE account_id = $1 \
           AND revoked_at IS NULL \
           AND (expires_at IS NULL OR expires_at > NOW()) \
         ORDER BY created_at",
    )
    .bind(account_id)
    .fetch_all(control.pool())
    .await
    .map_err(CloudError::db("listing cloud tokens"))
}

/// Revoke one of the account's tokens by id (the hash). Returns `false`
/// when the id names no live token of THIS account — the account scoping
/// is the same cross-tenant chokepoint as verification, so one tenant's
/// revoke can never touch another's row.
pub async fn revoke_account_token(
    control: &ControlPlane,
    account_id: &str,
    token_id: &str,
) -> Result<bool, CloudError> {
    let result = sqlx::query(
        "UPDATE cloud_tokens SET revoked_at = NOW() \
         WHERE account_id = $1 AND hash = $2 AND revoked_at IS NULL",
    )
    .bind(account_id)
    .bind(token_id)
    .execute(control.pool())
    .await
    .map_err(CloudError::db("revoking cloud token"))?;
    Ok(result.rows_affected() > 0)
}

/// Verify a token plaintext against `account_id`.
///
/// Returns the live row — not revoked, not expired, and belonging to this
/// account — or `None`. The account scoping is the cross-tenant chokepoint:
/// the same plaintext presented for any other account verifies nothing.
/// A successful verification stamps `last_used_at` in the same statement.
pub async fn verify_token(
    control: &ControlPlane,
    account_id: &str,
    plaintext: &str,
) -> Result<Option<TokenRecord>, CloudError> {
    let row: Option<TokenRow> = sqlx::query_as(
        "UPDATE cloud_tokens SET last_used_at = NOW() \
         WHERE account_id = $1 AND hash = $2 \
           AND revoked_at IS NULL \
           AND (expires_at IS NULL OR expires_at > NOW()) \
         RETURNING hash, account_id, scope, allowed_db_id, name, \
                   created_at, last_used_at, expires_at, revoked_at",
    )
    .bind(account_id)
    .bind(sha256_hex(plaintext))
    .fetch_optional(control.pool())
    .await
    .map_err(CloudError::db("verifying cloud token"))?;
    row.map(TokenRecord::try_from).transpose()
}

/// Create a web session for `account_id`, expiring after `ttl`, and return
/// its plaintext (the value the cookie will carry). Only the SHA-256 hash
/// is stored. `ip_first_seen` / `ua_first_seen` are forensic breadcrumbs;
/// pass `None` when a proxy strips them.
pub async fn create_session(
    control: &ControlPlane,
    account_id: &str,
    ttl: Duration,
    ip_first_seen: Option<&str>,
    ua_first_seen: Option<&str>,
) -> Result<String, CloudError> {
    let ttl = chrono::Duration::from_std(ttl)
        .map_err(|_| CloudError::Invariant("session ttl out of range".to_string()))?;
    let expires_at = Utc::now() + ttl;
    let (plaintext, hash) = generate_secret(SESSION_PREFIX);
    sqlx::query(
        "INSERT INTO sessions (hash, account_id, expires_at, ip_first_seen, ua_first_seen) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(&hash)
    .bind(account_id)
    .bind(expires_at)
    .bind(ip_first_seen)
    .bind(ua_first_seen)
    .execute(control.pool())
    .await
    .map_err(CloudError::db("inserting session"))?;
    Ok(plaintext)
}

/// Verify a session plaintext against `account_id`. Returns the unexpired
/// row scoped to this account, or `None`. Same cross-tenant chokepoint as
/// [`verify_token`] — the shared `.atomic.cloud` cookie crosses subdomains
/// by design, so this `account_id` filter is what isolates tenants.
pub async fn verify_session(
    control: &ControlPlane,
    account_id: &str,
    plaintext: &str,
) -> Result<Option<SessionRecord>, CloudError> {
    sqlx::query_as(
        "SELECT hash, account_id, created_at, expires_at, ip_first_seen, ua_first_seen \
         FROM sessions \
         WHERE account_id = $1 AND hash = $2 AND expires_at > NOW()",
    )
    .bind(account_id)
    .bind(sha256_hex(plaintext))
    .fetch_optional(control.pool())
    .await
    .map_err(CloudError::db("verifying session"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_roundtrips_through_text() {
        for scope in [TokenScope::Account, TokenScope::Database, TokenScope::Mcp] {
            assert_eq!(scope.as_str().parse::<TokenScope>().unwrap(), scope);
        }
        assert!(matches!(
            "admin".parse::<TokenScope>(),
            Err(CloudError::InvalidTokenScope(_))
        ));
    }

    #[test]
    fn secrets_are_prefixed_unique_and_never_store_plaintext() {
        let (token, token_hash) = generate_secret(TOKEN_PREFIX);
        let (session, session_hash) = generate_secret(SESSION_PREFIX);
        assert!(token.starts_with(TOKEN_PREFIX));
        assert!(session.starts_with(SESSION_PREFIX));
        // 32 bytes -> 52 base32 chars; plus the 4-char prefix.
        assert_eq!(token.len(), TOKEN_PREFIX.len() + 52);
        // The stored value is the digest, not the plaintext.
        assert_eq!(token_hash, sha256_hex(&token));
        assert_ne!(token_hash, token);
        assert_eq!(token_hash.len(), 64);
        // Two draws never collide.
        assert_ne!(
            generate_secret(TOKEN_PREFIX).0,
            generate_secret(TOKEN_PREFIX).0
        );
        let _ = session_hash;
    }
}
