use super::PostgresStorage;
use crate::error::AtomicCoreError;
use crate::models::*;
use crate::storage::traits::*;
use crate::{
    atom_links, extract_title_and_snippet, parse_source, CreateAtomRequest, ListAtomsParams,
    UpdateAtomRequest,
};
use async_trait::async_trait;

fn escape_like_pattern(input: &str) -> String {
    let mut escaped = String::with_capacity(input.len());
    for ch in input.chars() {
        if matches!(ch, '\\' | '%' | '_') {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

impl PostgresStorage {
    /// Fetch tags for a single atom.
    async fn tags_for_atom(&self, atom_id: &str) -> StorageResult<Vec<Tag>> {
        let rows: Vec<(String, String, Option<String>, String, bool, String)> = sqlx::query_as(
            "SELECT t.id, t.name, t.parent_id, t.created_at, t.is_autotag_target, t.autotag_description
             FROM tags t
             JOIN atom_tags at ON t.id = at.tag_id
             WHERE at.atom_id = $1 AND at.db_id = $2
             ORDER BY t.name",
        )
        .bind(atom_id)
        .bind(&self.db_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        Ok(rows
            .into_iter()
            .map(
                |(id, name, parent_id, created_at, is_autotag_target, autotag_description)| Tag {
                    id,
                    name,
                    parent_id,
                    created_at,
                    is_autotag_target,
                    autotag_description,
                },
            )
            .collect())
    }

    /// Batch-fetch tags for a set of atom IDs. Returns a map of atom_id -> Vec<Tag>.
    async fn tags_for_atom_ids(
        &self,
        atom_ids: &[String],
    ) -> StorageResult<std::collections::HashMap<String, Vec<Tag>>> {
        use std::collections::HashMap;

        if atom_ids.is_empty() {
            return Ok(HashMap::new());
        }

        // Build dynamic placeholders $1, $2, ... (reserve $1 for db_id)
        let placeholders: Vec<String> = (2..=atom_ids.len() + 1)
            .map(|i| format!("${}", i))
            .collect();
        let sql = format!(
            "SELECT at.atom_id, t.id, t.name, t.parent_id, t.created_at, t.is_autotag_target, t.autotag_description
             FROM atom_tags at
             JOIN tags t ON t.id = at.tag_id
             WHERE at.db_id = $1 AND at.atom_id IN ({})
             ORDER BY t.name",
            placeholders.join(", ")
        );

        let mut query = sqlx::query_as::<
            _,
            (String, String, String, Option<String>, String, bool, String),
        >(&sql);
        query = query.bind(&self.db_id);
        for id in atom_ids {
            query = query.bind(id);
        }

        let rows = query
            .fetch_all(&self.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        let mut map: HashMap<String, Vec<Tag>> = HashMap::new();
        for (
            atom_id,
            tag_id,
            name,
            parent_id,
            created_at,
            is_autotag_target,
            autotag_description,
        ) in rows
        {
            map.entry(atom_id).or_default().push(Tag {
                id: tag_id,
                name,
                parent_id,
                created_at,
                is_autotag_target,
                autotag_description,
            });
        }

        Ok(map)
    }

    async fn replace_atom_links_for_content(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        source_atom_id: &str,
        content: &str,
        now: &str,
    ) -> StorageResult<()> {
        sqlx::query("DELETE FROM atom_links WHERE source_atom_id = $1 AND db_id = $2")
            .bind(source_atom_id)
            .bind(&self.db_id)
            .execute(&mut **tx)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        for token in atom_links::extract_atom_link_tokens(content) {
            let is_atom_id = atom_links::is_uuid_target(&token.raw_target);
            let target_exists = if is_atom_id {
                let exists: bool = sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM atoms WHERE id = $1 AND db_id = $2)",
                )
                .bind(&token.raw_target)
                .bind(&self.db_id)
                .fetch_one(&mut **tx)
                .await
                .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
                exists
            } else {
                false
            };
            let target_atom_id = target_exists.then(|| token.raw_target.clone());
            let target_kind = if is_atom_id { "atom_id" } else { "text" };
            let status = if target_exists {
                "resolved"
            } else if is_atom_id {
                "missing"
            } else {
                "unresolved"
            };

            sqlx::query(
                "INSERT INTO atom_links (
                    id, source_atom_id, target_atom_id, raw_target, label,
                    target_kind, status, start_offset, end_offset, created_at, updated_at, db_id
                 ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
            )
            .bind(uuid::Uuid::new_v4().to_string())
            .bind(source_atom_id)
            .bind(&target_atom_id)
            .bind(&token.raw_target)
            .bind(&token.label)
            .bind(target_kind)
            .bind(status)
            .bind(token.start_offset as i32)
            .bind(token.end_offset as i32)
            .bind(now)
            .bind(now)
            .bind(&self.db_id)
            .execute(&mut **tx)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        }

        Ok(())
    }

    /// Build an Atom from a full row tuple.
    fn atom_from_tuple(
        row: (
            String,         // id
            String,         // content
            String,         // title
            String,         // snippet
            Option<String>, // source_url
            Option<String>, // source
            Option<String>, // published_at
            String,         // created_at
            String,         // updated_at
            String,         // embedding_status
            String,         // tagging_status
            Option<String>, // embedding_error
            Option<String>, // tagging_error
            String,         // kind
        ),
    ) -> Atom {
        let kind = row
            .13
            .parse::<crate::models::AtomKind>()
            .unwrap_or(crate::models::AtomKind::Captured);
        Atom {
            id: row.0,
            content: row.1,
            title: row.2,
            snippet: row.3,
            source_url: row.4,
            source: row.5,
            published_at: row.6,
            created_at: row.7,
            updated_at: row.8,
            embedding_status: row.9,
            tagging_status: row.10,
            embedding_error: row.11,
            tagging_error: row.12,
            kind,
        }
    }
}

#[async_trait]
impl AtomStore for PostgresStorage {
    async fn get_all_atoms(&self) -> StorageResult<Vec<AtomWithTags>> {
        let rows: Vec<(
            String,
            String,
            String,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            String,
            String,
            String,
            String,
            Option<String>,
            Option<String>,
            String,
        )> = sqlx::query_as(
            "SELECT id, content, title, snippet, source_url, source, published_at,
                    created_at, updated_at,
                    COALESCE(embedding_status, 'pending'),
                    COALESCE(tagging_status, 'pending'),
                    embedding_error, tagging_error,
                    COALESCE(kind, 'captured')
             FROM atoms WHERE db_id = $1 ORDER BY updated_at DESC",
        )
        .bind(&self.db_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        let atom_ids: Vec<String> = rows.iter().map(|r| r.0.clone()).collect();
        let tag_map = self.tags_for_atom_ids(&atom_ids).await?;

        let result = rows
            .into_iter()
            .map(|row| {
                let id = row.0.clone();
                let atom = Self::atom_from_tuple(row);
                let tags = tag_map.get(&id).cloned().unwrap_or_default();
                AtomWithTags { atom, tags }
            })
            .collect();

        Ok(result)
    }

    async fn count_atoms(&self) -> StorageResult<i32> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM atoms WHERE db_id = $1")
            .bind(&self.db_id)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        Ok(count as i32)
    }

    async fn get_atom(&self, id: &str) -> StorageResult<Option<AtomWithTags>> {
        let row: Option<(
            String,
            String,
            String,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            String,
            String,
            String,
            String,
            Option<String>,
            Option<String>,
            String,
        )> = sqlx::query_as(
            "SELECT id, content, title, snippet, source_url, source, published_at,
                    created_at, updated_at,
                    COALESCE(embedding_status, 'pending'),
                    COALESCE(tagging_status, 'pending'),
                    embedding_error, tagging_error,
                    COALESCE(kind, 'captured')
             FROM atoms WHERE id = $1 AND db_id = $2",
        )
        .bind(id)
        .bind(&self.db_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        match row {
            Some(r) => {
                let atom = Self::atom_from_tuple(r);
                let tags = self.tags_for_atom(id).await?;
                Ok(Some(AtomWithTags { atom, tags }))
            }
            None => Ok(None),
        }
    }

    async fn insert_atom(
        &self,
        id: &str,
        request: &CreateAtomRequest,
        created_at: &str,
    ) -> StorageResult<AtomWithTags> {
        let (title, snippet) = extract_title_and_snippet(&request.content, 300);
        let source = request.source_url.as_deref().map(parse_source);
        let embedding_status = "pending";
        let tagging_status = "pending";

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        sqlx::query(
            "INSERT INTO atoms (id, content, source_url, source, published_at, created_at, updated_at, embedding_status, tagging_status, title, snippet, db_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)"
        )
        .bind(id)
        .bind(&request.content)
        .bind(&request.source_url)
        .bind(&source)
        .bind(&request.published_at)
        .bind(created_at)
        .bind(created_at)
        .bind(embedding_status)
        .bind(tagging_status)
        .bind(&title)
        .bind(&snippet)
        .bind(&self.db_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        self.replace_atom_links_for_content(&mut tx, id, &request.content, created_at)
            .await?;

        for tag_id in &request.tag_ids {
            sqlx::query("INSERT INTO atom_tags (atom_id, tag_id, db_id, source) VALUES ($1, $2, $3, 'manual')")
                .bind(id)
                .bind(tag_id)
                .bind(&self.db_id)
                .execute(&mut *tx)
                .await
                .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        }

        tx.commit()
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        let tags = self.tags_for_atom(id).await?;

        let atom = Atom {
            id: id.to_string(),
            content: request.content.clone(),
            title,
            snippet,
            source_url: request.source_url.clone(),
            source,
            published_at: request.published_at.clone(),
            created_at: created_at.to_string(),
            updated_at: created_at.to_string(),
            embedding_status: embedding_status.to_string(),
            tagging_status: tagging_status.to_string(),
            embedding_error: None,
            tagging_error: None,
            kind: crate::models::AtomKind::Captured,
        };

        Ok(AtomWithTags { atom, tags })
    }

    async fn insert_atoms_bulk(
        &self,
        atoms: &[(String, CreateAtomRequest, String)],
    ) -> StorageResult<Vec<AtomWithTags>> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        let mut atoms_with_tags: Vec<AtomWithTags> = Vec::with_capacity(atoms.len());

        for (id, request, created_at) in atoms {
            let (title, snippet) = extract_title_and_snippet(&request.content, 300);
            let source = request.source_url.as_deref().map(parse_source);

            sqlx::query(
                "INSERT INTO atoms (id, content, source_url, source, published_at, created_at, updated_at, embedding_status, tagging_status, title, snippet, db_id)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)"
            )
            .bind(id)
            .bind(&request.content)
            .bind(&request.source_url)
            .bind(&source)
            .bind(&request.published_at)
            .bind(created_at)
            .bind(created_at)
            .bind("pending")
            .bind("pending")
            .bind(&title)
            .bind(&snippet)
            .bind(&self.db_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

            for tag_id in &request.tag_ids {
                sqlx::query("INSERT INTO atom_tags (atom_id, tag_id, db_id, source) VALUES ($1, $2, $3, 'manual')")
                    .bind(id)
                    .bind(tag_id)
                    .bind(&self.db_id)
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
            }

            let atom = Atom {
                id: id.clone(),
                content: request.content.clone(),
                title,
                snippet,
                source_url: request.source_url.clone(),
                source,
                published_at: request.published_at.clone(),
                created_at: created_at.clone(),
                updated_at: created_at.clone(),
                embedding_status: "pending".to_string(),
                tagging_status: "pending".to_string(),
                embedding_error: None,
                tagging_error: None,
                kind: crate::models::AtomKind::Captured,
            };

            atoms_with_tags.push(AtomWithTags { atom, tags: vec![] });
        }

        for (id, request, created_at) in atoms {
            self.replace_atom_links_for_content(&mut tx, id, &request.content, created_at)
                .await?;
        }

        tx.commit()
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        // Batch-resolve tags for all created atoms
        let atom_ids: Vec<String> = atoms_with_tags.iter().map(|a| a.atom.id.clone()).collect();
        let tag_map = self.tags_for_atom_ids(&atom_ids).await?;
        for awt in &mut atoms_with_tags {
            awt.tags = tag_map.get(&awt.atom.id).cloned().unwrap_or_default();
        }

        Ok(atoms_with_tags)
    }

    async fn update_atom(
        &self,
        id: &str,
        request: &UpdateAtomRequest,
        updated_at: &str,
    ) -> StorageResult<AtomWithTags> {
        let (title, snippet) = extract_title_and_snippet(&request.content, 300);
        let source = request.source_url.as_deref().map(parse_source);
        let embedding_status = "pending";

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        sqlx::query(
            "UPDATE atoms
             SET content = $1,
                 source_url = $2,
                 source = $3,
                 published_at = $4,
                 updated_at = $5,
                 embedding_status = $6,
                 tagging_status = $7,
                 embedding_error = NULL,
                 tagging_error = NULL,
                 title = $8,
                 snippet = $9
             WHERE id = $10 AND db_id = $11",
        )
        .bind(&request.content)
        .bind(&request.source_url)
        .bind(&source)
        .bind(&request.published_at)
        .bind(updated_at)
        .bind(embedding_status)
        .bind("pending")
        .bind(&title)
        .bind(&snippet)
        .bind(id)
        .bind(&self.db_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        self.replace_atom_links_for_content(&mut tx, id, &request.content, updated_at)
            .await?;

        if let Some(ref tag_ids) = request.tag_ids {
            sqlx::query("DELETE FROM atom_tags WHERE atom_id = $1 AND db_id = $2")
                .bind(id)
                .bind(&self.db_id)
                .execute(&mut *tx)
                .await
                .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

            for tag_id in tag_ids {
                sqlx::query("INSERT INTO atom_tags (atom_id, tag_id, db_id, source) VALUES ($1, $2, $3, 'manual')")
                    .bind(id)
                    .bind(tag_id)
                    .bind(&self.db_id)
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
            }
        }

        tx.commit()
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        // Re-fetch the atom
        let row: (
            String,
            String,
            String,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            String,
            String,
            String,
            String,
            Option<String>,
            Option<String>,
            String,
        ) = sqlx::query_as(
            "SELECT id, content, title, snippet, source_url, source, published_at,
                    created_at, updated_at,
                    COALESCE(embedding_status, 'pending'),
                    COALESCE(tagging_status, 'pending'),
                    embedding_error, tagging_error,
                    COALESCE(kind, 'captured')
             FROM atoms WHERE id = $1 AND db_id = $2",
        )
        .bind(id)
        .bind(&self.db_id)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        let atom = Self::atom_from_tuple(row);
        let tags = self.tags_for_atom(id).await?;

        Ok(AtomWithTags { atom, tags })
    }

    async fn update_atom_if_unchanged(
        &self,
        id: &str,
        request: &UpdateAtomRequest,
        updated_at: &str,
        expected_updated_at: &str,
    ) -> StorageResult<AtomWithTags> {
        let (title, snippet) = extract_title_and_snippet(&request.content, 300);
        let source = request.source_url.as_deref().map(parse_source);

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        let result = sqlx::query(
            "UPDATE atoms
             SET content = $1,
                 source_url = $2,
                 source = $3,
                 published_at = $4,
                 updated_at = $5,
                 embedding_status = 'pending',
                 tagging_status = 'pending',
                 embedding_error = NULL,
                 tagging_error = NULL,
                 title = $6,
                 snippet = $7
             WHERE id = $8 AND db_id = $9 AND updated_at = $10",
        )
        .bind(&request.content)
        .bind(&request.source_url)
        .bind(&source)
        .bind(&request.published_at)
        .bind(updated_at)
        .bind(&title)
        .bind(&snippet)
        .bind(id)
        .bind(&self.db_id)
        .bind(expected_updated_at)
        .execute(&mut *tx)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        if result.rows_affected() == 0 {
            let current: Option<String> =
                sqlx::query_scalar("SELECT updated_at FROM atoms WHERE id = $1 AND db_id = $2")
                    .bind(id)
                    .bind(&self.db_id)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

            return match current {
                Some(_) => Err(AtomicCoreError::Conflict(format!(
                    "Atom {} changed before edits could be saved; reload the atom and retry",
                    id
                ))),
                None => Err(AtomicCoreError::NotFound(format!("Atom {}", id))),
            };
        }

        self.replace_atom_links_for_content(&mut tx, id, &request.content, updated_at)
            .await?;

        if let Some(ref tag_ids) = request.tag_ids {
            sqlx::query("DELETE FROM atom_tags WHERE atom_id = $1 AND db_id = $2")
                .bind(id)
                .bind(&self.db_id)
                .execute(&mut *tx)
                .await
                .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

            for tag_id in tag_ids {
                sqlx::query("INSERT INTO atom_tags (atom_id, tag_id, db_id, source) VALUES ($1, $2, $3, 'manual')")
                    .bind(id)
                    .bind(tag_id)
                    .bind(&self.db_id)
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
            }
        }

        tx.commit()
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        let row: (
            String,
            String,
            String,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            String,
            String,
            String,
            String,
            Option<String>,
            Option<String>,
            String,
        ) = sqlx::query_as(
            "SELECT id, content, title, snippet, source_url, source, published_at,
                    created_at, updated_at,
                    COALESCE(embedding_status, 'pending'),
                    COALESCE(tagging_status, 'pending'),
                    embedding_error, tagging_error,
                    COALESCE(kind, 'captured')
             FROM atoms WHERE id = $1 AND db_id = $2",
        )
        .bind(id)
        .bind(&self.db_id)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        let atom = Self::atom_from_tuple(row);
        let tags = self.tags_for_atom(id).await?;

        Ok(AtomWithTags { atom, tags })
    }

    async fn update_atom_content_only(
        &self,
        id: &str,
        request: &UpdateAtomRequest,
        updated_at: &str,
    ) -> StorageResult<AtomWithTags> {
        let (title, snippet) = extract_title_and_snippet(&request.content, 300);
        let source = request.source_url.as_deref().map(parse_source);

        let existing_content: Option<String> =
            sqlx::query_scalar("SELECT content FROM atoms WHERE id = $1 AND db_id = $2")
                .bind(id)
                .bind(&self.db_id)
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        let content_changed = existing_content.as_deref() != Some(request.content.as_str());

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        if content_changed {
            sqlx::query(
                "UPDATE atoms
                 SET content = $1,
                     source_url = $2,
                     source = $3,
                     published_at = $4,
                     updated_at = $5,
                     embedding_status = 'pending',
                     tagging_status = 'pending',
                     embedding_error = NULL,
                     tagging_error = NULL,
                     title = $6,
                     snippet = $7
                 WHERE id = $8 AND db_id = $9",
            )
            .bind(&request.content)
            .bind(&request.source_url)
            .bind(&source)
            .bind(&request.published_at)
            .bind(updated_at)
            .bind(&title)
            .bind(&snippet)
            .bind(id)
            .bind(&self.db_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        } else {
            sqlx::query(
                "UPDATE atoms
                 SET content = $1,
                     source_url = $2,
                     source = $3,
                     published_at = $4,
                     updated_at = $5,
                     title = $6,
                     snippet = $7
                 WHERE id = $8 AND db_id = $9",
            )
            .bind(&request.content)
            .bind(&request.source_url)
            .bind(&source)
            .bind(&request.published_at)
            .bind(updated_at)
            .bind(&title)
            .bind(&snippet)
            .bind(id)
            .bind(&self.db_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        }

        if let Some(ref tag_ids) = request.tag_ids {
            sqlx::query("DELETE FROM atom_tags WHERE atom_id = $1 AND db_id = $2")
                .bind(id)
                .bind(&self.db_id)
                .execute(&mut *tx)
                .await
                .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

            for tag_id in tag_ids {
                sqlx::query("INSERT INTO atom_tags (atom_id, tag_id, db_id, source) VALUES ($1, $2, $3, 'manual')")
                    .bind(id)
                    .bind(tag_id)
                    .bind(&self.db_id)
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
            }
        }

        self.replace_atom_links_for_content(&mut tx, id, &request.content, updated_at)
            .await?;

        tx.commit()
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        let row: (
            String,
            String,
            String,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            String,
            String,
            String,
            String,
            Option<String>,
            Option<String>,
            String,
        ) = sqlx::query_as(
            "SELECT id, content, title, snippet, source_url, source, published_at,
                    created_at, updated_at,
                    COALESCE(embedding_status, 'pending'),
                    COALESCE(tagging_status, 'pending'),
                    embedding_error, tagging_error,
                    COALESCE(kind, 'captured')
             FROM atoms WHERE id = $1 AND db_id = $2",
        )
        .bind(id)
        .bind(&self.db_id)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        let atom = Self::atom_from_tuple(row);
        let tags = self.tags_for_atom(id).await?;

        Ok(AtomWithTags { atom, tags })
    }

    async fn delete_atom(&self, id: &str) -> StorageResult<()> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        let now = chrono::Utc::now().to_rfc3339();

        sqlx::query("DELETE FROM atom_tags WHERE atom_id = $1 AND db_id = $2")
            .bind(id)
            .bind(&self.db_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        sqlx::query("DELETE FROM atom_links WHERE source_atom_id = $1 AND db_id = $2")
            .bind(id)
            .bind(&self.db_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        sqlx::query(
            "UPDATE atom_links
             SET target_atom_id = NULL, status = 'missing', updated_at = $1
             WHERE target_atom_id = $2 AND db_id = $3",
        )
        .bind(&now)
        .bind(id)
        .bind(&self.db_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        sqlx::query("DELETE FROM atoms WHERE id = $1 AND db_id = $2")
            .bind(id)
            .bind(&self.db_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        tx.commit()
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        Ok(())
    }

    async fn get_atoms_by_tag(
        &self,
        tag_id: &str,
        kinds: &crate::models::KindFilter,
    ) -> StorageResult<Vec<AtomWithTags>> {
        let kind_predicate = kinds.postgres_predicate("a.kind", "$3");
        let kind_strings = kinds.kind_strings();
        let rows: Vec<(
            String,
            String,
            String,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            String,
            String,
            String,
            String,
            Option<String>,
            Option<String>,
            String,
        )> = {
            let sql = format!(
                "WITH RECURSIVE descendant_tags(id) AS (
                    SELECT id FROM tags WHERE id = $1 AND db_id = $2
                    UNION ALL
                    SELECT t.id FROM tags t
                    INNER JOIN descendant_tags dt ON t.parent_id = dt.id
                )
                SELECT a.id, a.content, a.title, a.snippet, a.source_url, a.source, a.published_at,
                       a.created_at, a.updated_at,
                       COALESCE(a.embedding_status, 'pending'),
                       COALESCE(a.tagging_status, 'pending'),
                       a.embedding_error, a.tagging_error,
                       COALESCE(a.kind, 'captured')
                FROM atom_tags at
                INNER JOIN atoms a ON a.id = at.atom_id
                WHERE at.tag_id IN (SELECT id FROM descendant_tags)
                  AND {kind_predicate}
                GROUP BY a.id, a.content, a.title, a.snippet, a.source_url, a.source,
                         a.published_at, a.created_at, a.updated_at,
                         a.embedding_status, a.tagging_status,
                         a.embedding_error, a.tagging_error, a.kind
                ORDER BY a.updated_at DESC"
            );
            let mut q = sqlx::query_as(&sql).bind(tag_id).bind(&self.db_id);
            if kinds.has_bind_value() {
                q = q.bind(kind_strings);
            }
            q.fetch_all(&self.pool)
                .await
                .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?
        };

        let atom_ids: Vec<String> = rows.iter().map(|r| r.0.clone()).collect();
        let tag_map = self.tags_for_atom_ids(&atom_ids).await?;

        let result = rows
            .into_iter()
            .map(|row| {
                let id = row.0.clone();
                let atom = Self::atom_from_tuple(row);
                let tags = tag_map.get(&id).cloned().unwrap_or_default();
                AtomWithTags { atom, tags }
            })
            .collect();

        Ok(result)
    }

    async fn get_atom_links(&self, atom_id: &str) -> StorageResult<Vec<AtomLink>> {
        let rows: Vec<(
            String,
            String,
            Option<String>,
            Option<String>,
            String,
            Option<String>,
            String,
            String,
            Option<i32>,
            Option<i32>,
            String,
            String,
        )> = sqlx::query_as(
            "SELECT al.id,
                    al.source_atom_id,
                    al.target_atom_id,
                    target.title,
                    al.raw_target,
                    al.label,
                    al.target_kind,
                    al.status,
                    al.start_offset,
                    al.end_offset,
                    al.created_at,
                    al.updated_at
             FROM atom_links al
             LEFT JOIN atoms target ON target.id = al.target_atom_id AND target.db_id = al.db_id
             WHERE al.source_atom_id = $1 AND al.db_id = $2
             ORDER BY al.start_offset ASC NULLS LAST, al.created_at ASC",
        )
        .bind(atom_id)
        .bind(&self.db_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        Ok(rows
            .into_iter()
            .map(
                |(
                    id,
                    source_atom_id,
                    target_atom_id,
                    target_title,
                    raw_target,
                    label,
                    target_kind,
                    status,
                    start_offset,
                    end_offset,
                    created_at,
                    updated_at,
                )| AtomLink {
                    id,
                    source_atom_id,
                    target_atom_id,
                    target_title,
                    raw_target,
                    label,
                    target_kind,
                    status,
                    start_offset,
                    end_offset,
                    created_at,
                    updated_at,
                },
            )
            .collect())
    }

    async fn suggest_atom_links(
        &self,
        query: &str,
        limit: i32,
    ) -> StorageResult<Vec<AtomLinkSuggestion>> {
        let query = query.trim();

        let rows: Vec<(String, String, String, String)> = if query.is_empty() {
            sqlx::query_as(
                "SELECT id, title, snippet, updated_at
                 FROM atoms
                 WHERE db_id = $1 AND BTRIM(title) <> ''
                 ORDER BY updated_at DESC, id DESC
                 LIMIT $2",
            )
            .bind(&self.db_id)
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?
        } else {
            let escaped = escape_like_pattern(query);
            let contains = format!("%{}%", escaped);
            let prefix = format!("{}%", escaped);
            sqlx::query_as(
                "SELECT id, title, snippet, updated_at
                 FROM atoms
                 WHERE db_id = $1
                   AND BTRIM(title) <> ''
                   AND title ILIKE $2 ESCAPE '\\'
                 ORDER BY
                   CASE
                     WHEN LOWER(title) = LOWER($3) THEN 0
                     WHEN title ILIKE $4 ESCAPE '\\' THEN 1
                     ELSE 2
                   END,
                   updated_at DESC,
                   id DESC
                 LIMIT $5",
            )
            .bind(&self.db_id)
            .bind(&contains)
            .bind(query)
            .bind(&prefix)
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?
        };

        Ok(rows
            .into_iter()
            .map(|(id, title, snippet, updated_at)| AtomLinkSuggestion {
                id,
                title,
                snippet,
                updated_at,
            })
            .collect())
    }

    async fn list_atoms(
        &self,
        params: &ListAtomsParams,
        kinds: &crate::models::KindFilter,
    ) -> StorageResult<PaginatedAtoms> {
        let use_cursor = params.cursor.is_some() && params.cursor_id.is_some();

        // A non-All kind filter forces the slow count path because the
        // denormalized `tags.atom_count` is kind-blind. Mirrors SQLite.
        let has_kind_filter = !matches!(kinds, crate::models::KindFilter::All);
        let has_extra_filters = !matches!(params.source_filter, SourceFilter::All)
            || params.source_value.is_some()
            || has_kind_filter;

        // --- Build ORDER BY ---
        let sort_col = match params.sort_by {
            SortField::Updated => "a.updated_at",
            SortField::Created => "a.created_at",
            SortField::Published => "COALESCE(a.published_at, a.created_at)",
            SortField::Title => "a.title",
        };
        let sort_dir = match params.sort_order {
            SortOrder::Desc => "DESC",
            SortOrder::Asc => "ASC",
        };
        let cursor_cmp = match params.sort_order {
            SortOrder::Desc => "<",
            SortOrder::Asc => ">",
        };

        // --- Dynamic WHERE + bind values ---
        // We collect bind values as trait objects to apply them dynamically.
        // Since sqlx doesn't support dynamic binding easily, we build the query string
        // with numbered placeholders and use a helper approach.

        let mut where_clauses: Vec<String> = Vec::new();
        let mut param_idx: usize = 1;

        // We'll track the actual values to bind in order.
        // Using an enum to hold different types.
        enum BindVal {
            Str(String),
            Int(i32),
            Strs(Vec<String>),
        }
        let mut bind_values: Vec<BindVal> = Vec::new();

        // db_id scoping — always applied first
        where_clauses.push(format!("a.db_id = ${}", param_idx));
        bind_values.push(BindVal::Str(self.db_id.clone()));
        param_idx += 1;

        // Tag filter — recursive CTE to include full descendant subtree
        if let Some(ref tid) = params.tag_id {
            where_clauses.push(format!(
                "EXISTS (SELECT 1 FROM atom_tags at WHERE at.atom_id = a.id AND at.tag_id IN (\
                 WITH RECURSIVE descendant_tags(id) AS (\
                   SELECT ${p}::text \
                   UNION ALL \
                   SELECT t.id FROM tags t INNER JOIN descendant_tags dt ON t.parent_id = dt.id\
                 ) SELECT id FROM descendant_tags))",
                p = param_idx
            ));
            bind_values.push(BindVal::Str(tid.clone()));
            param_idx += 1;
        }

        // Source filter
        match params.source_filter {
            SourceFilter::All => {}
            SourceFilter::Manual => {
                where_clauses.push("a.source IS NULL".to_string());
            }
            SourceFilter::External => {
                where_clauses.push("a.source IS NOT NULL".to_string());
            }
        }

        // Source value filter
        if let Some(ref sv) = params.source_value {
            where_clauses.push(format!("a.source = ${}", param_idx));
            bind_values.push(BindVal::Str(sv.clone()));
            param_idx += 1;
        }

        // Cursor
        if use_cursor {
            where_clauses.push(format!(
                "({sort_col}, a.id) {cursor_cmp} (${p1}, ${p2})",
                sort_col = sort_col,
                cursor_cmp = cursor_cmp,
                p1 = param_idx,
                p2 = param_idx + 1,
            ));
            bind_values.push(BindVal::Str(params.cursor.clone().unwrap()));
            bind_values.push(BindVal::Str(params.cursor_id.clone().unwrap()));
            param_idx += 2;
        }

        // Kind filter
        if has_kind_filter {
            let pred = kinds.postgres_predicate("a.kind", &format!("${}", param_idx));
            where_clauses.push(pred);
            if matches!(kinds, crate::models::KindFilter::Only(ref v) if !v.is_empty()) {
                bind_values.push(BindVal::Strs(kinds.kind_strings()));
                param_idx += 1;
            }
        }

        let where_sql = if where_clauses.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", where_clauses.join(" AND "))
        };

        // --- Count query ---
        let total_count: i32 = if !has_extra_filters && params.tag_id.is_some() {
            let tid = params.tag_id.as_ref().unwrap();
            let has_children: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM tags WHERE parent_id = $1 AND db_id = $2)",
            )
            .bind(tid)
            .bind(&self.db_id)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

            if has_children {
                let count: i64 = sqlx::query_scalar(
                    "WITH RECURSIVE descendant_tags(id) AS (
                       SELECT id FROM tags WHERE id = $1 AND db_id = $2
                       UNION ALL
                       SELECT t.id FROM tags t INNER JOIN descendant_tags dt ON t.parent_id = dt.id
                     )
                     SELECT COUNT(DISTINCT at.atom_id)
                     FROM atom_tags at
                     WHERE at.tag_id IN (SELECT id FROM descendant_tags)",
                )
                .bind(tid)
                .bind(&self.db_id)
                .fetch_one(&self.pool)
                .await
                .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
                count as i32
            } else {
                let count: i32 =
                    sqlx::query_scalar("SELECT atom_count FROM tags WHERE id = $1 AND db_id = $2")
                        .bind(tid)
                        .bind(&self.db_id)
                        .fetch_one(&self.pool)
                        .await
                        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
                count
            }
        } else if has_extra_filters || params.tag_id.is_some() {
            // Build count query with filters (no cursor)
            let mut count_wheres: Vec<String> = Vec::new();
            let mut count_binds: Vec<BindVal> = Vec::new();
            let mut ci: usize = 1;

            // db_id scoping
            count_wheres.push(format!("a.db_id = ${}", ci));
            count_binds.push(BindVal::Str(self.db_id.clone()));
            ci += 1;

            if let Some(ref tid) = params.tag_id {
                count_wheres.push(format!(
                    "EXISTS (SELECT 1 FROM atom_tags at WHERE at.atom_id = a.id AND at.tag_id IN (\
                     WITH RECURSIVE descendant_tags(id) AS (\
                       SELECT ${p}::text \
                       UNION ALL \
                       SELECT t.id FROM tags t INNER JOIN descendant_tags dt ON t.parent_id = dt.id\
                     ) SELECT id FROM descendant_tags))",
                    p = ci
                ));
                count_binds.push(BindVal::Str(tid.clone()));
                ci += 1;
            }
            match params.source_filter {
                SourceFilter::All => {}
                SourceFilter::Manual => count_wheres.push("a.source IS NULL".to_string()),
                SourceFilter::External => count_wheres.push("a.source IS NOT NULL".to_string()),
            }
            if let Some(ref sv) = params.source_value {
                count_wheres.push(format!("a.source = ${}", ci));
                count_binds.push(BindVal::Str(sv.clone()));
                ci += 1;
            }
            if has_kind_filter {
                let pred = kinds.postgres_predicate("a.kind", &format!("${}", ci));
                count_wheres.push(pred);
                if matches!(kinds, crate::models::KindFilter::Only(ref v) if !v.is_empty()) {
                    count_binds.push(BindVal::Strs(kinds.kind_strings()));
                    // ci no longer used after this point
                }
            }

            let count_where = if count_wheres.is_empty() {
                String::new()
            } else {
                format!("WHERE {}", count_wheres.join(" AND "))
            };
            let count_sql = format!("SELECT COUNT(*) FROM atoms a {}", count_where);

            let mut query = sqlx::query_scalar::<_, i64>(&count_sql);
            for bv in &count_binds {
                match bv {
                    BindVal::Str(s) => query = query.bind(s),
                    BindVal::Int(i) => query = query.bind(i),
                    BindVal::Strs(v) => query = query.bind(v),
                }
            }
            let count = query
                .fetch_one(&self.pool)
                .await
                .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
            count as i32
        } else {
            let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM atoms WHERE db_id = $1")
                .bind(&self.db_id)
                .fetch_one(&self.pool)
                .await
                .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
            count as i32
        };

        // --- Data query ---
        let limit_param = param_idx;
        bind_values.push(BindVal::Int(params.limit));
        param_idx += 1;

        let data_sql = if use_cursor {
            format!(
                "SELECT a.id, a.title, a.snippet, a.source_url, a.source, a.published_at,
                        a.created_at, a.updated_at,
                        COALESCE(a.embedding_status, 'pending'), COALESCE(a.tagging_status, 'pending'),
                        a.embedding_error, a.tagging_error
                 FROM atoms a
                 {where_sql}
                 ORDER BY {sort_col} {sort_dir}, a.id {sort_dir}
                 LIMIT ${limit_param}",
            )
        } else {
            let offset_param = param_idx;
            bind_values.push(BindVal::Int(params.offset));
            // param_idx += 1;
            format!(
                "SELECT a.id, a.title, a.snippet, a.source_url, a.source, a.published_at,
                        a.created_at, a.updated_at,
                        COALESCE(a.embedding_status, 'pending'), COALESCE(a.tagging_status, 'pending'),
                        a.embedding_error, a.tagging_error
                 FROM atoms a
                 {where_sql}
                 ORDER BY {sort_col} {sort_dir}, a.id {sort_dir}
                 LIMIT ${limit_param} OFFSET ${offset_param}",
            )
        };

        let mut query = sqlx::query_as::<
            _,
            (
                String,
                String,
                String,
                Option<String>,
                Option<String>,
                Option<String>,
                String,
                String,
                String,
                String,
                Option<String>,
                Option<String>,
            ),
        >(&data_sql);
        for bv in &bind_values {
            match bv {
                BindVal::Str(s) => query = query.bind(s),
                BindVal::Int(i) => query = query.bind(i),
                BindVal::Strs(v) => query = query.bind(v),
            }
        }

        let rows = query
            .fetch_all(&self.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        // Batch-load tags
        let atom_ids: Vec<String> = rows.iter().map(|r| r.0.clone()).collect();
        let tag_map = self.tags_for_atom_ids(&atom_ids).await?;

        // Extract cursor from last result — must match the active sort column
        let (next_cursor, next_cursor_id) = rows
            .last()
            .map(|last| {
                let cursor_val = match params.sort_by {
                    SortField::Updated => last.7.clone(), // updated_at
                    SortField::Created => last.6.clone(), // created_at
                    SortField::Published => last.5.clone().unwrap_or_else(|| last.6.clone()), // COALESCE(published_at, created_at)
                    SortField::Title => last.1.clone(), // title
                };
                (Some(cursor_val), Some(last.0.clone()))
            })
            .unwrap_or((None, None));

        let summaries: Vec<AtomSummary> = rows
            .into_iter()
            .map(
                |(
                    id,
                    title,
                    snippet,
                    source_url,
                    source,
                    published_at,
                    created_at,
                    updated_at,
                    embedding_status,
                    tagging_status,
                    embedding_error,
                    tagging_error,
                )| {
                    let tags = tag_map.get(&id).cloned().unwrap_or_default();
                    AtomSummary {
                        id,
                        title,
                        snippet,
                        source_url,
                        source,
                        published_at,
                        created_at,
                        updated_at,
                        embedding_status,
                        tagging_status,
                        embedding_error,
                        tagging_error,
                        tags,
                    }
                },
            )
            .collect();

        Ok(PaginatedAtoms {
            atoms: summaries,
            total_count,
            limit: params.limit,
            offset: params.offset,
            next_cursor,
            next_cursor_id,
        })
    }

    async fn get_source_list(&self) -> StorageResult<Vec<SourceInfo>> {
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT source, COUNT(*) as cnt FROM atoms WHERE source IS NOT NULL AND db_id = $1 GROUP BY source ORDER BY cnt DESC",
        )
        .bind(&self.db_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        Ok(rows
            .into_iter()
            .map(|(source, count)| SourceInfo {
                source,
                atom_count: count as i32,
            })
            .collect())
    }

    async fn get_embedding_status(&self, atom_id: &str) -> StorageResult<String> {
        let status: String = sqlx::query_scalar(
            "SELECT COALESCE(embedding_status, 'pending') FROM atoms WHERE id = $1 AND db_id = $2",
        )
        .bind(atom_id)
        .bind(&self.db_id)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        Ok(status)
    }

    async fn get_tagging_status(&self, atom_id: &str) -> StorageResult<String> {
        let status: String = sqlx::query_scalar(
            "SELECT COALESCE(tagging_status, 'pending') FROM atoms WHERE id = $1 AND db_id = $2",
        )
        .bind(atom_id)
        .bind(&self.db_id)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        Ok(status)
    }

    async fn get_atom_positions(&self) -> StorageResult<Vec<AtomPosition>> {
        let rows: Vec<(String, f64, f64)> =
            sqlx::query_as("SELECT atom_id, x, y FROM atom_positions WHERE db_id = $1")
                .bind(&self.db_id)
                .fetch_all(&self.pool)
                .await
                .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        Ok(rows
            .into_iter()
            .map(|(atom_id, x, y)| AtomPosition { atom_id, x, y })
            .collect())
    }

    async fn save_atom_positions(&self, positions: &[AtomPosition]) -> StorageResult<()> {
        let now = chrono::Utc::now().to_rfc3339();

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        for pos in positions {
            sqlx::query(
                "INSERT INTO atom_positions (atom_id, x, y, updated_at, db_id) VALUES ($1, $2, $3, $4, $5)
                 ON CONFLICT (atom_id, db_id) DO UPDATE SET x = $2, y = $3, updated_at = $4",
            )
            .bind(&pos.atom_id)
            .bind(&pos.x)
            .bind(&pos.y)
            .bind(&now)
            .bind(&self.db_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        }

        tx.commit()
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        Ok(())
    }

    async fn get_atom_tag_ids(&self, atom_id: &str) -> StorageResult<Vec<String>> {
        let ids: Vec<(String,)> =
            sqlx::query_as("SELECT tag_id FROM atom_tags WHERE atom_id = $1 AND db_id = $2")
                .bind(atom_id)
                .bind(&self.db_id)
                .fetch_all(&self.pool)
                .await
                .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        Ok(ids.into_iter().map(|(id,)| id).collect())
    }

    async fn get_atom_content(&self, atom_id: &str) -> StorageResult<Option<String>> {
        let content: Option<String> =
            sqlx::query_scalar("SELECT content FROM atoms WHERE id = $1 AND db_id = $2")
                .bind(atom_id)
                .bind(&self.db_id)
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        Ok(content)
    }

    async fn get_atom_contents_batch(
        &self,
        atom_ids: &[String],
    ) -> StorageResult<Vec<(String, String)>> {
        if atom_ids.is_empty() {
            return Ok(vec![]);
        }
        // Build $1, $2, ... placeholders
        let placeholders: String = atom_ids
            .iter()
            .enumerate()
            .map(|(i, _)| format!("${}", i + 1))
            .collect::<Vec<_>>()
            .join(",");
        let query = format!(
            "SELECT id, content FROM atoms WHERE id IN ({}) AND db_id = ${}",
            placeholders,
            atom_ids.len() + 1,
        );
        let mut q = sqlx::query_as::<_, (String, String)>(&query);
        for id in atom_ids {
            q = q.bind(id);
        }
        q = q.bind(&self.db_id);
        let rows = q
            .fetch_all(&self.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        Ok(rows)
    }

    async fn get_atoms_with_embeddings(
        &self,
        kinds: &crate::models::KindFilter,
    ) -> StorageResult<Vec<AtomWithEmbedding>> {
        // Fetch all atoms
        let kind_predicate = kinds.postgres_predicate("kind", "$2");
        let sql = format!(
            "SELECT id, content, title, snippet, source_url, source, published_at,
                    created_at, updated_at,
                    COALESCE(embedding_status, 'pending'),
                    COALESCE(tagging_status, 'pending'),
                    embedding_error, tagging_error,
                    COALESCE(kind, 'captured')
             FROM atoms WHERE db_id = $1 AND {kind_predicate} ORDER BY updated_at DESC"
        );
        let mut q = sqlx::query_as::<
            _,
            (
                String,
                String,
                String,
                String,
                Option<String>,
                Option<String>,
                Option<String>,
                String,
                String,
                String,
                String,
                Option<String>,
                Option<String>,
                String,
            ),
        >(&sql)
        .bind(&self.db_id);
        if kinds.has_bind_value() {
            q = q.bind(kinds.kind_strings());
        }
        let rows = q
            .fetch_all(&self.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        let atom_ids: Vec<String> = rows.iter().map(|r| r.0.clone()).collect();
        let tag_map = self.tags_for_atom_ids(&atom_ids).await?;

        // Batch-load average embeddings for all atoms.
        // In Postgres with pgvector, embeddings are stored as vector type.
        // We average chunk embeddings per atom.
        let embedding_rows: Vec<(String, Vec<f32>)> = sqlx::query_as(
            "SELECT atom_id, avg(embedding)::real[] as avg_embedding
             FROM atom_chunks
             WHERE embedding IS NOT NULL AND db_id = $1
             GROUP BY atom_id",
        )
        .bind(&self.db_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        let mut embedding_map: std::collections::HashMap<String, Vec<f32>> =
            std::collections::HashMap::new();
        for (atom_id, emb) in embedding_rows {
            embedding_map.insert(atom_id, emb);
        }

        let result = rows
            .into_iter()
            .map(|row| {
                let id = row.0.clone();
                let atom = Self::atom_from_tuple(row);
                let tags = tag_map.get(&id).cloned().unwrap_or_default();
                let embedding = embedding_map.get(&id).cloned();
                AtomWithEmbedding {
                    atom: AtomWithTags { atom, tags },
                    embedding,
                }
            })
            .collect();

        Ok(result)
    }

    async fn check_existing_source_urls(
        &self,
        urls: &[String],
    ) -> StorageResult<std::collections::HashSet<String>> {
        if urls.is_empty() {
            return Ok(std::collections::HashSet::new());
        }
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT source_url FROM atoms WHERE source_url = ANY($1) AND db_id = $2",
        )
        .bind(urls)
        .bind(&self.db_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        Ok(rows.into_iter().map(|(url,)| url).collect())
    }

    async fn source_url_exists(&self, url: &str) -> StorageResult<bool> {
        let exists: Option<bool> = sqlx::query_scalar::<_, Option<bool>>(
            "SELECT EXISTS(SELECT 1 FROM atoms WHERE source_url = $1 AND db_id = $2)",
        )
        .bind(url)
        .bind(&self.db_id)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        Ok(exists.unwrap_or(false))
    }

    async fn get_atom_by_source_url(&self, url: &str) -> StorageResult<Option<AtomWithTags>> {
        let row: Option<(
            String,
            String,
            String,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            String,
            String,
            String,
            String,
            Option<String>,
            Option<String>,
            String,
        )> = sqlx::query_as(
            "SELECT id, content, title, snippet, source_url, source, published_at,
                    created_at, updated_at,
                    COALESCE(embedding_status, 'pending'),
                    COALESCE(tagging_status, 'pending'),
                    embedding_error, tagging_error,
                    COALESCE(kind, 'captured')
             FROM atoms WHERE source_url = $1 AND db_id = $2",
        )
        .bind(url)
        .bind(&self.db_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        match row {
            Some(r) => {
                let atom = Self::atom_from_tuple(r);
                let tags = self.tags_for_atom(&atom.id).await?;
                Ok(Some(AtomWithTags { atom, tags }))
            }
            None => Ok(None),
        }
    }

    async fn count_pending_embeddings(&self) -> StorageResult<i32> {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM atoms WHERE embedding_status = 'pending' AND db_id = $1",
        )
        .bind(&self.db_id)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        Ok(count as i32)
    }

    async fn get_all_embedding_pairs(&self) -> StorageResult<Vec<(String, Vec<f32>)>> {
        // Load all chunk embeddings, average per atom
        let rows: Vec<(String, Vec<f32>)> = sqlx::query_as(
            "SELECT atom_id, embedding::real[] FROM atom_chunks
             WHERE embedding IS NOT NULL AND db_id = $1
             ORDER BY atom_id",
        )
        .bind(&self.db_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        // Average chunks per atom
        let mut result: Vec<(String, Vec<f32>)> = Vec::new();
        let mut current_id: Option<String> = None;
        let mut current_sum: Vec<f32> = Vec::new();
        let mut current_count: f32 = 0.0;

        for (atom_id, embedding) in rows {
            if current_id.as_ref() != Some(&atom_id) {
                if let Some(prev_id) = current_id.take() {
                    if current_count > 0.0 {
                        for val in &mut current_sum {
                            *val /= current_count;
                        }
                        result.push((prev_id, current_sum.clone()));
                    }
                }
                current_id = Some(atom_id);
                current_sum = embedding;
                current_count = 1.0;
            } else {
                for (i, val) in embedding.iter().enumerate() {
                    if i < current_sum.len() {
                        current_sum[i] += val;
                    }
                }
                current_count += 1.0;
            }
        }
        if let Some(prev_id) = current_id {
            if current_count > 0.0 {
                for val in &mut current_sum {
                    *val /= current_count;
                }
                result.push((prev_id, current_sum));
            }
        }

        Ok(result)
    }

    async fn get_top_k_canvas_edges(&self, top_k: usize) -> StorageResult<Vec<CanvasEdgeData>> {
        let all_edges: Vec<(String, String, f32)> = sqlx::query_as(
            "SELECT source_atom_id, target_atom_id, similarity_score
             FROM semantic_edges
             WHERE similarity_score >= 0.5 AND db_id = $1
             ORDER BY similarity_score DESC",
        )
        .bind(&self.db_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        let mut per_atom: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        let mut kept: Vec<(String, String, f32)> = Vec::new();

        for (src, tgt, score) in all_edges {
            let src_count = per_atom.get(&src).copied().unwrap_or(0);
            let tgt_count = per_atom.get(&tgt).copied().unwrap_or(0);
            if src_count >= top_k && tgt_count >= top_k {
                continue;
            }
            *per_atom.entry(src.clone()).or_insert(0) += 1;
            *per_atom.entry(tgt.clone()).or_insert(0) += 1;
            kept.push((src, tgt, score));
        }

        let min_w = kept.iter().map(|(_, _, w)| *w).fold(f32::MAX, f32::min);
        let max_w = kept.iter().map(|(_, _, w)| *w).fold(f32::MIN, f32::max);
        let range = (max_w - min_w).max(0.001);

        Ok(kept
            .into_iter()
            .map(|(src, tgt, score)| CanvasEdgeData {
                source: src,
                target: tgt,
                weight: (score - min_w) / range,
            })
            .collect())
    }

    async fn get_all_atom_tag_ids(
        &self,
    ) -> StorageResult<std::collections::HashMap<String, Vec<String>>> {
        let rows: Vec<(String, String)> =
            sqlx::query_as("SELECT atom_id, tag_id FROM atom_tags WHERE db_id = $1")
                .bind(&self.db_id)
                .fetch_all(&self.pool)
                .await
                .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        let mut map: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        for (atom_id, tag_id) in rows {
            map.entry(atom_id).or_default().push(tag_id);
        }
        Ok(map)
    }

    async fn get_canvas_atom_metadata(&self) -> StorageResult<Vec<CanvasAtomPosition>> {
        let rows: Vec<(String, f64, f64, String, Option<String>, i64)> = sqlx::query_as(
            "SELECT ap.atom_id, ap.x, ap.y,
                    SUBSTRING(a.content FROM 1 FOR 80) as title,
                    (SELECT t.name FROM atom_tags at JOIN tags t ON at.tag_id = t.id
                     WHERE at.atom_id = ap.atom_id AND at.db_id = $1 AND t.db_id = $1 LIMIT 1) as primary_tag,
                    (SELECT COUNT(*) FROM atom_tags at WHERE at.atom_id = ap.atom_id AND at.db_id = $1) as tag_count
             FROM atom_positions ap
             JOIN atoms a ON ap.atom_id = a.id AND a.db_id = $1
             WHERE ap.db_id = $1",
        )
        .bind(&self.db_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        Ok(rows
            .into_iter()
            .map(|(atom_id, x, y, content, primary_tag, tag_count)| {
                let (title, _) = crate::extract_title_and_snippet(&content, 60);
                CanvasAtomPosition {
                    atom_id,
                    x,
                    y,
                    title,
                    primary_tag,
                    tag_count: tag_count as i32,
                    tag_ids: vec![],
                    source_url: None,
                }
            })
            .collect())
    }

    async fn get_canvas_atom_metadata_light(
        &self,
        kinds: &crate::models::KindFilter,
    ) -> StorageResult<Vec<(String, String, Option<String>, i32, Option<String>)>> {
        let kind_predicate = kinds.postgres_predicate("a.kind", "$2");
        let sql = format!(
            "SELECT a.id, a.title, MIN(t.name) AS primary_tag, COUNT(at.tag_id) AS tag_count, a.source_url
             FROM atoms a
             LEFT JOIN atom_tags at ON at.atom_id = a.id AND at.db_id = $1
             LEFT JOIN tags t ON t.id = at.tag_id AND t.db_id = $1
             WHERE a.db_id = $1 AND a.embedding_status = 'complete'
               AND {kind_predicate}
             GROUP BY a.id, a.title, a.source_url"
        );
        let mut q =
            sqlx::query_as::<_, (String, String, Option<String>, i64, Option<String>)>(&sql)
                .bind(&self.db_id);
        if kinds.has_bind_value() {
            q = q.bind(kinds.kind_strings());
        }
        let rows = q
            .fetch_all(&self.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        Ok(rows
            .into_iter()
            .map(|(id, title, tag, count, src)| (id, title, tag, count as i32, src))
            .collect())
    }

    async fn list_atoms_for_report_scope(
        &self,
        tag_ids: &[String],
        since: Option<&str>,
        kinds: &crate::models::KindFilter,
        limit: Option<i32>,
    ) -> StorageResult<Vec<AtomWithTags>> {
        // We bind in fixed positional order: db_id, then tag_ids (as
        // text[] for the recursive case), then since, then limit, then
        // kinds binds. Build the SQL to match.
        let no_tags = tag_ids.is_empty();
        // Placeholder accounting:
        //   $1 = db_id
        //   $2 = tag_ids array         (only present when !no_tags)
        //   $N = since                 (only present when since.is_some())
        //   $M = limit                 (only present when limit.is_some())
        //   last placeholder = kinds   (only present when kinds.has_bind_value())
        let mut next_param: usize = 2;
        let tags_ph = if no_tags {
            String::new()
        } else {
            let p = format!("${next_param}");
            next_param += 1;
            p
        };
        let since_ph = if since.is_some() {
            let p = format!("${next_param}");
            next_param += 1;
            Some(p)
        } else {
            None
        };
        let limit_ph = if limit.is_some() {
            let p = format!("${next_param}");
            next_param += 1;
            Some(p)
        } else {
            None
        };
        let kinds_ph = format!("${next_param}");
        let kind_predicate = kinds.postgres_predicate("a.kind", &kinds_ph);

        let since_pred = since_ph
            .as_ref()
            .map(|p| format!("AND a.created_at > {p}"))
            .unwrap_or_default();
        let limit_pred = limit_ph
            .as_ref()
            .map(|p| format!("LIMIT {p}"))
            .unwrap_or_default();

        let sql = if no_tags {
            format!(
                "SELECT a.id, a.content, a.title, a.snippet, a.source_url, a.source,
                        a.published_at, a.created_at, a.updated_at, a.embedding_status,
                        a.tagging_status, a.embedding_error, a.tagging_error,
                        COALESCE(a.kind, 'captured')
                 FROM atoms a
                 WHERE a.db_id = $1 AND {kind_predicate} {since_pred}
                 ORDER BY a.created_at DESC
                 {limit_pred}"
            )
        } else {
            format!(
                "WITH RECURSIVE descendant_tags(id) AS (
                     SELECT id FROM tags WHERE id = ANY({tags_ph}) AND db_id = $1
                     UNION
                     SELECT t.id FROM tags t
                     INNER JOIN descendant_tags dt ON t.parent_id = dt.id
                     WHERE t.db_id = $1
                 )
                 SELECT a.id, a.content, a.title, a.snippet, a.source_url, a.source,
                        a.published_at, a.created_at, a.updated_at, a.embedding_status,
                        a.tagging_status, a.embedding_error, a.tagging_error,
                        COALESCE(a.kind, 'captured')
                 FROM atoms a
                 WHERE a.db_id = $1
                   AND EXISTS (
                       SELECT 1 FROM atom_tags at
                       WHERE at.atom_id = a.id
                         AND at.db_id = $1
                         AND at.tag_id IN (SELECT id FROM descendant_tags)
                   )
                   AND {kind_predicate}
                   {since_pred}
                 GROUP BY a.id
                 ORDER BY MAX(a.created_at) DESC
                 {limit_pred}"
            )
        };

        let mut q = sqlx::query(&sql).bind(&self.db_id);
        if !no_tags {
            q = q.bind(tag_ids.to_vec());
        }
        if let Some(s) = since {
            q = q.bind(s);
        }
        if let Some(l) = limit {
            q = q.bind(l as i64);
        }
        if kinds.has_bind_value() {
            q = q.bind(kinds.kind_strings());
        }

        use crate::models::Atom;
        use sqlx::Row;
        let rows = q
            .fetch_all(&self.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        let atoms: Vec<Atom> = rows
            .iter()
            .map(|row| {
                let kind_str: String = row.get(13);
                let kind = kind_str
                    .parse::<crate::models::AtomKind>()
                    .unwrap_or(crate::models::AtomKind::Captured);
                Atom {
                    id: row.get(0),
                    content: row.get(1),
                    title: row.get(2),
                    snippet: row.get(3),
                    source_url: row.get(4),
                    source: row.get(5),
                    published_at: row.get(6),
                    created_at: row.get(7),
                    updated_at: row.get(8),
                    embedding_status: row.get(9),
                    tagging_status: row.get(10),
                    embedding_error: row.get(11),
                    tagging_error: row.get(12),
                    kind,
                }
            })
            .collect();

        if atoms.is_empty() {
            return Ok(Vec::new());
        }

        // Batch-load tags via the existing private helper.
        let atom_ids: Vec<String> = atoms.iter().map(|a| a.id.clone()).collect();
        let id_to_tags = self.tags_for_atom_ids(&atom_ids).await?;
        Ok(atoms
            .into_iter()
            .map(|atom| {
                let tags = id_to_tags.get(&atom.id).cloned().unwrap_or_default();
                AtomWithTags { atom, tags }
            })
            .collect())
    }

    async fn count_atoms_for_report_scope(
        &self,
        tag_ids: &[String],
        since: Option<&str>,
        kinds: &crate::models::KindFilter,
    ) -> StorageResult<i32> {
        let no_tags = tag_ids.is_empty();
        let mut next_param: usize = 2;
        let tags_ph = if no_tags {
            String::new()
        } else {
            let p = format!("${next_param}");
            next_param += 1;
            p
        };
        let since_ph = if since.is_some() {
            let p = format!("${next_param}");
            next_param += 1;
            Some(p)
        } else {
            None
        };
        let kinds_ph = format!("${next_param}");
        let kind_predicate = kinds.postgres_predicate("a.kind", &kinds_ph);
        let since_pred = since_ph
            .as_ref()
            .map(|p| format!("AND a.created_at > {p}"))
            .unwrap_or_default();

        let sql = if no_tags {
            format!(
                "SELECT COUNT(*) FROM atoms a
                 WHERE a.db_id = $1 AND {kind_predicate} {since_pred}"
            )
        } else {
            format!(
                "WITH RECURSIVE descendant_tags(id) AS (
                     SELECT id FROM tags WHERE id = ANY({tags_ph}) AND db_id = $1
                     UNION
                     SELECT t.id FROM tags t
                     INNER JOIN descendant_tags dt ON t.parent_id = dt.id
                     WHERE t.db_id = $1
                 )
                 SELECT COUNT(DISTINCT a.id) FROM atoms a
                 WHERE a.db_id = $1
                   AND EXISTS (
                       SELECT 1 FROM atom_tags at
                       WHERE at.atom_id = a.id
                         AND at.db_id = $1
                         AND at.tag_id IN (SELECT id FROM descendant_tags)
                   )
                   AND {kind_predicate}
                   {since_pred}"
            )
        };

        let mut q = sqlx::query_scalar::<_, i64>(&sql).bind(&self.db_id);
        if !no_tags {
            q = q.bind(tag_ids.to_vec());
        }
        if let Some(s) = since {
            q = q.bind(s);
        }
        if kinds.has_bind_value() {
            q = q.bind(kinds.kind_strings());
        }

        let count = q
            .fetch_one(&self.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        Ok(count as i32)
    }
}
