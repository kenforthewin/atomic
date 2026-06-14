//! Logical dump/restore runner (plan: "Backups & disaster recovery" → "v1:
//! nightly logical dumps").
//!
//! The dump mechanism is shelling out to `pg_dump -Fc` (custom format) per
//! database; restore is `pg_restore` into a freshly `CREATE`d database. Both
//! the dump and the restore connect to the cluster with the same parameters
//! the request path uses (derived from [`ClusterConfig::cluster_url`] exactly
//! as [`ClusterConfig::tenant_db_url`] derives a tenant URL), but they reach
//! Postgres through `libpq`'s command-line tools rather than `sqlx`, so the
//! connection is assembled as discrete flags.
//!
//! # Credential hygiene (load-bearing)
//!
//! The password is **never** an argv token — `argv` is world-readable via
//! `ps`/`/proc`. It is handed to the child process through the `PGPASSWORD`
//! environment variable ([`tokio::process::Command::env`]); the host, port,
//! user, and database go on the command line as `--host`/`--port`/
//! `--username`/`--dbname`. A `--dbname` *URL* (which would embed the
//! password) is likewise never used. Tests assert the sentinel password
//! appears only in the child's env, never in its argv.
//!
//! # Database-name validation
//!
//! Every database name is run through
//! [`is_tenant_db_name`](crate::provision::is_tenant_db_name) — the same
//! shape guard provisioning's DDL interpolation uses — before it reaches a
//! `--dbname` flag or a `CREATE DATABASE`. The control plane is the one
//! non-tenant database that's dumped; its name is validated by
//! [`is_safe_database_name`]-equivalent logic at the call site (it comes from
//! the control URL, not a tenant row).
//!
//! # Error surface
//!
//! A failed dump/restore is a [`CloudError::Backup`] carrying the operation,
//! the exit status, and a **bounded** tail of the tool's stderr — never the
//! password (it isn't in stderr; the tools don't echo `PGPASSWORD`) and never
//! the full unbounded output.

use std::process::Stdio;
use std::str::FromStr;

use sqlx::postgres::PgConnectOptions;
use sqlx::Connection;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::error::CloudError;
use crate::provision::{is_tenant_db_name, ClusterConfig};

/// Cap on captured `pg_dump`/`pg_restore` stderr in a [`CloudError::Backup`]
/// — enough to diagnose (the tool's last few lines name the failure) without
/// unbounded log/error growth on a pathological run. Mirrors
/// [`MIGRATION_ERROR_MAX_LEN`](crate::fleet_migration::MIGRATION_ERROR_MAX_LEN).
pub const DUMP_STDERR_MAX_LEN: usize = 4096;

/// Connection parameters for one database on the cluster, as discrete fields
/// so the password can be routed to the child's environment and everything
/// else to argv. Derived from a cluster/control URL via [`Self::from_url`].
///
/// `Debug` is deliberately **not** derived: the struct holds a plaintext
/// password, and a stray `?conn` in a log line would leak it. It is only ever
/// constructed locally, used to build a command, and dropped.
#[derive(Clone)]
pub struct DumpConnection {
    pub host: String,
    pub port: u16,
    pub user: String,
    /// Routed to `PGPASSWORD` in the child env, never argv. `None` when the
    /// URL carries no password (peer/ident auth, or a `.pgpass`/env-supplied
    /// secret libpq finds itself).
    pub password: Option<String>,
}

impl DumpConnection {
    /// Parse the host/port/user/password out of a Postgres URL the same way
    /// the rest of the crate parses the cluster URL. The database in the URL
    /// is ignored — callers pass the target database name explicitly so it
    /// can be shape-validated independently.
    pub fn from_url(url: &str) -> Result<Self, CloudError> {
        let opts = PgConnectOptions::from_str(url)
            .map_err(|e| CloudError::InvalidUrl(format!("cluster URL for backup: {e}")))?;
        Ok(Self {
            host: opts.get_host().to_string(),
            port: opts.get_port(),
            user: opts.get_username().to_string(),
            // PgConnectOptions doesn't expose the password; re-parse via the
            // url crate, which does. Both parsers accept the same URLs.
            password: url::Url::parse(url)
                .ok()
                .and_then(|u| u.password().map(|p| p.to_string())),
        })
    }

