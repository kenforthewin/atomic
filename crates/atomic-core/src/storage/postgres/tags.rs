use super::PostgresStorage;
use crate::compaction::{CompactionResult, TagMerge};
use crate::error::AtomicCoreError;
use crate::models::*;
use crate::storage::traits::*;
use async_trait::async_trait;
use chrono::Utc;
use std::collections::HashMap;
use uuid::Uuid;

impl PostgresStorage {
    /// Load all tags and their direct (denormalized) atom counts.
    async fn load_tags_and_counts(&self) -> StorageResult<(Vec<Tag>, HashMap<String, i32>)> {
        let rows: Vec<(String, String, Option<String>, String, i32, bool, String)> = sqlx::query_as(
            "SELECT id, name, parent_id, created_at, atom_count, is_autotag_target, autotag_description FROM tags WHERE db_id = $1 ORDER BY name",
        )
        .bind(&self.db_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        let mut direct_counts: HashMap<String, i32> = HashMap::new();
        let all_tags: Vec<Tag> = rows
            .into_iter()
            .map(
                |(
                    id,
                    name,
                    parent_id,
                    created_at,
                    count,
                    is_autotag_target,
                    autotag_description,
                )| {
                    direct_counts.insert(id.clone(), count);
                    Tag {
                        id,
                        name,
                        parent_id,
                        created_at,
                        is_autotag_target,
                        autotag_description,
                    }
                },
            )
            .collect();

        Ok((all_tags, direct_counts))
    }

    /// Check if a tag is a descendant of another tag (for merge safety).
    async fn is_descendant_of(
        &self,
        potential_child: &str,
        potential_parent: &str,
    ) -> StorageResult<bool> {
        let mut current = potential_child.to_string();
        let mut visited = std::collections::HashSet::new();

        loop {
            if current == potential_parent {
                return Ok(true);
            }
            if visited.contains(&current) {
                return Ok(false);
            }
            visited.insert(current.clone());

            let parent: Option<String> =
                sqlx::query_scalar("SELECT parent_id FROM tags WHERE id = $1 AND db_id = $2")
                    .bind(&current)
                    .bind(&self.db_id)
                    .fetch_optional(&self.pool)
                    .await
                    .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?
                    .flatten();

            match parent {
                Some(p) => current = p,
                None => return Ok(false),
            }
        }
    }

    /// Look up a tag ID by its name (case-insensitive).
    async fn get_tag_id_by_name(&self, name: &str) -> StorageResult<Option<String>> {
        let id: Option<String> =
            sqlx::query_scalar("SELECT id FROM tags WHERE LOWER(name) = LOWER($1) AND db_id = $2")
                .bind(name.trim())
                .bind(&self.db_id)
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        Ok(id)
    }

    /// Execute a single tag merge: move atoms from loser to winner, reparent children, delete loser.
    async fn execute_tag_merge(&self, merge: &TagMerge) -> Result<(bool, i32), String> {
        let winner_id = match self
            .get_tag_id_by_name(&merge.winner_name)
            .await
            .map_err(|e| e.to_string())?
        {
            Some(id) => id,
            None => {
                tracing::warn!(winner = %merge.winner_name, "Skipping merge: winner not found");
                return Ok((false, 0));
            }
        };

        let loser_id = match self
            .get_tag_id_by_name(&merge.loser_name)
            .await
            .map_err(|e| e.to_string())?
        {
            Some(id) => id,
            None => {
                tracing::warn!(loser = %merge.loser_name, "Skipping merge: loser not found");
                return Ok((false, 0));
            }
        };

        if winner_id == loser_id {
            tracing::warn!(
                winner = %merge.winner_name,
                loser = %merge.loser_name,
                "Skipping merge: same tag"
            );
            return Ok((false, 0));
        }

        if self
            .is_descendant_of(&loser_id, &winner_id)
            .await
            .map_err(|e| e.to_string())?
        {
            tracing::warn!(
                loser = %merge.loser_name,
                winner = %merge.winner_name,
                "Skipping merge: loser is a descendant of winner"
            );
            return Ok((false, 0));
        }
        if self
            .is_descendant_of(&winner_id, &loser_id)
            .await
            .map_err(|e| e.to_string())?
        {
            tracing::warn!(
                winner = %merge.winner_name,
                loser = %merge.loser_name,
                "Skipping merge: winner is a descendant of loser"
            );
            return Ok((false, 0));
        }

        // Get atoms tagged with the loser
        let atoms_with_loser: Vec<String> =
            sqlx::query_scalar("SELECT atom_id FROM atom_tags WHERE tag_id = $1 AND db_id = $2")
                .bind(&loser_id)
                .bind(&self.db_id)
                .fetch_all(&self.pool)
                .await
                .map_err(|e| format!("Failed to query atoms: {}", e))?;

        let mut atoms_retagged: i32 = 0;
        for atom_id in &atoms_with_loser {
            // INSERT ... ON CONFLICT DO NOTHING replaces INSERT OR IGNORE.
            // Tag merge: source defaults to 'auto'. The original loser-row's
            // source isn't preserved here (rare admin operation); a manual
            // assignment of the *winner* tag is preserved by ON CONFLICT.
            let result = sqlx::query(
                "INSERT INTO atom_tags (atom_id, tag_id, db_id, source) VALUES ($1, $2, $3, 'auto') ON CONFLICT DO NOTHING",
            )
            .bind(atom_id)
            .bind(&winner_id)
            .bind(&self.db_id)
            .execute(&self.pool)
            .await
            .map_err(|e| format!("Failed to add winner tag: {}", e))?;

            if result.rows_affected() > 0 {
                atoms_retagged += 1;
            }
        }

        // Reparent children of the loser to the winner
        sqlx::query("UPDATE tags SET parent_id = $1 WHERE parent_id = $2 AND db_id = $3")
            .bind(&winner_id)
            .bind(&loser_id)
            .bind(&self.db_id)
            .execute(&self.pool)
            .await
            .map_err(|e| format!("Failed to reparent children: {}", e))?;

        // Delete the loser tag (atom_tags rows will be cleaned by cascade or explicit delete)
        sqlx::query("DELETE FROM atom_tags WHERE tag_id = $1 AND db_id = $2")
            .bind(&loser_id)
            .bind(&self.db_id)
            .execute(&self.pool)
            .await
            .map_err(|e| format!("Failed to delete loser atom_tags: {}", e))?;

        sqlx::query("DELETE FROM tags WHERE id = $1 AND db_id = $2")
            .bind(&loser_id)
            .bind(&self.db_id)
            .execute(&self.pool)
            .await
            .map_err(|e| format!("Failed to delete loser tag: {}", e))?;

        tracing::info!(
            loser = %merge.loser_name,
            winner = %merge.winner_name,
            atoms_retagged,
            reason = %merge.reason,
            "Merged tags"
        );

        Ok((true, atoms_retagged))
    }
}

#[async_trait]
impl TagStore for PostgresStorage {
    async fn get_all_tags(&self) -> StorageResult<Vec<TagWithCount>> {
        self.get_all_tags_filtered(0).await
    }

