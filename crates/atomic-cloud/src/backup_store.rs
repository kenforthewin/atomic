//! Object storage for logical backups, behind a trait (plan: "Backups &
//! disaster recovery" → "v1: nightly logical dumps").
//!
//! Backups are opaque `pg_dump -Fc` blobs written under deterministic keys
//! (`backups/<date>/acct_<uuid>.dump`, `backups/<date>/control.dump`,
//! `backups/final/<uuid>-<ts>.dump`). The store is the narrow seam between
//! the dump runner ([`crate::backup`]) and wherever the bytes physically
//! land — exactly the [`crate::email::EmailSender`] /
//! [`crate::billing::BillingProvider`] / [`crate::provisioning_api`] shape: a
//! trait, a real impl, and a local impl that every test uses so no test ever
//! touches the network.
//!
//! Two implementations:
//!
//! - [`LocalFileSystemStore`] — a configured base directory, pure
//!   `tokio::fs`. The dev default and the **only** store the test suite ever
//!   constructs; it is always available and never opens a socket. Keys map
//!   to relative paths under the base dir, with traversal (`..`, absolute
//!   keys) rejected so a corrupt key can't escape the tree.
//! - [`S3Store`] — production, backed by the well-maintained
//!   [`object_store`] crate's `AmazonS3` client (S3 and any S3-compatible
//!   endpoint: R2, MinIO, GCS-via-interop). Chosen over hand-rolling AWS
//!   SigV4: a single dependency that already implements signing, retries, and
//!   multipart, with a battle-tested correctness story we don't have to own.
//!   Retention (14 daily + 8 weekly; 30 days for finals) is **bucket
//!   lifecycle policy, not code** (plan) — this store only ever writes,
//!   reads, lists, and probes.
//!
//! S3 credentials are **environment-only**, never argv (they would leak into
//! process listings): `serve --backup-store s3` reads `--backup-bucket`,
//! `--backup-region`, `--backup-endpoint`, `--backup-prefix` from
//! flags/env, and the access-key/secret from the environment alone (the
//! standard `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY`, which
//! [`object_store`] reads itself). See `main.rs`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;

use crate::error::CloudError;

/// Where a backup blob lives, addressed by an opaque key.
///
/// Keys are forward-slash paths the dump runner builds
/// ([`crate::backup::nightly_tenant_key`] and friends). Implementations are
/// free to map them onto a filesystem tree or an object-store prefix, but
/// must round-trip: a [`put`](BackupStore::put) under key `k` is readable by
/// [`get`](BackupStore::get) under the same `k`, visible to
/// [`list`](BackupStore::list) for any prefix of `k`, and
/// [`exists`](BackupStore::exists) returns true.
#[async_trait]
pub trait BackupStore: Send + Sync {
    /// Write `bytes` at `key`, overwriting any existing object. A backup is
    /// written exactly once per (tenant, date); re-running a day's pass
    /// overwrites, which is the intended idempotency.
    async fn put(&self, key: &str, bytes: Vec<u8>) -> Result<(), CloudError>;

    /// Read the object at `key`. [`CloudError::BackupStore`] when it is
    /// absent or unreadable — restore is the only `get` caller and a missing
    /// dump must fail loudly, never silently restore nothing.
    async fn get(&self, key: &str) -> Result<Vec<u8>, CloudError>;

    /// Every key under `prefix` (the prefix is matched literally, not as a
    /// glob). Order is unspecified. Used by the restore tooling to enumerate
    /// a date's dumps and by tests; the nightly pass never lists.
    async fn list(&self, prefix: &str) -> Result<Vec<String>, CloudError>;

    /// Whether an object exists at `key`, without fetching it. The
    /// staleness/verification tooling probes with this.
    async fn exists(&self, key: &str) -> Result<bool, CloudError>;
}

/// A backup store backed by a local directory tree. Dev default and the only
/// store used by tests — always available, never network.
pub struct LocalFileSystemStore {
    base: PathBuf,
}

impl LocalFileSystemStore {
    /// Root the store at `base`. The directory (and any key's parent dirs)
    /// are created lazily on `put`, so a fresh base dir needn't pre-exist.
    pub fn new(base: impl Into<PathBuf>) -> Self {
        Self { base: base.into() }
    }