    /// Connection params for a tenant/control database on this cluster.
    pub fn for_cluster(cluster: &ClusterConfig) -> Result<Self, CloudError> {
        Self::from_url(&cluster.cluster_url)
    }

    /// The connection flags every `pg_dump`/`pg_restore`/`psql` invocation
    /// shares, EXCLUDING the password (which goes to the child env). `dbname`
    /// is the validated database name.
    fn conn_args(&self, dbname: &str) -> Vec<String> {
        vec![
            "--host".into(),
            self.host.clone(),
            "--port".into(),
            self.port.to_string(),
            "--username".into(),
            self.user.clone(),
            "--dbname".into(),
            dbname.into(),
        ]
    }

    /// Apply `PGPASSWORD` to a spawned command's environment when a password
    /// is set. The ONLY place the password is handed to a child — never argv.
    fn apply_password(&self, command: &mut Command) {
        if let Some(password) = &self.password {
            command.env("PGPASSWORD", password);
        }
    }
}

/// Validate a tenant database name before it reaches a dump/restore command
/// line or DDL. Reuses provisioning's shape guard verbatim.
fn checked_tenant_db_name(name: &str) -> Result<&str, CloudError> {
    if !is_tenant_db_name(name) {
        return Err(CloudError::InvalidDatabaseName(name.to_string()));
    }
    Ok(name)
}

/// Validate a control-plane database name: the conservative DDL-safe charset
/// (`[A-Za-z0-9_-]`), matching [`control_plane`]'s own guard. The control
/// plane is the one non-tenant database the nightly pass dumps.
///
/// [`control_plane`]: crate::control_plane
fn checked_control_db_name(name: &str) -> Result<&str, CloudError> {
    let safe = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if !safe {
        return Err(CloudError::InvalidDatabaseName(name.to_string()));
    }
    Ok(name)
}

/// Whether `pg_dump` and `pg_restore` are on `PATH`. Tests probe this and
/// skip with a message when a CI image lacks them (mirroring the PG-gating
/// idiom); locally, where the pgvector/pg16 cluster lives, they run.
pub async fn backup_tools_available() -> bool {
    binary_present("pg_dump").await && binary_present("pg_restore").await
}

