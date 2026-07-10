//! Background SQLite → Postgres migration jobs.
//!
//! Two job kinds share one manager and status shape:
//!
//! - **Import** (Postgres mode): an uploaded SQLite file is copied into a new
//!   logical database via `atomic_core::migrate`.
//! - **Push** (SQLite mode, i.e. the desktop sidecar): a local database is
//!   snapshotted, uploaded to a remote Atomic server's import endpoint, and
//!   the remote import job is polled to completion — so the desktop frontend
//!   only ever watches one local job.

use atomic_core::migrate::{MigrationEvent, MigrationOptions};
use atomic_core::{AtomicCore, AtomicCoreError, DatabaseManager};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

const COMPLETED_JOB_RETENTION: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);
const FAILED_JOB_RETENTION: std::time::Duration = std::time::Duration::from_secs(60 * 60);
const REMOTE_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// Default cap on uploaded SQLite files. Slim migration snapshots strip
/// embeddings, so even large knowledge bases stay well under this;
/// overridable via `ATOMIC_MAX_MIGRATION_UPLOAD_BYTES` for outliers.
const DEFAULT_MAX_UPLOAD_BYTES: u64 = 2 * 1024 * 1024 * 1024;

pub fn max_upload_bytes() -> u64 {
    std::env::var("ATOMIC_MAX_MIGRATION_UPLOAD_BYTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_MAX_UPLOAD_BYTES)
}

#[derive(Clone)]
pub struct MigrationJobManager {
    work_dir: Arc<PathBuf>,
    jobs: Arc<Mutex<HashMap<String, Arc<MigrationJob>>>>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MigrationJobStatus {
    Queued,
    Running,
    Complete,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize)]
pub struct MigrationJobResponse {
    pub id: String,
    /// "import" (Postgres side) or "push" (SQLite side).
    pub kind: String,
    pub status: MigrationJobStatus,
    pub phase: String,
    /// Destination database display name.
    pub db_name: String,
    pub total_rows: i64,
    pub processed_rows: i64,
    /// Destination database id once complete (the remote id for push jobs).
    pub db_id: Option<String>,
    /// Full `MigrationReport` once complete.
    pub report: Option<serde_json::Value>,
    pub error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub completed_at: Option<String>,
}

/// Parameters for a push job (desktop sidecar → remote server).
#[derive(Debug, Clone, Deserialize, utoipa::ToSchema)]
pub struct PushRequest {
    pub target_url: String,
    pub target_token: String,
    pub database_id: String,
    /// Destination name; defaults to the local database's name.
    pub name: Option<String>,
    /// Defaults to true for pushes: the local instance keeps running, so the
    /// destination should not start polling the same feeds.
    pub pause_feeds: Option<bool>,
}

struct MigrationJob {
    id: String,
    kind: &'static str,
    db_name: String,
    dir: PathBuf,
    cancel: AtomicBool,
    state: Mutex<MigrationJobState>,
}

struct MigrationJobState {
    status: MigrationJobStatus,
    phase: String,
    total_rows: i64,
    /// Rows finished in fully-copied tables.
    rows_done_base: i64,
    /// Rows copied so far in the in-flight table.
    current_table_rows: i64,
    db_id: Option<String>,
    report: Option<serde_json::Value>,
    error: Option<String>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    completed_at: Option<DateTime<Utc>>,
}

impl MigrationJobManager {
    pub fn new(work_dir: impl AsRef<Path>) -> Result<Self, AtomicCoreError> {
        let work_dir = work_dir.as_ref().to_path_buf();
        if work_dir.exists() {
            std::fs::remove_dir_all(&work_dir)?;
        }
        std::fs::create_dir_all(&work_dir)?;
        Ok(Self {
            work_dir: Arc::new(work_dir),
            jobs: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    pub fn for_tests(work_dir: impl AsRef<Path>) -> Self {
        Self::new(work_dir).expect("failed to create test migration manager")
    }

    /// Reserve a working directory + upload path for a not-yet-started job.
    /// The route handler streams the request body here before `start_import`.
    pub fn new_upload_path(&self) -> Result<(String, PathBuf), AtomicCoreError> {
        let job_id = uuid::Uuid::new_v4().to_string();
        let dir = self.work_dir.join(&job_id);
        std::fs::create_dir_all(&dir)?;
        Ok((job_id, dir.join("upload.db")))
    }

    /// Start an import job over an uploaded SQLite file (Postgres mode).
    pub fn start_import(
        &self,
        manager: Arc<DatabaseManager>,
        job_id: String,
        upload_path: PathBuf,
        db_name: String,
        pause_feeds: bool,
    ) -> Result<MigrationJobResponse, AtomicCoreError> {
        let dir = self.work_dir.join(&job_id);
        let job = Arc::new(MigrationJob::new(job_id.clone(), "import", db_name, dir));
        self.register(job_id.clone(), Arc::clone(&job))?;

        let jobs = self.clone();
        tokio::spawn(async move {
            let result = jobs
                .run_import(Arc::clone(&job), manager, upload_path, pause_feeds)
                .await;
            jobs.finish(job, result);
        });
        self.status(&job_id)
    }

    /// Start a push job (SQLite mode): snapshot → upload → poll remote.
    pub fn start_push(
        &self,
        core: AtomicCore,
        request: PushRequest,
        local_db_name: String,
    ) -> Result<MigrationJobResponse, AtomicCoreError> {
        let job_id = uuid::Uuid::new_v4().to_string();
        let dir = self.work_dir.join(&job_id);
        std::fs::create_dir_all(&dir)?;
        let db_name = request
            .name
            .clone()
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| local_db_name.clone());
        let job = Arc::new(MigrationJob::new(job_id.clone(), "push", db_name, dir));
        self.register(job_id.clone(), Arc::clone(&job))?;

        let jobs = self.clone();
        tokio::spawn(async move {
            let result = jobs.run_push(Arc::clone(&job), core, request).await;
            job.remove_snapshot_files();
            jobs.finish(job, result);
        });
        self.status(&job_id)
    }

    pub fn status(&self, job_id: &str) -> Result<MigrationJobResponse, AtomicCoreError> {
        Ok(self.get_job(job_id)?.response())
    }

    pub fn cancel_or_delete(&self, job_id: &str) -> Result<MigrationJobResponse, AtomicCoreError> {
        let job = self.get_job(job_id)?;
        let status = job.status();
        if matches!(
            status,
            MigrationJobStatus::Queued | MigrationJobStatus::Running
        ) {
            job.cancel.store(true, Ordering::SeqCst);
            job.set_phase("cancelling");
            return Ok(job.response());
        }

        let response = job.response();
        job.remove_artifacts();
        self.jobs
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?
            .remove(job_id);
        Ok(response)
    }

    fn register(&self, job_id: String, job: Arc<MigrationJob>) -> Result<(), AtomicCoreError> {
        let mut jobs = self
            .jobs
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        jobs.insert(job_id, job);
        Ok(())
    }

    fn get_job(&self, job_id: &str) -> Result<Arc<MigrationJob>, AtomicCoreError> {
        self.jobs
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?
            .get(job_id)
            .cloned()
            .ok_or_else(|| AtomicCoreError::NotFound(format!("Migration job '{}'", job_id)))
    }

    fn finish(&self, job: Arc<MigrationJob>, result: Result<(), AtomicCoreError>) {
        match result {
            Ok(()) => {
                job.mark_complete();
                self.schedule_cleanup(job.id.clone(), COMPLETED_JOB_RETENTION);
            }
            Err(e) if job.cancel.load(Ordering::SeqCst) => {
                job.mark_cancelled();
                self.schedule_cleanup(job.id.clone(), FAILED_JOB_RETENTION);
                tracing::info!(job_id = %job.id, kind = job.kind, "migration cancelled");
                let _ = e;
            }
            Err(e) => {
                job.mark_failed(e.to_string());
                self.schedule_cleanup(job.id.clone(), FAILED_JOB_RETENTION);
                tracing::warn!(job_id = %job.id, kind = job.kind, error = %e, "migration failed");
            }
        }
    }

    async fn run_import(
        &self,
        job: Arc<MigrationJob>,
        manager: Arc<DatabaseManager>,
        upload_path: PathBuf,
        pause_feeds: bool,
    ) -> Result<(), AtomicCoreError> {
        job.mark_running("validating upload");

        let event_job = Arc::clone(&job);
        let cancel_job = Arc::clone(&job);
        let result = manager
            .migrate_sqlite_to_postgres(
                &upload_path,
                MigrationOptions {
                    db_name: job.db_name.clone(),
                    dry_run: false,
                    pause_feeds,
                },
                move |event| event_job.apply_event(event),
                move || cancel_job.cancel.load(Ordering::SeqCst),
            )
            .await;

        job.remove_artifacts();
        let report = result?;
        job.set_result(report.db_id.clone(), serde_json::to_value(&report).ok());
        Ok(())
    }

    async fn run_push(
        &self,
        job: Arc<MigrationJob>,
        core: AtomicCore,
        request: PushRequest,
    ) -> Result<(), AtomicCoreError> {
        job.mark_running("snapshotting");
        let snapshot_path = job.dir.join("push.db");
        if !core.create_migration_snapshot(&snapshot_path).await? {
            return Err(AtomicCoreError::Configuration(
                "Only local SQLite databases can be pushed to a remote server".to_string(),
            ));
        }
        if job.cancel.load(Ordering::SeqCst) {
            return Err(AtomicCoreError::Conflict("Migration cancelled".to_string()));
        }

        job.set_phase("uploading");
        let base_url = request.target_url.trim_end_matches('/').to_string();
        let pause_feeds = request.pause_feeds.unwrap_or(true);
        let upload_url = format!(
            "{}/api/migrations/sqlite?name={}&pause_feeds={}",
            base_url,
            urlencode(&job.db_name),
            pause_feeds
        );

        let client = reqwest::Client::new();
        let file = tokio::fs::File::open(&snapshot_path).await?;
        let file_len = file.metadata().await?.len();
        let stream = tokio_util::io::ReaderStream::new(file);
        let response = client
            .post(&upload_url)
            .bearer_auth(&request.target_token)
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .header(reqwest::header::CONTENT_LENGTH, file_len)
            .body(reqwest::Body::wrap_stream(stream))
            .send()
            .await
            .map_err(|e| upload_error(&base_url, e))?;

        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(AtomicCoreError::Configuration(format!(
                "Remote server rejected the upload ({status}): {}",
                truncate(&body, 500)
            )));
        }
        let remote: serde_json::Value = serde_json::from_str(&body).map_err(|_| {
            AtomicCoreError::Configuration(format!(
                "Unexpected response from remote server: {}",
                truncate(&body, 200)
            ))
        })?;
        let remote_job_id = remote["id"]
            .as_str()
            .ok_or_else(|| {
                AtomicCoreError::Configuration("Remote server returned no job id".to_string())
            })?
            .to_string();

        // Mirror the remote import job's progress into this local job.
        let status_url = format!("{base_url}/api/migrations/{remote_job_id}");
        loop {
            if job.cancel.load(Ordering::SeqCst) {
                let _ = client
                    .delete(&status_url)
                    .bearer_auth(&request.target_token)
                    .send()
                    .await;
                return Err(AtomicCoreError::Conflict("Migration cancelled".to_string()));
            }

            tokio::time::sleep(REMOTE_POLL_INTERVAL).await;
            let remote: serde_json::Value = client
                .get(&status_url)
                .bearer_auth(&request.target_token)
                .send()
                .await
                .map_err(|e| upload_error(&base_url, e))?
                .json()
                .await
                .map_err(|e| {
                    AtomicCoreError::Configuration(format!("Remote status unreadable: {e}"))
                })?;

            job.mirror_remote(&remote);
            match remote["status"].as_str() {
                Some("complete") => {
                    job.set_result(
                        remote["db_id"].as_str().map(String::from),
                        Some(remote["report"].clone()),
                    );
                    return Ok(());
                }
                Some("failed") | Some("cancelled") => {
                    return Err(AtomicCoreError::Configuration(format!(
                        "Remote migration {}: {}",
                        remote["status"].as_str().unwrap_or("failed"),
                        remote["error"].as_str().unwrap_or("unknown error")
                    )));
                }
                _ => {}
            }
        }
    }

    fn schedule_cleanup(&self, job_id: String, delay: std::time::Duration) {
        let manager = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            if let Ok(job) = manager.get_job(&job_id) {
                job.remove_artifacts();
            }
            if let Ok(mut jobs) = manager.jobs.lock() {
                jobs.remove(&job_id);
            }
        });
    }
}

