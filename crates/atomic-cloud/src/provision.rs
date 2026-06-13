//! Account provisioning and deletion (plan: "Provisioning lifecycle").
//!
//! These are pure library functions — no HTTP, no email. The signup HTTP
//! flow and magic links live in [`crate::account_plane`]; managed provider
//! keys are threaded in via [`ManagedKeys`] (plan: "Provider management" →
//! "Managed key lifecycle"). Callers compose them under whatever transport
//! they like.
//!
//! # Idempotency
//!
//! Every provisioning step is independently idempotent so a crashed signup
//! can be re-run to completion (plan: "Signup" → "Idempotency"):
//!
//! - The subdomain claim resumes an account stuck in `status='provisioning'`
//!   when the email matches; any other conflict is [`CloudError::SubdomainTaken`].
//! - `CREATE DATABASE` checks `pg_database` first and treats a concurrent
//!   creator's `duplicate_database` as success.
//! - Tenant migrations and seeding run through `DatabaseManager::new_postgres`
//!   — atomic-core's own advisory-locked, versioned migration runner and
//!   default-KB seeding, not a cloud reimplementation.
//! - Managed-key provisioning (step 9) checks for an existing
//!   `provider_credentials` row first — key creation itself is not
//!   idempotent (see [`crate::managed_keys`]); the failure paths delete any
//!   key created before the failure, using the locally held
//!   `external_key_id`.
//! - Control-plane inserts use `ON CONFLICT DO NOTHING`.
//!
//! Deletion is likewise idempotent under retry: every step tolerates an
//! already-dropped database and already-deleted rows, and the freed
//! subdomain is reserved *before* the accounts row is hard-deleted so a
//! crash between the two can't lose the 90-day reservation.
//!
//! # Provision/deletion races
//!
//! Provisioning and deletion can interleave on a live system; three guards
//! in [`provision_account`] keep that safe:
//!
//! - [`ensure_claim_not_reserved`] re-checks `subdomains_reserved` *after*
//!   the claim INSERT, rolling the claim back if a concurrent deletion
//!   parked the subdomain between the pre-check and the claim.
//! - Tenant-database creation re-verifies the account is still
//!   `status = 'provisioning'`, and the `account_databases` INSERT treats a
//!   foreign-key violation (the accounts row vanished mid-provision) as
//!   "deletion won": the just-created database is dropped rather than
//!   orphaned.
//! - [`activate_account`] (step 11) checks `rows_affected`: an UPDATE that
//!   matches zero rows means the accounts row vanished *after* the mapping
//!   insert, and the provision fails typed instead of reporting success for
//!   a dead account.

use std::str::FromStr;

use atomic_core::DatabaseManager;
use sqlx::postgres::PgConnectOptions;
use sqlx::{Connection, PgConnection};
use uuid::Uuid;

use crate::control_plane::ControlPlane;
use crate::error::CloudError;
use crate::managed_keys::ManagedKeys;
use crate::reserved_subdomains;

/// Where tenant databases live: the shared Postgres cluster.
///
/// `cluster_id` is recorded on every `account_databases` row from day one so
/// a future shard split is mechanical (plan: "Tenant model"). v1 runs a
/// single cluster, so there is exactly one of these per deployment.
#[derive(Debug, Clone)]
pub struct ClusterConfig {
    /// Identifier stored in `account_databases.cluster_id`.
    pub cluster_id: String,
    /// Postgres URL of the shared tenant cluster. The database path
    /// component is replaced per-tenant; the user must be able to
    /// `CREATE DATABASE`. Query parameters (e.g. `sslmode`) are preserved.
    pub cluster_url: String,
}

impl ClusterConfig {
    /// Connection URL for a tenant database on this cluster.
    pub fn tenant_db_url(&self, db_name: &str) -> Result<String, CloudError> {
        let mut url = url::Url::parse(&self.cluster_url)
            .map_err(|e| CloudError::InvalidUrl(format!("cluster URL: {e}")))?;
        url.set_path(db_name);
        Ok(url.into())
    }

