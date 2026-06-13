//! Per-account OAuth storage in the control plane (plan: "Auth & tenant
//! routing" → control-plane schema `oauth_clients`/`oauth_codes`, and the
//! "OAuth" subsection).
//!
//! Cloud has its **own** OAuth flow — Dynamic Client Registration +
//! Authorization Code + PKCE — distinct from atomic-server's self-hosted
//! implementation, which is left untouched. The reason is structural: the
//! self-hosted handlers store clients and codes in atomic-core's *registry*,
//! and cloud runs in Postgres mode with no registry (`registry: None`). So
//! cloud's flow stores its OAuth state here, in the control plane, with one
//! invariant the self-hosted version doesn't need: **every row is scoped by
//! `account_id`** — each subdomain has its own OAuth identity, and a
//! `client_id` minted under account A must never resolve under account B.
//! That cross-tenant chokepoint lives in the WHERE clause of every query in
//! this module, exactly as it does for [`crate::tokens`] and
//! [`crate::magic_links`].
//!
//! # Secret hygiene
//!
//! Following the slice-1/2 rule (opaque random plaintext returned once, only
//! the SHA-256 hex persisted):
//!
//! - **`client_id`** ([`OAUTH_CLIENT_PREFIX`]) is a *public* identifier — the
//!   client presents it openly on every request — so it is stored in
//!   plaintext. It is opaque and unguessable purely so a client_id can't be
//!   forged to probe for another account's registration (the `account_id`
//!   scope is the real guard).
//! - **client secret** is the DCR-issued credential. The plaintext is
//!   returned by [`create_oauth_client`] once and never stored; only its
//!   SHA-256 hex (`client_secret_hash`) is persisted, verified later by the
//!   token endpoint.
//! - **authorization code** ([`OAUTH_CODE_PREFIX`]) is hash-only like a magic
//!   link: [`insert_oauth_code`] returns the plaintext (it lives only in the
//!   redirect back to the client) and persists `SHA-256(code)`.
//! - **`code_challenge`** is the PKCE challenge, already a hash of the
//!   client's secret verifier — not a secret at rest — so it is stored as-is
//!   and compared at exchange time against the freshly-hashed verifier.

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::Value as JsonValue;

use crate::control_plane::ControlPlane;
use crate::error::CloudError;
use crate::tokens::{generate_secret, sha256_hex, TokenScope};

/// Prefix on `oauth_clients.client_id`. Distinct from the `atm_`/`ats_`/`aml_`
/// credential prefixes so a leaked value is immediately classifiable. "occ" =
/// OAuth Cloud Client.
pub const OAUTH_CLIENT_PREFIX: &str = "occ_";

/// Prefix on authorization-code plaintexts. "oac" = OAuth Authorization Code.
pub const OAUTH_CODE_PREFIX: &str = "oac_";

/// Default authorization-code lifetime. Auth codes are exchanged for a token
/// within seconds of the redirect; RFC 6749 §4.1.2 recommends a maximum of 10
/// minutes and "strongly" prefers shorter. 60s is comfortable for a real
/// round-trip yet leaves a stolen code almost no window. The token endpoint
/// also enforces single-use, so the TTL is defense in depth. Issuance takes
/// an explicit TTL so tests can shrink (or stretch) it; every production
/// caller passes this.
pub const OAUTH_CODE_TTL: Duration = Duration::from_secs(60);

/// A Dynamically-Registered OAuth client, as returned by
/// [`get_oauth_client`]. Scoped to one account; carries everything the
/// authorize/token endpoints need to validate a request.
#[derive(Debug, Clone)]
pub struct OAuthClient {
    pub client_id: String,
    pub account_id: String,
    /// SHA-256 hex of the DCR-issued client secret — the token endpoint
    /// hashes the presented secret and compares.
    pub client_secret_hash: String,
    pub client_name: String,
    /// The redirect URIs the client registered. The authorize endpoint
    /// refuses any `redirect_uri` not in this list.
    pub redirect_uris: Vec<String>,
    pub created_at: DateTime<Utc>,
}

