//! Deterministic knowledge-quality signals.
//!
//! Signals are opportunities to improve the knowledge base, not system health
//! checks. Providers are deterministic and emit normalized `KnowledgeSignal`
//! rows that the dashboard and briefing can render.

use crate::error::AtomicCoreError;
use crate::storage::StorageBackend;
use crate::AtomicCore;
use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::cmp::Ordering;
use std::collections::HashMap;

pub const WIKI_CANDIDATE_PROVIDER_ID: &str = "wiki_candidate";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct KnowledgeSignal {
    pub id: String,
    pub provider_id: String,
    pub target: KnowledgeSignalTarget,
    pub score: f32,
    pub confidence: f32,
    pub severity: KnowledgeSignalSeverity,
    pub title: String,
    pub summary: String,
    pub reasons: Vec<KnowledgeSignalReason>,
    #[serde(default)]
    pub evidence: serde_json::Value,
    pub suggested_actions: Vec<KnowledgeSignalAction>,
    pub created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
}

trait KnowledgeSignalEvidence: Serialize {
    const SCHEMA: &'static str;
    const SCHEMA_VERSION: u32 = 1;

    fn to_value(&self) -> Result<Value, AtomicCoreError> {
        let mut value = serde_json::to_value(self)?;
        if let Some(obj) = value.as_object_mut() {
            obj.insert("schema".to_string(), json!(Self::SCHEMA));
            obj.insert("schema_version".to_string(), json!(Self::SCHEMA_VERSION));
        }
        Ok(value)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct KnowledgeSignalTarget {
    pub kind: String,
    pub id: String,
    pub label: String,
}

impl KnowledgeSignalTarget {
    fn tag(id: String, label: String) -> Self {
        Self {
            kind: "tag".to_string(),
            id,
            label,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum KnowledgeSignalSeverity {
    Info,
    Opportunity,
    Review,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct KnowledgeSignalReason {
    pub kind: String,
    pub label: String,
    pub value: serde_json::Value,
    pub contribution: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct KnowledgeSignalAction {
    pub id: String,
    pub label: String,
    pub kind: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct KnowledgeSignalFilter {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
    #[serde(default)]
    pub include_dismissed: bool,
    #[serde(default)]
    pub include_snoozed: bool,
    #[serde(default)]
    pub limit: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct KnowledgeSignalProviderConfig {
    pub provider_id: String,
    pub enabled: bool,
    pub weight: f32,
    pub min_score: f32,
    pub min_confidence: f32,
    pub show_on_dashboard: bool,
    pub include_in_briefing: bool,
    #[serde(default)]
    pub config_json: serde_json::Value,
}

impl KnowledgeSignalProviderConfig {
    fn default_for(provider_id: &str) -> Self {
        Self {
            provider_id: provider_id.to_string(),
            enabled: true,
            weight: 1.0,
            min_score: 0.0,
            min_confidence: 0.0,
            show_on_dashboard: true,
            include_in_briefing: false,
            config_json: json!({}),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnowledgeSignalFeedback {
    pub signal_key: String,
    pub provider_id: String,
    pub target_type: String,
    pub target_id: Option<String>,
    pub state: String,
    pub snoozed_until: Option<String>,
}

#[async_trait]
pub trait KnowledgeSignalProvider: Send + Sync {
    fn id(&self) -> &'static str;
    fn name(&self) -> &'static str;
    fn default_config(&self) -> KnowledgeSignalProviderConfig {
        KnowledgeSignalProviderConfig::default_for(self.id())
    }

    async fn evaluate(
        &self,
        core: &AtomicCore,
        config: &KnowledgeSignalProviderConfig,
    ) -> Result<Vec<KnowledgeSignal>, AtomicCoreError>;
}

pub async fn list_knowledge_signals(
    core: &AtomicCore,
    filter: KnowledgeSignalFilter,
) -> Result<Vec<KnowledgeSignal>, AtomicCoreError> {
    let providers: Vec<Box<dyn KnowledgeSignalProvider>> = vec![Box::new(WikiCandidateProvider)];
    let feedback = list_feedback(core).await?;
    let now = Utc::now();
    let mut out = Vec::new();

    for provider in providers {
        if let Some(filter_provider) = filter.provider_id.as_deref() {
            if filter_provider != provider.id() {
                continue;
            }
        }

        let config = get_provider_config(core, provider.id()).await?;
        if !config.enabled {
            continue;
        }

        let mut signals = provider.evaluate(core, &config).await?;
        signals.retain(|signal| {
            signal.score >= config.min_score && signal.confidence >= config.min_confidence
        });
        out.extend(signals);
    }

    out.retain(|signal| {
        let Some(fb) = feedback.get(&signal.id) else {
            return true;
        };

        match fb.state.as_str() {
            "dismissed" | "ignored" => filter.include_dismissed,
            "snoozed" => {
                if filter.include_snoozed {
                    return true;
                }
                match fb
                    .snoozed_until
                    .as_deref()
                    .and_then(|raw| chrono::DateTime::parse_from_rfc3339(raw).ok())
                {
                    Some(until) => until.with_timezone(&Utc) <= now,
                    None => true,
                }
            }
            _ => true,
        }
    });

    out.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| {
                b.confidence
                    .partial_cmp(&a.confidence)
                    .unwrap_or(Ordering::Equal)
            })
    });

    if let Some(limit) = filter.limit {
        if limit >= 0 {
            out.truncate(limit as usize);
        }
    }

    Ok(out)
}

pub async fn list_briefing_knowledge_signals(
    core: &AtomicCore,
    _window_start: DateTime<Utc>,
    _window_end: DateTime<Utc>,
    limit: i32,
) -> Result<Vec<KnowledgeSignal>, AtomicCoreError> {
    let providers: Vec<Box<dyn KnowledgeSignalProvider>> = vec![Box::new(WikiCandidateProvider)];
    let feedback = list_feedback(core).await?;
    let now = Utc::now();
    let mut out = Vec::new();

    for provider in providers {
        let config = get_provider_config(core, provider.id()).await?;
        if !config.enabled || !config.include_in_briefing {
            continue;
        }

        let mut signals = provider.evaluate(core, &config).await?;
        signals.retain(|signal| {
            signal.score >= config.min_score
                && signal.confidence >= config.min_confidence
                && match feedback.get(&signal.id) {
                    Some(fb) if matches!(fb.state.as_str(), "dismissed" | "ignored") => false,
                    Some(fb) if fb.state == "snoozed" => fb
                        .snoozed_until
                        .as_deref()
                        .and_then(|raw| chrono::DateTime::parse_from_rfc3339(raw).ok())
                        .map(|until| until.with_timezone(&Utc) <= now)
                        .unwrap_or(true),
                    _ => true,
                }
        });
        out.extend(signals);
    }

    out.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| {
                b.confidence
                    .partial_cmp(&a.confidence)
                    .unwrap_or(Ordering::Equal)
            })
    });

    if limit >= 0 {
        out.truncate(limit as usize);
    }

    Ok(out)
}

pub async fn dismiss_signal(core: &AtomicCore, signal_key: &str) -> Result<(), AtomicCoreError> {
    set_feedback(core, signal_key, "dismissed", None).await
}

pub async fn snooze_signal(
    core: &AtomicCore,
    signal_key: &str,
    until: &str,
) -> Result<(), AtomicCoreError> {
    chrono::DateTime::parse_from_rfc3339(until).map_err(|_| {
        AtomicCoreError::Validation("snoozed_until must be an RFC3339 timestamp".to_string())
    })?;
    set_feedback(core, signal_key, "snoozed", Some(until)).await
}

pub async fn set_provider_config(
    core: &AtomicCore,
    provider_id: &str,
    mut config: KnowledgeSignalProviderConfig,
) -> Result<KnowledgeSignalProviderConfig, AtomicCoreError> {
    config.provider_id = provider_id.to_string();
    match &core.storage {
        StorageBackend::Sqlite(storage) => {
            let storage = storage.clone();
            let config_to_store = config.clone();
            tokio::task::spawn_blocking(move || {
                let now = Utc::now().to_rfc3339();
                let config_json = serde_json::to_string(&config_to_store.config_json)?;
                let conn = storage
                    .database()
                    .conn
                    .lock()
                    .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
                conn.execute(
                    "INSERT INTO knowledge_signal_preferences
                        (provider_id, enabled, weight, min_score, min_confidence,
                         show_on_dashboard, include_in_briefing, config_json, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                     ON CONFLICT(provider_id) DO UPDATE SET
                        enabled = excluded.enabled,
                        weight = excluded.weight,
                        min_score = excluded.min_score,
                        min_confidence = excluded.min_confidence,
                        show_on_dashboard = excluded.show_on_dashboard,
                        include_in_briefing = excluded.include_in_briefing,
                        config_json = excluded.config_json,
                        updated_at = excluded.updated_at",
                    params![
                        config_to_store.provider_id,
                        if config_to_store.enabled { 1 } else { 0 },
                        config_to_store.weight,
                        config_to_store.min_score,
                        config_to_store.min_confidence,
                        if config_to_store.show_on_dashboard {
                            1
                        } else {
                            0
                        },
                        if config_to_store.include_in_briefing {
                            1
                        } else {
                            0
                        },
                        config_json,
                        now,
                    ],
                )?;
                Ok::<(), AtomicCoreError>(())
            })
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))??;
            Ok(config)
        }
        #[cfg(feature = "postgres")]
        StorageBackend::Postgres(storage) => {
            let now = Utc::now().to_rfc3339();
            let config_json = serde_json::to_string(&config.config_json)?;
            sqlx::query(
                "INSERT INTO knowledge_signal_preferences
                    (db_id, provider_id, enabled, weight, min_score, min_confidence,
                     show_on_dashboard, include_in_briefing, config_json, updated_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
                 ON CONFLICT(db_id, provider_id) DO UPDATE SET
                    enabled = excluded.enabled,
                    weight = excluded.weight,
                    min_score = excluded.min_score,
                    min_confidence = excluded.min_confidence,
                    show_on_dashboard = excluded.show_on_dashboard,
                    include_in_briefing = excluded.include_in_briefing,
                    config_json = excluded.config_json,
                    updated_at = excluded.updated_at",
            )
            .bind(&storage.db_id)
            .bind(&config.provider_id)
            .bind(config.enabled)
            .bind(config.weight)
            .bind(config.min_score)
            .bind(config.min_confidence)
            .bind(config.show_on_dashboard)
            .bind(config.include_in_briefing)
            .bind(config_json)
            .bind(now)
            .execute(&storage.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
            Ok(config)
        }
    }
}

pub async fn restore_signal(core: &AtomicCore, signal_key: &str) -> Result<(), AtomicCoreError> {
    match &core.storage {
        StorageBackend::Sqlite(storage) => {
            let storage = storage.clone();
            let key = signal_key.to_string();
            tokio::task::spawn_blocking(move || {
                let conn = storage
                    .database()
                    .conn
                    .lock()
                    .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
                conn.execute(
                    "DELETE FROM knowledge_signal_feedback WHERE signal_key = ?1",
                    params![key],
                )?;
                Ok(())
            })
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?
        }
        #[cfg(feature = "postgres")]
        StorageBackend::Postgres(storage) => {
            sqlx::query(
                "DELETE FROM knowledge_signal_feedback
                 WHERE db_id = $1 AND signal_key = $2",
            )
            .bind(&storage.db_id)
            .bind(signal_key)
            .execute(&storage.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
            Ok(())
        }
    }
}

async fn get_provider_config(
    core: &AtomicCore,
    provider_id: &str,
) -> Result<KnowledgeSignalProviderConfig, AtomicCoreError> {
    match &core.storage {
        StorageBackend::Sqlite(storage) => {
            let storage = storage.clone();
            let id = provider_id.to_string();
            tokio::task::spawn_blocking(move || {
                let conn = storage.database().read_conn()?;
                let row = conn
                    .query_row(
                        "SELECT enabled, weight, min_score, min_confidence, show_on_dashboard,
                                include_in_briefing, config_json
                         FROM knowledge_signal_preferences
                         WHERE provider_id = ?1",
                        params![id],
                        |row| {
                            Ok((
                                row.get::<_, i32>(0)?,
                                row.get::<_, f32>(1)?,
                                row.get::<_, f32>(2)?,
                                row.get::<_, f32>(3)?,
                                row.get::<_, i32>(4)?,
                                row.get::<_, i32>(5)?,
                                row.get::<_, String>(6)?,
                            ))
                        },
                    )
                    .optional()?;

                let Some((enabled, weight, min_score, min_confidence, show, briefing, json)) = row
                else {
                    return Ok(default_provider_config(&id));
                };

                Ok(KnowledgeSignalProviderConfig {
                    provider_id: id,
                    enabled: enabled != 0,
                    weight,
                    min_score,
                    min_confidence,
                    show_on_dashboard: show != 0,
                    include_in_briefing: briefing != 0,
                    config_json: serde_json::from_str(&json).unwrap_or_else(|_| json!({})),
                })
            })
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?
        }
        #[cfg(feature = "postgres")]
        StorageBackend::Postgres(storage) => {
            let row = sqlx::query_as::<_, (bool, f32, f32, f32, bool, bool, String)>(
                "SELECT enabled, weight, min_score, min_confidence, show_on_dashboard,
                        include_in_briefing, config_json
                 FROM knowledge_signal_preferences
                 WHERE db_id = $1 AND provider_id = $2",
            )
            .bind(&storage.db_id)
            .bind(provider_id)
            .fetch_optional(&storage.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

            let Some((enabled, weight, min_score, min_confidence, show, briefing, config_json)) =
                row
            else {
                return Ok(default_provider_config(provider_id));
            };

            Ok(KnowledgeSignalProviderConfig {
                provider_id: provider_id.to_string(),
                enabled,
                weight,
                min_score,
                min_confidence,
                show_on_dashboard: show,
                include_in_briefing: briefing,
                config_json: serde_json::from_str(&config_json).unwrap_or_else(|_| json!({})),
            })
        }
    }
}

fn default_provider_config(provider_id: &str) -> KnowledgeSignalProviderConfig {
    let mut config = KnowledgeSignalProviderConfig::default_for(provider_id);
    if provider_id == WIKI_CANDIDATE_PROVIDER_ID {
        config.include_in_briefing = true;
    }
    config
}

async fn list_feedback(
    core: &AtomicCore,
) -> Result<HashMap<String, KnowledgeSignalFeedback>, AtomicCoreError> {
    match &core.storage {
        StorageBackend::Sqlite(storage) => {
            let storage = storage.clone();
            tokio::task::spawn_blocking(move || {
                let conn = storage.database().read_conn()?;
                let mut stmt = conn.prepare(
                    "SELECT signal_key, provider_id, target_type, target_id, state, snoozed_until
                     FROM knowledge_signal_feedback",
                )?;
                let rows = stmt.query_map([], |row| {
                    Ok(KnowledgeSignalFeedback {
                        signal_key: row.get(0)?,
                        provider_id: row.get(1)?,
                        target_type: row.get(2)?,
                        target_id: row.get(3)?,
                        state: row.get(4)?,
                        snoozed_until: row.get(5)?,
                    })
                })?;

                let mut out = HashMap::new();
                for row in rows {
                    let fb = row?;
                    out.insert(fb.signal_key.clone(), fb);
                }
                Ok(out)
            })
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?
        }
        #[cfg(feature = "postgres")]
        StorageBackend::Postgres(storage) => {
            let rows = sqlx::query_as::<
                _,
                (
                    String,
                    String,
                    String,
                    Option<String>,
                    String,
                    Option<String>,
                ),
            >(
                "SELECT signal_key, provider_id, target_type, target_id, state, snoozed_until
                 FROM knowledge_signal_feedback
                 WHERE db_id = $1",
            )
            .bind(&storage.db_id)
            .fetch_all(&storage.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

            let mut out = HashMap::new();
            for (signal_key, provider_id, target_type, target_id, state, snoozed_until) in rows {
                out.insert(
                    signal_key.clone(),
                    KnowledgeSignalFeedback {
                        signal_key,
                        provider_id,
                        target_type,
                        target_id,
                        state,
                        snoozed_until,
                    },
                );
            }
            Ok(out)
        }
    }
}

async fn set_feedback(
    core: &AtomicCore,
    signal_key: &str,
    state: &str,
    snoozed_until: Option<&str>,
) -> Result<(), AtomicCoreError> {
    let (provider_id, target_type, target_id) = parse_signal_key(signal_key)?;
    match &core.storage {
        StorageBackend::Sqlite(storage) => {
            let storage = storage.clone();
            let key = signal_key.to_string();
            let state = state.to_string();
            let snoozed_until = snoozed_until.map(|s| s.to_string());
            tokio::task::spawn_blocking(move || {
                let now = Utc::now().to_rfc3339();
                let conn = storage
                    .database()
                    .conn
                    .lock()
                    .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
                conn.execute(
                    "INSERT INTO knowledge_signal_feedback
                        (signal_key, provider_id, target_type, target_id, state, snoozed_until, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)
                     ON CONFLICT(signal_key) DO UPDATE SET
                        state = excluded.state,
                        snoozed_until = excluded.snoozed_until,
                        updated_at = excluded.updated_at",
                    params![
                        key,
                        provider_id,
                        target_type,
                        target_id,
                        state,
                        snoozed_until,
                        now
                    ],
                )?;
                Ok(())
            })
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?
        }
        #[cfg(feature = "postgres")]
        StorageBackend::Postgres(storage) => {
            let now = Utc::now().to_rfc3339();
            sqlx::query(
                "INSERT INTO knowledge_signal_feedback
                    (db_id, signal_key, provider_id, target_type, target_id, state,
                     snoozed_until, created_at, updated_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $8)
                 ON CONFLICT(db_id, signal_key) DO UPDATE SET
                    state = excluded.state,
                    snoozed_until = excluded.snoozed_until,
                    updated_at = excluded.updated_at",
            )
            .bind(&storage.db_id)
            .bind(signal_key)
            .bind(provider_id)
            .bind(target_type)
            .bind(target_id)
            .bind(state)
            .bind(snoozed_until)
            .bind(now)
            .execute(&storage.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
            Ok(())
        }
    }
}

fn parse_signal_key(signal_key: &str) -> Result<(String, String, Option<String>), AtomicCoreError> {
    let parts: Vec<&str> = signal_key.split(':').collect();
    if parts.len() < 3 {
        return Err(AtomicCoreError::Validation(format!(
            "Invalid knowledge signal key: {signal_key}"
        )));
    }
    let provider_id = parts[0].to_string();
    let target_type = parts[1].to_string();
    let target_id = parts.get(2).map(|s| s.to_string());
    Ok((provider_id, target_type, target_id))
}

struct WikiCandidateProvider;

#[async_trait]
impl KnowledgeSignalProvider for WikiCandidateProvider {
    fn id(&self) -> &'static str {
        WIKI_CANDIDATE_PROVIDER_ID
    }

    fn name(&self) -> &'static str {
        "Wiki candidates"
    }

    async fn evaluate(
        &self,
        core: &AtomicCore,
        _config: &KnowledgeSignalProviderConfig,
    ) -> Result<Vec<KnowledgeSignal>, AtomicCoreError> {
        match &core.storage {
            StorageBackend::Sqlite(storage) => {
                let storage = storage.clone();
                tokio::task::spawn_blocking(move || {
                    let cutoff = (Utc::now() - Duration::days(14)).to_rfc3339();
                    let conn = storage.database().read_conn()?;
                    let mut stmt = conn.prepare(
                        "WITH link_mentions AS (
                            SELECT tag_id, SUM(cnt) as link_count FROM (
                                SELECT wl.target_tag_id as tag_id, COUNT(*) as cnt
                                FROM wiki_links wl
                                WHERE wl.target_tag_id IS NOT NULL
                                GROUP BY wl.target_tag_id
                                UNION ALL
                                SELECT t2.id as tag_id, COUNT(*) as cnt
                                FROM wiki_links wl
                                JOIN tags t2 ON wl.target_tag_name = t2.name COLLATE NOCASE
                                WHERE wl.target_tag_id IS NULL
                                GROUP BY t2.id
                            )
                            GROUP BY tag_id
                        ),
                        tag_atoms AS (
                            SELECT
                                at.tag_id,
                                COUNT(DISTINCT a.id) as atom_count,
                                COUNT(DISTINCT CASE
                                    WHEN a.source_url IS NOT NULL AND length(trim(a.source_url)) > 0
                                    THEN a.source_url
                                END) as source_count,
                                SUM(CASE WHEN length(trim(a.content)) >= 200 THEN 1 ELSE 0 END) as substantive_count,
                                SUM(CASE WHEN a.created_at >= ?1 THEN 1 ELSE 0 END) as recent_count
                            FROM atom_tags at
                            JOIN atoms a ON a.id = at.atom_id
                            GROUP BY at.tag_id
                        ),
                        intra_edges AS (
                            SELECT
                                at1.tag_id,
                                COUNT(*) as edge_count,
                                AVG(se.similarity_score) as avg_similarity
                            FROM semantic_edges se
                            JOIN atom_tags at1 ON at1.atom_id = se.source_atom_id
                            JOIN atom_tags at2 ON at2.atom_id = se.target_atom_id AND at2.tag_id = at1.tag_id
                            GROUP BY at1.tag_id
                        )
                        SELECT
                            t.id,
                            t.name,
                            COALESCE(ta.atom_count, 0) as atom_count,
                            COALESCE(lm.link_count, 0) as mention_count,
                            COALESCE(ta.source_count, 0) as source_count,
                            COALESCE(ta.substantive_count, 0) as substantive_count,
                            COALESCE(ta.recent_count, 0) as recent_count,
                            COALESCE(ie.edge_count, 0) as edge_count,
                            COALESCE(ie.avg_similarity, 0.0) as avg_similarity
                        FROM tags t
                        JOIN tag_atoms ta ON ta.tag_id = t.id
                        LEFT JOIN link_mentions lm ON lm.tag_id = t.id
                        LEFT JOIN intra_edges ie ON ie.tag_id = t.id
                        WHERE t.parent_id IS NOT NULL
                          AND NOT EXISTS (SELECT 1 FROM wiki_articles wa WHERE wa.tag_id = t.id)
                          AND t.name GLOB '*[^0-9]*'
                          AND length(t.name) >= 2
                          AND ta.atom_count > 0",
                    )?;

                    let rows = stmt.query_map(params![cutoff], |row| {
                        Ok(WikiCandidateRow {
                            tag_id: row.get(0)?,
                            tag_name: row.get(1)?,
                            atom_count: row.get(2)?,
                            mention_count: row.get(3)?,
                            source_count: row.get(4)?,
                            substantive_count: row.get(5)?,
                            recent_count: row.get(6)?,
                            edge_count: row.get(7)?,
                            avg_similarity: row.get(8)?,
                        })
                    })?;

                    let now = Utc::now().to_rfc3339();
                    let mut signals = Vec::new();
                    for row in rows {
                        signals.push(row?.into_signal(&now)?);
                    }
                    signals.sort_by(|a, b| {
                        b.score
                            .partial_cmp(&a.score)
                            .unwrap_or(Ordering::Equal)
                    });
                    Ok(signals)
                })
                .await
                .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?
            }
            #[cfg(feature = "postgres")]
            StorageBackend::Postgres(storage) => {
                let cutoff = (Utc::now() - Duration::days(14)).to_rfc3339();
                let rows = sqlx::query_as::<
                    _,
                    (String, String, i64, i64, i64, i64, i64, i64, f64),
                >(
                    "WITH link_mentions AS (
                        SELECT wl.target_tag_id as tag_id, COUNT(*)::BIGINT as link_count
                        FROM wiki_links wl
                        WHERE wl.target_tag_id IS NOT NULL
                          AND wl.db_id = $2
                        GROUP BY wl.target_tag_id
                    ),
                    tag_atoms AS (
                        SELECT
                            at.tag_id,
                            COUNT(DISTINCT a.id)::BIGINT as atom_count,
                            COUNT(DISTINCT CASE
                                WHEN a.source_url IS NOT NULL AND length(trim(a.source_url)) > 0
                                THEN a.source_url
                            END)::BIGINT as source_count,
                            SUM(CASE WHEN length(trim(a.content)) >= 200 THEN 1 ELSE 0 END)::BIGINT as substantive_count,
                            SUM(CASE WHEN a.created_at >= $1 THEN 1 ELSE 0 END)::BIGINT as recent_count
                        FROM atom_tags at
                        JOIN atoms a ON a.id = at.atom_id AND a.db_id = at.db_id
                        WHERE at.db_id = $2
                        GROUP BY at.tag_id
                    ),
                    intra_edges AS (
                        SELECT
                            at1.tag_id,
                            COUNT(*)::BIGINT as edge_count,
                            AVG(se.similarity_score)::FLOAT8 as avg_similarity
                        FROM semantic_edges se
                        JOIN atom_tags at1
                          ON at1.atom_id = se.source_atom_id
                         AND at1.db_id = se.db_id
                        JOIN atom_tags at2
                          ON at2.atom_id = se.target_atom_id
                         AND at2.tag_id = at1.tag_id
                         AND at2.db_id = se.db_id
                        WHERE se.db_id = $2
                        GROUP BY at1.tag_id
                    )
                    SELECT
                        t.id,
                        t.name,
                        COALESCE(ta.atom_count, 0)::BIGINT as atom_count,
                        COALESCE(lm.link_count, 0)::BIGINT as mention_count,
                        COALESCE(ta.source_count, 0)::BIGINT as source_count,
                        COALESCE(ta.substantive_count, 0)::BIGINT as substantive_count,
                        COALESCE(ta.recent_count, 0)::BIGINT as recent_count,
                        COALESCE(ie.edge_count, 0)::BIGINT as edge_count,
                        COALESCE(ie.avg_similarity, 0.0)::FLOAT8 as avg_similarity
                    FROM tags t
                    JOIN tag_atoms ta ON ta.tag_id = t.id
                    LEFT JOIN link_mentions lm ON lm.tag_id = t.id
                    LEFT JOIN intra_edges ie ON ie.tag_id = t.id
                    WHERE t.db_id = $2
                      AND t.parent_id IS NOT NULL
                      AND NOT EXISTS (
                          SELECT 1 FROM wiki_articles wa
                          WHERE wa.tag_id = t.id AND wa.db_id = $2
                      )
                      AND t.name ~ '[^0-9]'
                      AND length(t.name) >= 2
                      AND ta.atom_count > 0",
                )
                .bind(cutoff)
                .bind(&storage.db_id)
                .fetch_all(&storage.pool)
                .await
                .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

                let now = Utc::now().to_rfc3339();
                let mut signals = Vec::with_capacity(rows.len());
                for (
                    tag_id,
                    tag_name,
                    atom_count,
                    mention_count,
                    source_count,
                    substantive_count,
                    recent_count,
                    edge_count,
                    avg_similarity,
                ) in rows
                {
                    signals.push(
                        WikiCandidateRow {
                            tag_id,
                            tag_name,
                            atom_count: atom_count as i32,
                            mention_count: mention_count as i32,
                            source_count: source_count as i32,
                            substantive_count: substantive_count as i32,
                            recent_count: recent_count as i32,
                            edge_count: edge_count as i32,
                            avg_similarity: avg_similarity as f32,
                        }
                        .into_signal(&now)?,
                    );
                }
                signals.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal));
                Ok(signals)
            }
        }
    }
}

struct WikiCandidateRow {
    tag_id: String,
    tag_name: String,
    atom_count: i32,
    mention_count: i32,
    source_count: i32,
    substantive_count: i32,
    recent_count: i32,
    edge_count: i32,
    avg_similarity: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct WikiCandidateEvidence {
    pub tag_id: String,
    pub tag_name: String,
    pub atom_count: i32,
    pub mention_count: i32,
    pub source_count: i32,
    pub substantive_count: i32,
    pub recent_count: i32,
    pub semantic_edge_count: i32,
    pub avg_similarity: f32,
}

impl KnowledgeSignalEvidence for WikiCandidateEvidence {
    const SCHEMA: &'static str = "wiki_candidate";
}

impl WikiCandidateRow {
    fn into_signal(self, now: &str) -> Result<KnowledgeSignal, AtomicCoreError> {
        let atom_volume = scaled_ln(self.atom_count, 25.0);
        let source_diversity = if self.atom_count <= 1 {
            0.0
        } else {
            (self.source_count as f32 / self.atom_count.min(10) as f32).min(1.0)
        };
        let substantive = if self.atom_count == 0 {
            0.0
        } else {
            (self.substantive_count as f32 / self.atom_count as f32).min(1.0)
        };
        let recent_growth = (self.recent_count as f32 / 5.0).min(1.0);
        let mention_strength = (self.mention_count as f32 / 5.0).min(1.0);
        let semantic_edge_cohesion = if self.edge_count == 0 {
            0.0
        } else {
            ((self.avg_similarity - 0.5) / 0.35).clamp(0.0, 1.0)
        };

        let score = 100.0
            * (0.30 * atom_volume
                + 0.20 * source_diversity
                + 0.15 * substantive
                + 0.15 * recent_growth
                + 0.10 * mention_strength
                + 0.10 * semantic_edge_cohesion);

        let confidence = (0.35 * atom_volume
            + 0.25 * substantive
            + 0.20 * source_diversity
            + 0.20 * semantic_edge_cohesion)
            .clamp(0.0, 1.0);

        let mut reasons = vec![
            KnowledgeSignalReason {
                kind: "atom_volume".to_string(),
                label: format!(
                    "{} atom{}",
                    self.atom_count,
                    if self.atom_count == 1 { "" } else { "s" }
                ),
                value: json!(self.atom_count),
                contribution: atom_volume,
            },
            KnowledgeSignalReason {
                kind: "source_diversity".to_string(),
                label: format!(
                    "{} distinct source{}",
                    self.source_count,
                    if self.source_count == 1 { "" } else { "s" }
                ),
                value: json!(self.source_count),
                contribution: source_diversity,
            },
        ];

        if self.recent_count > 0 {
            reasons.push(KnowledgeSignalReason {
                kind: "recent_growth".to_string(),
                label: format!("{} added in the last 14 days", self.recent_count),
                value: json!(self.recent_count),
                contribution: recent_growth,
            });
        }

        if self.mention_count > 0 {
            reasons.push(KnowledgeSignalReason {
                kind: "wiki_mentions".to_string(),
                label: format!(
                    "{} wiki mention{}",
                    self.mention_count,
                    if self.mention_count == 1 { "" } else { "s" }
                ),
                value: json!(self.mention_count),
                contribution: mention_strength,
            });
        }

        if self.edge_count > 0 {
            reasons.push(KnowledgeSignalReason {
                kind: "semantic_edge_cohesion".to_string(),
                label: format!("semantic edge cohesion {:.0}%", self.avg_similarity * 100.0),
                value: json!({
                    "edge_count": self.edge_count,
                    "avg_similarity": self.avg_similarity,
                }),
                contribution: semantic_edge_cohesion,
            });
        }

        let evidence = WikiCandidateEvidence {
            tag_id: self.tag_id.clone(),
            tag_name: self.tag_name.clone(),
            atom_count: self.atom_count,
            mention_count: self.mention_count,
            source_count: self.source_count,
            substantive_count: self.substantive_count,
            recent_count: self.recent_count,
            semantic_edge_count: self.edge_count,
            avg_similarity: self.avg_similarity,
        };

        Ok(KnowledgeSignal {
            id: format!("wiki_candidate:tag:{}", self.tag_id),
            provider_id: WIKI_CANDIDATE_PROVIDER_ID.to_string(),
            target: KnowledgeSignalTarget::tag(self.tag_id.clone(), self.tag_name.clone()),
            score,
            confidence,
            severity: KnowledgeSignalSeverity::Opportunity,
            title: format!("Generate a wiki for {}", self.tag_name),
            summary: "Strong candidate for synthesis based on tag usage and source material."
                .to_string(),
            reasons,
            evidence: evidence.to_value()?,
            suggested_actions: vec![
                KnowledgeSignalAction {
                    id: "generate_wiki".to_string(),
                    label: "Generate wiki".to_string(),
                    kind: "wiki".to_string(),
                },
                KnowledgeSignalAction {
                    id: "review_tag".to_string(),
                    label: "Review tag".to_string(),
                    kind: "open".to_string(),
                },
            ],
            created_at: now.to_string(),
            expires_at: None,
        })
    }
}

fn scaled_ln(value: i32, cap_at: f32) -> f32 {
    if value <= 0 {
        0.0
    } else {
        ((value as f32 + 1.0).ln() / cap_at.ln()).min(1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CreateAtomRequest;
    use tempfile::NamedTempFile;

    fn long_note(seed: &str) -> String {
        format!(
            "# {seed}\n\n{}",
            "This note contains enough substantive content for wiki candidate scoring. ".repeat(5)
        )
    }

    async fn test_core() -> (AtomicCore, NamedTempFile) {
        let temp = NamedTempFile::new().unwrap();
        let core = AtomicCore::open_or_create(temp.path()).unwrap();
        (core, temp)
    }

    async fn create_child_tag(core: &AtomicCore, name: &str) -> crate::Tag {
        let parent = core.create_tag("Research Areas", None).await.unwrap();
        core.create_tag(name, Some(&parent.id)).await.unwrap()
    }

    #[tokio::test]
    async fn wiki_candidate_signal_has_typed_evidence_and_reasons() {
        let (core, _temp) = test_core().await;
        let tag = create_child_tag(&core, "Distributed Systems").await;

        for i in 0..3 {
            core.create_atom(
                CreateAtomRequest {
                    content: long_note(&format!("Distributed Systems {i}")),
                    source_url: Some(format!("https://example.com/systems/{i}")),
                    tag_ids: vec![tag.id.clone()],
                    ..Default::default()
                },
                |_| {},
            )
            .await
            .unwrap()
            .unwrap();
        }

        let signals = list_knowledge_signals(
            &core,
            KnowledgeSignalFilter {
                provider_id: Some(WIKI_CANDIDATE_PROVIDER_ID.to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let signal = signals
            .iter()
            .find(|signal| signal.target.id == tag.id)
            .expect("wiki candidate signal");

        assert_eq!(signal.id, format!("wiki_candidate:tag:{}", tag.id));
        assert_eq!(signal.provider_id, WIKI_CANDIDATE_PROVIDER_ID);
        assert!(signal.score > 0.0);
        assert!(signal.confidence > 0.0);
        assert!(signal
            .reasons
            .iter()
            .any(|reason| reason.kind == "atom_volume"));
        assert!(signal
            .reasons
            .iter()
            .any(|reason| reason.kind == "source_diversity"));
        assert_eq!(signal.evidence["schema"], "wiki_candidate");
        assert_eq!(signal.evidence["schema_version"], 1);
        assert_eq!(signal.evidence["tag_id"], tag.id);
        assert_eq!(signal.evidence["tag_name"], "Distributed Systems");
        assert_eq!(signal.evidence["atom_count"], 3);
        assert_eq!(signal.evidence["source_count"], 3);
    }

    #[tokio::test]
    async fn dismissed_wiki_candidate_is_hidden_until_included_or_restored() {
        let (core, _temp) = test_core().await;
        let tag = create_child_tag(&core, "Compiler Design").await;

        core.create_atom(
            CreateAtomRequest {
                content: long_note("Compiler Design"),
                source_url: Some("https://example.com/compiler-design".to_string()),
                tag_ids: vec![tag.id.clone()],
                ..Default::default()
            },
            |_| {},
        )
        .await
        .unwrap()
        .unwrap();

        let key = format!("wiki_candidate:tag:{}", tag.id);
        dismiss_signal(&core, &key).await.unwrap();

        let visible = list_knowledge_signals(
            &core,
            KnowledgeSignalFilter {
                provider_id: Some(WIKI_CANDIDATE_PROVIDER_ID.to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(!visible.iter().any(|signal| signal.id == key));

        let dismissed = list_knowledge_signals(
            &core,
            KnowledgeSignalFilter {
                provider_id: Some(WIKI_CANDIDATE_PROVIDER_ID.to_string()),
                include_dismissed: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(dismissed.iter().any(|signal| signal.id == key));

        restore_signal(&core, &key).await.unwrap();
        let restored = list_knowledge_signals(
            &core,
            KnowledgeSignalFilter {
                provider_id: Some(WIKI_CANDIDATE_PROVIDER_ID.to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(restored.iter().any(|signal| signal.id == key));
    }
}