    /// Short-lived connection to the cluster's `postgres` maintenance
    /// database, for `CREATE DATABASE` / `DROP DATABASE` / `pg_database`
    /// queries that can't run against the database they target. Crate-wide
    /// because the reaper's orphan scan needs the same connection.
    pub(crate) async fn connect_maintenance(&self) -> Result<PgConnection, CloudError> {
        let opts = PgConnectOptions::from_str(&self.cluster_url)
            .map_err(|e| CloudError::InvalidUrl(format!("cluster URL: {e}")))?
            .database("postgres");
        PgConnection::connect_with(&opts)
            .await
            .map_err(CloudError::db("connecting to cluster maintenance database"))
    }
}

/// Signup input for [`provision_account`].
#[derive(Debug, Clone)]
pub struct NewAccount {
    pub email: String,
    pub subdomain: String,
}

/// Result of a successful [`provision_account`] run.
#[derive(Debug, Clone)]
pub struct ProvisionedAccount {
    /// Account UUID (hyphenated lowercase), as stored in `accounts.id`.
    pub account_id: String,
    pub subdomain: String,
    /// Tenant database name on the cluster (see [`tenant_db_name`]).
    pub db_name: String,
}

/// Derive an account's tenant database name: `acct_<base32(uuid)>`.
///
/// The encoding is RFC 4648 base32 without padding, lowercased: the UUID's
/// 16 bytes become exactly 26 characters of `[a-z2-7]`, for a fixed 31-char
/// name. That makes the name a safe Postgres identifier by construction —
/// starts with a letter, lowercase-only (so quoted and unquoted forms
/// agree), well under the 63-byte identifier limit — and, being the UUID
/// rather than the subdomain, it survives future subdomain renames and
/// leaks nothing (plan: "Tenant model", decisions log 2026-05-25).
pub fn tenant_db_name(account_id: Uuid) -> String {
    let encoded = data_encoding::BASE32_NOPAD.encode(account_id.as_bytes());
    format!("acct_{}", encoded.to_ascii_lowercase())
}

/// Whether `name` has exactly the shape [`tenant_db_name`] generates.
///
/// Database names cannot be bound as statement parameters, so every
/// `CREATE DATABASE` / `DROP DATABASE` here interpolates the name into SQL.
/// This check is the guard on that interpolation: only names matching
/// `acct_` + 26 chars of `[a-z2-7]` ever reach DDL, even if a control-plane
/// row is corrupted.
pub fn is_tenant_db_name(name: &str) -> bool {
    name.strip_prefix("acct_").is_some_and(|suffix| {
        suffix.len() == 26 && suffix.chars().all(|c| matches!(c, 'a'..='z' | '2'..='7'))
    })
}

/// Inverse of [`tenant_db_name`]: recover the account UUID a tenant
/// database name encodes. `None` for anything that isn't exactly the
/// generated shape (including non-canonical base32 with stray trailing
/// bits). The reaper uses this to key its per-account advisory lock when
/// reclaiming an orphaned database — the database name is the only place
/// the account id survives once the control-plane rows are gone.
pub fn tenant_db_account_id(name: &str) -> Option<Uuid> {
    if !is_tenant_db_name(name) {
        return None;
    }
    let suffix = name.strip_prefix("acct_")?;
    let bytes = data_encoding::BASE32_NOPAD
        .decode(suffix.to_ascii_uppercase().as_bytes())
        .ok()?;
    Uuid::from_slice(&bytes).ok()
}

/// Validate and assert a tenant database name before SQL interpolation.
fn checked_tenant_db_name(name: &str) -> Result<&str, CloudError> {
    if !is_tenant_db_name(name) {
        return Err(CloudError::InvalidDatabaseName(name.to_string()));
    }
    Ok(name)
}

/// Signup slug rule from the plan: 3-32 chars of `[a-z0-9-]`. Shared with
/// the account plane's request-link validation, so the early UX check and
/// the authoritative provision-time check can never disagree.
pub(crate) fn subdomain_format_ok(subdomain: &str) -> bool {
    (3..=32).contains(&subdomain.len())
        && subdomain
            .chars()
            .all(|c| matches!(c, 'a'..='z' | '0'..='9' | '-'))
}

/// Minimal email shape check: non-empty local and domain parts, the domain
/// containing a dot, no whitespace. Real verification is the magic link —
/// clicking it proves ownership; this only rejects obvious garbage before
/// we burn an account row (or an email send) on it. Shared with the account
/// plane's request-link validation.
pub(crate) fn email_format_ok(email: &str) -> bool {
    if email.chars().any(char::is_whitespace) {
        return false;
    }
    matches!(email.split_once('@'),
        Some((local, domain)) if !local.is_empty() && domain.contains('.'))
}