async fn binary_present(bin: &str) -> bool {
    Command::new(bin)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

/// `pg_dump -Fc` the tenant database `db_name` and return the custom-format
/// dump bytes. The password rides in `PGPASSWORD` (never argv); `db_name` is
/// shape-validated first.
pub async fn dump_tenant_database(
    conn: &DumpConnection,
    db_name: &str,
) -> Result<Vec<u8>, CloudError> {
    let db_name = checked_tenant_db_name(db_name)?;
    dump_database(conn, db_name).await
}

/// `pg_dump -Fc` the control-plane database and return the dump bytes.
pub async fn dump_control_database(
    conn: &DumpConnection,
    db_name: &str,
) -> Result<Vec<u8>, CloudError> {
    let db_name = checked_control_db_name(db_name)?;
    dump_database(conn, db_name).await
}

/// Build the `pg_dump` command for an already-validated database name —
/// argv-only connection flags plus `PGPASSWORD` in the child env. Shared by
/// [`dump_database`] and the credential-hygiene test, so the test inspects the
/// EXACT command the runner spawns.
fn build_dump_command(conn: &DumpConnection, db_name: &str) -> Command {
    let mut command = Command::new("pg_dump");
    command
        .arg("--format=custom")
        // No owners/privileges: restore lands in a fresh DB owned by the
        // restoring role, so role grants from the source cluster are noise
        // that only makes restore brittle.
        .arg("--no-owner")
        .arg("--no-privileges")
        .args(conn.conn_args(db_name))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    conn.apply_password(&mut command);
    command
}

/// Shared dump primitive over an already-validated database name. Writes the
/// `-Fc` dump to stdout, which is captured into memory. Tenant dumps are
/// small in v1 (a personal KB); if and when they aren't, this becomes a
/// streamed temp file — the [`BackupStore`](crate::backup_store::BackupStore)
/// seam already takes owned bytes so that swap is local.
async fn dump_database(conn: &DumpConnection, db_name: &str) -> Result<Vec<u8>, CloudError> {
    let mut command = build_dump_command(conn, db_name);
    let output = command
        .output()
        .await
        .map_err(|e| CloudError::Backup(format!("spawning pg_dump for {db_name:?}: {e}")))?;

    if !output.status.success() {
        return Err(CloudError::Backup(format!(
            "pg_dump for {db_name:?} exited {}: {}",
            output.status,
            bounded_stderr(&output.stderr)
        )));
    }
    if output.stdout.is_empty() {
        return Err(CloudError::Backup(format!(
            "pg_dump for {db_name:?} produced an empty dump"
        )));
    }
    Ok(output.stdout)
}

/// Restore a `-Fc` dump into a **fresh** database named `target_db_name` on
/// the cluster: create the database (failing if it already exists — a restore
/// must never clobber a live tenant), then `pg_restore` the bytes into it.
/// The runbook's restore-into-fresh-DB step (plan: "Restore runbook").
///
/// `target_db_name` is shape-validated as a tenant name (restore targets are
/// always tenant databases — the control plane is restored by an operator
/// out-of-band, not through this path).
pub async fn restore_database(
    cluster: &ClusterConfig,
    conn: &DumpConnection,
    target_db_name: &str,
    dump: &[u8],
) -> Result<(), CloudError> {
    let target_db_name = checked_tenant_db_name(target_db_name)?;

    // Create the fresh target. CREATE DATABASE can't run in a transaction or
    // bind its identifier, so it goes through a maintenance connection with
    // the shape-validated, quoted name (same posture as provision.rs). A
    // pre-existing database is a hard error: restoring over live data is the
    // exact accident this guards against.
    let mut maint = cluster.connect_maintenance().await?;
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_database WHERE datname = $1)")
            .bind(target_db_name)
            .fetch_one(&mut maint)
            .await
            .map_err(CloudError::db("checking restore-target existence"))?;
    if exists {
        let _ = maint.close().await;
        return Err(CloudError::Backup(format!(
            "restore target {target_db_name:?} already exists; restore into a fresh \
             database name and repoint account_databases.db_name (see the restore runbook)"
        )));
    }
    sqlx::raw_sql(&format!("CREATE DATABASE \"{target_db_name}\""))
        .execute(&mut maint)
        .await
        .map_err(CloudError::db("creating restore-target database"))?;
    let _ = maint.close().await;

    // pg_restore the bytes into the fresh database via stdin. --no-owner /
    // --no-privileges match the dump flags; --exit-on-error makes any object
    // failure fatal rather than a partial restore that looks successful.
    let mut command = Command::new("pg_restore");
    command
        .arg("--no-owner")
        .arg("--no-privileges")
        .arg("--exit-on-error")
        .args(conn.conn_args(target_db_name))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    conn.apply_password(&mut command);

    let mut child = command.spawn().map_err(|e| {
        CloudError::Backup(format!("spawning pg_restore for {target_db_name:?}: {e}"))
    })?;

    // Stream the dump to the child's stdin, then await completion. Take the
    // handle so it's dropped (closing stdin / signalling EOF) before we wait.
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| CloudError::Backup("pg_restore stdin was not captured".into()))?;
    stdin
        .write_all(dump)
        .await
        .map_err(|e| CloudError::Backup(format!("writing dump to pg_restore stdin: {e}")))?;
    stdin
        .shutdown()
        .await
        .map_err(|e| CloudError::Backup(format!("closing pg_restore stdin: {e}")))?;
    drop(stdin);

    let output = child.wait_with_output().await.map_err(|e| {
        CloudError::Backup(format!("awaiting pg_restore for {target_db_name:?}: {e}"))
    })?;
    if !output.status.success() {
        return Err(CloudError::Backup(format!(
            "pg_restore for {target_db_name:?} exited {}: {}",
            output.status,
            bounded_stderr(&output.stderr)
        )));
    }
    Ok(())
}

