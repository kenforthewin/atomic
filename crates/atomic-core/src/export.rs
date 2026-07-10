//! Export helpers for database-level portable archives.

use crate::models::{AtomWithTags, Tag, TagWithCount};
use crate::{AtomicCore, AtomicCoreError, ListAtomsParams, SortField, SortOrder, SourceFilter};
use chrono::Utc;
use serde::Serialize;
use std::collections::HashMap;
use std::fs::File;
use std::io::{Seek, Write};
use std::path::Path;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

#[derive(Debug, Clone, Copy, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum MarkdownArchiveFormat {
    Zip,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct MarkdownExportProgress {
    pub total_atoms: usize,
    pub processed_atoms: usize,
    pub bytes_written: u64,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct MarkdownExportResult {
    pub format: MarkdownArchiveFormat,
    pub atom_count: usize,
    pub bytes_written: u64,
}

impl AtomicCore {
    /// Create a consistent SQLite snapshot using `VACUUM INTO`.
    ///
    /// Returns `Ok(false)` for non-SQLite storage backends.
    pub async fn create_sqlite_snapshot(
        &self,
        snapshot_path: impl AsRef<Path>,
    ) -> Result<bool, AtomicCoreError> {
        let Some(db) = self.database() else {
            return Ok(false);
        };

        let source_path = db.db_path.clone();
        let snapshot_path = snapshot_path.as_ref().to_path_buf();
        tokio::task::spawn_blocking(move || -> Result<bool, AtomicCoreError> {
            if let Some(parent) = snapshot_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            if snapshot_path.exists() {
                std::fs::remove_file(&snapshot_path)?;
            }

            let conn = rusqlite::Connection::open(source_path)?;
            let snapshot = snapshot_path.to_string_lossy().to_string();
            conn.execute("VACUUM main INTO ?1", [snapshot])?;
            Ok(true)
        })
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(format!("snapshot task failed: {e}")))?
    }

    /// Create a slimmed SQLite snapshot for cloud migration.
    ///
    /// Like [`Self::create_sqlite_snapshot`], but strips everything the
    /// migration deliberately leaves behind — chunk embeddings, tag
    /// centroids, the vector index tables, semantic edges, clusters, and
    /// transient pipeline/scheduler state — so the upload is a fraction of
    /// the source size (embeddings dominate the file). Chunk text stays:
    /// the destination serves keyword search from it while embeddings
    /// rebuild.
    ///
    /// Returns `Ok(false)` for non-SQLite storage backends.
    pub async fn create_migration_snapshot(
        &self,
        snapshot_path: impl AsRef<Path>,
    ) -> Result<bool, AtomicCoreError> {
        let snapshot_path = snapshot_path.as_ref().to_path_buf();
        if !self.create_sqlite_snapshot(&snapshot_path).await? {
            return Ok(false);
        }

        tokio::task::spawn_blocking(move || -> Result<bool, AtomicCoreError> {
            // sqlite-vec is process-globally registered (this core already
            // opened a Database), so dropping the vec0 virtual tables works
            // on this plain connection.
            let conn = rusqlite::Connection::open(&snapshot_path)?;
            conn.execute_batch(
                "UPDATE atom_chunks SET embedding = NULL;
                 DELETE FROM tag_embeddings;
                 DELETE FROM semantic_edges;
                 DELETE FROM atom_clusters;
                 DELETE FROM atom_pipeline_jobs;
                 DELETE FROM task_runs;
                 DROP TABLE IF EXISTS vec_chunks;
                 DROP TABLE IF EXISTS vec_tags;
                 VACUUM;",
            )?;
            Ok(true)
        })
        .await
        .map_err(|e| {
            AtomicCoreError::DatabaseOperation(format!("snapshot slim task failed: {e}"))
        })?
    }

    /// Export every atom in this database as markdown files in a ZIP archive.
    ///
    /// Each atom becomes `atoms/<title>--<short-id>.md`. The markdown body is
    /// preserved exactly after YAML-like front matter containing source,
    /// timestamps, and tag paths.
    pub async fn export_markdown_zip_to_path<F, C>(
        &self,
        zip_path: impl AsRef<Path>,
        mut on_progress: F,
        is_cancelled: C,
    ) -> Result<MarkdownExportResult, AtomicCoreError>
    where
        F: FnMut(MarkdownExportProgress) + Send,
        C: Fn() -> bool + Send,
    {
        if is_cancelled() {
            return Err(AtomicCoreError::Conflict("Export cancelled".to_string()));
        }

        let total_atoms = self.count_atoms().await?.max(0) as usize;
        let tags = self.get_all_tags().await?;
        let tag_paths = build_tag_paths(&tags);

        let bytes_written = Arc::new(AtomicU64::new(0));
        let file = File::create(zip_path)?;
        let writer = CountingWriter::new(file, Arc::clone(&bytes_written));
        let mut zip = ZipWriter::new(writer);
        let options = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Deflated)
            .unix_permissions(0o644);

        zip.start_file("manifest.json", options).map_err(zip_err)?;
        let manifest = serde_json::json!({
            "format": "atomic_markdown_zip",
            "version": 1,
            "exported_at": Utc::now().to_rfc3339(),
            "atom_count": total_atoms,
        });
        serde_json::to_writer_pretty(&mut zip, &manifest)?;
        zip.write_all(b"\n")?;

        let mut used_paths: HashMap<String, usize> = HashMap::new();
        let mut processed_atoms = 0usize;
        let mut cursor = None;
        let mut cursor_id = None;

        loop {
            if is_cancelled() {
                return Err(AtomicCoreError::Conflict("Export cancelled".to_string()));
            }

            let page = self
                .list_atoms(
                    &ListAtomsParams {
                        tag_id: None,
                        limit: 250,
                        offset: 0,
                        cursor,
                        cursor_id,
                        source_filter: SourceFilter::All,
                        source_value: None,
                        sort_by: SortField::Created,
                        sort_order: SortOrder::Asc,
                    },
                    // Export everything the user has — captured notes plus
                    // any agent-emitted findings — so a full export is a
                    // faithful snapshot of what's in the DB.
                    &crate::models::KindFilter::All,
                )
                .await?;

            if page.atoms.is_empty() {
                break;
            }

            for summary in &page.atoms {
                if is_cancelled() {
                    return Err(AtomicCoreError::Conflict("Export cancelled".to_string()));
                }

                let Some(atom) = self.get_atom(&summary.id).await? else {
                    continue;
                };
                let markdown = atom_markdown(&atom, &tag_paths);
                let path = unique_atom_path(&atom, &mut used_paths);
                zip.start_file(path, options).map_err(zip_err)?;
                zip.write_all(markdown.as_bytes())?;

                processed_atoms += 1;
                on_progress(MarkdownExportProgress {
                    total_atoms,
                    processed_atoms,
                    bytes_written: bytes_written.load(Ordering::Relaxed),
                });
            }

            match (page.next_cursor, page.next_cursor_id) {
                (Some(next_cursor), Some(next_cursor_id)) => {
                    cursor = Some(next_cursor);
                    cursor_id = Some(next_cursor_id);
                }
                _ => break,
            }
        }

        let writer = zip.finish().map_err(zip_err)?;
        Ok(MarkdownExportResult {
            format: MarkdownArchiveFormat::Zip,
            atom_count: processed_atoms,
            bytes_written: writer.bytes_written(),
        })
    }
}