/// Provision a new account end-to-end: claim the subdomain, create the
/// tenant database, run tenant migrations, seed the default knowledge base,
/// provision the managed provider key, record the tenant mapping, and
/// activate the account.
///
/// Implements signup steps 1, 3, 4, 5, 6, 9, 10 and 11 from the plan
/// ("Provisioning lifecycle" → "Signup"). Steps 2 and 12 (magic link;
/// session + redirect) live in the HTTP layer ([`crate::account_plane`]);
/// the rest in later slices: 7 (cloud-curated per-DB settings; atomic-core's
/// own defaults are seeded here as part of opening the tenant), 8 (default
/// report — reports slice). Step 9 is skipped entirely when `managed` is
/// [`ManagedKeys::Disabled`] — the account runs keyless.
///
/// Re-running for an account stuck in `status='provisioning'` (same email,
/// same subdomain) resumes and completes it without duplicating rows. The
/// synchronous-signup concurrency cap (4-8 in-flight per process) belongs
/// to the HTTP layer, not here.
pub async fn provision_account(
    control: &ControlPlane,
    cluster: &ClusterConfig,
    managed: &ManagedKeys,
    new_account: NewAccount,
) -> Result<ProvisionedAccount, CloudError> {
    let NewAccount { email, subdomain } = new_account;

    // Step 1 — validate. The static blocklist and the active holds in
    // `subdomains_reserved` are distinct mechanisms (see
    // reserved_subdomains.rs); check both. Expired holds don't count.
    if !email_format_ok(&email) {
        return Err(CloudError::InvalidEmail(email));
    }
    if !subdomain_format_ok(&subdomain) {
        return Err(CloudError::InvalidSubdomain(subdomain));
    }
    if reserved_subdomains::is_reserved(&subdomain) {
        return Err(CloudError::SubdomainReserved(subdomain));
    }
    let actively_reserved: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM subdomains_reserved \
         WHERE subdomain = $1 AND expires_at > NOW())",
    )
    .bind(&subdomain)
    .fetch_one(control.pool())
    .await
    .map_err(CloudError::db("checking subdomain reservation"))?;
    if actively_reserved {
        return Err(CloudError::SubdomainReserved(subdomain));
    }

    // Step 3 — claim the subdomain atomically. The UNIQUE constraint on
    // accounts.subdomain is what makes "taken" race-free: losers of a
    // concurrent claim see SQLSTATE 23505. A conflicting row that is still
    // 'provisioning' for the same email (compared case-insensitively, like
    // every other email comparison in the crate) is a crashed earlier
    // attempt — resume it. Anything else (an active account, or another
    // email's in-flight claim) reports taken.
    let account_id = Uuid::new_v4();
    let claim = sqlx::query(
        "INSERT INTO accounts (id, subdomain, email, status, plan) \
         VALUES ($1, $2, $3, 'provisioning', 'free')",
    )
    .bind(account_id.to_string())
    .bind(&subdomain)
    .bind(&email)
    .execute(control.pool())
    .await;

    let account_id = match claim {
        Ok(_) => {
            // Step 3½ — close the reservation TOCTOU: a `delete_account`
            // for this subdomain may have parked it between the pre-check
            // above and the INSERT. See `ensure_claim_not_reserved` for why
            // the post-claim re-check is sufficient.
            ensure_claim_not_reserved(control, account_id, &subdomain).await?;
            account_id
        }
        Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some("23505") => {
            let existing: Option<(String, String, String)> =
                sqlx::query_as("SELECT id, email, status FROM accounts WHERE subdomain = $1")
                    .bind(&subdomain)
                    .fetch_optional(control.pool())
                    .await
                    .map_err(CloudError::db("looking up conflicting subdomain claim"))?;
            match existing {
                Some((id, row_email, status))
                    if status == "provisioning"
                        && row_email.to_lowercase() == email.to_lowercase() =>
                {
                    Uuid::parse_str(&id).map_err(|_| {
                        CloudError::Invariant(format!("accounts.id {id:?} is not a UUID"))
                    })?
                }
                _ => return Err(CloudError::SubdomainTaken(subdomain)),
            }
        }
        Err(e) => return Err(CloudError::db("claiming subdomain")(e)),
    };

    // Step 4 — create the tenant database. Identifiers can't be bound as
    // parameters, hence the pg_database check, the shape assertion, and the
    // quoted interpolation.
    //
    // Guard against a concurrent `delete_account` first: once deletion has
    // dropped the tenant database and removed the accounts row, re-creating
    // the database here would orphan it — the `account_databases` INSERT
    // below would hit a foreign-key violation, and with no accounts row left
    // nothing would ever derive the database's name to drop it. Verify the
    // account is still mid-provision immediately before CREATE DATABASE
    // (the FK-violation handler below covers a deletion landing after this
    // point).
    let still_provisioning: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM accounts WHERE id = $1 AND status = 'provisioning')",
    )
    .bind(account_id.to_string())
    .fetch_one(control.pool())
    .await
    .map_err(CloudError::db(
        "re-checking account before tenant database creation",
    ))?;
    if !still_provisioning {
        return Err(CloudError::AccountNoLongerProvisioning(
            account_id.to_string(),
        ));
    }

    let db_name = tenant_db_name(account_id);
    let db_name = checked_tenant_db_name(&db_name)?.to_string();
    let mut conn = cluster.connect_maintenance().await?;
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_database WHERE datname = $1)")
            .bind(&db_name)
            .fetch_one(&mut conn)
            .await
            .map_err(CloudError::db("checking tenant database existence"))?;
    if !exists {
        match sqlx::raw_sql(&format!("CREATE DATABASE \"{db_name}\""))
            .execute(&mut conn)
            .await
        {
            Ok(_) => tracing::info!(db_name, "created tenant database"),
            // 42P04 duplicate_database: a concurrent provision won the race.
            Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some("42P04") => {}
            Err(e) => return Err(CloudError::db("creating tenant database")(e)),
        }
    }
    let _ = conn.close().await;

    // Steps 5 + 6 — run tenant migrations and seed the default knowledge
    // base, via the same production code path the self-hosted Postgres
    // server boots through. `new_postgres` connects, applies atomic-core's
    // versioned migrations under its advisory lock, and seeds the default
    // `databases` row when the table is empty — no seeding logic duplicated
    // here. The manager (and its pool) is dropped immediately; the cloud
    // request path opens tenants through the AccountCache instead.
    let tenant_url = cluster.tenant_db_url(&db_name)?;
    let manager = DatabaseManager::new_postgres(".", &tenant_url)
        .await
        .map_err(CloudError::core(
            "running tenant migrations and seeding defaults",
        ))?;
    drop(manager);

    // Step 9 — managed provider key (plan: "Provider management" →
    // "Managed key lifecycle"; skipped entirely in Disabled mode). The
    // helper is idempotent — an existing credentials row short-circuits, so
    // a resumed provision never mints a second key — and a failure here
    // propagates typed: the account stays 'provisioning' and the reaper
    // retries or rolls it back (no half state). The returned id is held
    // locally for the lost-race cleanup paths below, where the accounts-row
    // CASCADE has already swept the credentials row that referenced it.
    let managed_key_id = managed
        .ensure_managed_key(control, &account_id.to_string())
        .await?;

    // Step 10 — record the account → tenant-database mapping, stamped with
    // the schema version steps 5+6 just migrated the tenant to (the compiled
    // target) so a fresh tenant is never a straggler to CloudAuth's gate
    // (see crate::fleet_migration). ON CONFLICT keeps a resumed provision
    // from duplicating the row; the DO UPDATE arm re-records the success the
    // resume's own migration run just earned — GREATEST so an old binary
    // resuming a row a newer binary already stamped can't regress it, and
    // the failure/backoff columns reset because a full `initialize()`
    // verifiably succeeded moments ago.
    let recorded = sqlx::query(
        "INSERT INTO account_databases \
             (account_id, cluster_id, db_name, status, \
              last_migrated_version, last_migrated_at) \
         VALUES ($1, $2, $3, 'active', $4, NOW()) \
         ON CONFLICT (account_id, db_name) DO UPDATE \
         SET last_migrated_version = \
                 GREATEST(account_databases.last_migrated_version, EXCLUDED.last_migrated_version), \
             last_migrated_at = NOW(), \
             migration_failed_at = NULL, \
             last_migration_error = NULL, \
             migration_retry_after = NULL, \
             migration_retry_count = 0",
    )
    .bind(account_id.to_string())
    .bind(&cluster.cluster_id)
    .bind(&db_name)
    .bind(crate::fleet_migration::tenant_schema_target())
    .execute(control.pool())
    .await;
    if let Err(e) = recorded {
        // SQLSTATE 23503 foreign_key_violation: the accounts row vanished
        // while migrations ran — a concurrent `delete_account` won despite
        // the pre-CREATE guard above. Its DROP pass ran before our CREATE
        // DATABASE (or never saw it), so the database we just created would
        // be orphaned forever: no accounts row means nothing can derive its
        // name again. Drop it now (best-effort, logged) before surfacing
        // the typed error. The same applies to the managed key step 9 just
        // handled: the CASCADE swept its credentials row, so the locally
        // held id is the only remaining reference — delete the key too
        // (best-effort; the winning deletion may have already deleted it
        // via its own step 3, which the 404-tolerant delete absorbs).
        if matches!(&e, sqlx::Error::Database(d) if d.code().as_deref() == Some("23503")) {
            drop_tenant_database_best_effort(cluster, &db_name).await;
            if let Some(external_key_id) = &managed_key_id {
                managed
                    .delete_external_key_best_effort(&account_id.to_string(), external_key_id)
                    .await;
            }
            return Err(CloudError::AccountNoLongerProvisioning(
                account_id.to_string(),
            ));
        }
        return Err(CloudError::db("recording account database")(e));
    }

    // Step 11 — activate. The zero-row case is the same lost race as the
    // 23503 above, one step later: the accounts row (and, via CASCADE, the
    // step-9 credentials row) is gone, so the locally held key id gets the
    // same best-effort cleanup. The tenant database itself is the winning
    // deletion's to drop — it read the mapping row this provision just
    // wrote (or derives the name from the account UUID).
    if let Err(e) = activate_account(control, account_id).await {
        if matches!(e, CloudError::AccountNoLongerProvisioning(_)) {
            if let Some(external_key_id) = &managed_key_id {
                managed
                    .delete_external_key_best_effort(&account_id.to_string(), external_key_id)
                    .await;
            }
        }
        return Err(e);
    }

    tracing::info!(
        account_id = %account_id,
        subdomain,
        db_name,
        "provisioned account"
    );

    Ok(ProvisionedAccount {
        account_id: account_id.to_string(),
        subdomain,
        db_name,
    })
}