    /// Resolve `key` to an absolute path under [`Self::base`], rejecting any
    /// key that would escape the tree. Keys are internal (built by the dump
    /// runner), so this is defense in depth against a corrupt control-plane
    /// row feeding a traversal — the same posture as
    /// [`is_tenant_db_name`](crate::provision::is_tenant_db_name) guarding
    /// DDL interpolation.
    fn resolve(&self, key: &str) -> Result<PathBuf, CloudError> {
        if key.is_empty() {
            return Err(CloudError::BackupStore("backup key is empty".into()));
        }
        let rel = Path::new(key);
        if rel.is_absolute()
            || rel.components().any(|c| {
                matches!(
                    c,
                    std::path::Component::ParentDir | std::path::Component::RootDir
                )
            })
        {
            return Err(CloudError::BackupStore(format!(
                "backup key {key:?} escapes the store root"
            )));
        }
        Ok(self.base.join(rel))
    }
}

#[async_trait]
impl BackupStore for LocalFileSystemStore {
    async fn put(&self, key: &str, bytes: Vec<u8>) -> Result<(), CloudError> {
        let path = self.resolve(key)?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(CloudError::backup_io(format!(
                    "creating backup directory for {key:?}"
                )))?;
        }
        tokio::fs::write(&path, bytes)
            .await
            .map_err(CloudError::backup_io(format!("writing backup {key:?}")))
    }

    async fn get(&self, key: &str) -> Result<Vec<u8>, CloudError> {
        let path = self.resolve(key)?;
        tokio::fs::read(&path)
            .await
            .map_err(CloudError::backup_io(format!("reading backup {key:?}")))
    }

    async fn list(&self, prefix: &str) -> Result<Vec<String>, CloudError> {
        // Walk the whole tree and keep keys (forward-slash, base-relative)
        // that start with `prefix`. Simpler than mapping a prefix to a
        // subdirectory, and robust to a prefix that ends mid-path-component
        // (e.g. `backups/2026-01` matching `backups/2026-01-01/...`).
        let mut keys = Vec::new();
        collect_keys(&self.base, &self.base, &mut keys).await?;
        keys.retain(|k| k.starts_with(prefix));
        Ok(keys)
    }

    async fn exists(&self, key: &str) -> Result<bool, CloudError> {
        let path = self.resolve(key)?;
        match tokio::fs::metadata(&path).await {
            Ok(meta) => Ok(meta.is_file()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(CloudError::backup_io(format!("probing backup {key:?}"))(e)),
        }
    }
}

/// Recursively collect base-relative, forward-slash keys for every regular
/// file under `dir`. A missing root is an empty store (nothing written yet),
/// not an error.
fn collect_keys<'a>(
    root: &'a Path,
    dir: &'a Path,
    out: &'a mut Vec<String>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), CloudError>> + Send + 'a>> {
    Box::pin(async move {
        let mut entries = match tokio::fs::read_dir(dir).await {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => {
                return Err(CloudError::backup_io(format!(
                    "listing backup directory {}",
                    dir.display()
                ))(e))
            }
        };
        while let Some(entry) =
            entries
                .next_entry()
                .await
                .map_err(CloudError::backup_io(format!(
                    "reading backup directory entry in {}",
                    dir.display()
                )))?
        {
            let path = entry.path();
            let file_type = entry
                .file_type()
                .await
                .map_err(CloudError::backup_io(format!(
                    "stat backup entry {}",
                    path.display()
                )))?;
            if file_type.is_dir() {
                collect_keys(root, &path, out).await?;
            } else if file_type.is_file() {
                if let Ok(rel) = path.strip_prefix(root) {
                    // Internal keys are always forward-slash; normalize so a
                    // Windows base dir wouldn't change the key surface.
                    let key = rel
                        .components()
                        .map(|c| c.as_os_str().to_string_lossy())
                        .collect::<Vec<_>>()
                        .join("/");
                    out.push(key);
                }
            }
        }
        Ok(())
    })
}