/// A bounded, lossy-UTF-8 tail of captured stderr for error messages — the
/// last [`DUMP_STDERR_MAX_LEN`] bytes (the failure is at the end), trimmed.
fn bounded_stderr(stderr: &[u8]) -> String {
    let tail = if stderr.len() > DUMP_STDERR_MAX_LEN {
        &stderr[stderr.len() - DUMP_STDERR_MAX_LEN..]
    } else {
        stderr
    };
    String::from_utf8_lossy(tail).trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_url_extracts_all_parts() {
        let conn = DumpConnection::from_url(
            "postgres://atomic:s3cr3t@db.internal:5433/atomic_test?sslmode=require",
        )
        .unwrap();
        assert_eq!(conn.host, "db.internal");
        assert_eq!(conn.port, 5433);
        assert_eq!(conn.user, "atomic");
        assert_eq!(conn.password.as_deref(), Some("s3cr3t"));
    }

    #[test]
    fn from_url_handles_missing_password() {
        let conn = DumpConnection::from_url("postgres://atomic@localhost:5432/x").unwrap();
        assert_eq!(conn.password, None);
    }

    #[test]
    fn conn_args_carry_no_password_and_a_validated_dbname() {
        let conn = DumpConnection::from_url("postgres://u:pw@h:5432/ignored").unwrap();
        let args = conn.conn_args("acct_aaaaaaaaaaaaaaaaaaaaaaaaaa");
        // The password is NOWHERE in the connection flags (it goes to env).
        assert!(
            !args.iter().any(|a| a.contains("pw")),
            "password must never appear in argv: {args:?}"
        );
        assert!(args.contains(&"--host".to_string()));
        assert!(args.contains(&"h".to_string()));
        assert!(args.contains(&"--dbname".to_string()));
    }

    #[test]
    fn tenant_name_validation_rejects_injection() {
        assert!(checked_tenant_db_name("acct_\"; DROP DATABASE x; --").is_err());
        assert!(checked_tenant_db_name("not_a_tenant").is_err());
        assert!(checked_tenant_db_name(&format!("acct_{}", "a".repeat(26))).is_ok());
    }

    #[test]
    fn control_name_validation_allows_safe_rejects_unsafe() {
        assert!(checked_control_db_name("atomic_cloud_control").is_ok());
        assert!(checked_control_db_name("atomic_cloud_test_abc123").is_ok());
        assert!(checked_control_db_name("evil\"; DROP DATABASE x; --").is_err());
        assert!(checked_control_db_name("").is_err());
    }

    #[test]
    fn password_rides_in_env_never_in_argv() {
        // The load-bearing credential-hygiene check: a sentinel password
        // must appear ONLY in the spawned command's environment (PGPASSWORD),
        // never in any argv token (argv is world-readable via ps/proc).
        const SENTINEL: &str = "sup3r-s3cret-sentinel";
        let conn = DumpConnection::from_url(&format!(
            "postgres://atomic:{SENTINEL}@localhost:5433/atomic_test"
        ))
        .unwrap();
        let command = build_dump_command(&conn, "acct_aaaaaaaaaaaaaaaaaaaaaaaaaa");
        let std = command.as_std();

        // No argv token contains the password.
        let args: Vec<String> = std
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(
            !args.iter().any(|a| a.contains(SENTINEL)),
            "password leaked into argv: {args:?}"
        );
        // The program name doesn't either (belt and braces).
        assert!(!std.get_program().to_string_lossy().contains(SENTINEL));

        // It IS in the child env, under PGPASSWORD.
        let pgpassword = std
            .get_envs()
            .find(|(k, _)| *k == std::ffi::OsStr::new("PGPASSWORD"))
            .and_then(|(_, v)| v)
            .map(|v| v.to_string_lossy().into_owned());
        assert_eq!(
            pgpassword.as_deref(),
            Some(SENTINEL),
            "password must be passed via PGPASSWORD in the child environment"
        );
    }

    #[test]
    fn bounded_stderr_truncates_to_the_tail() {
        let big = vec![b'x'; DUMP_STDERR_MAX_LEN + 1000];
        let out = bounded_stderr(&big);
        assert!(out.len() <= DUMP_STDERR_MAX_LEN);
        let small = b"  pg_restore: error: boom\n";
        assert_eq!(bounded_stderr(small), "pg_restore: error: boom");
    }
}
