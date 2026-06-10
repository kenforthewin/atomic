use std::collections::HashMap;

use super::PostgresStorage;
use crate::error::AtomicCoreError;
use crate::models::*;
use crate::storage::traits::*;
use async_trait::async_trait;

#[async_trait]
impl FeedStore for PostgresStorage {
    async fn create_feed(
        &self,
        url: &str,
        title: Option<&str>,
        site_url: Option<&str>,
        poll_interval: i32,
        tag_ids: &[String],
    ) -> StorageResult<Feed> {
        // Check uniqueness
        let exists: Option<bool> = sqlx::query_scalar::<_, Option<bool>>(
            "SELECT EXISTS(SELECT 1 FROM feeds WHERE url = $1 AND db_id = $2)",
        )
        .bind(url)
        .bind(&self.db_id)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        let exists = exists.unwrap_or(false);

        if exists {
            return Err(AtomicCoreError::Validation(format!(
                "Feed already exists: {}",
                url
            )));
        }

        let id = uuid::Uuid::new_v4().to_string();
        let now = chrono::Utc::now().to_rfc3339();

        sqlx::query(
            "INSERT INTO feeds (id, url, title, site_url, poll_interval, created_at, db_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(&id)
        .bind(url)
        .bind(title)
        .bind(site_url)
        .bind(poll_interval)
        .bind(&now)
        .bind(&self.db_id)
        .execute(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        for tag_id in tag_ids {
            sqlx::query("INSERT INTO feed_tags (feed_id, tag_id, db_id) VALUES ($1, $2, $3)")
                .bind(&id)
                .bind(tag_id)
                .bind(&self.db_id)
                .execute(&self.pool)
                .await
                .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        }

        Ok(Feed {
            id,
            url: url.to_string(),
            title: title.map(|s| s.to_string()),
            site_url: site_url.map(|s| s.to_string()),
            poll_interval,
            last_polled_at: None,
            last_error: None,
            created_at: now,
            is_paused: false,
            tag_ids: tag_ids.to_vec(),
        })
    }

    async fn list_feeds(&self) -> StorageResult<Vec<Feed>> {
        let rows = sqlx::query_as::<_, (String, String, Option<String>, Option<String>, i32, Option<String>, Option<String>, String, bool)>(
            "SELECT id, url, title, site_url, poll_interval, last_polled_at, last_error, created_at, is_paused::int::boolean
             FROM feeds WHERE db_id = $1 ORDER BY created_at DESC",
        )
        .bind(&self.db_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        let mut feeds: Vec<Feed> = rows
            .into_iter()
            .map(
                |(
                    id,
                    url,
                    title,
                    site_url,
                    poll_interval,
                    last_polled_at,
                    last_error,
                    created_at,
                    is_paused,
                )| {
                    Feed {
                        id,
                        url,
                        title,
                        site_url,
                        poll_interval,
                        last_polled_at,
                        last_error,
                        created_at,
                        is_paused,
                        tag_ids: vec![],
                    }
                },
            )
            .collect();

        // Batch load feed tags
        let tag_rows = sqlx::query_as::<_, (String, String)>(
            "SELECT feed_id, tag_id FROM feed_tags WHERE db_id = $1",
        )
        .bind(&self.db_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        let mut tag_map: HashMap<String, Vec<String>> = HashMap::new();
        for (feed_id, tag_id) in tag_rows {
            tag_map.entry(feed_id).or_default().push(tag_id);
        }

        for feed in &mut feeds {
            feed.tag_ids = tag_map.remove(&feed.id).unwrap_or_default();
        }

        Ok(feeds)
    }

    async fn get_feed(&self, id: &str) -> StorageResult<Feed> {
        let row = sqlx::query_as::<_, (String, String, Option<String>, Option<String>, i32, Option<String>, Option<String>, String, bool)>(
            "SELECT id, url, title, site_url, poll_interval, last_polled_at, last_error, created_at, is_paused::int::boolean
             FROM feeds WHERE id = $1 AND db_id = $2",
        )
        .bind(id)
        .bind(&self.db_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?
        .ok_or_else(|| AtomicCoreError::NotFound(format!("Feed not found: {}", id)))?;

        let tag_ids: Vec<String> =
            sqlx::query_scalar("SELECT tag_id FROM feed_tags WHERE feed_id = $1 AND db_id = $2")
                .bind(id)
                .bind(&self.db_id)
                .fetch_all(&self.pool)
                .await
                .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        Ok(Feed {
            id: row.0,
            url: row.1,
            title: row.2,
            site_url: row.3,
            poll_interval: row.4,
            last_polled_at: row.5,
            last_error: row.6,
            created_at: row.7,
            is_paused: row.8,
            tag_ids,
        })
    }

    async fn update_feed(
        &self,
        id: &str,
        title: Option<&str>,
        poll_interval: Option<i32>,
        is_paused: Option<bool>,
        tag_ids: Option<&[String]>,
    ) -> StorageResult<Feed> {
        // Verify feed exists
        let exists: Option<bool> = sqlx::query_scalar::<_, Option<bool>>(
            "SELECT EXISTS(SELECT 1 FROM feeds WHERE id = $1 AND db_id = $2)",
        )
        .bind(id)
        .bind(&self.db_id)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        let exists = exists.unwrap_or(false);

        if !exists {
            return Err(AtomicCoreError::NotFound(format!("Feed not found: {}", id)));
        }

        if let Some(t) = title {
            sqlx::query("UPDATE feeds SET title = $1 WHERE id = $2 AND db_id = $3")
                .bind(t)
                .bind(id)
                .bind(&self.db_id)
                .execute(&self.pool)
                .await
                .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        }

        if let Some(interval) = poll_interval {
            sqlx::query("UPDATE feeds SET poll_interval = $1 WHERE id = $2 AND db_id = $3")
                .bind(interval)
                .bind(id)
                .bind(&self.db_id)
                .execute(&self.pool)
                .await
                .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        }

        if let Some(paused) = is_paused {
            sqlx::query("UPDATE feeds SET is_paused = $1 WHERE id = $2 AND db_id = $3")
                .bind(if paused { 1i32 } else { 0i32 })
                .bind(id)
                .bind(&self.db_id)
                .execute(&self.pool)
                .await
                .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        }

        if let Some(tags) = tag_ids {
            sqlx::query("DELETE FROM feed_tags WHERE feed_id = $1 AND db_id = $2")
                .bind(id)
                .bind(&self.db_id)
                .execute(&self.pool)
                .await
                .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

            for tag_id in tags {
                sqlx::query("INSERT INTO feed_tags (feed_id, tag_id, db_id) VALUES ($1, $2, $3)")
                    .bind(id)
                    .bind(tag_id)
                    .bind(&self.db_id)
                    .execute(&self.pool)
                    .await
                    .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
            }
        }

        self.get_feed(id).await
    }

    async fn delete_feed(&self, id: &str) -> StorageResult<()> {
        let result = sqlx::query("DELETE FROM feeds WHERE id = $1 AND db_id = $2")
            .bind(id)
            .bind(&self.db_id)
            .execute(&self.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        if result.rows_affected() == 0 {
            return Err(AtomicCoreError::NotFound(format!("Feed not found: {}", id)));
        }

        Ok(())
    }

    async fn get_due_feeds(&self) -> StorageResult<Vec<Feed>> {
        // Select non-paused feeds
        let rows = sqlx::query_as::<_, (String, String, Option<String>, Option<String>, i32, Option<String>, Option<String>, String, bool)>(
            "SELECT id, url, title, site_url, poll_interval, last_polled_at, last_error, created_at, is_paused::int::boolean
             FROM feeds WHERE is_paused = 0 AND db_id = $1",
        )
        .bind(&self.db_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        let now = chrono::Utc::now();

        let feeds: Vec<Feed> = rows
            .into_iter()
            .map(
                |(
                    id,
                    url,
                    title,
                    site_url,
                    poll_interval,
                    last_polled_at,
                    last_error,
                    created_at,
                    is_paused,
                )| {
                    Feed {
                        id,
                        url,
                        title,
                        site_url,
                        poll_interval,
                        last_polled_at,
                        last_error,
                        created_at,
                        is_paused,
                        tag_ids: vec![],
                    }
                },
            )
            .collect();

        // Filter to due feeds based on poll_interval
        let due_feeds: Vec<Feed> = feeds
            .into_iter()
            .filter(|f| match &f.last_polled_at {
                None => true,
                Some(ts) => {
                    if let Ok(last) = chrono::DateTime::parse_from_rfc3339(ts) {
                        let elapsed = now.signed_duration_since(last);
                        elapsed.num_minutes() >= f.poll_interval as i64
                    } else {
                        true
                    }
                }
            })
            .collect();

        if due_feeds.is_empty() {
            return Ok(due_feeds);
        }

        // Batch load tags for due feeds
        let feed_ids: Vec<String> = due_feeds.iter().map(|f| f.id.clone()).collect();

        let tag_rows = sqlx::query_as::<_, (String, String)>(
            "SELECT feed_id, tag_id FROM feed_tags WHERE feed_id = ANY($1) AND db_id = $2",
        )
        .bind(&feed_ids)
        .bind(&self.db_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        let mut tag_map: HashMap<String, Vec<String>> = HashMap::new();
        for (feed_id, tag_id) in tag_rows {
            tag_map.entry(feed_id).or_default().push(tag_id);
        }

        Ok(due_feeds
            .into_iter()
            .map(|mut f| {
                f.tag_ids = tag_map.remove(&f.id).unwrap_or_default();
                f
            })
            .collect())
    }

    async fn mark_feed_polled(&self, id: &str, error: Option<&str>) -> StorageResult<()> {
        let now = chrono::Utc::now().to_rfc3339();

        match error {
            Some(err) => {
                sqlx::query(
                    "UPDATE feeds SET last_polled_at = $1, last_error = $2 WHERE id = $3 AND db_id = $4",
                )
                .bind(&now)
                .bind(err)
                .bind(id)
                .bind(&self.db_id)
                .execute(&self.pool)
                .await
                .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
            }
            None => {
                sqlx::query(
                    "UPDATE feeds SET last_polled_at = $1, last_error = NULL WHERE id = $2 AND db_id = $3",
                )
                .bind(&now)
                .bind(id)
                .bind(&self.db_id)
                .execute(&self.pool)
                .await
                .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
            }
        }

        Ok(())
    }

    async fn set_feed_error(&self, id: &str, error: &str) -> StorageResult<()> {
        sqlx::query("UPDATE feeds SET last_error = $1 WHERE id = $2 AND db_id = $3")
            .bind(error)
            .bind(id)
            .bind(&self.db_id)
            .execute(&self.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        Ok(())
    }

    async fn claim_feed_item(&self, feed_id: &str, guid: &str) -> StorageResult<bool> {
        let now = chrono::Utc::now().to_rfc3339();

        let result = sqlx::query(
            "INSERT INTO feed_items (feed_id, guid, skipped, seen_at, db_id)
             VALUES ($1, $2, 0, $3, $4)
             ON CONFLICT DO NOTHING",
        )
        .bind(feed_id)
        .bind(guid)
        .bind(&now)
        .bind(&self.db_id)
        .execute(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        Ok(result.rows_affected() > 0)
    }

    async fn complete_feed_item(
        &self,
        feed_id: &str,
        guid: &str,
        atom_id: &str,
    ) -> StorageResult<()> {
        sqlx::query(
            "UPDATE feed_items SET atom_id = $1 WHERE feed_id = $2 AND guid = $3 AND db_id = $4",
        )
        .bind(atom_id)
        .bind(feed_id)
        .bind(guid)
        .bind(&self.db_id)
        .execute(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        Ok(())
    }

    async fn mark_feed_item_skipped(
        &self,
        feed_id: &str,
        guid: &str,
        reason: &str,
    ) -> StorageResult<()> {
        sqlx::query(
            "UPDATE feed_items SET skipped = 1, skip_reason = $1 WHERE feed_id = $2 AND guid = $3 AND db_id = $4",
        )
        .bind(reason)
        .bind(feed_id)
        .bind(guid)
        .bind(&self.db_id)
        .execute(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        Ok(())
    }

    async fn backfill_feed_metadata(
        &self,
        id: &str,
        title: Option<&str>,
        site_url: Option<&str>,
    ) -> StorageResult<()> {
        if let Some(t) = title {
            sqlx::query(
                "UPDATE feeds SET title = COALESCE(title, $1) WHERE id = $2 AND db_id = $3",
            )
            .bind(t)
            .bind(id)
            .bind(&self.db_id)
            .execute(&self.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        }
        if let Some(s) = site_url {
            sqlx::query(
                "UPDATE feeds SET site_url = COALESCE(site_url, $1) WHERE id = $2 AND db_id = $3",
            )
            .bind(s)
            .bind(id)
            .bind(&self.db_id)
            .execute(&self.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        }
        Ok(())
    }
}