/// Production object store, backed by [`object_store`]'s `AmazonS3` client.
pub struct S3Store {
    inner: Arc<dyn object_store::ObjectStore>,
    /// Normalized key prefix (no surrounding slashes), or `None`. Prepended to
    /// every key so operators can share one bucket (`--backup-prefix`).
    prefix: Option<String>,
}

/// What [`S3Store`] needs to address a bucket. Credentials are NOT here —
/// they come from the process environment ([`object_store`] reads
/// `AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY` itself), keeping secrets out
/// of argv and out of this struct's `Debug`.
#[derive(Debug, Clone)]
pub struct S3Config {
    pub bucket: String,
    /// AWS region (`us-east-1`). Required by SigV4 even against S3-compatible
    /// endpoints, which typically accept `auto`/`us-east-1`.
    pub region: String,
    /// Override endpoint for S3-compatible providers (R2, MinIO). `None`
    /// targets AWS S3 proper.
    pub endpoint: Option<String>,
    /// Optional key prefix prepended to every object key, for operators
    /// sharing one bucket across deployments (`--backup-prefix`). Composes
    /// *in front of* the existing `backups/` layout: with prefix `prod` a
    /// nightly dump lands at `prod/backups/<date>/<db>.dump`. Empty/`None`
    /// keeps the bare `backups/...` layout. Surrounding slashes are
    /// normalized so `prod`, `prod/`, and `/prod/` are equivalent.
    pub prefix: Option<String>,
}

impl S3Store {
    /// Build the client from config plus environment credentials. Construction
    /// only assembles the client (no network), so a misconfigured bucket
    /// surfaces on the first `put`, not here — except an unparseable config,
    /// which fails typed at boot.
    pub fn new(config: &S3Config) -> Result<Self, CloudError> {
        let mut builder = object_store::aws::AmazonS3Builder::from_env()
            .with_bucket_name(&config.bucket)
            .with_region(&config.region);
        if let Some(endpoint) = &config.endpoint {
            // S3-compatible endpoints are virtually always https with
            // path-style addressing (per-bucket subdomains aren't set up);
            // allow http only for an explicit http:// endpoint (local MinIO).
            let allow_http = endpoint.starts_with("http://");
            builder = builder
                .with_endpoint(endpoint)
                .with_allow_http(allow_http)
                .with_virtual_hosted_style_request(false);
        }
        let store = builder
            .build()
            .map_err(|e| CloudError::BackupStore(format!("building S3 client: {e}")))?;
        Ok(Self {
            inner: Arc::new(store),
            prefix: normalize_prefix(config.prefix.as_deref()),
        })
    }

    /// The full object path for a backup `key`: the configured prefix (if any)
    /// joined in front of the bare key. The `backups/...` layout is preserved
    /// underneath the prefix.
    fn full_key(&self, key: &str) -> String {
        match &self.prefix {
            Some(prefix) => format!("{prefix}/{key}"),
            None => key.to_string(),
        }
    }

    /// Strip the configured prefix back off a listed key so callers see the
    /// same bare `backups/...` keys they put. A listing that (defensively)
    /// returns a key outside the prefix is passed through unchanged.
    fn strip_prefix<'a>(&self, full: &'a str) -> &'a str {
        match &self.prefix {
            Some(prefix) => full
                .strip_prefix(prefix)
                .map(|rest| rest.trim_start_matches('/'))
                .unwrap_or(full),
            None => full,
        }
    }
}