/// Step 11 — flip a claimed account from `'provisioning'` to `'active'`,
/// **verifying the row still exists**.
///
/// `rows_affected == 0` means the accounts row vanished between the
/// `account_databases` insert and here: a concurrent [`delete_account`] (or
/// a reaper rollback) won the race and the CASCADE already swept the mapping
/// row this provision just wrote. Returning `Ok` would report success for a
/// dead account — the caller would mint a session against a missing FK, the
/// reaper would log a resume that didn't happen, the CLI would print
/// success — so the zero-row case is the same typed
/// [`CloudError::AccountNoLongerProvisioning`] the earlier guards use.
/// Re-activating an already-`'active'` row matches one row and stays `Ok`,
/// keeping resumed provisions idempotent.
///
/// Public as a test seam (like [`ensure_claim_not_reserved`]): the window
/// between steps 10 and 11 is too narrow to drive end-to-end, so the
/// regression test exercises this function against a deleted row directly;
/// production code reaches it only through [`provision_account`].
pub async fn activate_account(control: &ControlPlane, account_id: Uuid) -> Result<(), CloudError> {
    let updated = sqlx::query("UPDATE accounts SET status = 'active' WHERE id = $1")
        .bind(account_id.to_string())
        .execute(control.pool())
        .await
        .map_err(CloudError::db("activating account"))?
        .rows_affected();
    if updated == 0 {
        return Err(CloudError::AccountNoLongerProvisioning(
            account_id.to_string(),
        ));
    }
    Ok(())
}

