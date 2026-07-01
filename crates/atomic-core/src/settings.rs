//! Settings management for atomic-core
//!
//! This module provides key-value storage for application configuration.

use crate::error::AtomicCoreError;
use rusqlite::Connection;
use std::collections::HashMap;

/// Default Ollama host URL
pub const DEFAULT_OLLAMA_HOST: &str = "http://127.0.0.1:11434";

/// Settings whose values are properties of the user/machine, not the knowledge
/// base, so they always live in `registry.db` and can never be overridden
/// per-database. Everything *not* in this list is overridable: the registry
/// row holds the workspace default, and a per-DB row (when present) overrides
/// it for that database only.
pub const WORKSPACE_ONLY_KEYS: &[&str] = &[
    // UI preferences — one per user
    "theme",
    "font",
    "timezone",
    // Credentials — one set per user/account
    "openrouter_api_key",
    "openai_compat_api_key",
    // Machine-level URLs — one host per machine
    "ollama_host",
    "openai_compat_base_url",
];

/// True if `key` must live in `registry.db` and cannot be overridden per-DB.
pub fn is_workspace_only(key: &str) -> bool {
    WORKSPACE_ONLY_KEYS.contains(&key)
}

/// Settings whose resolved value defines the embedding vector space. Changing
/// or clearing any of these requires re-embedding the affected database.
pub const EMBEDDING_SPACE_KEYS: &[&str] = &[
    "provider",
    "embedding_model",
    "ollama_embedding_model",
    "openai_compat_embedding_model",
    "openai_compat_embedding_dimension",
];

/// True if `key` affects the embedding vector space.
pub fn is_embedding_space_key(key: &str) -> bool {
    EMBEDDING_SPACE_KEYS.contains(&key)
}

/// Default settings with their values
pub const DEFAULT_SETTINGS: &[(&str, &str)] = &[
    ("provider", "openrouter"),
    ("timezone", ""),
    ("ollama_host", DEFAULT_OLLAMA_HOST),
    ("ollama_embedding_model", "nomic-embed-text"),
    ("ollama_llm_model", "llama3.2"),
    ("ollama_timeout_secs", "120"), // 2 minutes default for Ollama (local models can be slow)
    ("ollama_context_length", "65536"),
    ("openrouter_context_length", ""),
    ("embedding_model", crate::providers::DEFAULT_EMBEDDING_MODEL),
    ("tagging_model", crate::providers::DEFAULT_TAGGING_MODEL),
    // Pipeline strategy defaults. These are intentionally conservative: whole-atom
    // rechunking and cost-bounded full-content tagging with truncation.
    ("embedding_strategy", "rechunk_whole_atom"),
    ("tagging_strategy", "truncated_full_content"),
    ("wiki_model", crate::providers::DEFAULT_AGENTIC_MODEL),
    ("wiki_strategy", "centroid"),
    ("chat_model", crate::providers::DEFAULT_AGENTIC_MODEL),
    ("auto_tagging_enabled", "true"),
    ("openai_compat_base_url", ""),
    ("openai_compat_embedding_model", ""),
    ("openai_compat_llm_model", ""),
    ("openai_compat_embedding_dimension", "1536"),
    ("openai_compat_context_length", "65536"),
    ("openai_compat_timeout_secs", "300"), // 5 minutes default for OpenAI-compatible servers
    ("wiki_generation_prompt", ""),
    ("wiki_update_prompt", ""),
    ("chat_prompt", ""),
    ("tagging_prompt", ""),
    // Scheduled tasks — see crate::scheduler::state for key format.
    // The daily briefing was retired in phase 3; its prompt and schedule
    // live on the seeded "Daily Briefing" report row, not in settings.
    ("task.draft_pipeline.enabled", "true"),
    ("task.draft_pipeline.interval_minutes", "1"),
    ("task.draft_pipeline.quiet_minutes", "1"),
];

/// Migrate settings - add any missing default settings
pub fn migrate_settings(conn: &Connection) -> Result<(), AtomicCoreError> {
    for (key, default_value) in DEFAULT_SETTINGS {
        // Only set if the key doesn't exist
        let exists: bool = conn
            .query_row("SELECT 1 FROM settings WHERE key = ?1", [key], |_| Ok(true))
            .unwrap_or(false);

        if !exists {
            set_setting(conn, key, default_value)?;
        }
    }
    Ok(())
}

/// Get a setting with a default fallback
pub fn get_setting_or_default(conn: &Connection, key: &str) -> String {
    get_setting(conn, key).unwrap_or_else(|_| {
        DEFAULT_SETTINGS
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v.to_string())
            .unwrap_or_default()
    })
}

/// Get all settings as a HashMap
pub fn get_all_settings(conn: &Connection) -> Result<HashMap<String, String>, AtomicCoreError> {
    let mut stmt = conn.prepare("SELECT key, value FROM settings")?;

    let settings = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<HashMap<_, _>, _>>()?;

    Ok(settings)
}

/// Get a single setting by key
pub fn get_setting(conn: &Connection, key: &str) -> Result<String, AtomicCoreError> {
    conn.query_row("SELECT value FROM settings WHERE key = ?1", [key], |row| {
        row.get(0)
    })
    .map_err(|e| AtomicCoreError::Configuration(format!("Failed to get setting '{}': {}", key, e)))
}

/// Migrate settings into a connection that has a `settings` table.
/// Used by the registry to seed defaults into registry.db.
pub fn migrate_settings_to(conn: &Connection) -> Result<(), AtomicCoreError> {
    migrate_settings(conn)
}

/// Set a setting (upsert)
pub fn set_setting(conn: &Connection, key: &str, value: &str) -> Result<(), AtomicCoreError> {
    conn.execute(
        "INSERT INTO settings (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [key, value],
    )?;

    Ok(())
}

