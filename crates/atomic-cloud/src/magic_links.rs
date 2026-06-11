//! Magic links — the only authentication entry point for cloud accounts
//! (plan: "Provisioning lifecycle" → "Signup" step 2; decisions log
//! 2026-05-25: magic-link only, no password infrastructure).
//!
//! A magic link is a one-time secret emailed to the user. It follows the
//! exact credential discipline of [`crate::tokens`]: an opaque random
//! plaintext (`aml_` + 32 bytes of OS entropy) is returned to the caller
//! exactly once — it exists only inside the emailed URL — and only its
//! SHA-256 hex is persisted. The pre-rewrite prototype stored raw link
//! tokens; this module exists so that never happens again.
//!
//! Two purposes share the table:
//!
//! - [`MagicLinkPurpose::Signup`] — carries the requested subdomain. The
//!   subdomain is *not* reserved by the link; the authoritative claim is the
//!   `accounts.subdomain` UNIQUE constraint at consume time
//!   ([`crate::provision::provision_account`]), so two pending signup links
//!   for the same slug are fine — first consumer wins.
//! - [`MagicLinkPurpose::Login`] — no subdomain; the account is found by
//!   email when the link is consumed.
//!
//! Links are short-lived ([`MAGIC_LINK_TTL`], 15 minutes) and single-use:
//! [`consume_magic_link`] is one atomic UPDATE whose WHERE clause
//! (`consumed_at IS NULL AND expires_at > NOW()`) makes expired and
//! already-consumed rows inert. This slice covers issuance and consumption
//! as library functions; the request-link HTTP routes live in
//! [`crate::account_plane`], and the consume routes (signup completion →
//! provisioning, login completion → session) arrive with the rest of the
//! signup slice.

use std::str::FromStr;
use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::control_plane::ControlPlane;
use crate::error::CloudError;
use crate::tokens::{generate_secret, sha256_hex};

/// Prefix on magic-link plaintexts. Distinct from the `atm_`/`ats_`
/// prefixes so a leaked value is immediately classifiable.
pub const MAGIC_LINK_PREFIX: &str = "aml_";

/// Default link lifetime (plan: the emailed copy promises "expires in 15
/// minutes"). Issuance takes an explicit TTL so tests can shrink it, but
/// every production caller passes this.
pub const MAGIC_LINK_TTL: Duration = Duration::from_secs(15 * 60);

/// What consuming a link does. Serialized to text in `magic_links.purpose`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MagicLinkPurpose {
    /// Completes account creation: claims the requested subdomain and
    /// provisions the tenant.
    Signup,
    /// Signs an existing account in: creates a session.
    Login,
}

impl MagicLinkPurpose {
    /// The text stored in `magic_links.purpose`.
    pub fn as_str(self) -> &'static str {
        match self {
            MagicLinkPurpose::Signup => "signup",
            MagicLinkPurpose::Login => "login",
        }
    }
}

impl std::fmt::Display for MagicLinkPurpose {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for MagicLinkPurpose {
    type Err = CloudError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "signup" => Ok(MagicLinkPurpose::Signup),
            "login" => Ok(MagicLinkPurpose::Login),
            other => Err(CloudError::InvalidMagicLinkPurpose(other.to_string())),
        }
    }
}

/// A consumed `magic_links` row, as returned by [`consume_magic_link`].
#[derive(Debug, Clone)]
pub struct MagicLinkRecord {
    /// SHA-256 hex of the plaintext — the row's primary key.
    pub token_hash: String,
    pub email: String,
    pub purpose: MagicLinkPurpose,
    /// For signup links: the subdomain the user asked for. A request, not a
    /// reservation — the consume-time claim is authoritative.
    pub requested_subdomain: Option<String>,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    /// Set by the consumption that returned this record.
    pub consumed_at: Option<DateTime<Utc>>,
    /// Forensic breadcrumb: the client IP that requested the link, when
    /// derivable.
    pub request_ip: Option<String>,
}