/// Post-claim reservation guard, closing the TOCTOU between
/// [`provision_account`]'s `subdomains_reserved` pre-check and its claim
/// INSERT.
///
/// A concurrent [`delete_account`] for the same subdomain can interleave:
/// the pre-check sees no hold, deletion then parks the subdomain and
/// hard-deletes its accounts row, and the claim INSERT lands on the freshly
/// parked name. Re-checking *after* the claim is sufficient because deletion
/// writes the reservation **before** deleting the accounts row (the ordering
/// invariant documented on [`delete_account`]): any deletion that freed the
/// subdomain for our INSERT necessarily made its hold visible first. On a
/// live hold, the just-inserted claim is rolled back and the typed
/// reservation error returned.
///
/// Public so the interleaving regression test (`tests/provisioning.rs`) can
/// drive the claim and the hold by direct SQL; production code reaches it
/// only through [`provision_account`].
pub async fn ensure_claim_not_reserved(
    control: &ControlPlane,
    account_id: Uuid,
    subdomain: &str,
) -> Result<(), CloudError> {
    let reserved: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM subdomains_reserved \
         WHERE subdomain = $1 AND expires_at > NOW())",
    )
    .bind(subdomain)
    .fetch_one(control.pool())
    .await
    .map_err(CloudError::db("re-checking subdomain reservation"))?;
    if !reserved {
        return Ok(());
    }

    // Roll back only our own fresh claim — the status filter keeps this
    // from ever deleting an activated account.
    sqlx::query("DELETE FROM accounts WHERE id = $1 AND status = 'provisioning'")
        .bind(account_id.to_string())
        .execute(control.pool())
        .await
        .map_err(CloudError::db("rolling back reserved subdomain claim"))?;
    tracing::info!(
        account_id = %account_id,
        subdomain,
        "rolled back subdomain claim that raced a deletion's reservation"
    );
    Err(CloudError::SubdomainReserved(subdomain.to_string()))
}