impl MigrationJob {
    fn new(id: String, kind: &'static str, db_name: String, dir: PathBuf) -> Self {
        let now = Utc::now();
        Self {
            id,
            kind,
            db_name,
            dir,
            cancel: AtomicBool::new(false),
            state: Mutex::new(MigrationJobState {
                status: MigrationJobStatus::Queued,
                phase: "queued".to_string(),
                total_rows: 0,
                rows_done_base: 0,
                current_table_rows: 0,
                db_id: None,
                report: None,
                error: None,
                created_at: now,
                updated_at: now,
                completed_at: None,
            }),
        }
    }

    fn response(&self) -> MigrationJobResponse {
        let state = self.state.lock().expect("migration job state poisoned");
        MigrationJobResponse {
            id: self.id.clone(),
            kind: self.kind.to_string(),
            status: state.status.clone(),
            phase: state.phase.clone(),
            db_name: self.db_name.clone(),
            total_rows: state.total_rows,
            processed_rows: state.rows_done_base + state.current_table_rows,
            db_id: state.db_id.clone(),
            report: state.report.clone(),
            error: state.error.clone(),
            created_at: state.created_at.to_rfc3339(),
            updated_at: state.updated_at.to_rfc3339(),
            completed_at: state.completed_at.map(|dt| dt.to_rfc3339()),
        }
    }

