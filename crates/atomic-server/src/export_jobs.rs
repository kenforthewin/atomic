//! Background export jobs and signed artifact downloads.

use atomic_core::{AtomicCore, AtomicCoreError, DatabaseManager, MarkdownExportProgress};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use chrono::{DateTime, Duration, Utc};
use rand::RngCore;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

const DOWNLOAD_TOKEN_TTL: Duration = Duration::minutes(30);
const COMPLETED_JOB_RETENTION: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);
const FAILED_JOB_RETENTION: std::time::Duration = std::time::Duration::from_secs(60 * 60);

#[derive(Clone)]
pub struct ExportJobManager {
    export_dir: Arc<PathBuf>,
    jobs: Arc<Mutex<HashMap<String, Arc<ExportJob>>>>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExportJobStatus {
    Queued,
    Running,
    Complete,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExportJobResponse {
    pub id: String,
    pub db_id: String,
    pub db_name: String,
    pub status: ExportJobStatus,
    pub phase: String,
    pub total_atoms: usize,
    pub processed_atoms: usize,
    pub bytes_written: u64,
    pub created_at: String,
    pub updated_at: String,
    pub completed_at: Option<String>,
    pub error: Option<String>,
    pub download_path: Option<String>,
    pub download_expires_at: Option<String>,
}

#[derive(Debug, Clone)]
pub struct DownloadArtifact {
    pub path: PathBuf,
    pub filename: String,
}

struct ExportJob {
    id: String,
    db_id: String,
    db_name: String,
    dir: PathBuf,
    cancel: AtomicBool,
    state: Mutex<ExportJobState>,
}

struct ExportJobState {
    status: ExportJobStatus,
    phase: String,
    total_atoms: usize,
    processed_atoms: usize,
    bytes_written: u64,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    completed_at: Option<DateTime<Utc>>,
    error: Option<String>,
    zip_path: Option<PathBuf>,
    download_token_hash: Option<String>,
    download_expires_at: Option<DateTime<Utc>>,
}

impl ExportJobManager {
    pub fn new(export_dir: impl AsRef<Path>) -> Result<Self, AtomicCoreError> {
        let export_dir = export_dir.as_ref().to_path_buf();
        if export_dir.exists() {
            std::fs::remove_dir_all(&export_dir)?;
        }
        std::fs::create_dir_all(&export_dir)?;

        Ok(Self {
            export_dir: Arc::new(export_dir),
            jobs: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    pub fn for_tests(export_dir: impl AsRef<Path>) -> Self {
        Self::new(export_dir).expect("failed to create test export manager")
    }

    /// Jobs currently queued or running — metrics instrumentation (export
    /// artifacts share the data volume, so in-flight exports are a disk
    /// signal). A poisoned lock reads as 0 rather than panicking a scrape.
    pub fn active_jobs(&self) -> usize {
        self.jobs
            .lock()
            .map(|jobs| {
                jobs.values()
                    .filter(|job| {
                        matches!(
                            job.state.lock().map(|s| s.status.clone()),
                            Ok(ExportJobStatus::Queued | ExportJobStatus::Running)
                        )
                    })
                    .count()
            })
            .unwrap_or(0)
    }

    /// True when the manager tracks nothing at all — no queued or running
    /// jobs and no completed jobs still inside their artifact-retention
    /// window. An idle manager owns no state worth keeping and may be
    /// dropped by its host; recreating one later wipes and recreates its
    /// (empty) directory. A poisoned lock reads as not-idle so a host never
    /// drops a manager it can't inspect.
    pub fn is_idle(&self) -> bool {
        self.jobs.lock().map(|jobs| jobs.is_empty()).unwrap_or(false)
    }

    pub async fn start_markdown_export(
        &self,
        manager: Arc<DatabaseManager>,
        db_id: String,
    ) -> Result<ExportJobResponse, AtomicCoreError> {
        let db_name = manager
            .list_databases()
            .await?
            .0
            .into_iter()
            .find(|db| db.id == db_id)
            .map(|db| db.name)
            .ok_or_else(|| AtomicCoreError::NotFound(format!("Database '{}'", db_id)))?;

        {
            let jobs = self
                .jobs
                .lock()
                .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
            if let Some(existing) = jobs.values().find(|job| {
                job.db_id == db_id
                    && matches!(
                        job.state.lock().map(|s| s.status.clone()),
                        Ok(ExportJobStatus::Queued | ExportJobStatus::Running)
                    )
            }) {
                return Ok(existing.response(false));
            }
        }

        let job_id = uuid::Uuid::new_v4().to_string();
        let job_dir = self.export_dir.join(&job_id);
        std::fs::create_dir_all(&job_dir)?;

        let job = Arc::new(ExportJob::new(
            job_id.clone(),
            db_id.clone(),
            db_name,
            job_dir,
        ));

        {
            let mut jobs = self
                .jobs
                .lock()
                .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
            let old_jobs = jobs
                .iter()
                .filter_map(|(id, old)| {
                    if old.db_id == db_id
                        && !matches!(
                            old.state.lock().map(|s| s.status.clone()),
                            Ok(ExportJobStatus::Queued | ExportJobStatus::Running)
                        )
                    {
                        Some(id.clone())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>();
            for old_id in old_jobs {
                if let Some(old) = jobs.remove(&old_id) {
                    old.remove_artifacts();
                }
            }
            jobs.insert(job_id.clone(), Arc::clone(&job));
        }

        let jobs = self.clone();
        tokio::spawn(async move {
            jobs.run_markdown_export(job, manager).await;
        });

        self.status(&job_id, false)
    }

    pub fn status(
        &self,
        job_id: &str,
        issue_download_token: bool,
    ) -> Result<ExportJobResponse, AtomicCoreError> {
        let job = self.get_job(job_id)?;
        Ok(job.response(issue_download_token))
    }

    pub fn cancel_or_delete(&self, job_id: &str) -> Result<ExportJobResponse, AtomicCoreError> {
        let job = self.get_job(job_id)?;
        let status = job.status();

        if matches!(status, ExportJobStatus::Queued | ExportJobStatus::Running) {
            job.cancel.store(true, Ordering::SeqCst);
            job.set_phase("cancelling");
            return Ok(job.response(false));
        }

        let response = job.response(false);
        job.remove_artifacts();
        let mut jobs = self
            .jobs
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        jobs.remove(job_id);
        Ok(response)
    }

    pub fn validate_download(
        &self,
        job_id: &str,
        token: &str,
    ) -> Result<DownloadArtifact, AtomicCoreError> {
        let job = self.get_job(job_id)?;
        let state = job
            .state
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;

        if state.status != ExportJobStatus::Complete {
            return Err(AtomicCoreError::Conflict(
                "Export is not complete".to_string(),
            ));
        }
        let Some(expires_at) = state.download_expires_at else {
            return Err(AtomicCoreError::Validation(
                "Missing download token".to_string(),
            ));
        };
        if expires_at < Utc::now() {
            return Err(AtomicCoreError::Validation(
                "Download token expired".to_string(),
            ));
        }
        let Some(expected_hash) = &state.download_token_hash else {
            return Err(AtomicCoreError::Validation(
                "Missing download token".to_string(),
            ));
        };
        if token_hash(token) != *expected_hash {
            return Err(AtomicCoreError::Validation(
                "Invalid download token".to_string(),
            ));
        }
        let Some(path) = &state.zip_path else {
            return Err(AtomicCoreError::NotFound("Export artifact".to_string()));
        };
        if !path.exists() {
            return Err(AtomicCoreError::NotFound("Export artifact".to_string()));
        }

        Ok(DownloadArtifact {
            path: path.clone(),
            filename: format!("{}-markdown.zip", archive_filename_stem(&job.db_name)),
        })
    }

    fn get_job(&self, job_id: &str) -> Result<Arc<ExportJob>, AtomicCoreError> {
        self.jobs
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?
            .get(job_id)
            .cloned()
            .ok_or_else(|| AtomicCoreError::NotFound(format!("Export job '{}'", job_id)))
    }

    async fn run_markdown_export(&self, job: Arc<ExportJob>, manager: Arc<DatabaseManager>) {
        let result = self
            .run_markdown_export_inner(Arc::clone(&job), manager)
            .await;

        match result {
            Ok((zip_path, atom_count, bytes_written)) => {
                job.mark_complete(zip_path, atom_count, bytes_written);
                self.schedule_cleanup(job.id.clone(), COMPLETED_JOB_RETENTION);
            }
            Err(e) if job.cancel.load(Ordering::SeqCst) => {
                job.mark_cancelled();
                job.remove_partial_artifacts();
                self.schedule_cleanup(job.id.clone(), FAILED_JOB_RETENTION);
                tracing::info!(job_id = %job.id, "markdown export cancelled");
                let _ = e;
            }
            Err(e) => {
                job.mark_failed(e.to_string());
                job.remove_partial_artifacts();
                self.schedule_cleanup(job.id.clone(), FAILED_JOB_RETENTION);
                tracing::warn!(job_id = %job.id, error = %e, "markdown export failed");
            }
        }
    }

    async fn run_markdown_export_inner(
        &self,
        job: Arc<ExportJob>,
        manager: Arc<DatabaseManager>,
    ) -> Result<(PathBuf, usize, u64), AtomicCoreError> {
        job.mark_running("loading database");
        let source_core = manager.get_core(&job.db_id).await?;

        let snapshot_path = job.dir.join("snapshot.db");
        let export_core = if source_core.database().is_some() {
            job.set_phase("snapshotting");
            source_core.create_sqlite_snapshot(&snapshot_path).await?;
            AtomicCore::open_for_server(&snapshot_path)?
        } else {
            source_core
        };

        let partial_zip_path = job.dir.join("markdown.zip.part");
        let final_zip_path = job.dir.join("markdown.zip");
        let export_zip_path = partial_zip_path.clone();
        let cancel_job = Arc::clone(&job);
        let progress_job = Arc::clone(&job);
        let handle = tokio::runtime::Handle::current();

        job.set_phase("writing zip");
        let export_result = tokio::task::spawn_blocking(move || {
            handle.block_on(async move {
                export_core
                    .export_markdown_zip_to_path(
                        &export_zip_path,
                        |progress: MarkdownExportProgress| {
                            progress_job.update_progress(progress);
                        },
                        || cancel_job.cancel.load(Ordering::SeqCst),
                    )
                    .await
            })
        })
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(format!("export task failed: {e}")))??;

        if job.cancel.load(Ordering::SeqCst) {
            return Err(AtomicCoreError::Conflict("Export cancelled".to_string()));
        }

        std::fs::rename(&partial_zip_path, &final_zip_path)?;
        remove_sqlite_snapshot_files(&snapshot_path);
        Ok((
            final_zip_path,
            export_result.atom_count,
            export_result.bytes_written,
        ))
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

impl ExportJob {
    fn new(id: String, db_id: String, db_name: String, dir: PathBuf) -> Self {
        let now = Utc::now();
        Self {
            id,
            db_id,
            db_name,
            dir,
            cancel: AtomicBool::new(false),
            state: Mutex::new(ExportJobState {
                status: ExportJobStatus::Queued,
                phase: "queued".to_string(),
                total_atoms: 0,
                processed_atoms: 0,
                bytes_written: 0,
                created_at: now,
                updated_at: now,
                completed_at: None,
                error: None,
                zip_path: None,
                download_token_hash: None,
                download_expires_at: None,
            }),
        }
    }

    fn response(&self, issue_download_token: bool) -> ExportJobResponse {
        let mut state = self.state.lock().expect("export job state poisoned");
        let mut download_path = None;
        if issue_download_token && state.status == ExportJobStatus::Complete {
            let token = new_download_token();
            let expires_at = Utc::now() + DOWNLOAD_TOKEN_TTL;
            state.download_token_hash = Some(token_hash(&token));
            state.download_expires_at = Some(expires_at);
            state.updated_at = Utc::now();
            download_path = Some(format!("/api/exports/{}/download?token={}", self.id, token));
        }

        ExportJobResponse {
            id: self.id.clone(),
            db_id: self.db_id.clone(),
            db_name: self.db_name.clone(),
            status: state.status.clone(),
            phase: state.phase.clone(),
            total_atoms: state.total_atoms,
            processed_atoms: state.processed_atoms,
            bytes_written: state.bytes_written,
            created_at: state.created_at.to_rfc3339(),
            updated_at: state.updated_at.to_rfc3339(),
            completed_at: state.completed_at.map(|dt| dt.to_rfc3339()),
            error: state.error.clone(),
            download_path,
            download_expires_at: state.download_expires_at.map(|dt| dt.to_rfc3339()),
        }
    }

    fn status(&self) -> ExportJobStatus {
        self.state
            .lock()
            .expect("export job state poisoned")
            .status
            .clone()
    }

    fn mark_running(&self, phase: &str) {
        let mut state = self.state.lock().expect("export job state poisoned");
        state.status = ExportJobStatus::Running;
        state.phase = phase.to_string();
        state.updated_at = Utc::now();
    }

    fn set_phase(&self, phase: &str) {
        let mut state = self.state.lock().expect("export job state poisoned");
        state.phase = phase.to_string();
        state.updated_at = Utc::now();
    }

    fn update_progress(&self, progress: MarkdownExportProgress) {
        let mut state = self.state.lock().expect("export job state poisoned");
        state.total_atoms = progress.total_atoms;
        state.processed_atoms = progress.processed_atoms;
        state.bytes_written = progress.bytes_written;
        state.updated_at = Utc::now();
    }

    fn mark_complete(&self, path: PathBuf, atom_count: usize, bytes_written: u64) {
        let mut state = self.state.lock().expect("export job state poisoned");
        let now = Utc::now();
        state.status = ExportJobStatus::Complete;
        state.phase = "complete".to_string();
        state.total_atoms = atom_count;
        state.processed_atoms = atom_count;
        state.bytes_written = bytes_written;
        state.completed_at = Some(now);
        state.updated_at = now;
        state.error = None;
        state.zip_path = Some(path);
    }

    fn mark_failed(&self, error: String) {
        let mut state = self.state.lock().expect("export job state poisoned");
        let now = Utc::now();
        state.status = ExportJobStatus::Failed;
        state.phase = "failed".to_string();
        state.completed_at = Some(now);
        state.updated_at = now;
        state.error = Some(error);
    }

    fn mark_cancelled(&self) {
        let mut state = self.state.lock().expect("export job state poisoned");
        let now = Utc::now();
        state.status = ExportJobStatus::Cancelled;
        state.phase = "cancelled".to_string();
        state.completed_at = Some(now);
        state.updated_at = now;
    }

    fn remove_partial_artifacts(&self) {
        remove_sqlite_snapshot_files(self.dir.join("snapshot.db"));
        remove_file_if_exists(self.dir.join("markdown.zip.part"));
    }

    fn remove_artifacts(&self) {
        if self.dir.exists() {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

fn remove_file_if_exists(path: impl AsRef<Path>) {
    let path = path.as_ref();
    if path.exists() {
        let _ = std::fs::remove_file(path);
    }
}

fn remove_sqlite_snapshot_files(path: impl AsRef<Path>) {
    let path = path.as_ref();
    remove_file_if_exists(path);
    remove_file_if_exists(path.with_extension("db-wal"));
    remove_file_if_exists(path.with_extension("db-shm"));
}

fn new_download_token() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

fn token_hash(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    URL_SAFE_NO_PAD.encode(digest)
}

fn archive_filename_stem(name: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;

    for ch in name.chars() {
        let normalized = if ch.is_ascii_alphanumeric() {
            Some(ch.to_ascii_lowercase())
        } else if matches!(ch, ' ' | '-' | '_' | '.') {
            Some('-')
        } else {
            None
        };

        if let Some(ch) = normalized {
            if ch == '-' {
                if !last_dash && !out.is_empty() {
                    out.push('-');
                    last_dash = true;
                }
            } else {
                out.push(ch);
                last_dash = false;
            }
        }
    }

    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "atomic-database".to_string()
    } else {
        trimmed.to_string()
    }
}
