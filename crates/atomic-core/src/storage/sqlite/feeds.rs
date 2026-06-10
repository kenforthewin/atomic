use std::collections::HashMap;

use super::SqliteStorage;
use crate::error::AtomicCoreError;
use crate::models::*;
use crate::storage::traits::*;
use async_trait::async_trait;
use uuid::Uuid;

impl SqliteStorage {
    pub(crate) fn create_feed_sync(
        &self,
        url: &str,
        title: Option<&str>,
        site_url: Option<&str>,
        poll_interval: i32,
        tag_ids: &[String],
    ) -> StorageResult<Feed> {
        let conn = self
            .db
            .conn
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;

        // Check uniqueness
        let exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM feeds WHERE url = ?1)",
                [url],
                |row| row.get(0),
            )
            .unwrap_or(false);
        if exists {
            return Err(AtomicCoreError::Validation(format!(
                "Feed already exists: {}",
                url
            )));
        }

        let id = Uuid::new_v4().to_string();
        let now = chrono::Utc::now().to_rfc3339();

        conn.execute(
            "INSERT INTO feeds (id, url, title, site_url, poll_interval, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![&id, url, title, site_url, poll_interval, &now],
        )?;

        for tag_id in tag_ids {
            conn.execute(
                "INSERT INTO feed_tags (feed_id, tag_id) VALUES (?1, ?2)",
                rusqlite::params![&id, tag_id],
            )?;
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

    pub(crate) fn list_feeds_sync(&self) -> StorageResult<Vec<Feed>> {
        let conn = self.db.read_conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, url, title, site_url, poll_interval, last_polled_at, last_error, created_at, is_paused
             FROM feeds ORDER BY created_at DESC",
        )?;

        let feeds: Vec<Feed> = stmt
            .query_map([], |row| {
                Ok(Feed {
                    id: row.get(0)?,
                    url: row.get(1)?,
                    title: row.get(2)?,
                    site_url: row.get(3)?,
                    poll_interval: row.get(4)?,
                    last_polled_at: row.get(5)?,
                    last_error: row.get(6)?,
                    created_at: row.get(7)?,
                    is_paused: row.get(8)?,
                    tag_ids: vec![],
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        // Batch load feed tags
        let mut tag_stmt = conn.prepare("SELECT feed_id, tag_id FROM feed_tags")?;
        let tag_pairs: Vec<(String, String)> = tag_stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;

        let mut tag_map: HashMap<String, Vec<String>> = HashMap::new();
        for (feed_id, tag_id) in tag_pairs {
            tag_map.entry(feed_id).or_default().push(tag_id);
        }

        Ok(feeds
            .into_iter()
            .map(|mut f| {
                f.tag_ids = tag_map.remove(&f.id).unwrap_or_default();
                f
            })
            .collect())
    }

    pub(crate) fn get_feed_sync(&self, id: &str) -> StorageResult<Feed> {
        let conn = self.db.read_conn()?;
        let feed = conn
            .query_row(
                "SELECT id, url, title, site_url, poll_interval, last_polled_at, last_error, created_at, is_paused
                 FROM feeds WHERE id = ?1",
                [id],
                |row| {
                    Ok(Feed {
                        id: row.get(0)?,
                        url: row.get(1)?,
                        title: row.get(2)?,
                        site_url: row.get(3)?,
                        poll_interval: row.get(4)?,
                        last_polled_at: row.get(5)?,
                        last_error: row.get(6)?,
                        created_at: row.get(7)?,
                        is_paused: row.get(8)?,
                        tag_ids: vec![],
                    })
                },
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    AtomicCoreError::NotFound(format!("Feed not found: {}", id))
                }
                _ => AtomicCoreError::Database(e),
            })?;

        let mut tag_stmt = conn.prepare("SELECT tag_id FROM feed_tags WHERE feed_id = ?1")?;
        let tag_ids: Vec<String> = tag_stmt
            .query_map([id], |row| row.get(0))?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Feed { tag_ids, ..feed })
    }

    pub(crate) fn update_feed_sync(
        &self,
        id: &str,
        title: Option<&str>,
        poll_interval: Option<i32>,
        is_paused: Option<bool>,
        tag_ids: Option<&[String]>,
    ) -> StorageResult<Feed> {
        let conn = self
            .db
            .conn
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;

        // Verify feed exists
        let exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM feeds WHERE id = ?1)",
                [id],
                |row| row.get(0),
            )
            .unwrap_or(false);
        if !exists {
            return Err(AtomicCoreError::NotFound(format!("Feed not found: {}", id)));
        }

        if let Some(t) = title {
            conn.execute(
                "UPDATE feeds SET title = ?1 WHERE id = ?2",
                rusqlite::params![t, id],
            )?;
        }

        if let Some(interval) = poll_interval {
            conn.execute(
                "UPDATE feeds SET poll_interval = ?1 WHERE id = ?2",
                rusqlite::params![interval, id],
            )?;
        }

        if let Some(paused) = is_paused {
            conn.execute(
                "UPDATE feeds SET is_paused = ?1 WHERE id = ?2",
                rusqlite::params![paused, id],
            )?;
        }

        if let Some(tags) = tag_ids {
            conn.execute("DELETE FROM feed_tags WHERE feed_id = ?1", [id])?;
            for tag_id in tags {
                conn.execute(
                    "INSERT INTO feed_tags (feed_id, tag_id) VALUES (?1, ?2)",
                    rusqlite::params![id, tag_id],
                )?;
            }
        }

        drop(conn);
        self.get_feed_sync(id)
    }

    pub(crate) fn delete_feed_sync(&self, id: &str) -> StorageResult<()> {
        let conn = self
            .db
            .conn
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        let changes = conn.execute("DELETE FROM feeds WHERE id = ?1", [id])?;
        if changes == 0 {
            return Err(AtomicCoreError::NotFound(format!("Feed not found: {}", id)));
        }
        Ok(())
    }

    pub(crate) fn get_due_feeds_sync(&self) -> StorageResult<Vec<Feed>> {
        let conn = self.db.read_conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, url, title, site_url, poll_interval, last_polled_at, last_error, created_at, is_paused
             FROM feeds WHERE is_paused = 0",
        )?;

        let now = chrono::Utc::now();
        let feeds: Vec<Feed> = stmt
            .query_map([], |row| {
                Ok(Feed {
                    id: row.get(0)?,
                    url: row.get(1)?,
                    title: row.get(2)?,
                    site_url: row.get(3)?,
                    poll_interval: row.get(4)?,
                    last_polled_at: row.get(5)?,
                    last_error: row.get(6)?,
                    created_at: row.get(7)?,
                    is_paused: row.get(8)?,
                    tag_ids: vec![],
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

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

        // Batch load tags for due feeds
        if due_feeds.is_empty() {
            return Ok(due_feeds);
        }

        let mut tag_stmt = conn.prepare("SELECT feed_id, tag_id FROM feed_tags")?;
        let tag_pairs: Vec<(String, String)> = tag_stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;

        let mut tag_map: HashMap<String, Vec<String>> = HashMap::new();
        for (feed_id, tag_id) in tag_pairs {
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

    pub(crate) fn mark_feed_polled_sync(&self, id: &str, error: Option<&str>) -> StorageResult<()> {
        let conn = self
            .db
            .conn
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        let now = chrono::Utc::now().to_rfc3339();

        match error {
            Some(err) => {
                conn.execute(
                    "UPDATE feeds SET last_polled_at = ?1, last_error = ?2 WHERE id = ?3",
                    rusqlite::params![&now, err, id],
                )?;
            }
            None => {
                conn.execute(
                    "UPDATE feeds SET last_polled_at = ?1, last_error = NULL WHERE id = ?2",
                    rusqlite::params![&now, id],
                )?;
            }
        }

        Ok(())
    }

    pub(crate) fn set_feed_error_sync(&self, id: &str, error: &str) -> StorageResult<()> {
        let conn = self
            .db
            .conn
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        conn.execute(
            "UPDATE feeds SET last_error = ?1 WHERE id = ?2",
            rusqlite::params![error, id],
        )?;
        Ok(())
    }

    pub(crate) fn claim_feed_item_sync(&self, feed_id: &str, guid: &str) -> StorageResult<bool> {
        let conn = self
            .db
            .conn
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        let now = chrono::Utc::now().to_rfc3339();
        let changes = conn.execute(
            "INSERT OR IGNORE INTO feed_items (feed_id, guid, skipped, seen_at)
             VALUES (?1, ?2, 0, ?3)",
            rusqlite::params![feed_id, guid, &now],
        )?;
        Ok(changes > 0)
    }

    pub(crate) fn complete_feed_item_sync(
        &self,
        feed_id: &str,
        guid: &str,
        atom_id: &str,
    ) -> StorageResult<()> {
        let conn = self
            .db
            .conn
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        conn.execute(
            "UPDATE feed_items SET atom_id = ?1 WHERE feed_id = ?2 AND guid = ?3",
            rusqlite::params![atom_id, feed_id, guid],
        )?;
        Ok(())
    }

    pub(crate) fn backfill_feed_metadata_sync(
        &self,
        id: &str,
        title: Option<&str>,
        site_url: Option<&str>,
    ) -> StorageResult<()> {
        let conn = self
            .db
            .conn
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        if let Some(t) = title {
            conn.execute(
                "UPDATE feeds SET title = COALESCE(title, ?1) WHERE id = ?2",
                rusqlite::params![t, id],
            )?;
        }
        if let Some(s) = site_url {
            conn.execute(
                "UPDATE feeds SET site_url = COALESCE(site_url, ?1) WHERE id = ?2",
                rusqlite::params![s, id],
            )?;
        }
        Ok(())
    }

    pub(crate) fn mark_feed_item_skipped_sync(
        &self,
        feed_id: &str,
        guid: &str,
        reason: &str,
    ) -> StorageResult<()> {
        let conn = self
            .db
            .conn
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        conn.execute(
            "UPDATE feed_items SET skipped = 1, skip_reason = ?1 WHERE feed_id = ?2 AND guid = ?3",
            rusqlite::params![reason, feed_id, guid],
        )?;
        Ok(())
    }
}

#[async_trait]
impl FeedStore for SqliteStorage {
    async fn create_feed(
        &self,
        url: &str,
        title: Option<&str>,
        site_url: Option<&str>,
        poll_interval: i32,
        tag_ids: &[String],
    ) -> StorageResult<Feed> {
        self.create_feed_sync(url, title, site_url, poll_interval, tag_ids)
    }

    async fn list_feeds(&self) -> StorageResult<Vec<Feed>> {
        self.list_feeds_sync()
    }

    async fn get_feed(&self, id: &str) -> StorageResult<Feed> {
        self.get_feed_sync(id)
    }

    async fn update_feed(
        &self,
        id: &str,
        title: Option<&str>,
        poll_interval: Option<i32>,
        is_paused: Option<bool>,
        tag_ids: Option<&[String]>,
    ) -> StorageResult<Feed> {
        self.update_feed_sync(id, title, poll_interval, is_paused, tag_ids)
    }

    async fn delete_feed(&self, id: &str) -> StorageResult<()> {
        self.delete_feed_sync(id)
    }

    async fn get_due_feeds(&self) -> StorageResult<Vec<Feed>> {
        self.get_due_feeds_sync()
    }

    async fn mark_feed_polled(&self, id: &str, error: Option<&str>) -> StorageResult<()> {
        self.mark_feed_polled_sync(id, error)
    }

    async fn set_feed_error(&self, id: &str, error: &str) -> StorageResult<()> {
        self.set_feed_error_sync(id, error)
    }

    async fn claim_feed_item(&self, feed_id: &str, guid: &str) -> StorageResult<bool> {
        self.claim_feed_item_sync(feed_id, guid)
    }

    async fn complete_feed_item(
        &self,
        feed_id: &str,
        guid: &str,
        atom_id: &str,
    ) -> StorageResult<()> {
        self.complete_feed_item_sync(feed_id, guid, atom_id)
    }

    async fn mark_feed_item_skipped(
        &self,
        feed_id: &str,
        guid: &str,
        reason: &str,
    ) -> StorageResult<()> {
        self.mark_feed_item_skipped_sync(feed_id, guid, reason)
    }

    async fn backfill_feed_metadata(
        &self,
        id: &str,
        title: Option<&str>,
        site_url: Option<&str>,
    ) -> StorageResult<()> {
        self.backfill_feed_metadata_sync(id, title, site_url)
    }
}