    fn apply_event(&self, event: MigrationEvent) {
        let mut state = self.state.lock().expect("migration job state poisoned");
        match event {
            MigrationEvent::Started { total_rows, .. } => {
                state.total_rows = total_rows;
                state.phase = "copying".to_string();
            }
            MigrationEvent::TableStarted { table, .. } => {
                state.phase = format!("copying {table}");
                state.current_table_rows = 0;
            }
            MigrationEvent::TableProgress { copied_rows, .. } => {
                state.current_table_rows = copied_rows;
            }
            MigrationEvent::TableCompleted { copied_rows, .. } => {
                state.rows_done_base += copied_rows;
                state.current_table_rows = 0;
            }
        }
        state.updated_at = Utc::now();
    }

    /// Copy the interesting fields of a remote import job into this push job.
    fn mirror_remote(&self, remote: &serde_json::Value) {
        let mut state = self.state.lock().expect("migration job state poisoned");
        if let Some(phase) = remote["phase"].as_str() {
            state.phase = format!("remote: {phase}");
        }
        if let Some(total) = remote["total_rows"].as_i64() {
            state.total_rows = total;
        }
        if let Some(processed) = remote["processed_rows"].as_i64() {
            state.rows_done_base = processed;
            state.current_table_rows = 0;
        }
        state.updated_at = Utc::now();
    }

