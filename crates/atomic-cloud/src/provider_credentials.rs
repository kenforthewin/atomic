//! Encrypted provider credentials in the control plane (plan: "Provider
//! management" → "Storage schema").
//!
//! One `provider_credentials` row per `(account, provider, origin)`:
//! `origin` separates platform-provisioned managed keys from user-provided
//! BYOK keys, so both can coexist and switching between them is a pointer
//! flip on `accounts (active_provider, active_origin)` — never a
//! re-provision. Model selection (`model_config`) lives with the key:
//! provider config is account-level in v1, not per-KB.
//!
//! # Secret discipline
//!
//! The API key is encrypted via [`KeyVault`] before it touches a query and
//! decrypted on the way out into a [`SecretKey`] (Debug/Display-redacted,
//! never `Serialize`). Plaintext keys exist only inside `SecretKey`
//! wrappers; nothing in this module logs, serializes, or errors with key
//! material. The vault binds each ciphertext to its `(account_id,
//! provider)` row, so a row copied across accounts fails authentication
//! instead of decrypting (see [`crate::keyvault`]).
//!
//! # Active-provider pointer
//!
//! `accounts.active_provider`/`active_origin` select the active row.
//! Invariants, enforced here and by the paired-NULL CHECK in migration 004:
//!
//! - Both columns are set together or NULL together.
//! - A non-NULL pointer always references an existing credentials row:
//!   [`set_active_provider`] refuses to point at a missing row, and
//!   [`delete_credentials`] clears the pointer in the same transaction when
//!   it deletes the active row.
//! - A NULL pointer means "no provider configured": callers translate that
//!   into a key-less `ProviderConfig`, and provider calls fail with a
//!   structured error (plan: "Plumbing — control plane → AtomicCore").

use std::str::FromStr;

use chrono::{DateTime, Utc};

use crate::control_plane::ControlPlane;
use crate::error::CloudError;
use crate::keyvault::{KeyVault, SecretKey};

/// Which AI provider a credential is for. Cloud supports exactly these two;
/// Ollama is local-only by definition (decisions log 2026-05-25).
/// Serialized to text in `provider_credentials.provider`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    OpenRouter,
    OpenAiCompat,
}

impl Provider {
    /// The text stored in `provider_credentials.provider` — and the string
    /// the [`KeyVault`] binds ciphertexts under.
    pub fn as_str(self) -> &'static str {
        match self {
            Provider::OpenRouter => "openrouter",
            Provider::OpenAiCompat => "openai_compat",
        }
    }
}

impl std::fmt::Display for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Provider {
    type Err = CloudError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "openrouter" => Ok(Provider::OpenRouter),
            "openai_compat" => Ok(Provider::OpenAiCompat),
            other => Err(CloudError::InvalidProvider(other.to_string())),
        }
    }
}

/// Who supplied the key (plan: "Managed key lifecycle"). Serialized to text
/// in `provider_credentials.origin`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialOrigin {
    /// Platform-provisioned via OpenRouter's provisioning API at signup;
    /// carries an `external_key_id` for PATCH/DELETE lifecycle calls.
    Managed,
    /// User-provided BYOK key entered through settings.
    User,
}

impl CredentialOrigin {
    /// The text stored in `provider_credentials.origin`.
    pub fn as_str(self) -> &'static str {
        match self {
            CredentialOrigin::Managed => "managed",
            CredentialOrigin::User => "user",
        }
    }
}

impl std::fmt::Display for CredentialOrigin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for CredentialOrigin {
    type Err = CloudError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "managed" => Ok(CredentialOrigin::Managed),
            "user" => Ok(CredentialOrigin::User),
            other => Err(CloudError::InvalidCredentialOrigin(other.to_string())),
        }
    }
}

/// A decrypted `provider_credentials` row.
///
/// Deliberately **not** `Serialize`: the status/audit surface (plan:
/// "Audit / visibility in settings UI") builds its own response types with
/// no key field — "existing key is never displayed". `Debug` is safe; the
/// key sits in a [`SecretKey`], which redacts.
#[derive(Debug, Clone)]
pub struct ProviderCredentials {
    pub account_id: String,
    pub provider: Provider,
    pub origin: CredentialOrigin,
    /// OpenRouter provisioning-API key id (managed rows only) — an opaque
    /// lifecycle reference, not a secret.
    pub external_key_id: Option<String>,
    /// The decrypted API key.
    pub api_key: SecretKey,
    /// Account-level model selection: `{ embedding_model, llm_model, ... }`.
    pub model_config: serde_json::Value,
    pub created_at: DateTime<Utc>,
    /// When the key bytes were last replaced (NULL until first rotation).
    pub rotated_at: Option<DateTime<Utc>>,
    pub last_used_at: Option<DateTime<Utc>>,
    /// Last *successful* validation of the stored key.
    pub last_validated_at: Option<DateTime<Utc>>,
    /// Most recent validation failure; cleared on success and on rotation.
    pub last_validation_error: Option<String>,
}