/// Delete a setting row. Returns Ok(()) whether or not the row existed.
/// Used to clear a per-DB override so the resolver falls back to the
/// workspace default in `registry.db`.
pub fn delete_setting(conn: &Connection, key: &str) -> Result<(), AtomicCoreError> {
    conn.execute("DELETE FROM settings WHERE key = ?1", [key])?;
    Ok(())
}

/// Source of a resolved setting value. Powers the override UI: the frontend
/// uses this to decide whether to render "Default", "Overridden", or to
/// suppress the override affordance entirely.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingSource {
    /// Key is in `WORKSPACE_ONLY_KEYS` — value lives in `registry.db` and
    /// can never be overridden per-DB.
    Workspace,
    /// Overridable key, currently using the workspace default from `registry.db`.
    WorkspaceDefault,
    /// Overridable key, currently overridden by a row in this DB's settings table.
    Override,
    /// No row in registry or per-DB; value comes from the `DEFAULT_SETTINGS`
    /// constant baked into the binary.
    BuiltinDefault,
}

/// Resolved setting: the value the caller will see, plus where it came from.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SettingValue {
    pub value: String,
    pub source: SettingSource,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;
    use tempfile::NamedTempFile;

    fn create_test_db() -> (Database, NamedTempFile) {
        let temp_file = NamedTempFile::new().unwrap();
        let db = Database::open_or_create(temp_file.path()).unwrap();
        (db, temp_file)
    }

    #[test]
    fn test_per_db_settings_table_starts_empty() {
        // Per-DB tables are no longer seeded with DEFAULT_SETTINGS — defaults
        // live in registry.db. A freshly-opened per-DB connection should have
        // an empty settings table; rows only appear when the user explicitly
        // overrides a value.
        let (db, _temp) = create_test_db();
        let conn = db.conn.lock().unwrap();

        let settings = get_all_settings(&conn).unwrap();
        assert!(
            settings.is_empty(),
            "Per-DB settings table should start empty (defaults live in registry)"
        );
    }

    #[test]
    fn test_migrate_settings_seeds_defaults() {
        // The migration function itself still seeds DEFAULT_SETTINGS — it's
        // just no longer called from the per-DB open path. Callers like
        // Registry::open invoke it explicitly to seed registry.db.
        let (db, _temp) = create_test_db();
        let conn = db.conn.lock().unwrap();

        migrate_settings(&conn).unwrap();
        let settings = get_all_settings(&conn).unwrap();

        assert!(
            !settings.is_empty(),
            "migrate_settings should seed defaults"
        );
        assert_eq!(
            settings.get("provider").map(String::as_str),
            Some("openrouter"),
            "provider default should be openrouter"
        );
    }

    #[test]
    fn test_set_and_get_setting() {
        let (db, _temp) = create_test_db();
        let conn = db.conn.lock().unwrap();

        // Set a custom setting
        set_setting(&conn, "my_custom_key", "my_custom_value").unwrap();

        // Get it back
        let value = get_setting(&conn, "my_custom_key").unwrap();
        assert_eq!(value, "my_custom_value");
    }

    #[test]
    fn test_update_existing_setting() {
        let (db, _temp) = create_test_db();
        let conn = db.conn.lock().unwrap();

        // Set initial value
        set_setting(&conn, "test_key", "initial_value").unwrap();
        let value1 = get_setting(&conn, "test_key").unwrap();
        assert_eq!(value1, "initial_value");

        // Update to new value (upsert)
        set_setting(&conn, "test_key", "updated_value").unwrap();
        let value2 = get_setting(&conn, "test_key").unwrap();
        assert_eq!(value2, "updated_value");
    }

    #[test]
    fn test_get_setting_or_default() {
        let (db, _temp) = create_test_db();
        let conn = db.conn.lock().unwrap();

        // For a key that doesn't exist, should return default
        let value = get_setting_or_default(&conn, "embedding_model");
        assert_eq!(value, "qwen/qwen3-embedding-8b");

        // For a key with no default, should return empty string
        let unknown = get_setting_or_default(&conn, "unknown_key");
        assert_eq!(unknown, "");
    }

    #[test]
    fn test_migrate_settings_idempotent() {
        let (db, _temp) = create_test_db();
        let conn = db.conn.lock().unwrap();

        // Per-DB connections aren't seeded automatically — drive migration
        // ourselves and confirm a second run leaves the row count unchanged.
        migrate_settings(&conn).unwrap();
        let settings1 = get_all_settings(&conn).unwrap();

        migrate_settings(&conn).unwrap();
        let settings2 = get_all_settings(&conn).unwrap();
        assert_eq!(settings1.len(), settings2.len());
    }

    #[test]
    fn test_workspace_only_classification() {
        // Sanity-check the small static list that gates the resolver.
        assert!(is_workspace_only("theme"));
        assert!(is_workspace_only("openrouter_api_key"));
        assert!(is_workspace_only("ollama_host"));
        assert!(!is_workspace_only("provider"));
        assert!(!is_workspace_only("embedding_model"));
        assert!(!is_workspace_only("auto_tagging_enabled"));
    }

    #[test]
    fn test_delete_setting_removes_row() {
        let (db, _temp) = create_test_db();
        let conn = db.conn.lock().unwrap();

        set_setting(&conn, "provider", "ollama").unwrap();
        assert_eq!(get_setting(&conn, "provider").unwrap(), "ollama");

        delete_setting(&conn, "provider").unwrap();
        assert!(
            get_setting(&conn, "provider").is_err(),
            "delete_setting should remove the row so subsequent reads fail"
        );

        // Deleting a missing key is a no-op (does not error).
        delete_setting(&conn, "never_existed").unwrap();
    }
}