    fn set_result(&self, db_id: Option<String>, report: Option<serde_json::Value>) {
        let mut state = self.state.lock().expect("migration job state poisoned");
        state.db_id = db_id;
        state.report = report;
        state.updated_at = Utc::now();
    }

    fn status(&self) -> MigrationJobStatus {
        self.state
            .lock()
            .expect("migration job state poisoned")
            .status
            .clone()
    }

    fn mark_running(&self, phase: &str) {
        let mut state = self.state.lock().expect("migration job state poisoned");
        state.status = MigrationJobStatus::Running;
        state.phase = phase.to_string();
        state.updated_at = Utc::now();
    }

    fn set_phase(&self, phase: &str) {
        let mut state = self.state.lock().expect("migration job state poisoned");
        state.phase = phase.to_string();
        state.updated_at = Utc::now();
    }

    fn mark_complete(&self) {
        let mut state = self.state.lock().expect("migration job state poisoned");
        let now = Utc::now();
        state.status = MigrationJobStatus::Complete;
        state.phase = "complete".to_string();
        state.rows_done_base += state.current_table_rows;
        state.current_table_rows = 0;
        state.completed_at = Some(now);
        state.updated_at = now;
        state.error = None;
    }

    fn mark_failed(&self, error: String) {
        let mut state = self.state.lock().expect("migration job state poisoned");
        let now = Utc::now();
        state.status = MigrationJobStatus::Failed;
        state.phase = "failed".to_string();
        state.completed_at = Some(now);
        state.updated_at = now;
        state.error = Some(error);
    }

    fn mark_cancelled(&self) {
        let mut state = self.state.lock().expect("migration job state poisoned");
        let now = Utc::now();
        state.status = MigrationJobStatus::Cancelled;
        state.phase = "cancelled".to_string();
        state.completed_at = Some(now);
        state.updated_at = now;
    }

    fn remove_snapshot_files(&self) {
        for suffix in ["push.db", "push.db-wal", "push.db-shm"] {
            let path = self.dir.join(suffix);
            if path.exists() {
                let _ = std::fs::remove_file(path);
            }
        }
    }

    fn remove_artifacts(&self) {
        if self.dir.exists() {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

fn upload_error(base_url: &str, e: reqwest::Error) -> AtomicCoreError {
    AtomicCoreError::Configuration(format!("Could not reach remote server {base_url}: {e}"))
}

fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn truncate(value: &str, max: usize) -> &str {
    match value.char_indices().nth(max) {
        Some((idx, _)) => &value[..idx],
        None => value,
    }
}