    async fn get_all_tags_filtered(&self, min_count: i32) -> StorageResult<Vec<TagWithCount>> {
        let (all_tags, direct_counts) = self.load_tags_and_counts().await?;
        Ok(crate::build_tag_tree_with_counts(
            &all_tags,
            None,
            &direct_counts,
            min_count,
        ))
    }

    async fn get_tag_children(
        &self,
        parent_id: &str,
        min_count: i32,
        limit: i32,
        offset: i32,
    ) -> StorageResult<PaginatedTagChildren> {
        // Fast total count
        let total: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM tags WHERE parent_id = $1 AND db_id = $2")
                .bind(parent_id)
                .bind(&self.db_id)
                .fetch_one(&self.pool)
                .await
                .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        let total = total as i32;

        if total == 0 {
            return Ok(PaginatedTagChildren {
                children: Vec::new(),
                total: 0,
            });
        }

        let rows: Vec<(String, String, Option<String>, String, i32, i64, bool, String)> = sqlx::query_as(
            "SELECT t.id, t.name, t.parent_id, t.created_at, t.atom_count,
                (SELECT COUNT(*) FROM tags c WHERE c.parent_id = t.id AND c.db_id = $2) AS children_total,
                t.is_autotag_target,
                t.autotag_description
            FROM tags t
            WHERE t.parent_id = $1 AND t.db_id = $2
            ORDER BY t.atom_count DESC
            LIMIT $3 OFFSET $4",
        )
        .bind(parent_id)
        .bind(&self.db_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        let mut children: Vec<TagWithCount> = rows
            .into_iter()
            .map(
                |(
                    id,
                    name,
                    parent_id,
                    created_at,
                    atom_count,
                    children_total,
                    is_autotag_target,
                    autotag_description,
                )| TagWithCount {
                    tag: Tag {
                        id,
                        name,
                        parent_id,
                        created_at,
                        is_autotag_target,
                        autotag_description,
                    },
                    atom_count,
                    children_total: children_total as i32,
                    children: Vec::new(),
                },
            )
            .collect();

        if min_count > 0 {
            children.retain(|t| t.atom_count >= min_count || t.children_total > 0);
        }

        Ok(PaginatedTagChildren { children, total })
    }

    async fn create_tag(&self, name: &str, parent_id: Option<&str>) -> StorageResult<Tag> {
        let id = Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();

        sqlx::query(
            "INSERT INTO tags (id, name, parent_id, created_at, db_id) VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(&id)
        .bind(name)
        .bind(parent_id)
        .bind(&now)
        .bind(&self.db_id)
        .execute(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        Ok(Tag {
            id,
            name: name.to_string(),
            parent_id: parent_id.map(String::from),
            created_at: now,
            is_autotag_target: false,
            autotag_description: String::new(),
        })
    }

    async fn update_tag(
        &self,
        id: &str,
        name: &str,
        parent_id: Option<&str>,
    ) -> StorageResult<Tag> {
        sqlx::query("UPDATE tags SET name = $1, parent_id = $2 WHERE id = $3 AND db_id = $4")
            .bind(name)
            .bind(parent_id)
            .bind(id)
            .bind(&self.db_id)
            .execute(&self.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        let row: (String, String, Option<String>, String, bool, String) = sqlx::query_as(
            "SELECT id, name, parent_id, created_at, is_autotag_target, autotag_description FROM tags WHERE id = $1 AND db_id = $2",
        )
        .bind(id)
        .bind(&self.db_id)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        Ok(Tag {
            id: row.0,
            name: row.1,
            parent_id: row.2,
            created_at: row.3,
            is_autotag_target: row.4,
            autotag_description: row.5,
        })
    }

    async fn set_tag_autotag_target(&self, id: &str, value: bool) -> StorageResult<()> {
        let result =
            sqlx::query("UPDATE tags SET is_autotag_target = $1 WHERE id = $2 AND db_id = $3")
                .bind(value)
                .bind(id)
                .bind(&self.db_id)
                .execute(&self.pool)
                .await
                .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        if result.rows_affected() == 0 {
            return Err(AtomicCoreError::NotFound(format!("tag {}", id)));
        }
        Ok(())
    }

    async fn set_tag_autotag_description(&self, id: &str, description: &str) -> StorageResult<()> {
        let result = sqlx::query(
            "UPDATE tags
             SET autotag_description = $1
             WHERE id = $2 AND db_id = $3 AND parent_id IS NULL",
        )
        .bind(description.trim())
        .bind(id)
        .bind(&self.db_id)
        .execute(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        if result.rows_affected() == 0 {
            return Err(AtomicCoreError::NotFound(format!("top-level tag {}", id)));
        }
        Ok(())
    }

    async fn configure_autotag_targets(
        &self,
        keep_default_names: &[String],
        add_custom_names: &[String],
    ) -> StorageResult<Vec<Tag>> {
        const DEFAULT_NAMES: &[&str] =
            &["Topics", "People", "Locations", "Organizations", "Events"];

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        let keep_lower: std::collections::HashSet<String> = keep_default_names
            .iter()
            .map(|n| n.trim().to_lowercase())
            .filter(|n| !n.is_empty())
            .collect();

        // Snapshot current top-level tags + counts.
        let top_level: Vec<(String, String, bool, i32, i64)> = sqlx::query_as(
            "SELECT t.id, t.name, t.is_autotag_target, t.atom_count,
                    (SELECT COUNT(*) FROM tags c WHERE c.parent_id = t.id AND c.db_id = $1) AS children_count
             FROM tags t
             WHERE t.parent_id IS NULL AND t.db_id = $1",
        )
        .bind(&self.db_id)
        .fetch_all(&mut *tx)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        let now = chrono::Utc::now().to_rfc3339();

        // Step 1: ensure each requested default exists and is flagged.
        for default_name in DEFAULT_NAMES {
            if !keep_lower.contains(&default_name.to_lowercase()) {
                continue;
            }
            let existing = top_level
                .iter()
                .find(|(_, n, _, _, _)| n.eq_ignore_ascii_case(default_name));
            let id = match existing {
                Some((id, _, _, _, _)) => id.clone(),
                None => {
                    let new_id = Uuid::new_v4().to_string();
                    sqlx::query(
                        "INSERT INTO tags (id, name, parent_id, created_at, is_autotag_target, db_id)
                         VALUES ($1, $2, NULL, $3, TRUE, $4)",
                    )
                    .bind(&new_id)
                    .bind(default_name)
                    .bind(&now)
                    .bind(&self.db_id)
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
                    new_id
                }
            };
            sqlx::query("UPDATE tags SET is_autotag_target = TRUE WHERE id = $1 AND db_id = $2")
                .bind(&id)
                .bind(&self.db_id)
                .execute(&mut *tx)
                .await
                .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        }

        // Step 2: handle unrequested defaults — delete if empty, otherwise unflag.
        for (id, name, is_target, atom_count, children_count) in &top_level {
            let is_default = DEFAULT_NAMES.iter().any(|d| d.eq_ignore_ascii_case(name));
            let is_kept = keep_lower.contains(&name.to_lowercase());
            if !is_default || is_kept {
                continue;
            }
            if *atom_count == 0 && *children_count == 0 {
                sqlx::query("DELETE FROM tags WHERE id = $1 AND db_id = $2")
                    .bind(id)
                    .bind(&self.db_id)
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
            } else if *is_target {
                sqlx::query(
                    "UPDATE tags SET is_autotag_target = FALSE WHERE id = $1 AND db_id = $2",
                )
                .bind(id)
                .bind(&self.db_id)
                .execute(&mut *tx)
                .await
                .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
            }
        }

        // Step 3: custom additions — re-query each name since Step 2 may have deleted rows.
        let mut custom_tags: Vec<Tag> = Vec::new();
        for name in add_custom_names {
            let trimmed = name.trim();
            if trimmed.is_empty() {
                continue;
            }
            let existing_id: Option<(String,)> = sqlx::query_as(
                "SELECT id FROM tags WHERE parent_id IS NULL AND LOWER(name) = LOWER($1) AND db_id = $2 LIMIT 1",
            )
            .bind(trimmed)
            .bind(&self.db_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
            let id = match existing_id {
                Some((id,)) => id,
                None => {
                    let new_id = Uuid::new_v4().to_string();
                    sqlx::query(
                        "INSERT INTO tags (id, name, parent_id, created_at, is_autotag_target, db_id)
                         VALUES ($1, $2, NULL, $3, TRUE, $4)",
                    )
                    .bind(&new_id)
                    .bind(trimmed)
                    .bind(&now)
                    .bind(&self.db_id)
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
                    new_id
                }
            };
            sqlx::query("UPDATE tags SET is_autotag_target = TRUE WHERE id = $1 AND db_id = $2")
                .bind(&id)
                .bind(&self.db_id)
                .execute(&mut *tx)
                .await
                .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
            let row: (String, String, Option<String>, String, bool, String) = sqlx::query_as(
                "SELECT id, name, parent_id, created_at, is_autotag_target, autotag_description FROM tags WHERE id = $1 AND db_id = $2",
            )
            .bind(&id)
            .bind(&self.db_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
            custom_tags.push(Tag {
                id: row.0,
                name: row.1,
                parent_id: row.2,
                created_at: row.3,
                is_autotag_target: row.4,
                autotag_description: row.5,
            });
        }

        tx.commit()
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        Ok(custom_tags)
    }

    async fn delete_tag(&self, id: &str, recursive: bool) -> StorageResult<()> {
        if recursive {
            // Delete tag and all descendants via recursive CTE.
            // In Postgres, we use a CTE with DELETE.
            sqlx::query(
                "WITH RECURSIVE descendants(id) AS (
                    SELECT id FROM tags WHERE id = $1 AND db_id = $2
                    UNION ALL
                    SELECT t.id FROM tags t JOIN descendants d ON t.parent_id = d.id
                )
                DELETE FROM tags WHERE id IN (SELECT id FROM descendants) AND db_id = $2",
            )
            .bind(id)
            .bind(&self.db_id)
            .execute(&self.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        } else {
            sqlx::query("DELETE FROM tags WHERE id = $1 AND db_id = $2")
                .bind(id)
                .bind(&self.db_id)
                .execute(&self.pool)
                .await
                .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        }

        Ok(())
    }

    async fn get_related_tags(&self, tag_id: &str, limit: usize) -> StorageResult<Vec<RelatedTag>> {
        // Get tag hierarchy (this tag + all descendants) for exclusion
        let source_tag_ids: Vec<String> = sqlx::query_scalar(
            "WITH RECURSIVE descendant_tags(id) AS (
                SELECT id FROM tags WHERE id = $1 AND db_id = $2
                UNION ALL
                SELECT t.id FROM tags t
                INNER JOIN descendant_tags dt ON t.parent_id = dt.id
            )
            SELECT id FROM descendant_tags",
        )
        .bind(tag_id)
        .bind(&self.db_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        if source_tag_ids.is_empty() {
            return Ok(Vec::new());
        }

        let exclude_set: std::collections::HashSet<&str> =
            source_tag_ids.iter().map(|s| s.as_str()).collect();

        let mut tags: Vec<RelatedTag> = Vec::new();
        let mut tag_map: HashMap<String, usize> = HashMap::new();

        // === Signal 1: Shared atoms (co-occurrence) ===
        {
            let shared_limit = (limit * 3).max(30) as i32;
            let rows: Vec<(String, String, i64, i32)> = sqlx::query_as(
                "SELECT t.id, t.name, COUNT(DISTINCT at1.atom_id) as shared_count,
                        CASE WHEN wa.id IS NOT NULL THEN 1 ELSE 0 END as has_article
                 FROM atom_tags at1
                 JOIN atom_tags at2 ON at1.atom_id = at2.atom_id
                 JOIN tags t ON at2.tag_id = t.id
                 LEFT JOIN wiki_articles wa ON t.id = wa.tag_id
                 WHERE at1.tag_id IN (SELECT id FROM tags WHERE (id = $1 OR parent_id = $1) AND db_id = $3)
                   AND at2.tag_id NOT IN (SELECT id FROM tags WHERE (id = $1 OR parent_id = $1) AND db_id = $3)
                   AND t.parent_id IS NOT NULL
                 GROUP BY t.id, t.name, wa.id
                 ORDER BY shared_count DESC
                 LIMIT $2",
            )
            .bind(tag_id)
            .bind(shared_limit)
            .bind(&self.db_id)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

            for (tid, tname, shared_count, has_article_int) in rows {
                let shared_atoms = shared_count as i32;
                let rt = RelatedTag {
                    tag_id: tid.clone(),
                    tag_name: tname,
                    score: (shared_atoms as f64) * 0.4,
                    shared_atoms,
                    semantic_edges: 0,
                    has_article: has_article_int == 1,
                };
                tag_map.insert(tid, tags.len());
                tags.push(rt);
            }
        }

        // === Signal 2: Tag centroid embedding similarity ===
        // In Postgres with pgvector, we query the tag_embeddings table using <-> operator.
        let source_embedding: Option<Vec<f32>> = sqlx::query_scalar(
            "SELECT embedding::real[] FROM tag_embeddings WHERE tag_id = $1 AND db_id = $2",
        )
        .bind(tag_id)
        .bind(&self.db_id)
        .fetch_optional(&self.pool)
        .await
        .unwrap_or(None);

        if let Some(ref _source_emb) = source_embedding {
            let centroid_limit = (limit * 3).max(30) as i64;
            // Use pgvector's <-> (L2 distance) operator for nearest-neighbor search
            let centroid_rows: Vec<(String, f64)> = sqlx::query_as(
                "SELECT te.tag_id, te.embedding <-> (SELECT embedding FROM tag_embeddings WHERE tag_id = $1 AND db_id = $3) as distance
                 FROM tag_embeddings te
                 WHERE te.tag_id != $1 AND te.db_id = $3
                 ORDER BY distance
                 LIMIT $2",
            )
            .bind(tag_id)
            .bind(centroid_limit)
            .bind(&self.db_id)
            .fetch_all(&self.pool)
            .await
            .unwrap_or_default();

            let mut new_candidates: Vec<(String, f64)> = Vec::new();
            for (candidate_tag_id, distance) in &centroid_rows {
                if exclude_set.contains(candidate_tag_id.as_str()) {
                    continue;
                }
                // Convert L2 distance to similarity: 1 - (d^2 / 2) for normalized vectors
                let centroid_sim = 1.0 - (distance * distance / 2.0);
                if centroid_sim < 0.3 {
                    continue;
                }
                let centroid_score = centroid_sim * 0.6;

                if let Some(&idx) = tag_map.get(candidate_tag_id) {
                    tags[idx].score += centroid_score;
                } else {
                    new_candidates.push((candidate_tag_id.clone(), centroid_score));
                }
            }

            // Batch lookup metadata for new centroid-only candidates
            if !new_candidates.is_empty() {
                let placeholders: Vec<String> = (2..=new_candidates.len() + 1)
                    .map(|i| format!("${}", i))
                    .collect();
                let query = format!(
                    "SELECT t.id, t.name, CASE WHEN wa.id IS NOT NULL THEN 1 ELSE 0 END
                     FROM tags t
                     LEFT JOIN wiki_articles wa ON t.id = wa.tag_id
                     WHERE t.db_id = $1 AND t.id IN ({}) AND t.parent_id IS NOT NULL",
                    placeholders.join(", ")
                );

                let mut q = sqlx::query_as::<_, (String, String, i32)>(&query);
                q = q.bind(&self.db_id);
                for (cid, _) in &new_candidates {
                    q = q.bind(cid);
                }

                let meta_rows = q.fetch_all(&self.pool).await.unwrap_or_default();

                let score_map: HashMap<&str, f64> = new_candidates
                    .iter()
                    .map(|(id, score)| (id.as_str(), *score))
                    .collect();

                for (id, name, has_article_int) in meta_rows {
                    let centroid_score = score_map.get(id.as_str()).copied().unwrap_or(0.0);
                    tag_map.insert(id.clone(), tags.len());
                    tags.push(RelatedTag {
                        tag_id: id,
                        tag_name: name,
                        score: centroid_score,
                        shared_atoms: 0,
                        semantic_edges: 0,
                        has_article: has_article_int == 1,
                    });
                }
            }
        }

        // === Signal 3: Content mentions ===
        // Tags whose names appear in this tag's wiki article content.
        let article_content: Option<String> = sqlx::query_scalar(
            "SELECT content FROM wiki_articles WHERE tag_id = $1 AND db_id = $2",
        )
        .bind(tag_id)
        .bind(&self.db_id)
        .fetch_optional(&self.pool)
        .await
        .unwrap_or(None);

        if let Some(content) = article_content {
            let content_lower = content.to_lowercase();

            // Build exclusion placeholders (reserve $1 for db_id)
            let placeholders: Vec<String> = (2..=source_tag_ids.len() + 1)
                .map(|i| format!("${}", i))
                .collect();
            let mention_query = format!(
                "SELECT t.id, t.name,
                        CASE WHEN wa.id IS NOT NULL THEN 1 ELSE 0 END as has_article
                 FROM tags t
                 LEFT JOIN wiki_articles wa ON t.id = wa.tag_id
                 WHERE t.db_id = $1
                   AND t.parent_id IS NOT NULL
                   AND t.id NOT IN ({})
                   AND length(t.name) >= 3
                   AND t.name ~ '[^0-9]'",
                placeholders.join(", ")
            );

            let mut q = sqlx::query_as::<_, (String, String, i32)>(&mention_query);
            q = q.bind(&self.db_id);
            for tid in &source_tag_ids {
                q = q.bind(tid);
            }

            let candidate_tags = q.fetch_all(&self.pool).await.unwrap_or_default();

            // Filter by whole-word name match in content
            let matched_tags: Vec<(String, String, bool)> = candidate_tags
                .into_iter()
                .filter(|(_, name, _)| {
                    let name_lower = name.to_lowercase();
                    if let Some(pos) = content_lower.find(&name_lower) {
                        let before_ok =
                            pos == 0 || !content_lower.as_bytes()[pos - 1].is_ascii_alphanumeric();
                        let end = pos + name_lower.len();
                        let after_ok = end >= content_lower.len()
                            || !content_lower.as_bytes()[end].is_ascii_alphanumeric();
                        before_ok && after_ok
                    } else {
                        false
                    }
                })
                .map(|(id, name, ha)| (id, name, ha == 1))
                .collect();

            for (tid, tname, has_article) in matched_tags {
                if !tag_map.contains_key(&tid) {
                    tag_map.insert(tid.clone(), tags.len());
                    tags.push(RelatedTag {
                        tag_id: tid,
                        tag_name: tname,
                        score: 0.1, // small boost for content mention
                        shared_atoms: 0,
                        semantic_edges: 0,
                        has_article,
                    });
                }
            }
        }

        // Sort by score and truncate
        tags.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        tags.truncate(limit);

        Ok(tags)
    }

    async fn get_tags_for_compaction(&self) -> StorageResult<String> {
        let rows: Vec<(String, Option<String>)> = sqlx::query_as(
            "SELECT t.name, p.name as parent_name
             FROM tags t
             LEFT JOIN tags p ON t.parent_id = p.id
             WHERE t.db_id = $1
             ORDER BY COALESCE(p.name, t.name), t.name",
        )
        .bind(&self.db_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        if rows.is_empty() {
            return Ok("(no existing tags)".to_string());
        }

        let mut result = String::new();
        let mut current_parent: Option<String> = None;

        for (name, parent) in rows {
            match (&parent, &current_parent) {
                (Some(p), Some(cp)) if p == cp => {
                    result.push_str(&format!("  - {}\n", name));
                }
                (Some(p), _) => {
                    result.push_str(&format!("{}\n", p));
                    result.push_str(&format!("  - {}\n", name));
                    current_parent = Some(p.clone());
                }
                (None, _) => {
                    result.push_str(&format!("{}\n", name));
                    current_parent = None;
                }
            }
        }

        Ok(result.trim_end().to_string())
    }

    async fn get_or_create_tag(
        &self,
        name: &str,
        parent_name: Option<&str>,
    ) -> StorageResult<String> {
        let trimmed_name = name.trim();

        // Validate tag name
        if trimmed_name.is_empty() || trimmed_name.eq_ignore_ascii_case("null") {
            return Err(AtomicCoreError::DatabaseOperation(format!(
                "Invalid tag name: '{}'",
                name
            )));
        }

        // Try to find existing tag (case-insensitive)
        let existing_id: Option<String> =
            sqlx::query_scalar("SELECT id FROM tags WHERE LOWER(name) = LOWER($1) AND db_id = $2")
                .bind(trimmed_name)
                .bind(&self.db_id)
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        if let Some(id) = existing_id {
            return Ok(id);
        }

        // Tag doesn't exist - require a valid parent for new tags
        let parent = parent_name.ok_or_else(|| {
            AtomicCoreError::DatabaseOperation(format!(
                "New tag '{}' requires a parent category",
                trimmed_name
            ))
        })?;

        let trimmed_parent = parent.trim();
        if trimmed_parent.is_empty() || trimmed_parent.eq_ignore_ascii_case("null") {
            return Err(AtomicCoreError::DatabaseOperation(format!(
                "New tag '{}' requires a valid parent category",
                trimmed_name
            )));
        }

        // Parent must be an existing top-level tag (parent_id IS NULL)
        let parent_id: String = sqlx::query_scalar(
            "SELECT id FROM tags WHERE LOWER(name) = LOWER($1) AND parent_id IS NULL AND db_id = $2",
        )
        .bind(trimmed_parent)
        .bind(&self.db_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?
        .ok_or_else(|| {
            AtomicCoreError::DatabaseOperation(format!(
                "Parent '{}' is not a valid top-level category for tag '{}'",
                trimmed_parent, trimmed_name,
            ))
        })?;

        // Create the tag under the validated parent, handling concurrent inserts
        let tag_id = Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();

        // Use ON CONFLICT to handle TOCTOU race with parallel auto-tagging
        let actual_id: String = sqlx::query_scalar(
            "INSERT INTO tags (id, name, parent_id, created_at, db_id) VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (LOWER(name), COALESCE(parent_id, ''), db_id)
             DO UPDATE SET name = tags.name  -- no-op update to return the row
             RETURNING id",
        )
        .bind(&tag_id)
        .bind(trimmed_name)
        .bind(&parent_id)
        .bind(&now)
        .bind(&self.db_id)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| {
            AtomicCoreError::DatabaseOperation(format!(
                "Failed to create tag '{}': {}",
                trimmed_name, e
            ))
        })?;

        Ok(actual_id)
    }

    async fn link_tags_to_atom(&self, atom_id: &str, tag_ids: &[String]) -> StorageResult<()> {
        for tag_id in tag_ids {
            // Auto-tagger path. Existing rows keep their source via ON CONFLICT
            // DO NOTHING — we don't want to silently demote a manual assignment.
            sqlx::query(
                "INSERT INTO atom_tags (atom_id, tag_id, db_id, source) VALUES ($1, $2, $3, 'auto') ON CONFLICT DO NOTHING",
            )
            .bind(atom_id)
            .bind(tag_id)
            .bind(&self.db_id)
            .execute(&self.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(
                format!("Failed to link tag to atom: {}", e),
            ))?;
        }
        Ok(())
    }

    async fn get_tag_tree_for_llm(&self) -> StorageResult<String> {
        // Step 1: Get top-level category tags flagged as auto-tag targets.
        let top_level_tags: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT id, name, autotag_description
             FROM tags
             WHERE parent_id IS NULL AND is_autotag_target = TRUE AND db_id = $1
             ORDER BY name",
        )
        .bind(&self.db_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        if top_level_tags.is_empty() {
            return Ok("(no existing tags)".to_string());
        }

        // Step 2: For each top-level tag, get top 50 most-used child tags by atom count
        let mut result = String::new();

        for (parent_id, parent_name, description) in &top_level_tags {
            result.push_str(parent_name);
            result.push('\n');
            let description = description.trim();
            if !description.is_empty() {
                result.push_str("Description: ");
                result.push_str(description);
                result.push('\n');
            }

            // Query top 50 children by atom count
            let children: Vec<(String,)> = sqlx::query_as(
                "SELECT t.name
                 FROM tags t
                 LEFT JOIN atom_tags at ON t.id = at.tag_id
                 WHERE t.parent_id = $1 AND t.db_id = $2
                 GROUP BY t.id, t.name
                 ORDER BY COUNT(at.atom_id) DESC, t.name ASC
                 LIMIT 50",
            )
            .bind(parent_id)
            .bind(&self.db_id)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

            // Add children with tree formatting
            for (j, (child_name,)) in children.iter().enumerate() {
                let is_last_child = j == children.len() - 1;
                let connector = if is_last_child {
                    "\u{2514}\u{2500}\u{2500} "
                } else {
                    "\u{251c}\u{2500}\u{2500} "
                };
                result.push_str(connector);
                result.push_str(child_name);
                result.push('\n');
            }
        }

        Ok(result.trim_end().to_string())
    }

    async fn compute_tag_centroids_batch(&self, tag_ids: &[String]) -> StorageResult<()> {
        use pgvector::Vector;

        for tag_id in tag_ids {
            // Get all descendant tag IDs (recursive CTE)
            let descendant_ids: Vec<(String,)> = sqlx::query_as(
                "WITH RECURSIVE descendant_tags(id) AS (
                    SELECT id FROM tags WHERE id = $1 AND db_id = $2
                    UNION ALL
                    SELECT t.id FROM tags t
                    INNER JOIN descendant_tags dt ON t.parent_id = dt.id
                )
                SELECT id FROM descendant_tags",
            )
            .bind(tag_id)
            .bind(&self.db_id)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| {
                AtomicCoreError::DatabaseOperation(format!("Failed to get tag descendants: {}", e))
            })?;

            let desc_ids: Vec<String> = descendant_ids.into_iter().map(|(id,)| id).collect();

            // Get all chunk embeddings for atoms tagged with any descendant tag
            let embeddings: Vec<(Vector,)> = sqlx::query_as(
                "SELECT ac.embedding
                 FROM atom_chunks ac
                 INNER JOIN atom_tags at ON ac.atom_id = at.atom_id
                 WHERE at.tag_id = ANY($1) AND ac.embedding IS NOT NULL AND ac.db_id = $2",
            )
            .bind(&desc_ids)
            .bind(&self.db_id)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| {
                AtomicCoreError::DatabaseOperation(format!(
                    "Failed to get embeddings for tag centroid: {}",
                    e
                ))
            })?;

            if embeddings.is_empty() {
                continue;
            }

            // Compute centroid (average of all embeddings)
            let embedding_vecs: Vec<Vec<f32>> =
                embeddings.into_iter().map(|(v,)| v.to_vec()).collect();

            let dim = embedding_vecs[0].len();
            let mut centroid = vec![0.0f32; dim];
            let n = embedding_vecs.len() as f32;

            for emb in &embedding_vecs {
                for (i, val) in emb.iter().enumerate() {
                    if i < dim {
                        centroid[i] += val;
                    }
                }
            }
            for val in centroid.iter_mut() {
                *val /= n;
            }

            // Normalize the centroid
            let magnitude: f32 = centroid.iter().map(|v| v * v).sum::<f32>().sqrt();
            if magnitude > 0.0 {
                for val in centroid.iter_mut() {
                    *val /= magnitude;
                }
            }

            // Save the centroid
            let pg_embedding = Vector::from(centroid);
            sqlx::query(
                "INSERT INTO tag_embeddings (tag_id, embedding, db_id)
                 VALUES ($1, $2, $3)
                 ON CONFLICT (tag_id, db_id) DO UPDATE SET embedding = EXCLUDED.embedding",
            )
            .bind(tag_id)
            .bind(&pg_embedding)
            .bind(&self.db_id)
            .execute(&self.pool)
            .await
            .map_err(|e| {
                AtomicCoreError::DatabaseOperation(format!("Failed to save tag centroid: {}", e))
            })?;
        }

        Ok(())
    }

    async fn cleanup_orphaned_parents(&self, tag_id: &str) -> StorageResult<()> {
        // Get parent of this tag
        let parent_id: Option<String> =
            sqlx::query_scalar("SELECT parent_id FROM tags WHERE id = $1 AND db_id = $2")
                .bind(tag_id)
                .bind(&self.db_id)
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?
                .flatten();

        if let Some(parent) = parent_id {
            // Check if parent has any children left
            let child_count: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM tags WHERE parent_id = $1 AND db_id = $2")
                    .bind(&parent)
                    .bind(&self.db_id)
                    .fetch_one(&self.pool)
                    .await
                    .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

            // Check if parent is linked to any atoms
            let atom_count: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM atom_tags WHERE tag_id = $1 AND db_id = $2",
            )
            .bind(&parent)
            .bind(&self.db_id)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

            // Check if parent has a wiki article
            let has_wiki: bool = sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS(SELECT 1 FROM wiki_articles WHERE tag_id = $1 AND db_id = $2)",
            )
            .bind(&parent)
            .bind(&self.db_id)
            .fetch_one(&self.pool)
            .await
            .unwrap_or(false);

            // If parent is unused and has no wiki, delete it and recurse
            if child_count == 0 && atom_count == 0 && !has_wiki {
                tracing::debug!(parent = %parent, "Cleaning up orphaned parent tag");
                sqlx::query("DELETE FROM tags WHERE id = $1 AND db_id = $2")
                    .bind(&parent)
                    .bind(&self.db_id)
                    .execute(&self.pool)
                    .await
                    .map_err(|e| {
                        AtomicCoreError::DatabaseOperation(format!(
                            "Failed to delete orphaned parent: {}",
                            e
                        ))
                    })?;
                // Recurse to grandparent using Box::pin for async recursion
                Box::pin(self.cleanup_orphaned_parents(&parent)).await?;
            }
        }

        Ok(())
    }

    async fn apply_tag_merges(&self, merges: &[TagMerge]) -> StorageResult<CompactionResult> {
        let mut tags_merged = 0;
        let mut atoms_retagged = 0;
        let mut errors = Vec::new();

        for merge in merges {
            match self.execute_tag_merge(merge).await {
                Ok((true, retagged)) => {
                    tags_merged += 1;
                    atoms_retagged += retagged;
                }
                Ok((false, _)) => {}
                Err(e) => errors.push(format!(
                    "Error merging '{}' -> '{}': {}",
                    merge.loser_name, merge.winner_name, e
                )),
            }
        }

        if !errors.is_empty() {
            tracing::error!(errors = ?errors, "Merge errors");
        }

        Ok(CompactionResult {
            tags_merged,
            atoms_retagged,
        })
    }

    async fn get_tag_hierarchy(&self, tag_id: &str) -> StorageResult<Vec<String>> {
        let rows: Vec<String> = sqlx::query_scalar(
            "WITH RECURSIVE descendant_tags(id) AS (
                SELECT id FROM tags WHERE id = $1 AND db_id = $2
                UNION ALL
                SELECT t.id FROM tags t
                INNER JOIN descendant_tags dt ON t.parent_id = dt.id
            )
            SELECT id FROM descendant_tags",
        )
        .bind(tag_id)
        .bind(&self.db_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        Ok(rows)
    }

    async fn count_atoms_with_tags(
        &self,
        tag_ids: &[String],
        kinds: &crate::models::KindFilter,
    ) -> StorageResult<i32> {
        if tag_ids.is_empty() {
            return Ok(0);
        }
        // Join through atoms so the kind filter can be applied. db_id keeps
        // the count scoped to the active database. Postgres uses ANY for the
        // tag_ids list and (when filtering) for the kinds list.
        let kind_predicate = kinds.postgres_predicate("a.kind", "$3");
        let sql = format!(
            "SELECT COUNT(DISTINCT at.atom_id)
             FROM atom_tags at
             INNER JOIN atoms a ON a.id = at.atom_id AND a.db_id = at.db_id
             WHERE at.tag_id = ANY($1) AND at.db_id = $2 AND {kind_predicate}"
        );
        let mut q = sqlx::query_scalar::<_, Option<i64>>(&sql)
            .bind(tag_ids)
            .bind(&self.db_id);
        if kinds.has_bind_value() {
            q = q.bind(kinds.kind_strings());
        }
        let count = q
            .fetch_one(&self.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        Ok(count.unwrap_or(0) as i32)
    }
}