/// Raw row shape; `purpose` parses into [`MagicLinkPurpose`] in `TryFrom`.
#[derive(sqlx::FromRow)]
struct MagicLinkRow {
    token_hash: String,
    email: String,
    purpose: String,
    requested_subdomain: Option<String>,
    created_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
    consumed_at: Option<DateTime<Utc>>,
    request_ip: Option<String>,
}

impl TryFrom<MagicLinkRow> for MagicLinkRecord {
    type Error = CloudError;

    fn try_from(row: MagicLinkRow) -> Result<Self, Self::Error> {
        Ok(MagicLinkRecord {
            purpose: row.purpose.parse()?,
            token_hash: row.token_hash,
            email: row.email,
            requested_subdomain: row.requested_subdomain,
            created_at: row.created_at,
            expires_at: row.expires_at,
            consumed_at: row.consumed_at,
            request_ip: row.request_ip,
        })
    }
}

/// Issue a magic link for `email` and return its plaintext — the value the
/// emailed URL carries, never persisted anywhere.
///
/// `requested_subdomain` must be `Some` for signup links and `None` for
/// login links; the caller validates it *before* issuance (the account
/// plane's request-link route returns honest 400s for bad slugs).
pub async fn issue_magic_link(
    control: &ControlPlane,
    email: &str,
    purpose: MagicLinkPurpose,
    requested_subdomain: Option<&str>,
    request_ip: Option<&str>,
    ttl: Duration,
) -> Result<String, CloudError> {
    let ttl = chrono::Duration::from_std(ttl)
        .map_err(|_| CloudError::Invariant("magic link ttl out of range".to_string()))?;
    let expires_at = Utc::now() + ttl;
    let (plaintext, hash) = generate_secret(MAGIC_LINK_PREFIX);
    sqlx::query(
        "INSERT INTO magic_links \
         (token_hash, email, purpose, requested_subdomain, expires_at, request_ip) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(&hash)
    .bind(email)
    .bind(purpose.as_str())
    .bind(requested_subdomain)
    .bind(expires_at)
    .bind(request_ip)
    .execute(control.pool())
    .await
    .map_err(CloudError::db("inserting magic link"))?;
    Ok(plaintext)
}

/// Atomically consume a magic link: mark it used and return its row, or
/// `None` when the plaintext matches nothing live.
///
/// Single-use and expiry are one WHERE clause — `consumed_at IS NULL AND
/// expires_at > NOW()` — on a single UPDATE, so two concurrent clicks of
/// the same link can never both succeed, and expired or already-consumed
/// rows are inert. Unlike token/session verification there is no
/// `account_id` scoping: at signup time no account exists yet, so the link
/// itself is the whole credential — which is exactly why it is short-lived,
/// single-use, and stored hash-only.
pub async fn consume_magic_link(
    control: &ControlPlane,
    plaintext: &str,
) -> Result<Option<MagicLinkRecord>, CloudError> {
    let row: Option<MagicLinkRow> = sqlx::query_as(
        "UPDATE magic_links SET consumed_at = NOW() \
         WHERE token_hash = $1 \
           AND consumed_at IS NULL \
           AND expires_at > NOW() \
         RETURNING token_hash, email, purpose, requested_subdomain, \
                   created_at, expires_at, consumed_at, request_ip",
    )
    .bind(sha256_hex(plaintext))
    .fetch_optional(control.pool())
    .await
    .map_err(CloudError::db("consuming magic link"))?;
    row.map(MagicLinkRecord::try_from).transpose()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn purpose_roundtrips_through_text() {
        for purpose in [MagicLinkPurpose::Signup, MagicLinkPurpose::Login] {
            assert_eq!(
                purpose.as_str().parse::<MagicLinkPurpose>().unwrap(),
                purpose
            );
        }
        assert!(matches!(
            "reset".parse::<MagicLinkPurpose>(),
            Err(CloudError::InvalidMagicLinkPurpose(_))
        ));
    }
}