/// Raw row shape; text columns parse into enums and the key decrypts in
/// [`decrypt_row`].
#[derive(sqlx::FromRow)]
struct CredentialRow {
    account_id: String,
    provider: String,
    origin: String,
    external_key_id: Option<String>,
    encrypted_key: Vec<u8>,
    nonce: Vec<u8>,
    encryption_version: i32,
    model_config: serde_json::Value,
    created_at: DateTime<Utc>,
    rotated_at: Option<DateTime<Utc>>,
    last_used_at: Option<DateTime<Utc>>,
    last_validated_at: Option<DateTime<Utc>>,
    last_validation_error: Option<String>,
}

/// Every column of `provider_credentials`, in [`CredentialRow`] order.
const CREDENTIAL_COLUMNS: [&str; 13] = [
    "account_id",
    "provider",
    "origin",
    "external_key_id",
    "encrypted_key",
    "nonce",
    "encryption_version",
    "model_config",
    "created_at",
    "rotated_at",
    "last_used_at",
    "last_validated_at",
    "last_validation_error",
];

/// SELECT list for [`CREDENTIAL_COLUMNS`], each column qualified with
/// `prefix` (e.g. `"pc."`) — joins against `accounts` share column names
/// (`account_id`, `created_at`), so unqualified lists would be ambiguous.
fn column_list(prefix: &str) -> String {
    CREDENTIAL_COLUMNS
        .iter()
        .map(|column| format!("{prefix}{column}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Parse + decrypt a fetched row. The vault binds the ciphertext to the
/// row's own `(account_id, provider)`, so a row that was tampered with —
/// or copied onto another account — fails here, typed.
fn decrypt_row(
    vault: &dyn KeyVault,
    row: CredentialRow,
) -> Result<ProviderCredentials, CloudError> {
    let provider: Provider = row.provider.parse()?;
    let plaintext = vault.decrypt(
        &row.account_id,
        provider.as_str(),
        &row.encrypted_key,
        &row.nonce,
        row.encryption_version,
    )?;
    let api_key = SecretKey::new(String::from_utf8(plaintext).map_err(|_| {
        CloudError::Invariant(format!(
            "decrypted provider key for account {} is not UTF-8",
            row.account_id
        ))
    })?);
    Ok(ProviderCredentials {
        provider,
        origin: row.origin.parse()?,
        account_id: row.account_id,
        external_key_id: row.external_key_id,
        api_key,
        model_config: row.model_config,
        created_at: row.created_at,
        rotated_at: row.rotated_at,
        last_used_at: row.last_used_at,
        last_validated_at: row.last_validated_at,
        last_validation_error: row.last_validation_error,
    })
}

/// What [`upsert_credentials`] stores. The key arrives already wrapped in
/// a [`SecretKey`] so the plaintext is redaction-protected from the moment
/// it enters cloud code (BYOK request body, provisioning-API response).
pub struct NewCredentials {
    pub provider: Provider,
    pub origin: CredentialOrigin,
    pub api_key: SecretKey,
    /// Required for managed rows (lifecycle PATCH/DELETE); `None` for BYOK.
    pub external_key_id: Option<String>,
    pub model_config: serde_json::Value,
}

/// Insert or replace the `(account, provider, origin)` credentials row.
///
/// The key is encrypted via `vault` before it reaches the query. Replacing
/// an existing row is a rotation: `rotated_at` is stamped and the
/// validation state (`last_validated_at`, `last_validation_error`) is
/// reset — those columns describe the *stored* key, and a fresh key hasn't
/// been validated yet (callers validate on save per the plan and record the
/// outcome via [`record_validation`]).
pub async fn upsert_credentials(
    control: &ControlPlane,
    vault: &dyn KeyVault,
    account_id: &str,
    new: NewCredentials,
) -> Result<(), CloudError> {
    let (ciphertext, nonce, version) = vault.encrypt(
        account_id,
        new.provider.as_str(),
        new.api_key.expose().as_bytes(),
    )?;
    sqlx::query(
        "INSERT INTO provider_credentials \
             (account_id, provider, origin, external_key_id, encrypted_key, \
              nonce, encryption_version, model_config) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
         ON CONFLICT (account_id, provider, origin) DO UPDATE SET \
             external_key_id = EXCLUDED.external_key_id, \
             encrypted_key = EXCLUDED.encrypted_key, \
             nonce = EXCLUDED.nonce, \
             encryption_version = EXCLUDED.encryption_version, \
             model_config = EXCLUDED.model_config, \
             rotated_at = NOW(), \
             last_validated_at = NULL, \
             last_validation_error = NULL",
    )
    .bind(account_id)
    .bind(new.provider.as_str())
    .bind(new.origin.as_str())
    .bind(new.external_key_id.as_deref())
    .bind(&ciphertext)
    .bind(&nonce)
    .bind(version)
    .bind(&new.model_config)
    .execute(control.pool())
    .await
    .map_err(CloudError::db("upserting provider credentials"))?;
    Ok(())
}

/// Fetch + decrypt one `(account, provider, origin)` row, or `None`.
pub async fn get_credentials(
    control: &ControlPlane,
    vault: &dyn KeyVault,
    account_id: &str,
    provider: Provider,
    origin: CredentialOrigin,
) -> Result<Option<ProviderCredentials>, CloudError> {
    let row: Option<CredentialRow> = sqlx::query_as(&format!(
        "SELECT {} FROM provider_credentials \
         WHERE account_id = $1 AND provider = $2 AND origin = $3",
        column_list("")
    ))
    .bind(account_id)
    .bind(provider.as_str())
    .bind(origin.as_str())
    .fetch_optional(control.pool())
    .await
    .map_err(CloudError::db("fetching provider credentials"))?;
    row.map(|r| decrypt_row(vault, r)).transpose()
}

/// Fetch + decrypt the account's **active** credentials — the row selected
/// by `accounts (active_provider, active_origin)`. `None` when the pointer
/// is NULL (no provider configured) or the account doesn't exist.
pub async fn get_active_credentials(
    control: &ControlPlane,
    vault: &dyn KeyVault,
    account_id: &str,
) -> Result<Option<ProviderCredentials>, CloudError> {
    let row: Option<CredentialRow> = sqlx::query_as(&format!(
        "SELECT {} FROM accounts a \
         JOIN provider_credentials pc \
           ON pc.account_id = a.id \
          AND pc.provider = a.active_provider \
          AND pc.origin = a.active_origin \
         WHERE a.id = $1",
        column_list("pc.")
    ))
    .bind(account_id)
    .fetch_optional(control.pool())
    .await
    .map_err(CloudError::db("fetching active provider credentials"))?;
    row.map(|r| decrypt_row(vault, r)).transpose()
}

/// Point the account at a `(provider, origin)` credentials row — the
/// "column flip" that switches between managed and BYOK (plan: "Managed
/// key lifecycle") — or clear the pointer with `None`.
///
/// Flipping to a row that doesn't exist is refused with
/// [`CloudError::MissingProviderCredentials`] (a dangling pointer would
/// make [`get_active_credentials`] silently `None` while the account claims
/// a provider). Clearing for a nonexistent account is an
/// [`CloudError::Invariant`] — a typo'd id should fail loudly, not no-op.
pub async fn set_active_provider(
    control: &ControlPlane,
    account_id: &str,
    active: Option<(Provider, CredentialOrigin)>,
) -> Result<(), CloudError> {
    match active {
        Some((provider, origin)) => {
            let result = sqlx::query(
                "UPDATE accounts SET active_provider = $2, active_origin = $3 \
                 WHERE id = $1 AND EXISTS ( \
                     SELECT 1 FROM provider_credentials \
                     WHERE account_id = $1 AND provider = $2 AND origin = $3)",
            )
            .bind(account_id)
            .bind(provider.as_str())
            .bind(origin.as_str())
            .execute(control.pool())
            .await
            .map_err(CloudError::db("setting active provider"))?;
            // Zero rows means either no such credentials row or no such
            // account; both refuse the flip with the same typed error.
            if result.rows_affected() == 0 {
                return Err(CloudError::MissingProviderCredentials {
                    account_id: account_id.to_string(),
                    provider,
                    origin,
                });
            }
        }
        None => {
            let result = sqlx::query(
                "UPDATE accounts SET active_provider = NULL, active_origin = NULL WHERE id = $1",
            )
            .bind(account_id)
            .execute(control.pool())
            .await
            .map_err(CloudError::db("clearing active provider"))?;
            if result.rows_affected() == 0 {
                return Err(CloudError::Invariant(format!(
                    "clearing active provider: account {account_id} not found"
                )));
            }
        }
    }
    Ok(())
}

/// Delete one `(account, provider, origin)` row; returns whether a row
/// existed. When the deleted row was the account's active one, the active
/// pointer is cleared in the same transaction — the pointer must never
/// dangle (callers deleting a *managed* row also delete the external
/// OpenRouter key via the provisioning API; that lifecycle lives with the
/// caller, not here).
pub async fn delete_credentials(
    control: &ControlPlane,
    account_id: &str,
    provider: Provider,
    origin: CredentialOrigin,
) -> Result<bool, CloudError> {
    let mut tx = control
        .pool()
        .begin()
        .await
        .map_err(CloudError::db("starting credential-delete transaction"))?;

    let deleted = sqlx::query(
        "DELETE FROM provider_credentials \
         WHERE account_id = $1 AND provider = $2 AND origin = $3",
    )
    .bind(account_id)
    .bind(provider.as_str())
    .bind(origin.as_str())
    .execute(&mut *tx)
    .await
    .map_err(CloudError::db("deleting provider credentials"))?
    .rows_affected()
        > 0;

    if deleted {
        sqlx::query(
            "UPDATE accounts SET active_provider = NULL, active_origin = NULL \
             WHERE id = $1 AND active_provider = $2 AND active_origin = $3",
        )
        .bind(account_id)
        .bind(provider.as_str())
        .bind(origin.as_str())
        .execute(&mut *tx)
        .await
        .map_err(CloudError::db("clearing active provider after delete"))?;
    }

    tx.commit()
        .await
        .map_err(CloudError::db("committing credential delete"))?;
    Ok(deleted)
}

/// Stamp `last_used_at` on a credentials row. Best-effort by design: a row
/// deleted by a concurrent rotation/delete makes this a no-op, which is
/// correct — there is nothing left to stamp.
pub async fn touch_last_used(
    control: &ControlPlane,
    account_id: &str,
    provider: Provider,
    origin: CredentialOrigin,
) -> Result<(), CloudError> {
    sqlx::query(
        "UPDATE provider_credentials SET last_used_at = NOW() \
         WHERE account_id = $1 AND provider = $2 AND origin = $3",
    )
    .bind(account_id)
    .bind(provider.as_str())
    .bind(origin.as_str())
    .execute(control.pool())
    .await
    .map_err(CloudError::db("stamping credential last_used_at"))?;
    Ok(())
}

/// Record a validation outcome. Success (`error = None`) stamps
/// `last_validated_at` and clears any prior error; failure stores the
/// message without touching `last_validated_at`, so the timestamp always
/// means "last *successful* validation" and the error always means "most
/// recent failure". Callers must pass provider error *messages*, never
/// echoes of the key being validated.
pub async fn record_validation(
    control: &ControlPlane,
    account_id: &str,
    provider: Provider,
    origin: CredentialOrigin,
    error: Option<&str>,
) -> Result<(), CloudError> {
    let sql = match error {
        None => {
            "UPDATE provider_credentials \
             SET last_validated_at = NOW(), last_validation_error = NULL \
             WHERE account_id = $1 AND provider = $2 AND origin = $3"
        }
        Some(_) => {
            "UPDATE provider_credentials SET last_validation_error = $4 \
             WHERE account_id = $1 AND provider = $2 AND origin = $3"
        }
    };
    let mut query = sqlx::query(sql)
        .bind(account_id)
        .bind(provider.as_str())
        .bind(origin.as_str());
    if let Some(message) = error {
        query = query.bind(message);
    }
    query
        .execute(control.pool())
        .await
        .map_err(CloudError::db("recording credential validation outcome"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_and_origin_roundtrip_through_text() {
        for provider in [Provider::OpenRouter, Provider::OpenAiCompat] {
            assert_eq!(provider.as_str().parse::<Provider>().unwrap(), provider);
        }
        assert!(matches!(
            "ollama".parse::<Provider>(),
            Err(CloudError::InvalidProvider(_))
        ));

        for origin in [CredentialOrigin::Managed, CredentialOrigin::User] {
            assert_eq!(origin.as_str().parse::<CredentialOrigin>().unwrap(), origin);
        }
        assert!(matches!(
            "platform".parse::<CredentialOrigin>(),
            Err(CloudError::InvalidCredentialOrigin(_))
        ));
    }
}