/// Terminate any backends connected to `db_name` and drop it. The shared
/// drop primitive for [`delete_account`], the provisioning FK-violation
/// cleanup, and the reaper's rollback/orphan-reclaim arms: explicit
/// termination keeps the drop from racing a reconnecting pool, and is
/// harmless when no backends exist.
pub(crate) async fn terminate_and_drop_database(
    conn: &mut PgConnection,
    db_name: &str,
) -> Result<(), CloudError> {
    let db_name = checked_tenant_db_name(db_name)?;
    sqlx::query(
        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
         WHERE datname = $1 AND pid <> pg_backend_pid()",
    )
    .bind(db_name)
    .execute(&mut *conn)
    .await
    .map_err(CloudError::db("terminating tenant database backends"))?;
    sqlx::raw_sql(&format!(
        "DROP DATABASE IF EXISTS \"{db_name}\" WITH (FORCE)"
    ))
    .execute(&mut *conn)
    .await
    .map_err(CloudError::db("dropping tenant database"))?;
    Ok(())
}

/// Best-effort [`terminate_and_drop_database`], for the provisioning path
/// that lost a race with [`delete_account`] (the SQLSTATE-23503 handler in
/// [`provision_account`]). The caller is already returning an error;
/// a failed drop here is logged loudly — it leaves an orphaned database
/// that only an operator (or a future reaper pass) can reclaim.
async fn drop_tenant_database_best_effort(cluster: &ClusterConfig, db_name: &str) {
    let result = async {
        let mut conn = cluster.connect_maintenance().await?;
        let dropped = terminate_and_drop_database(&mut conn, db_name).await;
        let _ = conn.close().await;
        dropped
    }
    .await;
    match result {
        Ok(()) => tracing::info!(
            db_name,
            "dropped tenant database created by a provision that raced a deletion"
        ),
        Err(e) => tracing::error!(
            db_name,
            error = %e,
            "failed to drop orphaned tenant database after losing a \
             provision/deletion race; manual cleanup required"
        ),
    }
}