/// Normalize an operator-supplied `--backup-prefix` to an internal form with no
/// leading/trailing slashes (empty → `None`), so `prod`, `prod/`, and `/prod/`
/// all yield `prod`. Empty after trimming means "no prefix".
fn normalize_prefix(prefix: Option<&str>) -> Option<String> {
    let trimmed = prefix?.trim_matches('/');
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[async_trait]
impl BackupStore for S3Store {
    async fn put(&self, key: &str, bytes: Vec<u8>) -> Result<(), CloudError> {
        let path = object_store::path::Path::from(self.full_key(key));
        self.inner
            .put(&path, bytes.into())
            .await
            .map(|_| ())
            .map_err(|e| CloudError::BackupStore(format!("S3 put {key:?}: {e}")))
    }

    async fn get(&self, key: &str) -> Result<Vec<u8>, CloudError> {
        let path = object_store::path::Path::from(self.full_key(key));
        let result = self
            .inner
            .get(&path)
            .await
            .map_err(|e| CloudError::BackupStore(format!("S3 get {key:?}: {e}")))?;
        let bytes = result
            .bytes()
            .await
            .map_err(|e| CloudError::BackupStore(format!("S3 read {key:?}: {e}")))?;
        Ok(bytes.to_vec())
    }

    async fn list(&self, prefix: &str) -> Result<Vec<String>, CloudError> {
        use futures::StreamExt;
        let path = object_store::path::Path::from(self.full_key(prefix));
        let mut stream = self.inner.list(Some(&path));
        let mut keys = Vec::new();
        while let Some(meta) = stream.next().await {
            let meta =
                meta.map_err(|e| CloudError::BackupStore(format!("S3 list {prefix:?}: {e}")))?;
            // Return bare keys (prefix stripped) so callers round-trip with
            // the same keys they put.
            keys.push(self.strip_prefix(&meta.location.to_string()).to_string());
        }
        Ok(keys)
    }

    async fn exists(&self, key: &str) -> Result<bool, CloudError> {
        let path = object_store::path::Path::from(self.full_key(key));
        match self.inner.head(&path).await {
            Ok(_) => Ok(true),
            Err(object_store::Error::NotFound { .. }) => Ok(false),
            Err(e) => Err(CloudError::BackupStore(format!("S3 head {key:?}: {e}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The local store is exercised end-to-end (put/get/list/exists round
    // trip) under a temp dir in tests/backup.rs; these unit tests pin the
    // path-safety guard, which has no PG dependency and must always run.

    #[test]
    fn resolve_rejects_traversal_and_absolute_keys() {
        let store = LocalFileSystemStore::new("/tmp/atomic-backups-base");
        assert!(store.resolve("backups/2026-01-01/acct_x.dump").is_ok());
        assert!(store.resolve("").is_err(), "empty key");
        assert!(store.resolve("../escape").is_err(), "parent traversal");
        assert!(
            store.resolve("backups/../../escape").is_err(),
            "embedded traversal"
        );
        assert!(store.resolve("/etc/passwd").is_err(), "absolute key");
    }

    #[test]
    fn resolve_joins_under_base() {
        let store = LocalFileSystemStore::new("/var/atomic/backups");
        let path = store.resolve("backups/final/abc.dump").unwrap();
        assert!(path.ends_with("backups/final/abc.dump"));
        assert!(path.starts_with("/var/atomic/backups"));
    }

    #[test]
    fn prefix_normalization_is_slash_insensitive() {
        // `prod`, `prod/`, `/prod/` all collapse to `prod`; empty → None.
        assert_eq!(normalize_prefix(Some("prod")).as_deref(), Some("prod"));
        assert_eq!(normalize_prefix(Some("prod/")).as_deref(), Some("prod"));
        assert_eq!(normalize_prefix(Some("/prod/")).as_deref(), Some("prod"));
        assert_eq!(normalize_prefix(Some("a/b")).as_deref(), Some("a/b"));
        assert_eq!(normalize_prefix(Some("")), None);
        assert_eq!(normalize_prefix(Some("///")), None);
        assert_eq!(normalize_prefix(None), None);
    }

    #[test]
    fn s3_prefix_composes_with_backups_layout_and_round_trips() {
        // full_key prepends the prefix in front of the bare backups/ key;
        // strip_prefix is its inverse, so list() returns the same bare keys
        // put() was given. Built without touching the network (the inner
        // client is never called).
        let store = S3Store {
            inner: Arc::new(object_store::memory::InMemory::new()),
            prefix: normalize_prefix(Some("/prod/")),
        };
        let bare = "backups/2026-06-09/acct_x.dump";
        let full = store.full_key(bare);
        assert_eq!(full, "prod/backups/2026-06-09/acct_x.dump");
        assert_eq!(store.strip_prefix(&full), bare);

        // No prefix is a pure passthrough.
        let none = S3Store {
            inner: Arc::new(object_store::memory::InMemory::new()),
            prefix: None,
        };
        assert_eq!(none.full_key(bare), bare);
        assert_eq!(none.strip_prefix(bare), bare);
    }
}