/// Raw `oauth_clients` row; `redirect_uris` is JSONB decoded into a
/// `Vec<String>` in [`TryFrom`].
#[derive(sqlx::FromRow)]
struct OAuthClientRow {
    client_id: String,
    account_id: String,
    client_secret_hash: String,
    client_name: String,
    redirect_uris: JsonValue,
    created_at: DateTime<Utc>,
}

impl TryFrom<OAuthClientRow> for OAuthClient {
    type Error = CloudError;

    fn try_from(row: OAuthClientRow) -> Result<Self, Self::Error> {
        let redirect_uris = serde_json::from_value(row.redirect_uris).map_err(|e| {
            CloudError::Invariant(format!(
                "oauth_clients.redirect_uris is not a string array: {e}"
            ))
        })?;
        Ok(OAuthClient {
            client_id: row.client_id,
            account_id: row.account_id,
            client_secret_hash: row.client_secret_hash,
            client_name: row.client_name,
            redirect_uris,
            created_at: row.created_at,
        })
    }
}

/// A consumed `oauth_codes` row, as returned by [`consume_oauth_code`].
/// Carries the PKCE challenge (verified by the token endpoint against the
/// presented `code_verifier`), the bound `client_id`/`redirect_uri` (both must
/// match the exchange request), and the scope/KB-pin to mint the token with.
#[derive(Debug, Clone)]
pub struct OAuthCode {
    /// SHA-256 hex of the plaintext code — the row's primary key.
    pub code_hash: String,
    pub account_id: String,
    pub client_id: String,
    /// The PKCE challenge (BASE64URL(SHA256(verifier)) for S256).
    pub code_challenge: String,
    pub code_challenge_method: String,
    pub redirect_uri: String,
    /// Scope to mint the exchanged token with. Defaults to account-scope for
    /// MCP (one MCP URL per account; see the slice decision in the plan), but
    /// a db-pinned authorization sets [`Self::allowed_db_id`].
    pub scope: TokenScope,
    /// Optional KB pin carried from authorization into the issued token.
    pub allowed_db_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    /// Set by the consumption that returned this record.
    pub consumed_at: Option<DateTime<Utc>>,
    pub token_id: Option<String>,
}

/// Raw `oauth_codes` row; `scope` parses into [`TokenScope`] in [`TryFrom`].
#[derive(sqlx::FromRow)]
struct OAuthCodeRow {
    code_hash: String,
    account_id: String,
    client_id: String,
    code_challenge: String,
    code_challenge_method: String,
    redirect_uri: String,
    scope: String,
    allowed_db_id: Option<String>,
    created_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
    consumed_at: Option<DateTime<Utc>>,
    token_id: Option<String>,
}

impl TryFrom<OAuthCodeRow> for OAuthCode {
    type Error = CloudError;

    fn try_from(row: OAuthCodeRow) -> Result<Self, Self::Error> {
        Ok(OAuthCode {
            scope: row.scope.parse()?,
            code_hash: row.code_hash,
            account_id: row.account_id,
            client_id: row.client_id,
            code_challenge: row.code_challenge,
            code_challenge_method: row.code_challenge_method,
            redirect_uri: row.redirect_uri,
            allowed_db_id: row.allowed_db_id,
            created_at: row.created_at,
            expires_at: row.expires_at,
            consumed_at: row.consumed_at,
            token_id: row.token_id,
        })
    }
}

/// Fields needed to mint an authorization code (everything the authorize
/// endpoint has gathered by approval time). Grouped into a struct so
/// [`insert_oauth_code`] keeps a readable signature rather than a wall of
/// positional arguments.
#[derive(Debug, Clone)]
pub struct NewOAuthCode<'a> {
    pub account_id: &'a str,
    pub client_id: &'a str,
    pub code_challenge: &'a str,
    pub code_challenge_method: &'a str,
    pub redirect_uri: &'a str,
    pub scope: TokenScope,
    pub allowed_db_id: Option<&'a str>,
}