/// Hard-delete an account: revoke its tokens, delete its sessions, drop its
/// tenant database, remove its control-plane rows, and park the freed
/// subdomain in `subdomains_reserved` for 90 days.
///
/// Implements the plan's deletion sequence ("Provisioning lifecycle" →
/// "Account deletion") minus the steps owned by later slices:
///
/// - The final logical dump to `backups/final/` before the drop (step 4) —
///   backups slice (plan: "Backups & disaster recovery"); until it lands,
///   deletion is genuinely unrecoverable.
/// - AccountCache eviction (step 5) — the serving composition layer owns
///   the cache. Its HTTP deletion route ([`crate::tenant_plane`]) evicts
///   *after* this returns: once the account rows are gone nothing can
///   rebuild the entry behind the eviction, and the pooled connections an
///   un-evicted entry holds during the drop are handled below
///   (`pg_terminate_backend` + `WITH (FORCE)`). A process-separate caller
///   (the CLI) has no cache to evict; a serve process's stale entry
///   self-heals — CloudAuth 404s the deleted account, and the idle TTL
///   reclaims the entry.
///
/// Idempotent under retry: an already-dropped database, already-deleted
/// rows, and an already-reserved subdomain are all no-ops, so calling this
/// for an unknown `account_id` succeeds quietly. One deliberate ordering
/// deviation from the plan: the subdomain reservation is written *before*
/// the accounts row is hard-deleted — the reverse order could crash between
/// the two and lose the reservation, since the subdomain is only knowable
/// from the row being deleted. **This ordering is also a correctness
/// invariant for provisioning**: [`ensure_claim_not_reserved`]'s post-claim
/// re-check is only sufficient because a deletion that frees a subdomain
/// makes its hold visible before the freed name can be re-claimed. Don't
/// reorder steps 7a/7b without revisiting that guard.
pub async fn delete_account(
    control: &ControlPlane,
    cluster: &ClusterConfig,
    managed: &ManagedKeys,
    account_id: &str,
) -> Result<(), CloudError> {
    // 1 — revoke all tokens. The accounts-row delete below cascades these
    // away entirely; revoking first closes the crash window where tokens
    // would still verify while the tenant database is being dropped.
    sqlx::query(
        "UPDATE cloud_tokens SET revoked_at = NOW() \
         WHERE account_id = $1 AND revoked_at IS NULL",
    )
    .bind(account_id)
    .execute(control.pool())
    .await
    .map_err(CloudError::db("revoking account tokens"))?;

    // 2 — invalidate all sessions.
    sqlx::query("DELETE FROM sessions WHERE account_id = $1")
        .bind(account_id)
        .execute(control.pool())
        .await
        .map_err(CloudError::db("deleting account sessions"))?;

    // 3 — delete the managed runtime key(s) via the provisioning API,
    // strictly BEFORE anything destroys the rows holding `external_key_id`
    // (the accounts-row CASCADE in step 6 sweeps `provider_credentials`).
    // Best-effort with loud logging: deletion must not wedge on a provider
    // outage, and once the rows are gone a failed delete cannot be retried
    // by the reaper — accepted residue, recovered via the master OpenRouter
    // account's key listing (see crate::managed_keys).
    managed
        .delete_managed_keys_for_account(control, account_id)
        .await?;

    // 4 — find the tenant database(s). `account_databases` is the source of
    // truth; the name derived from the account UUID is unioned in so a
    // provision that crashed before step 10 (database created, mapping row
    // not yet written) still gets cleaned up.
    let mut db_names: Vec<String> =
        sqlx::query_scalar("SELECT db_name FROM account_databases WHERE account_id = $1")
            .bind(account_id)
            .fetch_all(control.pool())
            .await
            .map_err(CloudError::db("listing account databases"))?;
    if let Ok(uuid) = Uuid::parse_str(account_id) {
        let derived = tenant_db_name(uuid);
        if !db_names.contains(&derived) {
            db_names.push(derived);
        }
    }

    // 5 — terminate stragglers and drop. WITH (FORCE) alone would do it,
    // but explicit termination keeps the drop from racing a reconnecting
    // pool, and is harmless when no backends exist.
    if !db_names.is_empty() {
        let mut conn = cluster.connect_maintenance().await?;
        for db_name in &db_names {
            terminate_and_drop_database(&mut conn, db_name).await?;
        }
        let _ = conn.close().await;
    }

    // 6 — remove the mapping rows.
    sqlx::query("DELETE FROM account_databases WHERE account_id = $1")
        .bind(account_id)
        .execute(control.pool())
        .await
        .map_err(CloudError::db("deleting account database rows"))?;

    // 7 — reserve the freed subdomain for 90 days (stale RSS readers and
    // MCP configs pointing at the old name must not silently reach a
    // stranger), then hard-delete the account row, cascading any remaining
    // token rows away.
    let subdomain: Option<String> =
        sqlx::query_scalar("SELECT subdomain FROM accounts WHERE id = $1")
            .bind(account_id)
            .fetch_optional(control.pool())
            .await
            .map_err(CloudError::db("reading subdomain for reservation"))?;
    if let Some(subdomain) = subdomain {
        // Re-upping `created_at` on conflict marks "a deletion is touching
        // this subdomain right now" — the reaper's self-reservation arm
        // only clears reservations older than its settle grace, so a
        // retried deletion is shielded from it again (see crate::reaper).
        sqlx::query(
            "INSERT INTO subdomains_reserved (subdomain, expires_at) \
             VALUES ($1, NOW() + INTERVAL '90 days') \
             ON CONFLICT (subdomain) DO UPDATE \
             SET expires_at = EXCLUDED.expires_at, created_at = NOW()",
        )
        .bind(&subdomain)
        .execute(control.pool())
        .await
        .map_err(CloudError::db("reserving freed subdomain"))?;

        sqlx::query("DELETE FROM accounts WHERE id = $1")
            .bind(account_id)
            .execute(control.pool())
            .await
            .map_err(CloudError::db("deleting account row"))?;

        tracing::info!(account_id, subdomain, "deleted account");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tenant_db_name_is_fixed_shape() {
        let uuid = Uuid::new_v4();
        let name = tenant_db_name(uuid);
        assert_eq!(name.len(), 31, "acct_ + 26 base32 chars");
        assert!(name.starts_with("acct_"));
        assert!(is_tenant_db_name(&name), "generated names pass the guard");
        // Deterministic, and distinct per UUID.
        assert_eq!(name, tenant_db_name(uuid));
        assert_ne!(name, tenant_db_name(Uuid::new_v4()));
    }

    #[test]
    fn tenant_db_name_known_vector() {
        // base32(00000000-0000-0000-0000-000000000000) = 26 'a's.
        let nil = Uuid::nil();
        assert_eq!(tenant_db_name(nil), format!("acct_{}", "a".repeat(26)));
    }

    #[test]
    fn tenant_db_name_guard_rejects_everything_else() {
        for bad in [
            "",
            "acct_",
            "acct_short",
            "default",
            "atomic_cloud_control",
            // Right length, wrong charset (base32 has no 0/1/8/9, no uppercase).
            &format!("acct_{}", "0".repeat(26)),
            &format!("acct_{}", "A".repeat(26)),
            // Injection attempts.
            "acct_\"; DROP DATABASE x; --",
            &format!("acct_{}x", "a".repeat(26)),
        ] {
            assert!(!is_tenant_db_name(bad), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn tenant_db_account_id_inverts_tenant_db_name() {
        for uuid in [Uuid::nil(), Uuid::new_v4(), Uuid::max()] {
            assert_eq!(tenant_db_account_id(&tenant_db_name(uuid)), Some(uuid));
        }
        for bad in ["", "acct_", "default", &format!("acct_{}", "0".repeat(26))] {
            assert_eq!(tenant_db_account_id(bad), None, "{bad:?} must not decode");
        }
    }

    #[test]
    fn subdomain_format_rule() {
        for ok in ["abc", "my-notes", "a-1", "kenny", &"a".repeat(32)] {
            assert!(subdomain_format_ok(ok), "{ok:?} should be valid");
        }
        for bad in [
            "",
            "ab",            // too short
            &"a".repeat(33), // too long
            "Has-Upper",
            "under_score",
            "dotted.name",
            "spaced name",
            "émoji",
        ] {
            assert!(!subdomain_format_ok(bad), "{bad:?} should be invalid");
        }
    }

    #[test]
    fn email_shape_rule() {
        for ok in ["k@example.com", "a.b+tag@sub.example.org"] {
            assert!(email_format_ok(ok), "{ok:?} should pass");
        }
        for bad in [
            "",
            "no-at.example.com",
            "@example.com",
            "k@nodot",
            "a b@x.com",
        ] {
            assert!(!email_format_ok(bad), "{bad:?} should fail");
        }
    }

    #[test]
    fn tenant_db_url_replaces_path_and_keeps_query() {
        let cluster = ClusterConfig {
            cluster_id: "c1".into(),
            cluster_url: "postgres://u:p@host:5433/atomic_test?sslmode=require".into(),
        };
        assert_eq!(
            cluster.tenant_db_url("acct_abc").unwrap(),
            "postgres://u:p@host:5433/acct_abc?sslmode=require"
        );
    }
}
