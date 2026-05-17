use std::collections::HashMap;

use super::PostgresStorage;
use crate::error::AtomicCoreError;
use crate::registry::DatabaseInfo;
use crate::storage::traits::*;
use crate::tokens::ApiTokenInfo;
use async_trait::async_trait;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use chrono::Utc;
use rand::RngCore;
use sha2::{Digest, Sha256};

// ==================== Settings ====================

#[async_trait]
impl SettingsStore for PostgresStorage {
    async fn get_all_settings(&self) -> StorageResult<HashMap<String, String>> {
        let rows = sqlx::query_as::<_, (String, String)>("SELECT key, value FROM settings")
            .fetch_all(&self.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        Ok(rows.into_iter().collect())
    }

    async fn get_setting(&self, key: &str) -> StorageResult<Option<String>> {
        let value: Option<String> = sqlx::query_scalar("SELECT value FROM settings WHERE key = $1")
            .bind(key)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        Ok(value)
    }

    async fn set_setting(&self, key: &str, value: &str) -> StorageResult<()> {
        sqlx::query(
            "INSERT INTO settings (key, value) VALUES ($1, $2)
             ON CONFLICT (key) DO UPDATE SET value = $2",
        )
        .bind(key)
        .bind(value)
        .execute(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        Ok(())
    }

    async fn delete_setting(&self, key: &str) -> StorageResult<()> {
        sqlx::query("DELETE FROM settings WHERE key = $1")
            .bind(key)
            .execute(&self.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        Ok(())
    }
}

// ==================== Tokens ====================

/// Generate a raw API token: `at_` + 32 random bytes base64url-encoded
fn generate_raw_token() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    format!("at_{}", URL_SAFE_NO_PAD.encode(bytes))
}

/// SHA-256 hex digest of a raw token
fn hash_token(raw: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(raw.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Extract the display prefix from a raw token (first 10 chars)
fn token_prefix(raw: &str) -> String {
    raw.chars().take(10).collect()
}

#[async_trait]
impl TokenStore for PostgresStorage {
    async fn create_api_token(&self, name: &str) -> StorageResult<(ApiTokenInfo, String)> {
        let id = uuid::Uuid::new_v4().to_string();
        let raw = generate_raw_token();
        let hash = hash_token(&raw);
        let prefix = token_prefix(&raw);
        let now = Utc::now().to_rfc3339();

        sqlx::query(
            "INSERT INTO api_tokens (id, name, token_hash, token_prefix, created_at)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(&id)
        .bind(name)
        .bind(&hash)
        .bind(&prefix)
        .bind(&now)
        .execute(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        let info = ApiTokenInfo {
            id,
            name: name.to_string(),
            token_prefix: prefix,
            created_at: now,
            last_used_at: None,
            is_revoked: false,
        };

        Ok((info, raw))
    }

    async fn list_api_tokens(&self) -> StorageResult<Vec<ApiTokenInfo>> {
        let rows = sqlx::query_as::<_, (String, String, String, String, Option<String>, bool)>(
            "SELECT id, name, token_prefix, created_at, last_used_at, (is_revoked != 0)
             FROM api_tokens ORDER BY created_at DESC",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        Ok(rows
            .into_iter()
            .map(
                |(id, name, token_prefix, created_at, last_used_at, is_revoked)| ApiTokenInfo {
                    id,
                    name,
                    token_prefix,
                    created_at,
                    last_used_at,
                    is_revoked,
                },
            )
            .collect())
    }

    async fn verify_api_token(&self, raw_token: &str) -> StorageResult<Option<ApiTokenInfo>> {
        let hash = hash_token(raw_token);

        let row = sqlx::query_as::<_, (String, String, String, String, Option<String>, bool)>(
            "SELECT id, name, token_prefix, created_at, last_used_at, (is_revoked != 0)
             FROM api_tokens WHERE token_hash = $1 AND is_revoked = 0",
        )
        .bind(&hash)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        Ok(row.map(
            |(id, name, token_prefix, created_at, last_used_at, is_revoked)| ApiTokenInfo {
                id,
                name,
                token_prefix,
                created_at,
                last_used_at,
                is_revoked,
            },
        ))
    }

    async fn revoke_api_token(&self, id: &str) -> StorageResult<()> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        let is_revoked: Option<bool> =
            sqlx::query_scalar("SELECT (is_revoked != 0) FROM api_tokens WHERE id = $1")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        let Some(is_revoked) = is_revoked else {
            return Err(AtomicCoreError::NotFound(format!("API token '{}'", id)));
        };

        if !is_revoked {
            let active_count: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM api_tokens WHERE is_revoked = 0")
                    .fetch_one(&mut *tx)
                    .await
                    .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
            if active_count <= 1 {
                return Err(AtomicCoreError::Conflict(
                    "Cannot revoke the last active API token. Create a replacement token first."
                        .to_string(),
                ));
            }
        }

        let result = sqlx::query("UPDATE api_tokens SET is_revoked = 1 WHERE id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        if result.rows_affected() == 0 {
            return Err(AtomicCoreError::NotFound(format!("API token '{}'", id)));
        }

        tx.commit()
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        Ok(())
    }

    async fn update_token_last_used(&self, id: &str) -> StorageResult<()> {
        let now = Utc::now().to_rfc3339();

        sqlx::query("UPDATE api_tokens SET last_used_at = $1 WHERE id = $2")
            .bind(&now)
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        Ok(())
    }

    async fn migrate_legacy_token(&self) -> StorageResult<bool> {
        // Check if legacy token exists in settings
        let legacy_token: Option<String> =
            sqlx::query_scalar("SELECT value FROM settings WHERE key = 'server_auth_token'")
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        let legacy_token = match legacy_token {
            Some(t) if !t.is_empty() => t,
            _ => return Ok(false),
        };

        // Only migrate if no api_tokens exist yet
        let token_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM api_tokens")
            .fetch_one(&self.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        if token_count > 0 {
            return Ok(false);
        }

        // Hash the existing UUID token and insert as a migrated token
        let id = uuid::Uuid::new_v4().to_string();
        let hash = hash_token(&legacy_token);
        let prefix = token_prefix(&legacy_token);
        let now = Utc::now().to_rfc3339();

        sqlx::query(
            "INSERT INTO api_tokens (id, name, token_hash, token_prefix, created_at)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(&id)
        .bind("default (migrated)")
        .bind(&hash)
        .bind(&prefix)
        .bind(&now)
        .execute(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        // Remove the legacy setting
        sqlx::query("DELETE FROM settings WHERE key = 'server_auth_token'")
            .execute(&self.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        Ok(true)
    }

    async fn ensure_default_token(&self) -> StorageResult<Option<(ApiTokenInfo, String)>> {
        let token_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM api_tokens")
            .fetch_one(&self.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        if token_count > 0 {
            return Ok(None);
        }

        let result = self.create_api_token("default").await?;
        Ok(Some(result))
    }
}

// ==================== DatabaseStore ====================

#[async_trait]
impl DatabaseStore for PostgresStorage {
    async fn list_databases(&self) -> StorageResult<Vec<DatabaseInfo>> {
        let rows = sqlx::query_as::<_, (String, String, i32, String, Option<String>)>(
            "SELECT id, name, is_default, created_at, last_opened_at
             FROM databases ORDER BY is_default DESC, created_at ASC",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        Ok(rows
            .into_iter()
            .map(
                |(id, name, is_default, created_at, last_opened_at)| DatabaseInfo {
                    id,
                    name,
                    is_default: is_default != 0,
                    created_at,
                    last_opened_at,
                },
            )
            .collect())
    }

    async fn create_database(&self, name: &str) -> StorageResult<DatabaseInfo> {
        let id = uuid::Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();

        sqlx::query(
            "INSERT INTO databases (id, name, is_default, created_at) VALUES ($1, $2, 0, $3)",
        )
        .bind(&id)
        .bind(name)
        .bind(&now)
        .execute(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        Ok(DatabaseInfo {
            id,
            name: name.to_string(),
            is_default: false,
            created_at: now,
            last_opened_at: None,
        })
    }

    async fn rename_database(&self, id: &str, name: &str) -> StorageResult<()> {
        let result = sqlx::query("UPDATE databases SET name = $1 WHERE id = $2")
            .bind(name)
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        if result.rows_affected() == 0 {
            return Err(AtomicCoreError::NotFound(format!("Database '{}'", id)));
        }

        Ok(())
    }

    async fn delete_database(&self, id: &str) -> StorageResult<()> {
        // Check if it's the default database
        let is_default: Option<i32> =
            sqlx::query_scalar("SELECT is_default FROM databases WHERE id = $1")
                .bind(id)
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        match is_default {
            None => {
                return Err(AtomicCoreError::NotFound(format!("Database '{}'", id)));
            }
            Some(v) if v != 0 => {
                return Err(AtomicCoreError::Validation(
                    "Cannot delete the default database".to_string(),
                ));
            }
            _ => {}
        }

        sqlx::query("DELETE FROM databases WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        Ok(())
    }

    async fn get_default_database_id(&self) -> StorageResult<String> {
        let id: Option<String> =
            sqlx::query_scalar("SELECT id FROM databases WHERE is_default = 1")
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        id.ok_or_else(|| {
            AtomicCoreError::Configuration("No default database configured".to_string())
        })
    }

    async fn set_default_database(&self, id: &str) -> StorageResult<()> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        // Verify the database exists
        let exists: Option<i32> = sqlx::query_scalar("SELECT 1 FROM databases WHERE id = $1")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        if exists.is_none() {
            return Err(AtomicCoreError::NotFound(format!("Database '{}'", id)));
        }

        sqlx::query("UPDATE databases SET is_default = 0 WHERE is_default = 1")
            .execute(&mut *tx)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        sqlx::query("UPDATE databases SET is_default = 1 WHERE id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        tx.commit()
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        Ok(())
    }

    async fn purge_database_data(&self, db_id: &str) -> StorageResult<()> {
        // Delete from all per-database tables in dependency order (children first).
        // Tables with FKs to other per-db tables are deleted first to avoid constraint violations.
        let tables = [
            "knowledge_signal_feedback",
            "knowledge_signal_preferences",
            "chat_citations",
            "chat_tool_calls",
            "chat_messages",
            "conversation_tags",
            "conversations",
            "wiki_citations",
            "wiki_links",
            "wiki_article_versions",
            "wiki_articles",
            "feed_items",
            "feed_tags",
            "feeds",
            "semantic_edges",
            "atom_clusters",
            "tag_embeddings",
            "atom_positions",
            // briefings before atoms (briefing_citations FK references atoms)
            "briefing_citations",
            "briefings",
            "atom_chunks",
            "atom_tags",
            "atoms",
            "tags",
        ];

        for table in &tables {
            sqlx::query(&format!("DELETE FROM {} WHERE db_id = $1", table))
                .bind(db_id)
                .execute(&self.pool)
                .await
                .map_err(|e| {
                    AtomicCoreError::DatabaseOperation(format!(
                        "Failed to purge {} for db_id {}: {}",
                        table, db_id, e
                    ))
                })?;
        }

        Ok(())
    }
}