/// Register a new OAuth client for `account_id` (Dynamic Client Registration).
/// Returns `(client_id, client_secret_plaintext)` — the plaintext secret is
/// the only time it exists outside the caller's hands; only its hash is
/// stored.
///
/// `client_id` is an opaque public identifier ([`OAUTH_CLIENT_PREFIX`] + OS
/// entropy); `redirect_uris` is persisted as a JSON array and validated at the
/// authorize/token endpoints.
pub async fn create_oauth_client(
    control: &ControlPlane,
    account_id: &str,
    client_name: &str,
    redirect_uris: &[String],
) -> Result<(String, String), CloudError> {
    // The client_id is public, so we only need the random *plaintext*; the
    // secret follows the issue-once/store-hash discipline.
    let (client_id, _client_id_hash) = generate_secret(OAUTH_CLIENT_PREFIX);
    let (secret_plaintext, secret_hash) = generate_secret("");
    let redirect_uris_json = serde_json::to_value(redirect_uris).map_err(|e| {
        CloudError::Invariant(format!("serializing oauth client redirect_uris: {e}"))
    })?;

    sqlx::query(
        "INSERT INTO oauth_clients \
         (client_id, account_id, client_secret_hash, client_name, redirect_uris) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(&client_id)
    .bind(account_id)
    .bind(&secret_hash)
    .bind(client_name)
    .bind(redirect_uris_json)
    .execute(control.pool())
    .await
    .map_err(CloudError::db("inserting oauth client"))?;

    Ok((client_id, secret_plaintext))
}

/// Look up an OAuth client by `client_id`, **scoped to `account_id`**. Returns
/// `None` when no such client exists for this account — the cross-tenant
/// chokepoint: a `client_id` registered under another account resolves to
/// `None` here, so it can neither authorize nor exchange a token on this
/// subdomain.
pub async fn get_oauth_client(
    control: &ControlPlane,
    account_id: &str,
    client_id: &str,
) -> Result<Option<OAuthClient>, CloudError> {
    let row: Option<OAuthClientRow> = sqlx::query_as(
        "SELECT client_id, account_id, client_secret_hash, client_name, \
                redirect_uris, created_at \
         FROM oauth_clients \
         WHERE account_id = $1 AND client_id = $2",
    )
    .bind(account_id)
    .bind(client_id)
    .fetch_optional(control.pool())
    .await
    .map_err(CloudError::db("looking up oauth client"))?;
    row.map(OAuthClient::try_from).transpose()
}

/// Mint an authorization code for `code` and return its plaintext — the value
/// the redirect back to the client carries, never persisted (only its hash
/// is). Expires after `ttl` (every production caller passes [`OAUTH_CODE_TTL`]).
///
/// The PKCE `code_challenge` is stored as-is; it is already a hash of the
/// client's verifier, and the token endpoint re-derives and compares.
pub async fn insert_oauth_code(
    control: &ControlPlane,
    code: NewOAuthCode<'_>,
    ttl: Duration,
) -> Result<String, CloudError> {
    let ttl = chrono::Duration::from_std(ttl)
        .map_err(|_| CloudError::Invariant("oauth code ttl out of range".to_string()))?;
    let expires_at = Utc::now() + ttl;
    let (plaintext, hash) = generate_secret(OAUTH_CODE_PREFIX);

    sqlx::query(
        "INSERT INTO oauth_codes \
         (code_hash, account_id, client_id, code_challenge, code_challenge_method, \
          redirect_uri, scope, allowed_db_id, expires_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(&hash)
    .bind(code.account_id)
    .bind(code.client_id)
    .bind(code.code_challenge)
    .bind(code.code_challenge_method)
    .bind(code.redirect_uri)
    .bind(code.scope.as_str())
    .bind(code.allowed_db_id)
    .bind(expires_at)
    .execute(control.pool())
    .await
    .map_err(CloudError::db("inserting oauth code"))?;

    Ok(plaintext)
}

/// Atomically consume an authorization code: mark it used and return its row,
/// or `None` when `plaintext` matches nothing live for `account_id`.
///
/// Single-use and expiry are one WHERE clause —
/// `account_id = $1 AND code_hash = $2 AND consumed_at IS NULL AND
/// expires_at > NOW()` — on a single UPDATE, so two concurrent exchanges of
/// the same code can never both succeed, and an expired or already-consumed
/// code is inert. The `account_id` scope is the cross-tenant chokepoint
/// (mirroring [`crate::tokens::verify_token`]): a code minted on another
/// subdomain consumes nothing here. The caller still verifies the PKCE
/// `code_verifier`, `redirect_uri`, and `client_id` from the returned row —
/// consume only proves the code is live and burns it.
pub async fn consume_oauth_code(
    control: &ControlPlane,
    account_id: &str,
    plaintext: &str,
) -> Result<Option<OAuthCode>, CloudError> {
    let row: Option<OAuthCodeRow> = sqlx::query_as(
        "UPDATE oauth_codes SET consumed_at = NOW() \
         WHERE account_id = $1 AND code_hash = $2 \
           AND consumed_at IS NULL \
           AND expires_at > NOW() \
         RETURNING code_hash, account_id, client_id, code_challenge, \
                   code_challenge_method, redirect_uri, scope, allowed_db_id, \
                   created_at, expires_at, consumed_at, token_id",
    )
    .bind(account_id)
    .bind(sha256_hex(plaintext))
    .fetch_optional(control.pool())
    .await
    .map_err(CloudError::db("consuming oauth code"))?;
    row.map(OAuthCode::try_from).transpose()
}

/// Record the `cloud_tokens` row a consumed code minted, as a forensic link
/// from the spent code to the token it produced. Best-effort bookkeeping run
/// after a successful exchange; scoped by `account_id` for the same
/// chokepoint reason. Returns how many rows were stamped (0 or 1).
pub async fn record_oauth_code_token(
    control: &ControlPlane,
    account_id: &str,
    code_hash: &str,
    token_id: &str,
) -> Result<u64, CloudError> {
    let result = sqlx::query(
        "UPDATE oauth_codes SET token_id = $3 \
         WHERE account_id = $1 AND code_hash = $2",
    )
    .bind(account_id)
    .bind(code_hash)
    .bind(token_id)
    .execute(control.pool())
    .await
    .map_err(CloudError::db("recording oauth code token id"))?;
    Ok(result.rows_affected())
}

/// Delete authorization codes that expired before `now`. Codes are tiny and
/// short-lived ([`OAUTH_CODE_TTL`]), but spent/expired rows accumulate; this
/// is the purge the reaper can call on its periodic pass (it is not wired into
/// any loop yet — left as a pointer for the lifecycle slice). Returns how many
/// rows were removed. Not scoped by account: it is a global housekeeping
/// sweep, and an expired code is inert regardless of tenant.
pub async fn purge_expired_oauth_codes(control: &ControlPlane) -> Result<u64, CloudError> {
    let result = sqlx::query("DELETE FROM oauth_codes WHERE expires_at <= NOW()")
        .execute(control.pool())
        .await
        .map_err(CloudError::db("purging expired oauth codes"))?;
    Ok(result.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_id_and_code_carry_their_prefixes() {
        let (client_id, _) = generate_secret(OAUTH_CLIENT_PREFIX);
        let (code, _) = generate_secret(OAUTH_CODE_PREFIX);
        assert!(client_id.starts_with(OAUTH_CLIENT_PREFIX));
        assert!(code.starts_with(OAUTH_CODE_PREFIX));
        // 32 bytes -> 52 base32 chars beyond the 4-char prefix.
        assert_eq!(client_id.len(), OAUTH_CLIENT_PREFIX.len() + 52);
        assert_eq!(code.len(), OAUTH_CODE_PREFIX.len() + 52);
    }
}