fn zip_err(error: zip::result::ZipError) -> AtomicCoreError {
    AtomicCoreError::DatabaseOperation(format!("ZIP export error: {error}"))
}

fn atom_markdown(atom: &AtomWithTags, tag_paths: &HashMap<String, String>) -> String {
    let mut out = String::new();
    out.push_str("---\n");
    push_yaml_string(&mut out, "id", &atom.atom.id);
    push_yaml_string(&mut out, "title", &atom.atom.title);
    push_yaml_optional_string(&mut out, "source_url", atom.atom.source_url.as_deref());
    push_yaml_optional_string(&mut out, "source", atom.atom.source.as_deref());
    push_yaml_optional_string(&mut out, "published_at", atom.atom.published_at.as_deref());
    push_yaml_string(&mut out, "created_at", &atom.atom.created_at);
    push_yaml_string(&mut out, "updated_at", &atom.atom.updated_at);
    let mut tags = atom
        .tags
        .iter()
        .map(|tag| {
            tag_paths
                .get(&tag.id)
                .cloned()
                .unwrap_or_else(|| tag.name.clone())
        })
        .collect::<Vec<_>>();
    tags.sort_by_key(|name| name.to_lowercase());

    if tags.is_empty() {
        out.push_str("tags: []\n");
    } else {
        out.push_str("tags:\n");
        for tag in tags {
            out.push_str("  - ");
            out.push_str(&yaml_quoted(&tag));
            out.push('\n');
        }
    }

    out.push_str("---\n\n");
    out.push_str(&atom.atom.content);
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

fn push_yaml_string(out: &mut String, key: &str, value: &str) {
    out.push_str(key);
    out.push_str(": ");
    out.push_str(&yaml_quoted(value));
    out.push('\n');
}

fn push_yaml_optional_string(out: &mut String, key: &str, value: Option<&str>) {
    match value {
        Some(value) => push_yaml_string(out, key, value),
        None => {
            out.push_str(key);
            out.push_str(": null\n");
        }
    }
}

fn yaml_quoted(value: &str) -> String {
    let escaped = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r");
    format!("\"{escaped}\"")
}

fn build_tag_paths(tags: &[TagWithCount]) -> HashMap<String, String> {
    let mut by_id = HashMap::new();
    flatten_tags(tags, &mut by_id);

    by_id
        .keys()
        .map(|id| (id.clone(), tag_path(id, &by_id)))
        .collect()
}

fn flatten_tags(tags: &[TagWithCount], by_id: &mut HashMap<String, Tag>) {
    for tag in tags {
        by_id.insert(tag.tag.id.clone(), tag.tag.clone());
        flatten_tags(&tag.children, by_id);
    }
}

fn tag_path(id: &str, by_id: &HashMap<String, Tag>) -> String {
    let mut names = Vec::new();
    let mut current = Some(id);
    let mut guard = 0;

    while let Some(tag_id) = current {
        guard += 1;
        if guard > 64 {
            break;
        }
        let Some(tag) = by_id.get(tag_id) else {
            break;
        };
        names.push(tag.name.clone());
        current = tag.parent_id.as_deref();
    }

    names.reverse();
    names.join("/")
}

fn unique_atom_path(atom: &AtomWithTags, used_paths: &mut HashMap<String, usize>) -> String {
    let mut base = sanitize_path_component(&atom.atom.title);
    if base.is_empty() {
        base = "untitled".to_string();
    }
    let short_id = atom.atom.id.chars().take(8).collect::<String>();
    let stem = format!("{base}--{short_id}");
    let count = used_paths.entry(stem.clone()).or_insert(0);
    let suffix = if *count == 0 {
        String::new()
    } else {
        format!("-{}", *count + 1)
    };
    *count += 1;
    format!("atoms/{stem}{suffix}.md")
}

fn sanitize_path_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len().min(80));
    let mut last_dash = false;

    for ch in value.chars() {
        if out.len() >= 80 {
            break;
        }

        let normalized = if ch.is_ascii_alphanumeric() {
            Some(ch.to_ascii_lowercase())
        } else if matches!(ch, ' ' | '-' | '_' | '.' | ':') {
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

    out.trim_matches('-').to_string()
}

struct CountingWriter<W> {
    inner: W,
    bytes_written: Arc<AtomicU64>,
}

impl<W> CountingWriter<W> {
    fn new(inner: W, bytes_written: Arc<AtomicU64>) -> Self {
        Self {
            inner,
            bytes_written,
        }
    }

    fn bytes_written(&self) -> u64 {
        self.bytes_written.load(Ordering::Relaxed)
    }
}

impl<W: Write> Write for CountingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let written = self.inner.write(buf)?;
        self.bytes_written
            .fetch_add(written as u64, Ordering::Relaxed);
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

impl<W: Seek> Seek for CountingWriter<W> {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        self.inner.seek(pos)
    }

    fn stream_position(&mut self) -> std::io::Result<u64> {
        self.inner.stream_position()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CreateAtomRequest;
    use tempfile::tempdir;

    #[tokio::test]
    async fn export_markdown_zip_contains_markdown_files_with_metadata() {
        let dir = tempdir().unwrap();
        let core = AtomicCore::open_or_create(dir.path().join("test.db")).unwrap();
        let parent = core.create_tag("Topics", None).await.unwrap();
        let child = core.create_tag("Rust", Some(&parent.id)).await.unwrap();

        let atom = core
            .create_atom(
                CreateAtomRequest {
                    content: "# Rust note\n\nA note about ownership.".to_string(),
                    source_url: Some("https://example.com/rust".to_string()),
                    tag_ids: vec![child.id],
                    ..Default::default()
                },
                |_| {},
            )
            .await
            .unwrap()
            .unwrap();

        let zip_path = dir.path().join("export.zip");
        let mut progress = Vec::new();
        let archive = core
            .export_markdown_zip_to_path(&zip_path, |p| progress.push(p.processed_atoms), || false)
            .await
            .unwrap();

        let file = File::open(&zip_path).unwrap();
        let mut zip = zip::ZipArchive::new(file).unwrap();
        let mut manifest = String::new();
        let mut note = String::new();
        assert!(zip.by_name("README.md").is_err());
        std::io::Read::read_to_string(&mut zip.by_name("manifest.json").unwrap(), &mut manifest)
            .unwrap();
        let note_path = format!("atoms/rust-note--{}.md", &atom.atom.id[..8]);
        std::io::Read::read_to_string(&mut zip.by_name(&note_path).unwrap(), &mut note).unwrap();

        assert_eq!(archive.atom_count, 1);
        assert!(archive.bytes_written > 0);
        assert_eq!(progress, vec![1]);
        assert!(manifest.contains("\"format\": \"atomic_markdown_zip\""));
        assert!(note.contains("id: "));
        assert!(note.contains("source_url: \"https://example.com/rust\""));
        assert!(note.contains("- \"Topics/Rust\""));
        assert!(note.contains("# Rust note\n\nA note about ownership."));
    }

    #[test]
    fn sanitize_path_component_keeps_paths_portable() {
        assert_eq!(sanitize_path_component("A/B: C? * D.md"), "ab-c-d-md");
        assert_eq!(sanitize_path_component("   "), "");
    }

    #[tokio::test]
    async fn migration_snapshot_strips_derived_data_but_keeps_content() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let core = AtomicCore::open_or_create(&db_path).unwrap();

        // Seed directly so the fixture is deterministic (no pipeline race):
        // an atom with an embedded chunk, a tag centroid, and a semantic edge.
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "INSERT INTO atoms (id, content, created_at, updated_at)
                 VALUES ('a1', 'ownership note', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z'),
                        ('a2', 'borrowing note', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z');
                 INSERT INTO atom_chunks (id, atom_id, chunk_index, content, embedding)
                 VALUES ('c1', 'a1', 0, 'ownership note', x'0011223344556677');
                 INSERT INTO tags (id, name, created_at) VALUES ('t1', 'Rust', '2026-01-01T00:00:00Z');
                 INSERT INTO tag_embeddings (tag_id, embedding, atom_count, updated_at)
                 VALUES ('t1', x'0011223344556677', 1, '2026-01-01T00:00:00Z');
                 INSERT INTO semantic_edges (id, source_atom_id, target_atom_id, similarity_score, created_at)
                 VALUES ('e1', 'a1', 'a2', 0.8, '2026-01-01T00:00:00Z');",
            )
            .unwrap();
        }

        let snapshot_path = dir.path().join("migration-snapshot.db");
        let created = core
            .create_migration_snapshot(&snapshot_path)
            .await
            .unwrap();
        assert!(created);

        let snap = rusqlite::Connection::open(&snapshot_path).unwrap();
        let chunks: i64 = snap
            .query_row("SELECT COUNT(*) FROM atom_chunks", [], |r| r.get(0))
            .unwrap();
        let stripped: i64 = snap
            .query_row(
                "SELECT COUNT(*) FROM atom_chunks WHERE embedding IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            (chunks, stripped),
            (1, 1),
            "chunk text kept, embedding nulled"
        );

        for table in ["tag_embeddings", "semantic_edges", "atom_clusters"] {
            let n: i64 = snap
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
                .unwrap();
            assert_eq!(n, 0, "{table} should be emptied");
        }
        let vec_tables: i64 = snap
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name IN ('vec_chunks', 'vec_tags')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(vec_tables, 0, "vector index tables dropped");
        let atoms: i64 = snap
            .query_row("SELECT COUNT(*) FROM atoms", [], |r| r.get(0))
            .unwrap();
        assert_eq!(atoms, 2, "user content intact");
    }
}
