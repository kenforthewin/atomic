//! Deterministic knowledge-quality signals.
//!
//! Signals are opportunities to improve the knowledge base, not system health
//! checks. Providers are deterministic and emit normalized `KnowledgeSignal`
//! rows that the dashboard can render.

use crate::error::AtomicCoreError;
use crate::storage::StorageBackend;
use crate::AtomicCore;
use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use rusqlite::{params, params_from_iter, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration as StdDuration, Instant};

pub const WIKI_CANDIDATE_PROVIDER_ID: &str = "wiki_candidate";
pub const WIKI_UPDATE_PROVIDER_ID: &str = "wiki_update";
pub const TAG_REDUNDANCY_PROVIDER_ID: &str = "tag_redundancy";
pub const EMPTY_TAG_PROVIDER_ID: &str = "empty_tag";
pub const MISSING_TAG_OVERLAP_PROVIDER_ID: &str = "missing_tag_overlap";
pub const NEAR_DUPLICATE_ATOM_PROVIDER_ID: &str = "near_duplicate_atom";
pub const SOURCE_DUPLICATE_PROVIDER_ID: &str = "source_duplicate";
pub const BROKEN_INTERNAL_LINK_PROVIDER_ID: &str = "broken_internal_link";
pub const UNDERCONNECTED_ATOM_PROVIDER_ID: &str = "underconnected_atom";

const TAG_REDUNDANCY_MAX_TAGS_PER_ATOM: i32 = 20;
const TAG_REDUNDANCY_CANDIDATE_LIMIT: i32 = 1000;
const MISSING_TAG_EDGE_CANDIDATE_LIMIT: i32 = 5000;
const MISSING_TAG_CANDIDATE_LIMIT: i32 = 750;
const SOURCE_DUPLICATE_GROUP_LIMIT: i32 = 500;
const SOURCE_DUPLICATE_ATOMS_PER_GROUP: i32 = 6;
const UNDERCONNECTED_CANDIDATE_ATOM_LIMIT: i32 = 5000;
const DASHBOARD_SIGNAL_CACHE_TTL: StdDuration = StdDuration::from_secs(15);

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

    fn atom(id: String, label: String) -> Self {
        Self {
            kind: "atom".to_string(),
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct KnowledgeSignalProviderSettings {
    pub provider_id: String,
    pub name: String,
    pub config: KnowledgeSignalProviderConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct DashboardKnowledgeSignals {
    pub generated_at: String,
    pub provider_settings: Vec<KnowledgeSignalProviderSettings>,
    pub groups: Vec<DashboardKnowledgeSignalGroup>,
    pub errors: Vec<DashboardKnowledgeSignalError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct DashboardKnowledgeSignalGroup {
    pub provider_id: String,
    pub name: String,
    pub evaluation_ms: u64,
    pub signals: Vec<KnowledgeSignal>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct DashboardKnowledgeSignalError {
    pub provider_id: String,
    pub name: String,
    pub evaluation_ms: u64,
    pub message: String,
}

#[derive(Clone, Default)]
pub struct DashboardKnowledgeSignalCache {
    inner: Arc<Mutex<Option<CachedDashboardKnowledgeSignals>>>,
}

#[derive(Clone)]
struct CachedDashboardKnowledgeSignals {
    per_provider_limit: i32,
    cached_at: Instant,
    response: DashboardKnowledgeSignals,
}

impl DashboardKnowledgeSignalCache {
    pub fn new() -> Self {
        Self::default()
    }

    fn get(&self, per_provider_limit: i32) -> Option<DashboardKnowledgeSignals> {
        let guard = self.inner.lock().ok()?;
        let cached = guard.as_ref()?;
        if cached.per_provider_limit != per_provider_limit {
            return None;
        }
        if cached.cached_at.elapsed() > DASHBOARD_SIGNAL_CACHE_TTL {
            return None;
        }
        Some(cached.response.clone())
    }

    fn set(&self, per_provider_limit: i32, response: DashboardKnowledgeSignals) {
        if let Ok(mut guard) = self.inner.lock() {
            *guard = Some(CachedDashboardKnowledgeSignals {
                per_provider_limit,
                cached_at: Instant::now(),
                response,
            });
        }
    }

    pub fn invalidate(&self) {
        if let Ok(mut guard) = self.inner.lock() {
            *guard = None;
        }
    }
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct KnowledgeSignalActionRequest {
    pub action: String,
    #[serde(default)]
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct KnowledgeSignalActionResult {
    pub action_log_id: String,
    pub signal_key: String,
    pub provider_id: String,
    pub action: String,
    pub status: String,
    pub undo_supported: bool,
    #[serde(default)]
    pub result: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct KnowledgeSignalActionLog {
    pub id: String,
    pub signal_key: String,
    pub provider_id: String,
    pub action: String,
    pub target_type: String,
    pub target_id: Option<String>,
    #[serde(default)]
    pub before_state: serde_json::Value,
    #[serde(default)]
    pub after_state: serde_json::Value,
    pub status: String,
    pub error: Option<String>,
    pub executed_at: String,
    pub undone_at: Option<String>,
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
    let providers = signal_providers();
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
        apply_provider_weight(&mut signals, config.weight);
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

pub async fn list_provider_configs(
    core: &AtomicCore,
) -> Result<Vec<KnowledgeSignalProviderSettings>, AtomicCoreError> {
    let providers = signal_providers();
    let mut out = Vec::with_capacity(providers.len());
    for provider in providers {
        out.push(KnowledgeSignalProviderSettings {
            provider_id: provider.id().to_string(),
            name: provider.name().to_string(),
            config: get_provider_config(core, provider.id()).await?,
        });
    }
    Ok(out)
}

pub async fn list_dashboard_knowledge_signals(
    core: &AtomicCore,
    per_provider_limit: i32,
) -> Result<DashboardKnowledgeSignals, AtomicCoreError> {
    if let Some(cached) = core.dashboard_signal_cache.get(per_provider_limit) {
        return Ok(cached);
    }

    let providers = signal_providers();
    let feedback = list_feedback(core).await?;
    let now = Utc::now();
    let generated_at = now.to_rfc3339();
    let mut provider_settings = Vec::with_capacity(providers.len());
    let mut groups = Vec::new();
    let mut errors = Vec::new();

    for provider in providers {
        let provider_id = provider.id().to_string();
        let name = provider.name().to_string();
        let config = get_provider_config(core, &provider_id).await?;
        provider_settings.push(KnowledgeSignalProviderSettings {
            provider_id: provider_id.clone(),
            name: name.clone(),
            config: config.clone(),
        });

        if !config.enabled || !config.show_on_dashboard {
            continue;
        }

        let started = Instant::now();
        match provider.evaluate(core, &config).await {
            Ok(mut signals) => {
                apply_provider_weight(&mut signals, config.weight);
                signals.retain(|signal| {
                    signal.score >= config.min_score
                        && signal.confidence >= config.min_confidence
                        && signal_is_visible_with_feedback(feedback.get(&signal.id), now)
                });
                signals.sort_by(|a, b| {
                    b.score
                        .partial_cmp(&a.score)
                        .unwrap_or(Ordering::Equal)
                        .then_with(|| {
                            b.confidence
                                .partial_cmp(&a.confidence)
                                .unwrap_or(Ordering::Equal)
                        })
                });
                if per_provider_limit >= 0 {
                    signals.truncate(per_provider_limit as usize);
                }
                groups.push(DashboardKnowledgeSignalGroup {
                    provider_id,
                    name,
                    evaluation_ms: started.elapsed().as_millis() as u64,
                    signals,
                });
            }
            Err(err) => {
                errors.push(DashboardKnowledgeSignalError {
                    provider_id,
                    name,
                    evaluation_ms: started.elapsed().as_millis() as u64,
                    message: err.to_string(),
                });
            }
        }
    }

    let response = DashboardKnowledgeSignals {
        generated_at,
        provider_settings,
        groups,
        errors,
    };
    core.dashboard_signal_cache
        .set(per_provider_limit, response.clone());
    Ok(response)
}

pub async fn list_briefing_knowledge_signals(
    core: &AtomicCore,
    _window_start: DateTime<Utc>,
    _window_end: DateTime<Utc>,
    limit: i32,
) -> Result<Vec<KnowledgeSignal>, AtomicCoreError> {
    let providers = signal_providers();
    let feedback = list_feedback(core).await?;
    let now = Utc::now();
    let mut out = Vec::new();

    for provider in providers {
        let config = get_provider_config(core, provider.id()).await?;
        if !config.enabled || !config.include_in_briefing {
            continue;
        }

        let mut signals = provider.evaluate(core, &config).await?;
        apply_provider_weight(&mut signals, config.weight);
        signals.retain(|signal| {
            signal.score >= config.min_score
                && signal.confidence >= config.min_confidence
                && signal_is_visible_with_feedback(feedback.get(&signal.id), now)
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

pub async fn list_feedback(
    core: &AtomicCore,
) -> Result<HashMap<String, KnowledgeSignalFeedback>, AtomicCoreError> {
    list_feedback_inner(core).await
}

pub fn signal_is_visible_with_feedback(
    feedback: Option<&KnowledgeSignalFeedback>,
    now: DateTime<Utc>,
) -> bool {
    match feedback {
        Some(fb) if matches!(fb.state.as_str(), "dismissed" | "ignored") => false,
        Some(fb) if fb.state == "snoozed" => fb
            .snoozed_until
            .as_deref()
            .and_then(|raw| chrono::DateTime::parse_from_rfc3339(raw).ok())
            .map(|until| until.with_timezone(&Utc) <= now)
            .unwrap_or(true),
        _ => true,
    }
}

fn apply_provider_weight(signals: &mut [KnowledgeSignal], weight: f32) {
    for signal in signals {
        signal.score = (signal.score * weight).clamp(0.0, 100.0);
    }
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
            core.dashboard_signal_cache.invalidate();
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
            core.dashboard_signal_cache.invalidate();
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
                Ok::<(), AtomicCoreError>(())
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
    }?;
    core.dashboard_signal_cache.invalidate();
    Ok(())
}

pub async fn apply_signal_action(
    core: &AtomicCore,
    signal_key: &str,
    request: KnowledgeSignalActionRequest,
) -> Result<KnowledgeSignalActionResult, AtomicCoreError> {
    let signal = find_signal_for_action(core, signal_key).await?;
    if !signal
        .suggested_actions
        .iter()
        .any(|action| action.id == request.action)
    {
        return Err(AtomicCoreError::Validation(format!(
            "Action '{}' is not available for this signal",
            request.action
        )));
    }

    let result = match request.action.as_str() {
        "generate_wiki" => apply_generate_wiki_action(core, &signal).await?,
        "update_wiki" => apply_update_wiki_action(core, &signal).await?,
        "add_tag_to_atom" => apply_add_tag_to_atom_action(core, &signal).await?,
        "delete_empty_tag" => apply_delete_empty_tag_action(core, &signal).await?,
        "merge_tags" => apply_merge_tags_action(core, &signal, &request.payload).await?,
        other => {
            return Err(AtomicCoreError::Validation(format!(
                "Unsupported knowledge signal action: {other}"
            )));
        }
    };

    set_feedback(core, signal_key, "dismissed", None).await?;
    Ok(result)
}

async fn apply_generate_wiki_action(
    core: &AtomicCore,
    signal: &KnowledgeSignal,
) -> Result<KnowledgeSignalActionResult, AtomicCoreError> {
    if signal.provider_id != WIKI_CANDIDATE_PROVIDER_ID {
        return Err(AtomicCoreError::Validation(
            "generate_wiki is only supported for wiki-candidate signals".to_string(),
        ));
    }
    let evidence: WikiCandidateEvidence = serde_json::from_value(signal.evidence.clone())?;
    let action_log_id = uuid::Uuid::new_v4().to_string();
    let executed_at = Utc::now().to_rfc3339();
    let tag_id = evidence.tag_id.clone();
    let tag_name = evidence.tag_name.clone();

    insert_pending_action_log(
        core,
        KnowledgeSignalActionLog {
            id: action_log_id.clone(),
            signal_key: signal.id.clone(),
            provider_id: signal.provider_id.clone(),
            action: "generate_wiki".to_string(),
            target_type: signal.target.kind.clone(),
            target_id: Some(tag_id.clone()),
            before_state: json!({
                "tag_id": tag_id.clone(),
                "tag_name": tag_name.clone(),
                "had_wiki": false,
                "atom_count": evidence.atom_count,
            }),
            after_state: json!({}),
            status: "pending".to_string(),
            error: None,
            executed_at,
            undone_at: None,
        },
    )
    .await?;

    match core.generate_wiki(&tag_id, &tag_name).await {
        Ok(article) => {
            let after = json!({
                "tag_id": tag_id.clone(),
                "tag_name": tag_name.clone(),
                "article_id": article.article.id,
                "citation_count": article.citations.len(),
            });
            update_action_log_status(core, &action_log_id, "applied", after.clone(), None).await?;
            Ok(KnowledgeSignalActionResult {
                action_log_id,
                signal_key: signal.id.clone(),
                provider_id: signal.provider_id.clone(),
                action: "generate_wiki".to_string(),
                status: "applied".to_string(),
                undo_supported: false,
                result: after,
            })
        }
        Err(err) => {
            let message = err.to_string();
            let _ = update_action_log_status(
                core,
                &action_log_id,
                "failed",
                json!({}),
                Some(message.clone()),
            )
            .await;
            Err(AtomicCoreError::Validation(message))
        }
    }
}

async fn apply_update_wiki_action(
    core: &AtomicCore,
    signal: &KnowledgeSignal,
) -> Result<KnowledgeSignalActionResult, AtomicCoreError> {
    if signal.provider_id != WIKI_UPDATE_PROVIDER_ID {
        return Err(AtomicCoreError::Validation(
            "update_wiki is only supported for wiki-update signals".to_string(),
        ));
    }
    let evidence: WikiUpdateEvidence = serde_json::from_value(signal.evidence.clone())?;
    let action_log_id = uuid::Uuid::new_v4().to_string();
    let executed_at = Utc::now().to_rfc3339();
    let tag_id = evidence.tag_id.clone();
    let tag_name = evidence.tag_name.clone();

    insert_pending_action_log(
        core,
        KnowledgeSignalActionLog {
            id: action_log_id.clone(),
            signal_key: signal.id.clone(),
            provider_id: signal.provider_id.clone(),
            action: "update_wiki".to_string(),
            target_type: signal.target.kind.clone(),
            target_id: Some(tag_id.clone()),
            before_state: json!({
                "tag_id": tag_id.clone(),
                "tag_name": tag_name.clone(),
                "article_id": evidence.article_id,
                "article_atom_count": evidence.article_atom_count,
                "current_atom_count": evidence.current_atom_count,
                "new_atom_count": evidence.new_atom_count,
            }),
            after_state: json!({}),
            status: "pending".to_string(),
            error: None,
            executed_at,
            undone_at: None,
        },
    )
    .await?;

    match core.propose_wiki_update(&tag_id, &tag_name).await {
        Ok(Some(proposal)) => {
            let after = json!({
                "tag_id": tag_id.clone(),
                "tag_name": tag_name.clone(),
                "status": "proposal_created",
                "proposal_id": proposal.id,
                "new_atom_count": proposal.new_atom_count,
            });
            update_action_log_status(core, &action_log_id, "applied", after.clone(), None).await?;
            Ok(KnowledgeSignalActionResult {
                action_log_id,
                signal_key: signal.id.clone(),
                provider_id: signal.provider_id.clone(),
                action: "update_wiki".to_string(),
                status: "applied".to_string(),
                undo_supported: false,
                result: after,
            })
        }
        Ok(None) => {
            let after = json!({
                "tag_id": tag_id.clone(),
                "tag_name": tag_name.clone(),
                "status": "no_update_needed",
            });
            update_action_log_status(core, &action_log_id, "applied", after.clone(), None).await?;
            Ok(KnowledgeSignalActionResult {
                action_log_id,
                signal_key: signal.id.clone(),
                provider_id: signal.provider_id.clone(),
                action: "update_wiki".to_string(),
                status: "applied".to_string(),
                undo_supported: false,
                result: after,
            })
        }
        Err(err) => {
            let message = err.to_string();
            let _ = update_action_log_status(
                core,
                &action_log_id,
                "failed",
                json!({}),
                Some(message.clone()),
            )
            .await;
            Err(AtomicCoreError::Validation(message))
        }
    }
}

pub async fn undo_signal_action(
    core: &AtomicCore,
    action_log_id: &str,
) -> Result<KnowledgeSignalActionResult, AtomicCoreError> {
    let log = get_action_log(core, action_log_id).await?;
    if log.status == "undone" {
        return Ok(KnowledgeSignalActionResult {
            action_log_id: log.id,
            signal_key: log.signal_key,
            provider_id: log.provider_id,
            action: log.action.clone(),
            status: "undone".to_string(),
            undo_supported: log.action == "add_tag_to_atom",
            result: json!({ "already_undone": true }),
        });
    }
    if log.status != "applied" {
        return Err(AtomicCoreError::Validation(
            "Only applied signal actions can be undone".to_string(),
        ));
    }
    match log.action.as_str() {
        "add_tag_to_atom" => undo_add_tag_to_atom_action(core, &log).await,
        _ => Err(AtomicCoreError::Validation(
            "Undo is not supported for this signal action".to_string(),
        )),
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
    if provider_id == WIKI_CANDIDATE_PROVIDER_ID || provider_id == WIKI_UPDATE_PROVIDER_ID {
        config.include_in_briefing = true;
    }
    if provider_id == WIKI_UPDATE_PROVIDER_ID {
        config.min_score = 10.0;
        config.min_confidence = 0.1;
    }
    if provider_id == TAG_REDUNDANCY_PROVIDER_ID {
        config.include_in_briefing = true;
        config.min_score = 45.0;
        config.min_confidence = 0.55;
    }
    if provider_id == EMPTY_TAG_PROVIDER_ID {
        config.include_in_briefing = false;
        config.min_score = 10.0;
        config.min_confidence = 0.8;
    }
    if provider_id == MISSING_TAG_OVERLAP_PROVIDER_ID {
        config.include_in_briefing = false;
        config.min_score = 50.0;
        config.min_confidence = 0.65;
    }
    if provider_id == NEAR_DUPLICATE_ATOM_PROVIDER_ID {
        config.include_in_briefing = false;
        config.min_score = 55.0;
        config.min_confidence = 0.60;
    }
    if provider_id == SOURCE_DUPLICATE_PROVIDER_ID {
        config.include_in_briefing = false;
        config.min_score = 60.0;
        config.min_confidence = 0.80;
    }
    if provider_id == BROKEN_INTERNAL_LINK_PROVIDER_ID {
        config.include_in_briefing = false;
        config.min_score = 35.0;
        config.min_confidence = 0.65;
    }
    if provider_id == UNDERCONNECTED_ATOM_PROVIDER_ID {
        config.include_in_briefing = false;
        config.min_score = 55.0;
        config.min_confidence = 0.60;
    }
    config
}

fn signal_providers() -> Vec<Box<dyn KnowledgeSignalProvider>> {
    vec![
        Box::new(WikiCandidateProvider),
        Box::new(WikiUpdateProvider),
        Box::new(TagRedundancyProvider),
        Box::new(EmptyTagProvider),
        Box::new(MissingTagOverlapProvider),
        Box::new(NearDuplicateAtomProvider),
        Box::new(SourceDuplicateProvider),
        Box::new(BrokenInternalLinkProvider),
        Box::new(UnderconnectedAtomProvider),
    ]
}

async fn list_feedback_inner(
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
                Ok::<(), AtomicCoreError>(())
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
    }?;
    core.dashboard_signal_cache.invalidate();
    Ok(())
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

async fn find_signal_for_action(
    core: &AtomicCore,
    signal_key: &str,
) -> Result<KnowledgeSignal, AtomicCoreError> {
    let (provider_id, _, _) = parse_signal_key(signal_key)?;
    let providers = signal_providers();
    let provider = providers
        .into_iter()
        .find(|provider| provider.id() == provider_id)
        .ok_or_else(|| AtomicCoreError::Validation("Unknown signal provider".to_string()))?;
    let config = get_provider_config(core, provider.id()).await?;
    if !config.enabled {
        return Err(AtomicCoreError::Validation(
            "This signal provider is disabled".to_string(),
        ));
    }
    let feedback = list_feedback(core).await?;
    let now = Utc::now();
    let mut signals = provider.evaluate(core, &config).await?;
    apply_provider_weight(&mut signals, config.weight);
    signals.retain(|signal| {
        signal.score >= config.min_score
            && signal.confidence >= config.min_confidence
            && signal_is_visible_with_feedback(feedback.get(&signal.id), now)
    });
    signals
        .into_iter()
        .find(|signal| signal.id == signal_key)
        .ok_or_else(|| {
            AtomicCoreError::Validation(
                "This knowledge signal is no longer available to act on".to_string(),
            )
        })
}

async fn apply_add_tag_to_atom_action(
    core: &AtomicCore,
    signal: &KnowledgeSignal,
) -> Result<KnowledgeSignalActionResult, AtomicCoreError> {
    if signal.provider_id != MISSING_TAG_OVERLAP_PROVIDER_ID {
        return Err(AtomicCoreError::Validation(
            "add_tag_to_atom is only supported for missing-tag signals".to_string(),
        ));
    }
    let evidence: MissingTagOverlapEvidence = serde_json::from_value(signal.evidence.clone())?;
    let action_log_id = uuid::Uuid::new_v4().to_string();
    let executed_at = Utc::now().to_rfc3339();

    match &core.storage {
        StorageBackend::Sqlite(storage) => {
            let storage = storage.clone();
            let signal = signal.clone();
            let evidence = evidence.clone();
            let action_log_id_for_task = action_log_id.clone();
            let executed_at_for_task = executed_at.clone();
            tokio::task::spawn_blocking(move || {
                let conn = storage
                    .database()
                    .conn
                    .lock()
                    .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
                let tx = conn.unchecked_transaction()?;
                let previous_updated_at: String = tx
                    .query_row(
                        "SELECT updated_at FROM atoms WHERE id = ?1",
                        [&evidence.atom_id],
                        |row| row.get(0),
                    )
                    .optional()?
                    .ok_or_else(|| AtomicCoreError::NotFound("Atom not found".to_string()))?;
                let tag_exists: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM tags WHERE id = ?1)",
                    [&evidence.suggested_tag.id],
                    |row| row.get(0),
                )?;
                if !tag_exists {
                    return Err(AtomicCoreError::NotFound("Tag not found".to_string()));
                }
                let assignment_existed_before: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM atom_tags WHERE atom_id = ?1 AND tag_id = ?2)",
                    rusqlite::params![&evidence.atom_id, &evidence.suggested_tag.id],
                    |row| row.get(0),
                )?;
                let inserted = tx.execute(
                    "INSERT OR IGNORE INTO atom_tags (atom_id, tag_id, source)
                     VALUES (?1, ?2, 'manual')",
                    rusqlite::params![&evidence.atom_id, &evidence.suggested_tag.id],
                )? as i32;
                let updated_at = Utc::now().to_rfc3339();
                if inserted > 0 {
                    tx.execute(
                        "UPDATE atoms SET updated_at = ?1 WHERE id = ?2",
                        rusqlite::params![&updated_at, &evidence.atom_id],
                    )?;
                }
                let before = json!({
                    "atom_id": evidence.atom_id,
                    "tag_id": evidence.suggested_tag.id,
                    "assignment_existed": assignment_existed_before,
                    "atom_updated_at": previous_updated_at,
                });
                let after = json!({
                    "atom_id": evidence.atom_id,
                    "tag_id": evidence.suggested_tag.id,
                    "assignment_exists": true,
                    "action_added_assignment": inserted > 0,
                    "atom_updated_at": if inserted > 0 { updated_at } else { previous_updated_at },
                });
                insert_action_log_sqlite(
                    &tx,
                    &KnowledgeSignalActionLog {
                        id: action_log_id_for_task,
                        signal_key: signal.id,
                        provider_id: signal.provider_id,
                        action: "add_tag_to_atom".to_string(),
                        target_type: signal.target.kind,
                        target_id: Some(evidence.atom_id),
                        before_state: before,
                        after_state: after,
                        status: "applied".to_string(),
                        error: None,
                        executed_at: executed_at_for_task,
                        undone_at: None,
                    },
                )?;
                tx.commit()?;
                Ok::<(), AtomicCoreError>(())
            })
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))??;
        }
        #[cfg(feature = "postgres")]
        StorageBackend::Postgres(storage) => {
            let mut tx = storage
                .pool
                .begin()
                .await
                .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
            let previous_updated_at: Option<String> =
                sqlx::query_scalar("SELECT updated_at FROM atoms WHERE id = $1 AND db_id = $2")
                    .bind(&evidence.atom_id)
                    .bind(&storage.db_id)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
            let previous_updated_at = previous_updated_at
                .ok_or_else(|| AtomicCoreError::NotFound("Atom not found".to_string()))?;
            let tag_exists: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM tags WHERE id = $1 AND db_id = $2)",
            )
            .bind(&evidence.suggested_tag.id)
            .bind(&storage.db_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
            if !tag_exists {
                return Err(AtomicCoreError::NotFound("Tag not found".to_string()));
            }
            let assignment_existed_before: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM atom_tags WHERE atom_id = $1 AND tag_id = $2 AND db_id = $3)",
            )
            .bind(&evidence.atom_id)
            .bind(&evidence.suggested_tag.id)
            .bind(&storage.db_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
            let inserted = sqlx::query(
                "INSERT INTO atom_tags (atom_id, tag_id, db_id, source)
                 VALUES ($1, $2, $3, 'manual')
                 ON CONFLICT DO NOTHING",
            )
            .bind(&evidence.atom_id)
            .bind(&evidence.suggested_tag.id)
            .bind(&storage.db_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?
            .rows_affected();
            let updated_at = Utc::now().to_rfc3339();
            if inserted > 0 {
                sqlx::query("UPDATE atoms SET updated_at = $1 WHERE id = $2 AND db_id = $3")
                    .bind(&updated_at)
                    .bind(&evidence.atom_id)
                    .bind(&storage.db_id)
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
            }
            let before = json!({
                "atom_id": evidence.atom_id,
                "tag_id": evidence.suggested_tag.id,
                "assignment_existed": assignment_existed_before,
                "atom_updated_at": previous_updated_at,
            });
            let after = json!({
                "atom_id": evidence.atom_id,
                "tag_id": evidence.suggested_tag.id,
                "assignment_exists": true,
                "action_added_assignment": inserted > 0,
                "atom_updated_at": if inserted > 0 { updated_at } else { previous_updated_at },
            });
            insert_action_log_postgres(
                &mut tx,
                &storage.db_id,
                &KnowledgeSignalActionLog {
                    id: action_log_id.clone(),
                    signal_key: signal.id.clone(),
                    provider_id: signal.provider_id.clone(),
                    action: "add_tag_to_atom".to_string(),
                    target_type: signal.target.kind.clone(),
                    target_id: Some(evidence.atom_id.clone()),
                    before_state: before,
                    after_state: after,
                    status: "applied".to_string(),
                    error: None,
                    executed_at: executed_at.clone(),
                    undone_at: None,
                },
            )
            .await?;
            tx.commit()
                .await
                .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        }
    }

    core.canvas_cache.invalidate();
    Ok(KnowledgeSignalActionResult {
        action_log_id,
        signal_key: signal.id.clone(),
        provider_id: signal.provider_id.clone(),
        action: "add_tag_to_atom".to_string(),
        status: "applied".to_string(),
        undo_supported: true,
        result: json!({
            "atom_id": evidence.atom_id,
            "tag_id": evidence.suggested_tag.id,
        }),
    })
}

async fn apply_delete_empty_tag_action(
    core: &AtomicCore,
    signal: &KnowledgeSignal,
) -> Result<KnowledgeSignalActionResult, AtomicCoreError> {
    if signal.provider_id != EMPTY_TAG_PROVIDER_ID {
        return Err(AtomicCoreError::Validation(
            "delete_empty_tag is only supported for empty-tag signals".to_string(),
        ));
    }
    let evidence: EmptyTagEvidence = serde_json::from_value(signal.evidence.clone())?;
    let action_log_id = uuid::Uuid::new_v4().to_string();
    let executed_at = Utc::now().to_rfc3339();

    match &core.storage {
        StorageBackend::Sqlite(storage) => {
            let storage = storage.clone();
            let signal = signal.clone();
            let evidence = evidence.clone();
            let action_log_id_for_task = action_log_id.clone();
            let executed_at_for_task = executed_at.clone();
            tokio::task::spawn_blocking(move || {
                let conn = storage
                    .database()
                    .conn
                    .lock()
                    .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
                let tx = conn.unchecked_transaction()?;
                let tag_exists: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM tags WHERE id = ?1)",
                    [&evidence.tag.id],
                    |row| row.get(0),
                )?;
                if !tag_exists {
                    return Err(AtomicCoreError::NotFound("Tag not found".to_string()));
                }
                let atom_assignment_count: i32 = tx.query_row(
                    "SELECT COUNT(*) FROM atom_tags WHERE tag_id = ?1",
                    [&evidence.tag.id],
                    |row| row.get(0),
                )?;
                let child_count: i32 = tx.query_row(
                    "SELECT COUNT(*) FROM tags WHERE parent_id = ?1",
                    [&evidence.tag.id],
                    |row| row.get(0),
                )?;
                if atom_assignment_count != 0 || child_count != 0 {
                    return Err(AtomicCoreError::Validation(
                        "Tag is no longer empty".to_string(),
                    ));
                }
                tx.execute("DELETE FROM tags WHERE id = ?1", [&evidence.tag.id])?;
                insert_action_log_sqlite(
                    &tx,
                    &KnowledgeSignalActionLog {
                        id: action_log_id_for_task,
                        signal_key: signal.id,
                        provider_id: signal.provider_id,
                        action: "delete_empty_tag".to_string(),
                        target_type: signal.target.kind,
                        target_id: Some(evidence.tag.id.clone()),
                        before_state: json!({ "tag": evidence.tag }),
                        after_state: json!({ "deleted": true }),
                        status: "applied".to_string(),
                        error: None,
                        executed_at: executed_at_for_task,
                        undone_at: None,
                    },
                )?;
                tx.commit()?;
                Ok::<(), AtomicCoreError>(())
            })
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))??;
        }
        #[cfg(feature = "postgres")]
        StorageBackend::Postgres(storage) => {
            let mut tx = storage
                .pool
                .begin()
                .await
                .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
            let tag_exists: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM tags WHERE id = $1 AND db_id = $2)",
            )
            .bind(&evidence.tag.id)
            .bind(&storage.db_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
            if !tag_exists {
                return Err(AtomicCoreError::NotFound("Tag not found".to_string()));
            }
            let atom_assignment_count: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM atom_tags WHERE tag_id = $1 AND db_id = $2",
            )
            .bind(&evidence.tag.id)
            .bind(&storage.db_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
            let child_count: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM tags WHERE parent_id = $1 AND db_id = $2")
                    .bind(&evidence.tag.id)
                    .bind(&storage.db_id)
                    .fetch_one(&mut *tx)
                    .await
                    .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
            if atom_assignment_count != 0 || child_count != 0 {
                return Err(AtomicCoreError::Validation(
                    "Tag is no longer empty".to_string(),
                ));
            }
            sqlx::query("DELETE FROM tags WHERE id = $1 AND db_id = $2")
                .bind(&evidence.tag.id)
                .bind(&storage.db_id)
                .execute(&mut *tx)
                .await
                .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
            insert_action_log_postgres(
                &mut tx,
                &storage.db_id,
                &KnowledgeSignalActionLog {
                    id: action_log_id.clone(),
                    signal_key: signal.id.clone(),
                    provider_id: signal.provider_id.clone(),
                    action: "delete_empty_tag".to_string(),
                    target_type: signal.target.kind.clone(),
                    target_id: Some(evidence.tag.id.clone()),
                    before_state: json!({ "tag": evidence.tag }),
                    after_state: json!({ "deleted": true }),
                    status: "applied".to_string(),
                    error: None,
                    executed_at: executed_at.clone(),
                    undone_at: None,
                },
            )
            .await?;
            tx.commit()
                .await
                .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        }
    }

    core.canvas_cache.invalidate();
    Ok(KnowledgeSignalActionResult {
        action_log_id,
        signal_key: signal.id.clone(),
        provider_id: signal.provider_id.clone(),
        action: "delete_empty_tag".to_string(),
        status: "applied".to_string(),
        undo_supported: false,
        result: json!({ "tag_id": evidence.tag.id }),
    })
}

async fn apply_merge_tags_action(
    core: &AtomicCore,
    signal: &KnowledgeSignal,
    payload: &Value,
) -> Result<KnowledgeSignalActionResult, AtomicCoreError> {
    if signal.provider_id != TAG_REDUNDANCY_PROVIDER_ID {
        return Err(AtomicCoreError::Validation(
            "merge_tags is only supported for tag-redundancy signals".to_string(),
        ));
    }
    let evidence: TagRedundancyEvidence = serde_json::from_value(signal.evidence.clone())?;
    let source_tag_id = payload
        .get("source_tag_id")
        .or_else(|| payload.get("sourceTagId"))
        .and_then(Value::as_str)
        .ok_or_else(|| AtomicCoreError::Validation("source_tag_id is required".to_string()))?;
    let target_tag_id = payload
        .get("target_tag_id")
        .or_else(|| payload.get("targetTagId"))
        .and_then(Value::as_str)
        .ok_or_else(|| AtomicCoreError::Validation("target_tag_id is required".to_string()))?;
    let source_in_pair =
        source_tag_id == evidence.primary_tag.id || source_tag_id == evidence.secondary_tag.id;
    let target_in_pair =
        target_tag_id == evidence.primary_tag.id || target_tag_id == evidence.secondary_tag.id;
    if !source_in_pair || !target_in_pair || source_tag_id == target_tag_id {
        return Err(AtomicCoreError::Validation(
            "Merge tags must match the reviewed signal pair".to_string(),
        ));
    }

    let action_log_id = uuid::Uuid::new_v4().to_string();
    let executed_at = Utc::now().to_rfc3339();
    let before = json!({
        "primary_tag": evidence.primary_tag,
        "secondary_tag": evidence.secondary_tag,
        "source_tag_id": source_tag_id,
        "target_tag_id": target_tag_id,
        "shared_atom_count": evidence.shared_atom_count,
        "primary_unique_atom_count": evidence.primary_unique_atom_count,
        "secondary_unique_atom_count": evidence.secondary_unique_atom_count,
    });

    insert_pending_action_log(
        core,
        KnowledgeSignalActionLog {
            id: action_log_id.clone(),
            signal_key: signal.id.clone(),
            provider_id: signal.provider_id.clone(),
            action: "merge_tags".to_string(),
            target_type: signal.target.kind.clone(),
            target_id: Some(target_tag_id.to_string()),
            before_state: before,
            after_state: json!({}),
            status: "pending".to_string(),
            error: None,
            executed_at: executed_at.clone(),
            undone_at: None,
        },
    )
    .await?;

    match core.merge_tags(source_tag_id, target_tag_id).await {
        Ok(result) => {
            let result_json = serde_json::to_value(&result)?;
            update_action_log_status(core, &action_log_id, "applied", result_json.clone(), None)
                .await?;
            Ok(KnowledgeSignalActionResult {
                action_log_id,
                signal_key: signal.id.clone(),
                provider_id: signal.provider_id.clone(),
                action: "merge_tags".to_string(),
                status: "applied".to_string(),
                undo_supported: false,
                result: result_json,
            })
        }
        Err(err) => {
            let message = err.to_string();
            let _ = update_action_log_status(
                core,
                &action_log_id,
                "failed",
                json!({}),
                Some(message.clone()),
            )
            .await;
            Err(AtomicCoreError::Validation(message))
        }
    }
}

async fn undo_add_tag_to_atom_action(
    core: &AtomicCore,
    log: &KnowledgeSignalActionLog,
) -> Result<KnowledgeSignalActionResult, AtomicCoreError> {
    let atom_id = log
        .after_state
        .get("atom_id")
        .and_then(Value::as_str)
        .ok_or_else(|| AtomicCoreError::Validation("Action log missing atom_id".to_string()))?;
    let tag_id = log
        .after_state
        .get("tag_id")
        .and_then(Value::as_str)
        .ok_or_else(|| AtomicCoreError::Validation("Action log missing tag_id".to_string()))?;
    let action_added_assignment = log
        .after_state
        .get("action_added_assignment")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    match &core.storage {
        StorageBackend::Sqlite(storage) => {
            let storage = storage.clone();
            let log_id = log.id.clone();
            let atom_id = atom_id.to_string();
            let tag_id = tag_id.to_string();
            let action_added_assignment = action_added_assignment;
            tokio::task::spawn_blocking(move || {
                let conn = storage
                    .database()
                    .conn
                    .lock()
                    .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
                let tx = conn.unchecked_transaction()?;
                if action_added_assignment {
                    tx.execute(
                        "DELETE FROM atom_tags WHERE atom_id = ?1 AND tag_id = ?2",
                        rusqlite::params![&atom_id, &tag_id],
                    )?;
                    tx.execute(
                        "UPDATE atoms SET updated_at = ?1 WHERE id = ?2",
                        rusqlite::params![Utc::now().to_rfc3339(), &atom_id],
                    )?;
                }
                tx.execute(
                    "UPDATE knowledge_signal_action_log
                     SET status = 'undone', undone_at = ?1
                     WHERE id = ?2 AND status = 'applied'",
                    rusqlite::params![Utc::now().to_rfc3339(), &log_id],
                )?;
                tx.commit()?;
                Ok::<(), AtomicCoreError>(())
            })
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))??;
        }
        #[cfg(feature = "postgres")]
        StorageBackend::Postgres(storage) => {
            let mut tx = storage
                .pool
                .begin()
                .await
                .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
            if action_added_assignment {
                sqlx::query(
                    "DELETE FROM atom_tags WHERE atom_id = $1 AND tag_id = $2 AND db_id = $3",
                )
                .bind(atom_id)
                .bind(tag_id)
                .bind(&storage.db_id)
                .execute(&mut *tx)
                .await
                .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
                sqlx::query("UPDATE atoms SET updated_at = $1 WHERE id = $2 AND db_id = $3")
                    .bind(Utc::now().to_rfc3339())
                    .bind(atom_id)
                    .bind(&storage.db_id)
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
            }
            sqlx::query(
                "UPDATE knowledge_signal_action_log
                 SET status = 'undone', undone_at = $1
                 WHERE db_id = $2 AND id = $3 AND status = 'applied'",
            )
            .bind(Utc::now().to_rfc3339())
            .bind(&storage.db_id)
            .bind(&log.id)
            .execute(&mut *tx)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
            tx.commit()
                .await
                .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        }
    }
    core.canvas_cache.invalidate();
    Ok(KnowledgeSignalActionResult {
        action_log_id: log.id.clone(),
        signal_key: log.signal_key.clone(),
        provider_id: log.provider_id.clone(),
        action: log.action.clone(),
        status: "undone".to_string(),
        undo_supported: true,
        result: json!({
            "atom_id": atom_id,
            "tag_id": tag_id,
            "removed_assignment": action_added_assignment,
        }),
    })
}

fn insert_action_log_sqlite(
    tx: &rusqlite::Transaction<'_>,
    log: &KnowledgeSignalActionLog,
) -> Result<(), AtomicCoreError> {
    tx.execute(
        "INSERT INTO knowledge_signal_action_log
            (id, signal_key, provider_id, action, target_type, target_id,
             before_state_json, after_state_json, status, error, executed_at, undone_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        rusqlite::params![
            log.id,
            log.signal_key,
            log.provider_id,
            log.action,
            log.target_type,
            log.target_id,
            serde_json::to_string(&log.before_state)?,
            serde_json::to_string(&log.after_state)?,
            log.status,
            log.error,
            log.executed_at,
            log.undone_at,
        ],
    )?;
    Ok(())
}

#[cfg(feature = "postgres")]
async fn insert_action_log_postgres(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    db_id: &str,
    log: &KnowledgeSignalActionLog,
) -> Result<(), AtomicCoreError> {
    sqlx::query(
        "INSERT INTO knowledge_signal_action_log
            (db_id, id, signal_key, provider_id, action, target_type, target_id,
             before_state_json, after_state_json, status, error, executed_at, undone_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)",
    )
    .bind(db_id)
    .bind(&log.id)
    .bind(&log.signal_key)
    .bind(&log.provider_id)
    .bind(&log.action)
    .bind(&log.target_type)
    .bind(&log.target_id)
    .bind(serde_json::to_string(&log.before_state)?)
    .bind(serde_json::to_string(&log.after_state)?)
    .bind(&log.status)
    .bind(&log.error)
    .bind(&log.executed_at)
    .bind(&log.undone_at)
    .execute(&mut **tx)
    .await
    .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
    Ok(())
}

async fn insert_pending_action_log(
    core: &AtomicCore,
    log: KnowledgeSignalActionLog,
) -> Result<(), AtomicCoreError> {
    match &core.storage {
        StorageBackend::Sqlite(storage) => {
            let storage = storage.clone();
            tokio::task::spawn_blocking(move || {
                let conn = storage
                    .database()
                    .conn
                    .lock()
                    .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
                let tx = conn.unchecked_transaction()?;
                insert_action_log_sqlite(&tx, &log)?;
                tx.commit()?;
                Ok(())
            })
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?
        }
        #[cfg(feature = "postgres")]
        StorageBackend::Postgres(storage) => {
            let mut tx = storage
                .pool
                .begin()
                .await
                .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
            insert_action_log_postgres(&mut tx, &storage.db_id, &log).await?;
            tx.commit()
                .await
                .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
            Ok(())
        }
    }
}

async fn update_action_log_status(
    core: &AtomicCore,
    action_log_id: &str,
    status: &str,
    after_state: Value,
    error: Option<String>,
) -> Result<(), AtomicCoreError> {
    match &core.storage {
        StorageBackend::Sqlite(storage) => {
            let storage = storage.clone();
            let id = action_log_id.to_string();
            let status = status.to_string();
            let after_state_json = serde_json::to_string(&after_state)?;
            tokio::task::spawn_blocking(move || {
                let conn = storage
                    .database()
                    .conn
                    .lock()
                    .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
                conn.execute(
                    "UPDATE knowledge_signal_action_log
                     SET status = ?1, after_state_json = ?2, error = ?3
                     WHERE id = ?4",
                    rusqlite::params![status, after_state_json, error, id],
                )?;
                Ok(())
            })
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?
        }
        #[cfg(feature = "postgres")]
        StorageBackend::Postgres(storage) => {
            sqlx::query(
                "UPDATE knowledge_signal_action_log
                 SET status = $1, after_state_json = $2, error = $3
                 WHERE db_id = $4 AND id = $5",
            )
            .bind(status)
            .bind(serde_json::to_string(&after_state)?)
            .bind(error)
            .bind(&storage.db_id)
            .bind(action_log_id)
            .execute(&storage.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
            Ok(())
        }
    }
}

async fn get_action_log(
    core: &AtomicCore,
    action_log_id: &str,
) -> Result<KnowledgeSignalActionLog, AtomicCoreError> {
    match &core.storage {
        StorageBackend::Sqlite(storage) => {
            let storage = storage.clone();
            let id = action_log_id.to_string();
            tokio::task::spawn_blocking(move || {
                let conn = storage.database().read_conn()?;
                conn.query_row(
                    "SELECT id, signal_key, provider_id, action, target_type, target_id,
                            before_state_json, after_state_json, status, error, executed_at, undone_at
                     FROM knowledge_signal_action_log
                     WHERE id = ?1",
                    [&id],
                    |row| {
                        let before: String = row.get(6)?;
                        let after: String = row.get(7)?;
                        Ok(KnowledgeSignalActionLog {
                            id: row.get(0)?,
                            signal_key: row.get(1)?,
                            provider_id: row.get(2)?,
                            action: row.get(3)?,
                            target_type: row.get(4)?,
                            target_id: row.get(5)?,
                            before_state: serde_json::from_str(&before).unwrap_or_else(|_| json!({})),
                            after_state: serde_json::from_str(&after).unwrap_or_else(|_| json!({})),
                            status: row.get(8)?,
                            error: row.get(9)?,
                            executed_at: row.get(10)?,
                            undone_at: row.get(11)?,
                        })
                    },
                )
                .optional()?
                .ok_or_else(|| AtomicCoreError::NotFound("Action log not found".to_string()))
            })
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?
        }
        #[cfg(feature = "postgres")]
        StorageBackend::Postgres(storage) => {
            let row: Option<(
                String,
                String,
                String,
                String,
                String,
                Option<String>,
                Option<String>,
                Option<String>,
                String,
                Option<String>,
                String,
                Option<String>,
            )> = sqlx::query_as(
                "SELECT id, signal_key, provider_id, action, target_type, target_id,
                        before_state_json, after_state_json, status, error, executed_at, undone_at
                 FROM knowledge_signal_action_log
                 WHERE db_id = $1 AND id = $2",
            )
            .bind(&storage.db_id)
            .bind(action_log_id)
            .fetch_optional(&storage.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
            let Some(row) = row else {
                return Err(AtomicCoreError::NotFound(
                    "Action log not found".to_string(),
                ));
            };
            Ok(KnowledgeSignalActionLog {
                id: row.0,
                signal_key: row.1,
                provider_id: row.2,
                action: row.3,
                target_type: row.4,
                target_id: row.5,
                before_state: row
                    .6
                    .as_deref()
                    .and_then(|raw| serde_json::from_str(raw).ok())
                    .unwrap_or_else(|| json!({})),
                after_state: row
                    .7
                    .as_deref()
                    .and_then(|raw| serde_json::from_str(raw).ok())
                    .unwrap_or_else(|| json!({})),
                status: row.8,
                error: row.9,
                executed_at: row.10,
                undone_at: row.11,
            })
        }
    }
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
                            JOIN atoms a ON a.id = at.atom_id AND a.kind = 'captured'
                            GROUP BY at.tag_id
                        ),
                        intra_edges AS (
                            SELECT
                                at1.tag_id,
                                COUNT(*) as edge_count,
                                AVG(se.similarity_score) as avg_similarity
                            FROM semantic_edges se
                            JOIN atoms source ON source.id = se.source_atom_id AND source.kind = 'captured'
                            JOIN atoms target ON target.id = se.target_atom_id AND target.kind = 'captured'
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
                        JOIN atoms a ON a.id = at.atom_id AND a.db_id = at.db_id AND a.kind = 'captured'
                        WHERE at.db_id = $2
                        GROUP BY at.tag_id
                    ),
                    intra_edges AS (
                        SELECT
                            at1.tag_id,
                            COUNT(*)::BIGINT as edge_count,
                            AVG(se.similarity_score)::FLOAT8 as avg_similarity
                        FROM semantic_edges se
                        JOIN atoms source
                          ON source.id = se.source_atom_id
                         AND source.db_id = se.db_id
                         AND source.kind = 'captured'
                        JOIN atoms target
                          ON target.id = se.target_atom_id
                         AND target.db_id = se.db_id
                         AND target.kind = 'captured'
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

struct WikiUpdateProvider;

#[async_trait]
impl KnowledgeSignalProvider for WikiUpdateProvider {
    fn id(&self) -> &'static str {
        WIKI_UPDATE_PROVIDER_ID
    }

    fn name(&self) -> &'static str {
        "Wiki updates"
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
                    let recent_cutoff = (Utc::now() - Duration::days(14)).to_rfc3339();
                    let conn = storage.database().read_conn()?;
                    let mut stmt = conn.prepare(
                        "WITH RECURSIVE descendant_tags(root_tag_id, tag_id) AS (
                            SELECT wa.tag_id, wa.tag_id
                            FROM wiki_articles wa
                            UNION ALL
                            SELECT dt.root_tag_id, t.id
                            FROM tags t
                            JOIN descendant_tags dt ON t.parent_id = dt.tag_id
                        ),
                        tag_atoms AS (
                            SELECT
                                dt.root_tag_id as tag_id,
                                COUNT(DISTINCT a.id) as current_atom_count,
                                COUNT(DISTINCT CASE WHEN a.created_at > wa.updated_at THEN a.id END) as new_atom_count,
                                COUNT(DISTINCT CASE
                                    WHEN a.created_at > wa.updated_at
                                     AND a.source_url IS NOT NULL
                                     AND length(trim(a.source_url)) > 0
                                    THEN a.source_url
                                END) as new_source_count,
                                SUM(CASE
                                    WHEN a.created_at > wa.updated_at AND length(trim(a.content)) >= 200
                                    THEN 1 ELSE 0
                                END) as new_substantive_count,
                                SUM(CASE
                                    WHEN a.created_at > wa.updated_at AND a.created_at >= ?1
                                    THEN 1 ELSE 0
                                END) as new_recent_count
                            FROM descendant_tags dt
                            JOIN wiki_articles wa ON wa.tag_id = dt.root_tag_id
                            JOIN atom_tags at ON at.tag_id = dt.tag_id
                            JOIN atoms a ON a.id = at.atom_id AND a.kind = 'captured'
                            GROUP BY dt.root_tag_id
                        ),
                        inbound_mentions AS (
                            SELECT tag_id, SUM(cnt) as inbound_count FROM (
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
                        )
                        SELECT
                            wa.id,
                            wa.tag_id,
                            t.name,
                            wa.atom_count,
                            COALESCE(ta.current_atom_count, 0) as current_atom_count,
                            COALESCE(ta.new_atom_count, 0) as new_atom_count,
                            COALESCE(ta.new_source_count, 0) as new_source_count,
                            COALESCE(ta.new_substantive_count, 0) as new_substantive_count,
                            COALESCE(ta.new_recent_count, 0) as new_recent_count,
                            COALESCE(im.inbound_count, 0) as inbound_link_count,
                            wa.updated_at
                        FROM wiki_articles wa
                        JOIN tags t ON t.id = wa.tag_id
                        LEFT JOIN tag_atoms ta ON ta.tag_id = wa.tag_id
                        LEFT JOIN inbound_mentions im ON im.tag_id = wa.tag_id
                        WHERE COALESCE(ta.new_atom_count, 0) > 0
                           OR COALESCE(ta.current_atom_count, 0) > wa.atom_count",
                    )?;

                    let rows = stmt.query_map(params![recent_cutoff], |row| {
                        Ok(WikiUpdateRow {
                            article_id: row.get(0)?,
                            tag_id: row.get(1)?,
                            tag_name: row.get(2)?,
                            article_atom_count: row.get(3)?,
                            current_atom_count: row.get(4)?,
                            new_atom_count: row.get(5)?,
                            new_source_count: row.get(6)?,
                            new_substantive_count: row.get(7)?,
                            new_recent_count: row.get(8)?,
                            inbound_link_count: row.get(9)?,
                            updated_at: row.get(10)?,
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
                let recent_cutoff = (Utc::now() - Duration::days(14)).to_rfc3339();
                let rows = sqlx::query_as::<
                    _,
                    (String, String, String, i32, i64, i64, i64, i64, i64, i64, String),
                >(
                    "WITH RECURSIVE descendant_tags(root_tag_id, tag_id) AS (
                        SELECT wa.tag_id, wa.tag_id
                        FROM wiki_articles wa
                        WHERE wa.db_id = $2
                        UNION ALL
                        SELECT dt.root_tag_id, t.id
                        FROM tags t
                        JOIN descendant_tags dt ON t.parent_id = dt.tag_id
                        WHERE t.db_id = $2
                    ),
                    tag_atoms AS (
                        SELECT
                            dt.root_tag_id as tag_id,
                            COUNT(DISTINCT a.id)::BIGINT as current_atom_count,
                            COUNT(DISTINCT CASE WHEN a.created_at > wa.updated_at THEN a.id END)::BIGINT as new_atom_count,
                            COUNT(DISTINCT CASE
                                WHEN a.created_at > wa.updated_at
                                 AND a.source_url IS NOT NULL
                                 AND length(trim(a.source_url)) > 0
                                THEN a.source_url
                            END)::BIGINT as new_source_count,
                            SUM(CASE
                                WHEN a.created_at > wa.updated_at AND length(trim(a.content)) >= 200
                                THEN 1 ELSE 0
                            END)::BIGINT as new_substantive_count,
                            SUM(CASE
                                WHEN a.created_at > wa.updated_at AND a.created_at >= $1
                                THEN 1 ELSE 0
                            END)::BIGINT as new_recent_count
                        FROM descendant_tags dt
                        JOIN wiki_articles wa ON wa.tag_id = dt.root_tag_id AND wa.db_id = $2
                        JOIN atom_tags at ON at.tag_id = dt.tag_id AND at.db_id = $2
                        JOIN atoms a ON a.id = at.atom_id AND a.db_id = $2 AND a.kind = 'captured'
                        GROUP BY dt.root_tag_id
                    ),
                    inbound_mentions AS (
                        SELECT wl.target_tag_id as tag_id, COUNT(*)::BIGINT as inbound_count
                        FROM wiki_links wl
                        WHERE wl.target_tag_id IS NOT NULL
                          AND wl.db_id = $2
                        GROUP BY wl.target_tag_id
                    )
                    SELECT
                        wa.id,
                        wa.tag_id,
                        t.name,
                        wa.atom_count,
                        COALESCE(ta.current_atom_count, 0)::BIGINT as current_atom_count,
                        COALESCE(ta.new_atom_count, 0)::BIGINT as new_atom_count,
                        COALESCE(ta.new_source_count, 0)::BIGINT as new_source_count,
                        COALESCE(ta.new_substantive_count, 0)::BIGINT as new_substantive_count,
                        COALESCE(ta.new_recent_count, 0)::BIGINT as new_recent_count,
                        COALESCE(im.inbound_count, 0)::BIGINT as inbound_link_count,
                        wa.updated_at
                    FROM wiki_articles wa
                    JOIN tags t ON t.id = wa.tag_id AND t.db_id = $2
                    LEFT JOIN tag_atoms ta ON ta.tag_id = wa.tag_id
                    LEFT JOIN inbound_mentions im ON im.tag_id = wa.tag_id
                    WHERE wa.db_id = $2
                      AND (
                        COALESCE(ta.new_atom_count, 0) > 0
                        OR COALESCE(ta.current_atom_count, 0) > wa.atom_count
                      )",
                )
                .bind(recent_cutoff)
                .bind(&storage.db_id)
                .fetch_all(&storage.pool)
                .await
                .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

                let now = Utc::now().to_rfc3339();
                let mut signals = Vec::with_capacity(rows.len());
                for (
                    article_id,
                    tag_id,
                    tag_name,
                    article_atom_count,
                    current_atom_count,
                    new_atom_count,
                    new_source_count,
                    new_substantive_count,
                    new_recent_count,
                    inbound_link_count,
                    updated_at,
                ) in rows
                {
                    signals.push(
                        WikiUpdateRow {
                            article_id,
                            tag_id,
                            tag_name,
                            article_atom_count,
                            current_atom_count: current_atom_count as i32,
                            new_atom_count: new_atom_count as i32,
                            new_source_count: new_source_count as i32,
                            new_substantive_count: new_substantive_count as i32,
                            new_recent_count: new_recent_count as i32,
                            inbound_link_count: inbound_link_count as i32,
                            updated_at,
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

struct WikiUpdateRow {
    article_id: String,
    tag_id: String,
    tag_name: String,
    article_atom_count: i32,
    current_atom_count: i32,
    new_atom_count: i32,
    new_source_count: i32,
    new_substantive_count: i32,
    new_recent_count: i32,
    inbound_link_count: i32,
    updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct WikiUpdateEvidence {
    pub article_id: String,
    pub tag_id: String,
    pub tag_name: String,
    pub article_atom_count: i32,
    pub current_atom_count: i32,
    pub new_atom_count: i32,
    pub new_source_count: i32,
    pub new_substantive_count: i32,
    pub new_recent_count: i32,
    pub inbound_link_count: i32,
    pub updated_at: String,
}

impl KnowledgeSignalEvidence for WikiUpdateEvidence {
    const SCHEMA: &'static str = "wiki_update";
}

impl WikiUpdateRow {
    fn into_signal(self, now: &str) -> Result<KnowledgeSignal, AtomicCoreError> {
        let update_atom_count = self
            .new_atom_count
            .max((self.current_atom_count - self.article_atom_count).max(0));
        let new_atom_volume = scaled_ln(update_atom_count, 12.0);
        let growth_ratio = if self.article_atom_count <= 0 {
            1.0
        } else {
            (update_atom_count as f32 / self.article_atom_count as f32 / 0.5).min(1.0)
        };
        let source_diversity = if update_atom_count <= 1 {
            0.0
        } else {
            (self.new_source_count as f32 / update_atom_count.min(8) as f32).min(1.0)
        };
        let substantive = if update_atom_count == 0 {
            0.0
        } else {
            (self.new_substantive_count as f32 / update_atom_count as f32).min(1.0)
        };
        let recent_growth = (self.new_recent_count as f32 / 5.0).min(1.0);
        let inbound_strength = (self.inbound_link_count as f32 / 5.0).min(1.0);

        let score = 100.0
            * (0.35 * new_atom_volume
                + 0.20 * growth_ratio
                + 0.15 * source_diversity
                + 0.15 * substantive
                + 0.10 * recent_growth
                + 0.05 * inbound_strength);

        let confidence = (0.40 * new_atom_volume
            + 0.25 * substantive
            + 0.20 * source_diversity
            + 0.15 * growth_ratio)
            .clamp(0.0, 1.0);

        let mut reasons = vec![
            KnowledgeSignalReason {
                kind: "new_atom_volume".to_string(),
                label: format!(
                    "{} atom{} not reflected",
                    update_atom_count,
                    if update_atom_count == 1 { "" } else { "s" }
                ),
                value: json!(update_atom_count),
                contribution: new_atom_volume,
            },
            KnowledgeSignalReason {
                kind: "growth_ratio".to_string(),
                label: format!(
                    "+{:.0}% since last update",
                    if self.article_atom_count <= 0 {
                        100.0
                    } else {
                        (update_atom_count as f32 / self.article_atom_count as f32) * 100.0
                    }
                ),
                value: json!({
                    "article_atom_count": self.article_atom_count,
                    "new_atom_count": self.new_atom_count,
                    "current_atom_count": self.current_atom_count,
                }),
                contribution: growth_ratio,
            },
        ];

        if self.new_source_count > 0 {
            reasons.push(KnowledgeSignalReason {
                kind: "new_source_diversity".to_string(),
                label: format!(
                    "{} new source{}",
                    self.new_source_count,
                    if self.new_source_count == 1 { "" } else { "s" }
                ),
                value: json!(self.new_source_count),
                contribution: source_diversity,
            });
        }

        if self.new_recent_count > 0 {
            reasons.push(KnowledgeSignalReason {
                kind: "recent_growth".to_string(),
                label: format!("{} added in the last 14 days", self.new_recent_count),
                value: json!(self.new_recent_count),
                contribution: recent_growth,
            });
        }

        if self.inbound_link_count > 0 {
            reasons.push(KnowledgeSignalReason {
                kind: "inbound_wiki_links".to_string(),
                label: format!(
                    "{} inbound wiki link{}",
                    self.inbound_link_count,
                    if self.inbound_link_count == 1 {
                        ""
                    } else {
                        "s"
                    }
                ),
                value: json!(self.inbound_link_count),
                contribution: inbound_strength,
            });
        }

        let evidence = WikiUpdateEvidence {
            article_id: self.article_id.clone(),
            tag_id: self.tag_id.clone(),
            tag_name: self.tag_name.clone(),
            article_atom_count: self.article_atom_count,
            current_atom_count: self.current_atom_count,
            new_atom_count: self.new_atom_count,
            new_source_count: self.new_source_count,
            new_substantive_count: self.new_substantive_count,
            new_recent_count: self.new_recent_count,
            inbound_link_count: self.inbound_link_count,
            updated_at: self.updated_at.clone(),
        };

        Ok(KnowledgeSignal {
            id: format!("wiki_update:tag:{}", self.tag_id),
            provider_id: WIKI_UPDATE_PROVIDER_ID.to_string(),
            target: KnowledgeSignalTarget::tag(self.tag_id.clone(), self.tag_name.clone()),
            score,
            confidence,
            severity: KnowledgeSignalSeverity::Opportunity,
            title: format!("Update the wiki for {}", self.tag_name),
            summary: "Tagged material has accumulated that is not reflected in this wiki."
                .to_string(),
            reasons,
            evidence: evidence.to_value()?,
            suggested_actions: vec![
                KnowledgeSignalAction {
                    id: "update_wiki".to_string(),
                    label: "Update wiki".to_string(),
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

struct TagRedundancyProvider;

#[async_trait]
impl KnowledgeSignalProvider for TagRedundancyProvider {
    fn id(&self) -> &'static str {
        TAG_REDUNDANCY_PROVIDER_ID
    }

    fn name(&self) -> &'static str {
        "Tag redundancy"
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
                    let conn = storage.database().read_conn()?;
                    evaluate_tag_redundancy_sqlite(&conn)
                })
                .await
                .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?
            }
            #[cfg(feature = "postgres")]
            StorageBackend::Postgres(storage) => evaluate_tag_redundancy_postgres(storage).await,
        }
    }
}

struct EmptyTagProvider;

#[async_trait]
impl KnowledgeSignalProvider for EmptyTagProvider {
    fn id(&self) -> &'static str {
        EMPTY_TAG_PROVIDER_ID
    }

    fn name(&self) -> &'static str {
        "Empty tags"
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
                    let conn = storage.database().read_conn()?;
                    evaluate_empty_tags_sqlite(&conn)
                })
                .await
                .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?
            }
            #[cfg(feature = "postgres")]
            StorageBackend::Postgres(storage) => evaluate_empty_tags_postgres(storage).await,
        }
    }
}

struct MissingTagOverlapProvider;

#[async_trait]
impl KnowledgeSignalProvider for MissingTagOverlapProvider {
    fn id(&self) -> &'static str {
        MISSING_TAG_OVERLAP_PROVIDER_ID
    }

    fn name(&self) -> &'static str {
        "Missing tag overlap"
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
                    let conn = storage.database().read_conn()?;
                    evaluate_missing_tag_overlap_sqlite(&conn)
                })
                .await
                .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?
            }
            #[cfg(feature = "postgres")]
            StorageBackend::Postgres(storage) => {
                evaluate_missing_tag_overlap_postgres(storage).await
            }
        }
    }
}

struct NearDuplicateAtomProvider;

#[async_trait]
impl KnowledgeSignalProvider for NearDuplicateAtomProvider {
    fn id(&self) -> &'static str {
        NEAR_DUPLICATE_ATOM_PROVIDER_ID
    }

    fn name(&self) -> &'static str {
        "Near-duplicate atoms"
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
                    let conn = storage.database().read_conn()?;
                    evaluate_near_duplicate_atoms_sqlite(&conn)
                })
                .await
                .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?
            }
            #[cfg(feature = "postgres")]
            StorageBackend::Postgres(storage) => {
                evaluate_near_duplicate_atoms_postgres(storage).await
            }
        }
    }
}

struct SourceDuplicateProvider;

#[async_trait]
impl KnowledgeSignalProvider for SourceDuplicateProvider {
    fn id(&self) -> &'static str {
        SOURCE_DUPLICATE_PROVIDER_ID
    }

    fn name(&self) -> &'static str {
        "Source duplicates"
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
                    let conn = storage.database().read_conn()?;
                    evaluate_source_duplicates_sqlite(&conn)
                })
                .await
                .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?
            }
            #[cfg(feature = "postgres")]
            StorageBackend::Postgres(storage) => evaluate_source_duplicates_postgres(storage).await,
        }
    }
}

struct BrokenInternalLinkProvider;

#[async_trait]
impl KnowledgeSignalProvider for BrokenInternalLinkProvider {
    fn id(&self) -> &'static str {
        BROKEN_INTERNAL_LINK_PROVIDER_ID
    }

    fn name(&self) -> &'static str {
        "Broken internal links"
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
                    let conn = storage.database().read_conn()?;
                    evaluate_broken_internal_links_sqlite(&conn)
                })
                .await
                .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?
            }
            #[cfg(feature = "postgres")]
            StorageBackend::Postgres(storage) => {
                evaluate_broken_internal_links_postgres(storage).await
            }
        }
    }
}

struct UnderconnectedAtomProvider;

#[async_trait]
impl KnowledgeSignalProvider for UnderconnectedAtomProvider {
    fn id(&self) -> &'static str {
        UNDERCONNECTED_ATOM_PROVIDER_ID
    }

    fn name(&self) -> &'static str {
        "Underconnected atoms"
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
                    let conn = storage.database().read_conn()?;
                    evaluate_underconnected_atoms_sqlite(&conn)
                })
                .await
                .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?
            }
            #[cfg(feature = "postgres")]
            StorageBackend::Postgres(storage) => {
                evaluate_underconnected_atoms_postgres(storage).await
            }
        }
    }
}

#[derive(Debug, Clone)]
struct TagCleanupTag {
    id: String,
    name: String,
    parent_id: Option<String>,
    path: Vec<String>,
    atom_count: i32,
    child_count: i32,
    has_wiki: bool,
    is_autotag_target: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct TagCleanupTagEvidence {
    pub id: String,
    pub name: String,
    pub parent_id: Option<String>,
    pub path: Vec<String>,
    pub atom_count: i32,
    pub child_count: i32,
    pub has_wiki: bool,
    pub is_autotag_target: bool,
}

impl From<&TagCleanupTag> for TagCleanupTagEvidence {
    fn from(tag: &TagCleanupTag) -> Self {
        Self {
            id: tag.id.clone(),
            name: tag.name.clone(),
            parent_id: tag.parent_id.clone(),
            path: tag.path.clone(),
            atom_count: tag.atom_count,
            child_count: tag.child_count,
            has_wiki: tag.has_wiki,
            is_autotag_target: tag.is_autotag_target,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct TagRedundancyEvidence {
    pub primary_tag: TagCleanupTagEvidence,
    pub secondary_tag: TagCleanupTagEvidence,
    pub shared_atom_count: i32,
    pub primary_unique_atom_count: i32,
    pub secondary_unique_atom_count: i32,
    pub jaccard_overlap: f32,
    pub containment_overlap: f32,
    pub centroid_similarity: Option<f32>,
    pub name_similarity: f32,
    pub hierarchy_relationship: String,
    pub review_posture: String,
}

impl KnowledgeSignalEvidence for TagRedundancyEvidence {
    const SCHEMA: &'static str = "tag_redundancy";
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct EmptyTagEvidence {
    pub tag: TagCleanupTagEvidence,
}

impl KnowledgeSignalEvidence for EmptyTagEvidence {
    const SCHEMA: &'static str = "empty_tag";
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct MissingTagOverlapEvidence {
    pub atom_id: String,
    pub atom_title: String,
    pub current_tag_count: i32,
    pub suggested_tag: TagCleanupTagEvidence,
    pub nearby_tagged_atom_count: i32,
    pub strongest_similarity: f32,
    pub average_similarity: f32,
}

impl KnowledgeSignalEvidence for MissingTagOverlapEvidence {
    const SCHEMA: &'static str = "missing_tag_overlap";
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct NearDuplicateAtomEvidence {
    pub primary_atom: NearDuplicateAtomEvidenceAtom,
    pub secondary_atom: NearDuplicateAtomEvidenceAtom,
    pub semantic_similarity: f32,
    pub source_match: String,
    pub title_similarity: f32,
    pub shared_tags: Vec<NearDuplicateTagEvidence>,
    pub shared_tag_count: i32,
    pub content_length_ratio: f32,
}

impl KnowledgeSignalEvidence for NearDuplicateAtomEvidence {
    const SCHEMA: &'static str = "near_duplicate_atom";
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct SourceDuplicateEvidence {
    pub primary_atom: NearDuplicateAtomEvidenceAtom,
    pub secondary_atom: NearDuplicateAtomEvidenceAtom,
    pub source_url: String,
    pub normalized_source_url: String,
    pub duplicate_count: i32,
    pub title_similarity: f32,
    pub content_length_ratio: f32,
}

impl KnowledgeSignalEvidence for SourceDuplicateEvidence {
    const SCHEMA: &'static str = "source_duplicate";
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct NearDuplicateAtomEvidenceAtom {
    pub id: String,
    pub title: String,
    pub source_url: Option<String>,
    pub content_length: i32,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct NearDuplicateTagEvidence {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct UnderconnectedAtomEvidence {
    pub atom_id: String,
    pub atom_title: String,
    pub source_url: Option<String>,
    pub content_length: i32,
    pub tag_count: i32,
    pub total_edge_count: i32,
    pub strong_edge_count: i32,
    pub strongest_similarity: Option<f32>,
    pub average_similarity: Option<f32>,
    pub captured_atom_count: i32,
    pub edges_status: String,
}

impl KnowledgeSignalEvidence for UnderconnectedAtomEvidence {
    const SCHEMA: &'static str = "underconnected_atom";
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct BrokenInternalLinkEvidence {
    pub link_id: String,
    pub source_atom_id: String,
    pub source_atom_title: String,
    pub raw_target: String,
    pub label: Option<String>,
    pub target_kind: String,
    pub status: String,
    pub start_offset: Option<i32>,
    pub end_offset: Option<i32>,
}

impl KnowledgeSignalEvidence for BrokenInternalLinkEvidence {
    const SCHEMA: &'static str = "broken_internal_link";
}

#[derive(Debug, Clone)]
struct TagPairCandidate {
    tag_a_id: String,
    tag_b_id: String,
    shared_atom_count: i32,
}

#[derive(Debug, Clone)]
struct MissingTagCandidate {
    atom_id: String,
    atom_title: String,
    tag_id: String,
    nearby_tagged_atom_count: i32,
    strongest_similarity: f32,
    average_similarity: f32,
}

#[derive(Debug, Clone)]
struct NearDuplicateAtomCandidate {
    primary: NearDuplicateAtomInfo,
    secondary: NearDuplicateAtomInfo,
    semantic_similarity: f32,
}

#[derive(Debug, Clone)]
struct NearDuplicateAtomInfo {
    id: String,
    title: String,
    source_url: Option<String>,
    content_length: i32,
    created_at: String,
    updated_at: String,
}

#[derive(Debug, Clone)]
struct SourceDuplicateCandidate {
    primary: NearDuplicateAtomInfo,
    secondary: NearDuplicateAtomInfo,
    normalized_source_url: String,
    duplicate_count: i32,
}

#[derive(Debug, Clone)]
struct AtomTagInfo {
    id: String,
    name: String,
}

#[derive(Debug, Clone)]
struct BrokenInternalLinkCandidate {
    link_id: String,
    source_atom_id: String,
    source_atom_title: String,
    raw_target: String,
    label: Option<String>,
    target_kind: String,
    status: String,
    start_offset: Option<i32>,
    end_offset: Option<i32>,
}

#[derive(Debug, Clone)]
struct UnderconnectedAtomCandidate {
    atom_id: String,
    atom_title: String,
    source_url: Option<String>,
    content_length: i32,
    tag_count: i32,
    total_edge_count: i32,
    strong_edge_count: i32,
    strongest_similarity: Option<f32>,
    average_similarity: Option<f32>,
    captured_atom_count: i32,
    edges_status: String,
}

fn evaluate_tag_redundancy_sqlite(
    conn: &rusqlite::Connection,
) -> Result<Vec<KnowledgeSignal>, AtomicCoreError> {
    let tags = load_sqlite_tag_cleanup_tags(conn)?;
    let now = Utc::now().to_rfc3339();
    let mut stmt = conn.prepare(
        "WITH eligible_atom_tags AS (
            SELECT at.atom_id, at.tag_id
            FROM atom_tags at
            JOIN atoms a ON a.id = at.atom_id AND a.kind = 'captured'
            WHERE at.atom_id IN (
                SELECT at_inner.atom_id
                FROM atom_tags at_inner
                JOIN atoms a_inner ON a_inner.id = at_inner.atom_id AND a_inner.kind = 'captured'
                GROUP BY at_inner.atom_id
                HAVING COUNT(*) BETWEEN 2 AND ?1
            )
         )
         SELECT at1.tag_id, at2.tag_id, COUNT(*) as shared_count
         FROM eligible_atom_tags at1
         JOIN atom_tags at2
           ON at1.atom_id = at2.atom_id
          AND at1.tag_id < at2.tag_id
         GROUP BY at1.tag_id, at2.tag_id
         HAVING COUNT(*) >= 3
         ORDER BY shared_count DESC
         LIMIT ?2",
    )?;
    let rows = stmt.query_map(
        params![
            TAG_REDUNDANCY_MAX_TAGS_PER_ATOM,
            TAG_REDUNDANCY_CANDIDATE_LIMIT
        ],
        |row| {
            Ok(TagPairCandidate {
                tag_a_id: row.get(0)?,
                tag_b_id: row.get(1)?,
                shared_atom_count: row.get(2)?,
            })
        },
    )?;

    let mut out = Vec::new();
    for row in rows {
        let candidate = row?;
        let Some(a) = tags.get(&candidate.tag_a_id) else {
            continue;
        };
        let Some(b) = tags.get(&candidate.tag_b_id) else {
            continue;
        };
        if let Some(signal) = tag_pair_signal(a, b, candidate.shared_atom_count, None, &now)? {
            out.push(signal);
        }
    }
    Ok(out)
}

fn evaluate_empty_tags_sqlite(
    conn: &rusqlite::Connection,
) -> Result<Vec<KnowledgeSignal>, AtomicCoreError> {
    let tags = load_sqlite_tag_cleanup_tags(conn)?;
    let now = Utc::now().to_rfc3339();
    let mut stmt = conn.prepare(
        "SELECT t.id
         FROM tags t
         LEFT JOIN atom_tags at ON at.tag_id = t.id
         LEFT JOIN atoms a ON a.id = at.atom_id AND a.kind = 'captured'
         LEFT JOIN tags child ON child.parent_id = t.id
         GROUP BY t.id
         HAVING COUNT(DISTINCT a.id) = 0
            AND COUNT(DISTINCT child.id) = 0",
    )?;
    let ids = stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;

    let mut out = Vec::new();
    for id in ids {
        let Some(tag) = tags.get(&id) else {
            continue;
        };
        if is_structural_tag(tag) {
            continue;
        }
        out.push(empty_tag_signal(tag, &now)?);
    }
    Ok(out)
}

fn evaluate_missing_tag_overlap_sqlite(
    conn: &rusqlite::Connection,
) -> Result<Vec<KnowledgeSignal>, AtomicCoreError> {
    let tags = load_sqlite_tag_cleanup_tags(conn)?;
    let current_tags = load_sqlite_atom_tag_ids(conn)?;
    let now = Utc::now().to_rfc3339();
    let mut stmt = conn.prepare(
        "WITH high_edges AS (
            SELECT source_atom_id, target_atom_id, similarity_score
            FROM semantic_edges
            WHERE similarity_score >= 0.55
            ORDER BY similarity_score DESC
            LIMIT ?1
         ),
         neighbor_edges AS (
            SELECT source_atom_id as atom_id, target_atom_id as neighbor_atom_id, similarity_score
            FROM high_edges
            UNION ALL
            SELECT target_atom_id as atom_id, source_atom_id as neighbor_atom_id, similarity_score
            FROM high_edges
         )
         SELECT
            ne.atom_id,
            a.title,
            nt.tag_id,
            COUNT(DISTINCT ne.neighbor_atom_id) as nearby_tagged_atom_count,
            MAX(ne.similarity_score) as strongest_similarity,
            AVG(ne.similarity_score) as average_similarity
         FROM neighbor_edges ne
         JOIN atoms a ON a.id = ne.atom_id AND a.kind = 'captured'
         JOIN atoms neighbor ON neighbor.id = ne.neighbor_atom_id AND neighbor.kind = 'captured'
         JOIN atom_tags nt ON nt.atom_id = ne.neighbor_atom_id
         LEFT JOIN atom_tags existing
           ON existing.atom_id = ne.atom_id
          AND existing.tag_id = nt.tag_id
         WHERE existing.tag_id IS NULL
         GROUP BY ne.atom_id, a.title, nt.tag_id
         HAVING COUNT(DISTINCT ne.neighbor_atom_id) >= 3
         ORDER BY average_similarity DESC, nearby_tagged_atom_count DESC
         LIMIT ?2",
    )?;
    let rows = stmt.query_map(
        params![
            MISSING_TAG_EDGE_CANDIDATE_LIMIT,
            MISSING_TAG_CANDIDATE_LIMIT
        ],
        |row| {
            Ok(MissingTagCandidate {
                atom_id: row.get(0)?,
                atom_title: row.get(1)?,
                tag_id: row.get(2)?,
                nearby_tagged_atom_count: row.get(3)?,
                strongest_similarity: row.get::<_, f32>(4)?,
                average_similarity: row.get::<_, f32>(5)?,
            })
        },
    )?;

    let mut out = Vec::new();
    for row in rows {
        let candidate = row?;
        let Some(tag) = tags.get(&candidate.tag_id) else {
            continue;
        };
        let atom_tag_ids = current_tags
            .get(&candidate.atom_id)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        if let Some(signal) = missing_tag_signal(&candidate, tag, atom_tag_ids, &tags, &now)? {
            out.push(signal);
        }
    }
    limit_missing_tag_signals(out)
}

fn evaluate_near_duplicate_atoms_sqlite(
    conn: &rusqlite::Connection,
) -> Result<Vec<KnowledgeSignal>, AtomicCoreError> {
    let now = Utc::now().to_rfc3339();
    let mut stmt = conn.prepare(
        "SELECT
            e.source_atom_id,
            COALESCE(a.title, ''),
            a.source_url,
            LENGTH(a.content),
            a.created_at,
            a.updated_at,
            e.target_atom_id,
            COALESCE(b.title, ''),
            b.source_url,
            LENGTH(b.content),
            b.created_at,
            b.updated_at,
            e.similarity_score
         FROM semantic_edges e
         JOIN atoms a ON a.id = e.source_atom_id AND a.kind = 'captured'
         JOIN atoms b ON b.id = e.target_atom_id AND b.kind = 'captured'
         WHERE e.similarity_score >= 0.84
         ORDER BY e.similarity_score DESC
         LIMIT 250",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(NearDuplicateAtomCandidate {
            primary: NearDuplicateAtomInfo {
                id: row.get(0)?,
                title: row.get(1)?,
                source_url: row.get(2)?,
                content_length: row.get(3)?,
                created_at: row.get(4)?,
                updated_at: row.get(5)?,
            },
            secondary: NearDuplicateAtomInfo {
                id: row.get(6)?,
                title: row.get(7)?,
                source_url: row.get(8)?,
                content_length: row.get(9)?,
                created_at: row.get(10)?,
                updated_at: row.get(11)?,
            },
            semantic_similarity: row.get(12)?,
        })
    })?;

    let candidates = rows.collect::<Result<Vec<_>, _>>()?;
    let candidate_atom_ids = collect_near_duplicate_atom_ids(&candidates);
    let atom_tags = load_sqlite_atom_tag_info_for_atoms(conn, &candidate_atom_ids)?;

    let mut out = Vec::new();
    for candidate in candidates {
        let shared_tags =
            shared_atom_tags(&candidate.primary.id, &candidate.secondary.id, &atom_tags);
        if let Some(signal) = near_duplicate_atom_signal(&candidate, shared_tags, &now)? {
            out.push(signal);
        }
    }
    limit_near_duplicate_atom_signals(out)
}

fn evaluate_source_duplicates_sqlite(
    conn: &rusqlite::Connection,
) -> Result<Vec<KnowledgeSignal>, AtomicCoreError> {
    let now = Utc::now().to_rfc3339();
    let mut stmt = conn.prepare(
        "WITH source_atoms AS (
            SELECT
                id,
                COALESCE(title, '') as title,
                source_url,
                lower(rtrim(source_url, '/')) as normalized_source_url,
                LENGTH(content) as content_length,
                created_at,
                updated_at
            FROM atoms
            WHERE kind = 'captured'
              AND source_url IS NOT NULL
              AND TRIM(source_url) != ''
         ),
         duplicate_sources AS (
            SELECT
                normalized_source_url,
                COUNT(*) as duplicate_count,
                MAX(updated_at) as latest_updated
            FROM source_atoms
            GROUP BY normalized_source_url
            HAVING COUNT(*) >= 2
            ORDER BY latest_updated DESC
            LIMIT ?1
         ),
         ranked_source_atoms AS (
            SELECT
                sa.*,
                ds.duplicate_count,
                ROW_NUMBER() OVER (
                    PARTITION BY sa.normalized_source_url
                    ORDER BY sa.updated_at DESC
                ) as source_rank
            FROM source_atoms sa
            JOIN duplicate_sources ds
              ON ds.normalized_source_url = sa.normalized_source_url
         )
         SELECT
            id,
            title,
            source_url,
            normalized_source_url,
            content_length,
            created_at,
            updated_at,
            duplicate_count
         FROM ranked_source_atoms
         WHERE source_rank <= ?2
         ORDER BY normalized_source_url, updated_at DESC",
    )?;
    let rows = stmt.query_map(
        params![
            SOURCE_DUPLICATE_GROUP_LIMIT,
            SOURCE_DUPLICATE_ATOMS_PER_GROUP
        ],
        |row| {
            Ok((
                row.get::<_, String>(3)?,
                row.get::<_, i32>(7)?,
                NearDuplicateAtomInfo {
                    id: row.get(0)?,
                    title: row.get(1)?,
                    source_url: row.get(2)?,
                    content_length: row.get(4)?,
                    created_at: row.get(5)?,
                    updated_at: row.get(6)?,
                },
            ))
        },
    )?;

    let mut by_source: HashMap<String, Vec<NearDuplicateAtomInfo>> = HashMap::new();
    let mut duplicate_counts: HashMap<String, i32> = HashMap::new();
    for row in rows {
        let (source, duplicate_count, atom) = row?;
        duplicate_counts.insert(source.clone(), duplicate_count);
        by_source.entry(source).or_default().push(atom);
    }

    let mut out = Vec::new();
    for (normalized_source_url, mut atoms) in by_source {
        if atoms.len() < 2 {
            continue;
        }
        atoms.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        let duplicate_count = duplicate_counts
            .get(&normalized_source_url)
            .copied()
            .unwrap_or(atoms.len() as i32);
        let primary = atoms[0].clone();
        for secondary in atoms.iter().skip(1).take(5) {
            let candidate = SourceDuplicateCandidate {
                primary: primary.clone(),
                secondary: secondary.clone(),
                normalized_source_url: normalized_source_url.clone(),
                duplicate_count,
            };
            if let Some(signal) = source_duplicate_signal(&candidate, &now)? {
                out.push(signal);
            }
        }
    }
    limit_near_duplicate_atom_signals(out)
}

fn evaluate_broken_internal_links_sqlite(
    conn: &rusqlite::Connection,
) -> Result<Vec<KnowledgeSignal>, AtomicCoreError> {
    let now = Utc::now().to_rfc3339();
    let mut stmt = conn.prepare(
        "SELECT
            al.id,
            al.source_atom_id,
            COALESCE(a.title, ''),
            al.raw_target,
            al.label,
            al.target_kind,
            al.status,
            al.start_offset,
            al.end_offset
         FROM atom_links al
         JOIN atoms a ON a.id = al.source_atom_id AND a.kind = 'captured'
         WHERE (
             (al.target_kind = 'atom_id' AND al.status = 'missing')
             OR (al.target_kind = 'text' AND al.status = 'unresolved')
         )
         ORDER BY
             CASE WHEN al.status = 'missing' THEN 0 ELSE 1 END,
             a.updated_at DESC,
             al.start_offset ASC
         LIMIT 150",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(BrokenInternalLinkCandidate {
            link_id: row.get(0)?,
            source_atom_id: row.get(1)?,
            source_atom_title: row.get(2)?,
            raw_target: row.get(3)?,
            label: row.get(4)?,
            target_kind: row.get(5)?,
            status: row.get(6)?,
            start_offset: row.get(7)?,
            end_offset: row.get(8)?,
        })
    })?;

    let mut out = Vec::new();
    for row in rows {
        if let Some(signal) = broken_internal_link_signal(&row?, &now)? {
            out.push(signal);
        }
    }
    limit_broken_internal_link_signals(out)
}

fn evaluate_underconnected_atoms_sqlite(
    conn: &rusqlite::Connection,
) -> Result<Vec<KnowledgeSignal>, AtomicCoreError> {
    let now = Utc::now().to_rfc3339();
    let mut stmt = conn.prepare(
        "WITH eligible_atoms AS (
            SELECT id
            FROM atoms
            WHERE kind = 'captured'
              AND edges_status = 'complete'
              AND LENGTH(content) >= 180
            ORDER BY updated_at DESC
            LIMIT ?1
         ),
         db_size AS (
            SELECT COUNT(*) as captured_atom_count
            FROM atoms
            WHERE kind = 'captured'
              AND edges_status = 'complete'
         ),
         edge_summary AS (
            SELECT
                atom_id,
                COUNT(*) as total_edge_count,
                SUM(CASE WHEN similarity_score >= 0.55 THEN 1 ELSE 0 END) as strong_edge_count,
                MAX(similarity_score) as strongest_similarity,
                AVG(similarity_score) as average_similarity
            FROM (
                SELECT e.source_atom_id as atom_id, e.similarity_score
                FROM semantic_edges e
                JOIN eligible_atoms ea ON ea.id = e.source_atom_id
                JOIN atoms source ON source.id = e.source_atom_id AND source.kind = 'captured'
                JOIN atoms target ON target.id = e.target_atom_id AND target.kind = 'captured'
                UNION ALL
                SELECT e.target_atom_id as atom_id, e.similarity_score
                FROM semantic_edges e
                JOIN eligible_atoms ea ON ea.id = e.target_atom_id
                JOIN atoms source ON source.id = e.source_atom_id AND source.kind = 'captured'
                JOIN atoms target ON target.id = e.target_atom_id AND target.kind = 'captured'
            )
            GROUP BY atom_id
         ),
         tag_counts AS (
            SELECT at.atom_id, COUNT(DISTINCT at.tag_id) as tag_count
            FROM atom_tags at
            JOIN eligible_atoms ea ON ea.id = at.atom_id
            GROUP BY at.atom_id
         )
         SELECT
            a.id,
            COALESCE(a.title, ''),
            a.source_url,
            LENGTH(a.content),
            COALESCE(tc.tag_count, 0),
            COALESCE(es.total_edge_count, 0),
            COALESCE(es.strong_edge_count, 0),
            es.strongest_similarity,
            es.average_similarity,
            db_size.captured_atom_count,
            a.edges_status
         FROM atoms a
         JOIN eligible_atoms ea ON ea.id = a.id
         CROSS JOIN db_size
         LEFT JOIN edge_summary es ON es.atom_id = a.id
         LEFT JOIN tag_counts tc ON tc.atom_id = a.id
         WHERE a.kind = 'captured'
           AND a.edges_status = 'complete'
           AND db_size.captured_atom_count >= 8
           AND LENGTH(a.content) >= 180
           AND (
                COALESCE(es.strong_edge_count, 0) = 0
                OR (
                    COALESCE(es.strong_edge_count, 0) <= 1
                    AND COALESCE(tc.tag_count, 0) <= 1
                    AND COALESCE(es.strongest_similarity, 0.0) < 0.62
                )
           )
         ORDER BY COALESCE(es.strong_edge_count, 0) ASC,
                  COALESCE(es.strongest_similarity, 0.0) ASC,
                  LENGTH(a.content) DESC
         LIMIT 100",
    )?;
    let rows = stmt.query_map(params![UNDERCONNECTED_CANDIDATE_ATOM_LIMIT], |row| {
        Ok(UnderconnectedAtomCandidate {
            atom_id: row.get(0)?,
            atom_title: row.get(1)?,
            source_url: row.get(2)?,
            content_length: row.get(3)?,
            tag_count: row.get(4)?,
            total_edge_count: row.get(5)?,
            strong_edge_count: row.get(6)?,
            strongest_similarity: row.get(7)?,
            average_similarity: row.get(8)?,
            captured_atom_count: row.get(9)?,
            edges_status: row.get(10)?,
        })
    })?;

    let mut out = Vec::new();
    for row in rows {
        if let Some(signal) = underconnected_atom_signal(&row?, &now)? {
            out.push(signal);
        }
    }
    limit_underconnected_atom_signals(out)
}

fn load_sqlite_atom_tag_ids(
    conn: &rusqlite::Connection,
) -> Result<HashMap<String, Vec<String>>, AtomicCoreError> {
    let mut stmt = conn.prepare(
        "SELECT at.atom_id, at.tag_id
         FROM atom_tags at
         JOIN atoms a ON a.id = at.atom_id AND a.kind = 'captured'",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    for row in rows {
        let (atom_id, tag_id) = row?;
        out.entry(atom_id).or_default().push(tag_id);
    }
    Ok(out)
}

fn load_sqlite_atom_tag_info_for_atoms(
    conn: &rusqlite::Connection,
    atom_ids: &[String],
) -> Result<HashMap<String, Vec<AtomTagInfo>>, AtomicCoreError> {
    if atom_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let placeholders = std::iter::repeat("?")
        .take(atom_ids.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT at.atom_id, t.id, t.name
         FROM atom_tags at
         JOIN tags t ON t.id = at.tag_id
         WHERE at.atom_id IN ({placeholders})"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(atom_ids), |row| {
        Ok((
            row.get::<_, String>(0)?,
            AtomTagInfo {
                id: row.get(1)?,
                name: row.get(2)?,
            },
        ))
    })?;
    let mut out: HashMap<String, Vec<AtomTagInfo>> = HashMap::new();
    for row in rows {
        let (atom_id, tag) = row?;
        out.entry(atom_id).or_default().push(tag);
    }
    Ok(out)
}

fn collect_near_duplicate_atom_ids(candidates: &[NearDuplicateAtomCandidate]) -> Vec<String> {
    let mut ids = Vec::new();
    for candidate in candidates {
        if !ids.contains(&candidate.primary.id) {
            ids.push(candidate.primary.id.clone());
        }
        if !ids.contains(&candidate.secondary.id) {
            ids.push(candidate.secondary.id.clone());
        }
    }
    ids
}

fn load_sqlite_tag_cleanup_tags(
    conn: &rusqlite::Connection,
) -> Result<HashMap<String, TagCleanupTag>, AtomicCoreError> {
    let mut stmt = conn.prepare(
        "SELECT
            t.id,
            t.name,
            t.parent_id,
            COALESCE(COUNT(DISTINCT a.id), 0) as atom_count,
            COALESCE(COUNT(DISTINCT child.id), 0) as child_count,
            EXISTS(SELECT 1 FROM wiki_articles w WHERE w.tag_id = t.id) as has_wiki,
            COALESCE(t.is_autotag_target, 0) as is_autotag_target
         FROM tags t
         LEFT JOIN atom_tags at ON at.tag_id = t.id
         LEFT JOIN atoms a ON a.id = at.atom_id AND a.kind = 'captured'
         LEFT JOIN tags child ON child.parent_id = t.id
         GROUP BY t.id, t.name, t.parent_id, t.is_autotag_target",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, i32>(3)?,
            row.get::<_, i32>(4)?,
            row.get::<_, i32>(5)? != 0,
            row.get::<_, i32>(6)? != 0,
        ))
    })?;

    let mut raw = HashMap::new();
    for row in rows {
        let (id, name, parent_id, atom_count, child_count, has_wiki, is_autotag_target) = row?;
        raw.insert(
            id,
            (
                name,
                parent_id,
                atom_count,
                child_count,
                has_wiki,
                is_autotag_target,
            ),
        );
    }
    Ok(build_tag_cleanup_tags(raw))
}

#[cfg(feature = "postgres")]
async fn evaluate_tag_redundancy_postgres(
    storage: &crate::storage::postgres::PostgresStorage,
) -> Result<Vec<KnowledgeSignal>, AtomicCoreError> {
    let tags = load_postgres_tag_cleanup_tags(storage).await?;
    let now = Utc::now().to_rfc3339();
    let rows: Vec<(String, String, i64)> = sqlx::query_as(
        "WITH eligible_atom_tags AS (
            SELECT at.atom_id, at.tag_id
            FROM atom_tags at
            JOIN atoms a
              ON a.id = at.atom_id
             AND a.db_id = at.db_id
             AND a.kind = 'captured'
            WHERE at.db_id = $1
              AND at.atom_id IN (
                SELECT at_inner.atom_id
                FROM atom_tags at_inner
                JOIN atoms a_inner
                  ON a_inner.id = at_inner.atom_id
                 AND a_inner.db_id = at_inner.db_id
                 AND a_inner.kind = 'captured'
                WHERE at_inner.db_id = $1
                GROUP BY at_inner.atom_id
                HAVING COUNT(*) BETWEEN 2 AND $2
              )
         )
         SELECT at1.tag_id, at2.tag_id, COUNT(*) as shared_count
         FROM eligible_atom_tags at1
         JOIN atom_tags at2
           ON at1.atom_id = at2.atom_id
          AND at1.tag_id < at2.tag_id
          AND at2.db_id = $1
         GROUP BY at1.tag_id, at2.tag_id
         HAVING COUNT(*) >= 3
         ORDER BY shared_count DESC
         LIMIT $3",
    )
    .bind(&storage.db_id)
    .bind(TAG_REDUNDANCY_MAX_TAGS_PER_ATOM as i64)
    .bind(TAG_REDUNDANCY_CANDIDATE_LIMIT as i64)
    .fetch_all(&storage.pool)
    .await
    .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

    let mut out = Vec::new();
    for (tag_a_id, tag_b_id, shared_atom_count) in rows {
        let Some(a) = tags.get(&tag_a_id) else {
            continue;
        };
        let Some(b) = tags.get(&tag_b_id) else {
            continue;
        };
        if let Some(signal) = tag_pair_signal(a, b, shared_atom_count as i32, None, &now)? {
            out.push(signal);
        }
    }
    Ok(out)
}

#[cfg(feature = "postgres")]
async fn evaluate_empty_tags_postgres(
    storage: &crate::storage::postgres::PostgresStorage,
) -> Result<Vec<KnowledgeSignal>, AtomicCoreError> {
    let tags = load_postgres_tag_cleanup_tags(storage).await?;
    let now = Utc::now().to_rfc3339();
    let rows: Vec<String> = sqlx::query_scalar(
        "SELECT t.id
         FROM tags t
         LEFT JOIN atom_tags at ON at.tag_id = t.id AND at.db_id = t.db_id
         LEFT JOIN atoms a ON a.id = at.atom_id AND a.db_id = t.db_id AND a.kind = 'captured'
         LEFT JOIN tags child ON child.parent_id = t.id AND child.db_id = t.db_id
         WHERE t.db_id = $1
         GROUP BY t.id
         HAVING COUNT(DISTINCT a.id) = 0
            AND COUNT(DISTINCT child.id) = 0",
    )
    .bind(&storage.db_id)
    .fetch_all(&storage.pool)
    .await
    .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

    let mut out = Vec::new();
    for id in rows {
        let Some(tag) = tags.get(&id) else {
            continue;
        };
        if is_structural_tag(tag) {
            continue;
        }
        out.push(empty_tag_signal(tag, &now)?);
    }
    Ok(out)
}

#[cfg(feature = "postgres")]
async fn evaluate_missing_tag_overlap_postgres(
    storage: &crate::storage::postgres::PostgresStorage,
) -> Result<Vec<KnowledgeSignal>, AtomicCoreError> {
    let tags = load_postgres_tag_cleanup_tags(storage).await?;
    let current_tags = load_postgres_atom_tag_ids(storage).await?;
    let now = Utc::now().to_rfc3339();
    let rows: Vec<(String, String, String, i64, f32, f32)> = sqlx::query_as(
        "WITH high_edges AS (
            SELECT source_atom_id, target_atom_id, similarity_score
            FROM semantic_edges
            WHERE similarity_score >= 0.55 AND db_id = $1
            ORDER BY similarity_score DESC
            LIMIT $2
         ),
         neighbor_edges AS (
            SELECT source_atom_id as atom_id, target_atom_id as neighbor_atom_id, similarity_score
            FROM high_edges
            UNION ALL
            SELECT target_atom_id as atom_id, source_atom_id as neighbor_atom_id, similarity_score
            FROM high_edges
         )
         SELECT
            ne.atom_id,
            a.title,
            nt.tag_id,
            COUNT(DISTINCT ne.neighbor_atom_id)::BIGINT as nearby_tagged_atom_count,
            MAX(ne.similarity_score)::REAL as strongest_similarity,
            AVG(ne.similarity_score)::REAL as average_similarity
         FROM neighbor_edges ne
         JOIN atoms a ON a.id = ne.atom_id AND a.db_id = $1 AND a.kind = 'captured'
         JOIN atoms neighbor
           ON neighbor.id = ne.neighbor_atom_id
          AND neighbor.db_id = $1
          AND neighbor.kind = 'captured'
         JOIN atom_tags nt ON nt.atom_id = ne.neighbor_atom_id AND nt.db_id = $1
         LEFT JOIN atom_tags existing
           ON existing.atom_id = ne.atom_id
          AND existing.tag_id = nt.tag_id
          AND existing.db_id = $1
         WHERE existing.tag_id IS NULL
         GROUP BY ne.atom_id, a.title, nt.tag_id
         HAVING COUNT(DISTINCT ne.neighbor_atom_id) >= 3
         ORDER BY average_similarity DESC, nearby_tagged_atom_count DESC
         LIMIT $3",
    )
    .bind(&storage.db_id)
    .bind(MISSING_TAG_EDGE_CANDIDATE_LIMIT as i64)
    .bind(MISSING_TAG_CANDIDATE_LIMIT as i64)
    .fetch_all(&storage.pool)
    .await
    .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

    let mut out = Vec::new();
    for (
        atom_id,
        atom_title,
        tag_id,
        nearby_tagged_atom_count,
        strongest_similarity,
        average_similarity,
    ) in rows
    {
        let Some(tag) = tags.get(&tag_id) else {
            continue;
        };
        let atom_tag_ids = current_tags.get(&atom_id).map(Vec::as_slice).unwrap_or(&[]);
        let candidate = MissingTagCandidate {
            atom_id,
            atom_title,
            tag_id,
            nearby_tagged_atom_count: nearby_tagged_atom_count as i32,
            strongest_similarity,
            average_similarity,
        };
        if let Some(signal) = missing_tag_signal(&candidate, tag, atom_tag_ids, &tags, &now)? {
            out.push(signal);
        }
    }
    limit_missing_tag_signals(out)
}

#[cfg(feature = "postgres")]
async fn evaluate_near_duplicate_atoms_postgres(
    storage: &crate::storage::postgres::PostgresStorage,
) -> Result<Vec<KnowledgeSignal>, AtomicCoreError> {
    let now = Utc::now().to_rfc3339();
    let rows: Vec<(
        String,
        String,
        Option<String>,
        i32,
        String,
        String,
        String,
        String,
        Option<String>,
        i32,
        String,
        String,
        f32,
    )> = sqlx::query_as(
        "SELECT
            e.source_atom_id,
            COALESCE(a.title, ''),
            a.source_url,
            LENGTH(a.content)::INTEGER,
            a.created_at,
            a.updated_at,
            e.target_atom_id,
            COALESCE(b.title, ''),
            b.source_url,
            LENGTH(b.content)::INTEGER,
            b.created_at,
            b.updated_at,
            e.similarity_score
         FROM semantic_edges e
         JOIN atoms a
           ON a.id = e.source_atom_id
          AND a.db_id = e.db_id
          AND a.kind = 'captured'
         JOIN atoms b
           ON b.id = e.target_atom_id
          AND b.db_id = e.db_id
          AND b.kind = 'captured'
         WHERE e.db_id = $1
           AND e.similarity_score >= 0.84
         ORDER BY e.similarity_score DESC
         LIMIT 250",
    )
    .bind(&storage.db_id)
    .fetch_all(&storage.pool)
    .await
    .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

    let candidates = rows
        .into_iter()
        .map(
            |(
                primary_id,
                primary_title,
                primary_source_url,
                primary_content_length,
                primary_created_at,
                primary_updated_at,
                secondary_id,
                secondary_title,
                secondary_source_url,
                secondary_content_length,
                secondary_created_at,
                secondary_updated_at,
                semantic_similarity,
            )| NearDuplicateAtomCandidate {
                primary: NearDuplicateAtomInfo {
                    id: primary_id,
                    title: primary_title,
                    source_url: primary_source_url,
                    content_length: primary_content_length,
                    created_at: primary_created_at,
                    updated_at: primary_updated_at,
                },
                secondary: NearDuplicateAtomInfo {
                    id: secondary_id,
                    title: secondary_title,
                    source_url: secondary_source_url,
                    content_length: secondary_content_length,
                    created_at: secondary_created_at,
                    updated_at: secondary_updated_at,
                },
                semantic_similarity,
            },
        )
        .collect::<Vec<_>>();
    let candidate_atom_ids = collect_near_duplicate_atom_ids(&candidates);
    let atom_tags = load_postgres_atom_tag_info_for_atoms(storage, &candidate_atom_ids).await?;

    let mut out = Vec::new();
    for candidate in candidates {
        let shared_tags =
            shared_atom_tags(&candidate.primary.id, &candidate.secondary.id, &atom_tags);
        if let Some(signal) = near_duplicate_atom_signal(&candidate, shared_tags, &now)? {
            out.push(signal);
        }
    }
    limit_near_duplicate_atom_signals(out)
}

#[cfg(feature = "postgres")]
async fn evaluate_source_duplicates_postgres(
    storage: &crate::storage::postgres::PostgresStorage,
) -> Result<Vec<KnowledgeSignal>, AtomicCoreError> {
    let now = Utc::now().to_rfc3339();
    let rows: Vec<(
        String,
        String,
        Option<String>,
        String,
        i32,
        String,
        String,
        i64,
    )> = sqlx::query_as(
        "WITH source_atoms AS (
            SELECT
                id,
                COALESCE(title, '') as title,
                source_url,
                lower(rtrim(source_url, '/')) as normalized_source_url,
                LENGTH(content)::INTEGER as content_length,
                created_at,
                updated_at
            FROM atoms
            WHERE db_id = $1
              AND kind = 'captured'
              AND source_url IS NOT NULL
              AND BTRIM(source_url) != ''
         ),
         duplicate_sources AS (
            SELECT
                normalized_source_url,
                COUNT(*)::BIGINT as duplicate_count,
                MAX(updated_at) as latest_updated
            FROM source_atoms
            GROUP BY normalized_source_url
            HAVING COUNT(*) >= 2
            ORDER BY latest_updated DESC
            LIMIT $2
         ),
         ranked_source_atoms AS (
            SELECT
                sa.*,
                ds.duplicate_count,
                ROW_NUMBER() OVER (
                    PARTITION BY sa.normalized_source_url
                    ORDER BY sa.updated_at DESC
                ) as source_rank
            FROM source_atoms sa
            JOIN duplicate_sources ds
              ON ds.normalized_source_url = sa.normalized_source_url
         )
         SELECT
            id,
            title,
            source_url,
            normalized_source_url,
            content_length,
            created_at,
            updated_at,
            duplicate_count
         FROM ranked_source_atoms
         WHERE source_rank <= $3
         ORDER BY normalized_source_url, updated_at DESC",
    )
    .bind(&storage.db_id)
    .bind(SOURCE_DUPLICATE_GROUP_LIMIT as i64)
    .bind(SOURCE_DUPLICATE_ATOMS_PER_GROUP as i64)
    .fetch_all(&storage.pool)
    .await
    .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

    let mut by_source: HashMap<String, Vec<NearDuplicateAtomInfo>> = HashMap::new();
    let mut duplicate_counts: HashMap<String, i32> = HashMap::new();
    for (
        id,
        title,
        source_url,
        normalized_source_url,
        content_length,
        created_at,
        updated_at,
        duplicate_count,
    ) in rows
    {
        duplicate_counts.insert(normalized_source_url.clone(), duplicate_count as i32);
        by_source
            .entry(normalized_source_url)
            .or_default()
            .push(NearDuplicateAtomInfo {
                id,
                title,
                source_url,
                content_length,
                created_at,
                updated_at,
            });
    }

    let mut out = Vec::new();
    for (normalized_source_url, mut atoms) in by_source {
        if atoms.len() < 2 {
            continue;
        }
        atoms.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        let duplicate_count = duplicate_counts
            .get(&normalized_source_url)
            .copied()
            .unwrap_or(atoms.len() as i32);
        let primary = atoms[0].clone();
        for secondary in atoms.iter().skip(1).take(5) {
            let candidate = SourceDuplicateCandidate {
                primary: primary.clone(),
                secondary: secondary.clone(),
                normalized_source_url: normalized_source_url.clone(),
                duplicate_count,
            };
            if let Some(signal) = source_duplicate_signal(&candidate, &now)? {
                out.push(signal);
            }
        }
    }
    limit_near_duplicate_atom_signals(out)
}

#[cfg(feature = "postgres")]
async fn evaluate_broken_internal_links_postgres(
    storage: &crate::storage::postgres::PostgresStorage,
) -> Result<Vec<KnowledgeSignal>, AtomicCoreError> {
    let now = Utc::now().to_rfc3339();
    let rows: Vec<(
        String,
        String,
        String,
        String,
        Option<String>,
        String,
        String,
        Option<i32>,
        Option<i32>,
    )> = sqlx::query_as(
        "SELECT
            al.id,
            al.source_atom_id,
            COALESCE(a.title, ''),
            al.raw_target,
            al.label,
            al.target_kind,
            al.status,
            al.start_offset,
            al.end_offset
         FROM atom_links al
         JOIN atoms a
           ON a.id = al.source_atom_id
          AND a.db_id = al.db_id
          AND a.kind = 'captured'
         WHERE al.db_id = $1
           AND (
             (al.target_kind = 'atom_id' AND al.status = 'missing')
             OR (al.target_kind = 'text' AND al.status = 'unresolved')
           )
         ORDER BY
             CASE WHEN al.status = 'missing' THEN 0 ELSE 1 END,
             a.updated_at DESC,
             al.start_offset ASC
         LIMIT 150",
    )
    .bind(&storage.db_id)
    .fetch_all(&storage.pool)
    .await
    .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

    let mut out = Vec::new();
    for (
        link_id,
        source_atom_id,
        source_atom_title,
        raw_target,
        label,
        target_kind,
        status,
        start_offset,
        end_offset,
    ) in rows
    {
        let candidate = BrokenInternalLinkCandidate {
            link_id,
            source_atom_id,
            source_atom_title,
            raw_target,
            label,
            target_kind,
            status,
            start_offset,
            end_offset,
        };
        if let Some(signal) = broken_internal_link_signal(&candidate, &now)? {
            out.push(signal);
        }
    }
    limit_broken_internal_link_signals(out)
}

#[cfg(feature = "postgres")]
async fn evaluate_underconnected_atoms_postgres(
    storage: &crate::storage::postgres::PostgresStorage,
) -> Result<Vec<KnowledgeSignal>, AtomicCoreError> {
    let now = Utc::now().to_rfc3339();
    let rows: Vec<(
        String,
        String,
        Option<String>,
        i32,
        i64,
        i64,
        i64,
        Option<f32>,
        Option<f32>,
        i64,
        String,
    )> = sqlx::query_as(
        "WITH eligible_atoms AS (
            SELECT id
            FROM atoms
            WHERE db_id = $1
              AND kind = 'captured'
              AND edges_status = 'complete'
              AND LENGTH(content) >= 180
            ORDER BY updated_at DESC
            LIMIT $2
         ),
         db_size AS (
            SELECT COUNT(*)::BIGINT as captured_atom_count
            FROM atoms
            WHERE db_id = $1
              AND kind = 'captured'
              AND edges_status = 'complete'
         ),
         edge_summary AS (
            SELECT
                atom_id,
                COUNT(*)::BIGINT as total_edge_count,
                SUM(CASE WHEN similarity_score >= 0.55 THEN 1 ELSE 0 END)::BIGINT as strong_edge_count,
                MAX(similarity_score)::REAL as strongest_similarity,
                AVG(similarity_score)::REAL as average_similarity
            FROM (
                SELECT e.source_atom_id as atom_id, e.similarity_score
                FROM semantic_edges e
                JOIN eligible_atoms ea ON ea.id = e.source_atom_id
                JOIN atoms source
                  ON source.id = e.source_atom_id
                 AND source.db_id = e.db_id
                 AND source.kind = 'captured'
                JOIN atoms target
                  ON target.id = e.target_atom_id
                 AND target.db_id = e.db_id
                 AND target.kind = 'captured'
                WHERE e.db_id = $1
                UNION ALL
                SELECT e.target_atom_id as atom_id, e.similarity_score
                FROM semantic_edges e
                JOIN eligible_atoms ea ON ea.id = e.target_atom_id
                JOIN atoms source
                  ON source.id = e.source_atom_id
                 AND source.db_id = e.db_id
                 AND source.kind = 'captured'
                JOIN atoms target
                  ON target.id = e.target_atom_id
                 AND target.db_id = e.db_id
                 AND target.kind = 'captured'
                WHERE e.db_id = $1
            ) edges
            GROUP BY atom_id
         ),
         tag_counts AS (
            SELECT at.atom_id, COUNT(DISTINCT at.tag_id)::BIGINT as tag_count
            FROM atom_tags at
            JOIN eligible_atoms ea ON ea.id = at.atom_id
            WHERE at.db_id = $1
            GROUP BY at.atom_id
         )
         SELECT
            a.id,
            COALESCE(a.title, ''),
            a.source_url,
            LENGTH(a.content)::INTEGER,
            COALESCE(tc.tag_count, 0)::BIGINT,
            COALESCE(es.total_edge_count, 0)::BIGINT,
            COALESCE(es.strong_edge_count, 0)::BIGINT,
            es.strongest_similarity,
            es.average_similarity,
            db_size.captured_atom_count,
            a.edges_status
         FROM atoms a
         JOIN eligible_atoms ea ON ea.id = a.id
         CROSS JOIN db_size
         LEFT JOIN edge_summary es ON es.atom_id = a.id
         LEFT JOIN tag_counts tc ON tc.atom_id = a.id
         WHERE a.db_id = $1
           AND a.kind = 'captured'
           AND a.edges_status = 'complete'
           AND db_size.captured_atom_count >= 8
           AND LENGTH(a.content) >= 180
           AND (
                COALESCE(es.strong_edge_count, 0) = 0
                OR (
                    COALESCE(es.strong_edge_count, 0) <= 1
                    AND COALESCE(tc.tag_count, 0) <= 1
                    AND COALESCE(es.strongest_similarity, 0.0) < 0.62
                )
           )
         ORDER BY COALESCE(es.strong_edge_count, 0) ASC,
                  COALESCE(es.strongest_similarity, 0.0) ASC,
                  LENGTH(a.content) DESC
         LIMIT 100",
    )
    .bind(&storage.db_id)
    .bind(UNDERCONNECTED_CANDIDATE_ATOM_LIMIT as i64)
    .fetch_all(&storage.pool)
    .await
    .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

    let mut out = Vec::new();
    for (
        atom_id,
        atom_title,
        source_url,
        content_length,
        tag_count,
        total_edge_count,
        strong_edge_count,
        strongest_similarity,
        average_similarity,
        captured_atom_count,
        edges_status,
    ) in rows
    {
        let candidate = UnderconnectedAtomCandidate {
            atom_id,
            atom_title,
            source_url,
            content_length,
            tag_count: tag_count as i32,
            total_edge_count: total_edge_count as i32,
            strong_edge_count: strong_edge_count as i32,
            strongest_similarity,
            average_similarity,
            captured_atom_count: captured_atom_count as i32,
            edges_status,
        };
        if let Some(signal) = underconnected_atom_signal(&candidate, &now)? {
            out.push(signal);
        }
    }
    limit_underconnected_atom_signals(out)
}

#[cfg(feature = "postgres")]
async fn load_postgres_atom_tag_ids(
    storage: &crate::storage::postgres::PostgresStorage,
) -> Result<HashMap<String, Vec<String>>, AtomicCoreError> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT at.atom_id, at.tag_id
             FROM atom_tags at
             JOIN atoms a ON a.id = at.atom_id AND a.db_id = at.db_id AND a.kind = 'captured'
             WHERE at.db_id = $1",
    )
    .bind(&storage.db_id)
    .fetch_all(&storage.pool)
    .await
    .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    for (atom_id, tag_id) in rows {
        out.entry(atom_id).or_default().push(tag_id);
    }
    Ok(out)
}

#[cfg(feature = "postgres")]
async fn load_postgres_atom_tag_info_for_atoms(
    storage: &crate::storage::postgres::PostgresStorage,
    atom_ids: &[String],
) -> Result<HashMap<String, Vec<AtomTagInfo>>, AtomicCoreError> {
    if atom_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let ids = atom_ids.to_vec();
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT at.atom_id, t.id, t.name
         FROM atom_tags at
         JOIN tags t ON t.id = at.tag_id AND t.db_id = at.db_id
         WHERE at.db_id = $1
           AND at.atom_id = ANY($2)",
    )
    .bind(&storage.db_id)
    .bind(ids)
    .fetch_all(&storage.pool)
    .await
    .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
    let mut out: HashMap<String, Vec<AtomTagInfo>> = HashMap::new();
    for (atom_id, tag_id, tag_name) in rows {
        out.entry(atom_id).or_default().push(AtomTagInfo {
            id: tag_id,
            name: tag_name,
        });
    }
    Ok(out)
}

#[cfg(feature = "postgres")]
async fn load_postgres_tag_cleanup_tags(
    storage: &crate::storage::postgres::PostgresStorage,
) -> Result<HashMap<String, TagCleanupTag>, AtomicCoreError> {
    let rows: Vec<(String, String, Option<String>, i64, i64, bool, bool)> = sqlx::query_as(
        "SELECT
            t.id,
            t.name,
            t.parent_id,
            COALESCE(COUNT(DISTINCT a.id), 0) as atom_count,
            COALESCE(COUNT(DISTINCT child.id), 0) as child_count,
            EXISTS(
                SELECT 1
                FROM wiki_articles w
                WHERE w.tag_id = t.id AND w.db_id = t.db_id
            ) as has_wiki,
            t.is_autotag_target
         FROM tags t
         LEFT JOIN atom_tags at ON at.tag_id = t.id AND at.db_id = t.db_id
         LEFT JOIN atoms a ON a.id = at.atom_id AND a.db_id = t.db_id AND a.kind = 'captured'
         LEFT JOIN tags child ON child.parent_id = t.id AND child.db_id = t.db_id
         WHERE t.db_id = $1
         GROUP BY t.id, t.name, t.parent_id, t.db_id, t.is_autotag_target",
    )
    .bind(&storage.db_id)
    .fetch_all(&storage.pool)
    .await
    .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

    let raw = rows
        .into_iter()
        .map(
            |(id, name, parent_id, atom_count, child_count, has_wiki, is_autotag_target)| {
                (
                    id,
                    (
                        name,
                        parent_id,
                        atom_count as i32,
                        child_count as i32,
                        has_wiki,
                        is_autotag_target,
                    ),
                )
            },
        )
        .collect();
    Ok(build_tag_cleanup_tags(raw))
}

fn build_tag_cleanup_tags(
    raw: HashMap<String, (String, Option<String>, i32, i32, bool, bool)>,
) -> HashMap<String, TagCleanupTag> {
    fn path_for(
        id: &str,
        raw: &HashMap<String, (String, Option<String>, i32, i32, bool, bool)>,
        memo: &mut HashMap<String, Vec<String>>,
    ) -> Vec<String> {
        if let Some(path) = memo.get(id) {
            return path.clone();
        }
        let Some((name, parent_id, _, _, _, _)) = raw.get(id) else {
            return Vec::new();
        };
        let mut path = parent_id
            .as_deref()
            .map(|parent| path_for(parent, raw, memo))
            .unwrap_or_default();
        path.push(name.clone());
        memo.insert(id.to_string(), path.clone());
        path
    }

    let mut memo = HashMap::new();
    raw.iter()
        .map(
            |(id, (name, parent_id, atom_count, child_count, has_wiki, is_autotag_target))| {
                (
                    id.clone(),
                    TagCleanupTag {
                        id: id.clone(),
                        name: name.clone(),
                        parent_id: parent_id.clone(),
                        path: path_for(id, &raw, &mut memo),
                        atom_count: *atom_count,
                        child_count: *child_count,
                        has_wiki: *has_wiki,
                        is_autotag_target: *is_autotag_target,
                    },
                )
            },
        )
        .collect()
}

fn missing_tag_signal(
    candidate: &MissingTagCandidate,
    tag: &TagCleanupTag,
    current_tag_ids: &[String],
    tags: &HashMap<String, TagCleanupTag>,
    now: &str,
) -> Result<Option<KnowledgeSignal>, AtomicCoreError> {
    if is_structural_tag(tag) {
        return Ok(None);
    }
    if tag.atom_count < 5 && candidate.average_similarity < 0.70 {
        return Ok(None);
    }
    if current_tag_ids
        .iter()
        .any(|current| tags_are_hierarchically_related(current, &tag.id, tags))
    {
        return Ok(None);
    }

    let tag_count_penalty = if current_tag_ids.len() >= 8 {
        0.85
    } else {
        1.0
    };
    let neighbor_strength = (candidate.nearby_tagged_atom_count as f32 / 5.0).min(1.0);
    let tag_size = scaled_ln(tag.atom_count, 25.0);
    let score = (100.0
        * (0.42 * candidate.average_similarity
            + 0.22 * candidate.strongest_similarity
            + 0.24 * neighbor_strength
            + 0.12 * tag_size)
        * tag_count_penalty)
        .clamp(0.0, 100.0);
    let confidence = (0.48 * candidate.average_similarity
        + 0.24 * candidate.strongest_similarity
        + 0.20 * neighbor_strength
        + 0.08 * if current_tag_ids.len() <= 6 { 1.0 } else { 0.5 })
    .clamp(0.0, 1.0);

    let evidence = MissingTagOverlapEvidence {
        atom_id: candidate.atom_id.clone(),
        atom_title: candidate.atom_title.clone(),
        current_tag_count: current_tag_ids.len() as i32,
        suggested_tag: tag.into(),
        nearby_tagged_atom_count: candidate.nearby_tagged_atom_count,
        strongest_similarity: candidate.strongest_similarity,
        average_similarity: candidate.average_similarity,
    };

    Ok(Some(KnowledgeSignal {
        id: format!(
            "{}:atom_tag:{}:{}",
            MISSING_TAG_OVERLAP_PROVIDER_ID, candidate.atom_id, tag.id
        ),
        provider_id: MISSING_TAG_OVERLAP_PROVIDER_ID.to_string(),
        target: KnowledgeSignalTarget::atom(
            candidate.atom_id.clone(),
            candidate.atom_title.clone(),
        ),
        score,
        confidence,
        severity: KnowledgeSignalSeverity::Opportunity,
        title: format!("Add {} to {}", tag.name, candidate.atom_title),
        summary: format!(
            "{} nearby atoms use {}, but this atom does not.",
            candidate.nearby_tagged_atom_count, tag.name
        ),
        reasons: vec![
            KnowledgeSignalReason {
                kind: "nearby_tagged_atoms".to_string(),
                label: format!(
                    "{} nearby atoms use this tag",
                    candidate.nearby_tagged_atom_count
                ),
                value: json!(candidate.nearby_tagged_atom_count),
                contribution: neighbor_strength * 100.0,
            },
            KnowledgeSignalReason {
                kind: "average_similarity".to_string(),
                label: format!(
                    "{:.0}% average similarity",
                    candidate.average_similarity * 100.0
                ),
                value: json!(candidate.average_similarity),
                contribution: candidate.average_similarity * 100.0,
            },
            KnowledgeSignalReason {
                kind: "tag_size".to_string(),
                label: format!("{} atoms already tagged", tag.atom_count),
                value: json!(tag.atom_count),
                contribution: tag_size * 100.0,
            },
        ],
        evidence: evidence.to_value()?,
        suggested_actions: vec![
            KnowledgeSignalAction {
                id: "add_tag_to_atom".to_string(),
                label: "Add tag".to_string(),
                kind: "update".to_string(),
            },
            KnowledgeSignalAction {
                id: "open_atom".to_string(),
                label: "Open atom".to_string(),
                kind: "open".to_string(),
            },
            KnowledgeSignalAction {
                id: "dismiss".to_string(),
                label: "Dismiss".to_string(),
                kind: "dismiss".to_string(),
            },
        ],
        created_at: now.to_string(),
        expires_at: None,
    }))
}

fn near_duplicate_atom_signal(
    candidate: &NearDuplicateAtomCandidate,
    shared_tags: Vec<AtomTagInfo>,
    now: &str,
) -> Result<Option<KnowledgeSignal>, AtomicCoreError> {
    let title_similarity = name_similarity(&candidate.primary.title, &candidate.secondary.title);
    let source_match = source_match_status(
        candidate.primary.source_url.as_deref(),
        candidate.secondary.source_url.as_deref(),
    );
    let shared_tag_count = shared_tags.len() as i32;
    let content_length_ratio = content_length_ratio(
        candidate.primary.content_length,
        candidate.secondary.content_length,
    );

    let same_source = source_match == "same_source";
    let strong_support = same_source || title_similarity >= 0.75 || shared_tag_count >= 2;
    if candidate.semantic_similarity < 0.92
        && !(candidate.semantic_similarity >= 0.86 && strong_support)
    {
        return Ok(None);
    }

    let source_component = if same_source { 1.0 } else { 0.0 };
    let shared_tag_component = (shared_tag_count as f32 / 4.0).min(1.0);
    let score = (100.0
        * (0.62 * candidate.semantic_similarity
            + 0.14 * source_component
            + 0.10 * title_similarity
            + 0.08 * shared_tag_component
            + 0.06 * content_length_ratio))
        .clamp(0.0, 100.0);
    let confidence = (0.55 * candidate.semantic_similarity
        + 0.16 * source_component
        + 0.12 * title_similarity
        + 0.10 * shared_tag_component
        + 0.07 * content_length_ratio)
        .clamp(0.0, 1.0);

    let (primary, secondary) = order_duplicate_atoms(&candidate.primary, &candidate.secondary);
    let evidence = NearDuplicateAtomEvidence {
        primary_atom: primary.into(),
        secondary_atom: secondary.into(),
        semantic_similarity: candidate.semantic_similarity,
        source_match: source_match.to_string(),
        title_similarity,
        shared_tag_count,
        shared_tags: shared_tags
            .iter()
            .map(|tag| NearDuplicateTagEvidence {
                id: tag.id.clone(),
                name: tag.name.clone(),
            })
            .collect(),
        content_length_ratio,
    };

    let mut reasons = vec![KnowledgeSignalReason {
        kind: "semantic_similarity".to_string(),
        label: "Very similar meaning".to_string(),
        value: json!(candidate.semantic_similarity),
        contribution: candidate.semantic_similarity * 100.0,
    }];
    if same_source {
        reasons.push(KnowledgeSignalReason {
            kind: "source_match".to_string(),
            label: "Same source".to_string(),
            value: json!(source_match),
            contribution: 100.0,
        });
    }
    if title_similarity >= 0.70 {
        reasons.push(KnowledgeSignalReason {
            kind: "title_similarity".to_string(),
            label: "Similar title".to_string(),
            value: json!(title_similarity),
            contribution: title_similarity * 100.0,
        });
    }
    if shared_tag_count > 0 {
        reasons.push(KnowledgeSignalReason {
            kind: "shared_tags".to_string(),
            label: format!("{shared_tag_count} shared tags"),
            value: json!(shared_tag_count),
            contribution: shared_tag_component * 100.0,
        });
    }

    Ok(Some(KnowledgeSignal {
        id: near_duplicate_atom_signal_key(&candidate.primary.id, &candidate.secondary.id),
        provider_id: NEAR_DUPLICATE_ATOM_PROVIDER_ID.to_string(),
        target: KnowledgeSignalTarget::atom(primary.id.clone(), primary.title.clone()),
        score,
        confidence,
        severity: KnowledgeSignalSeverity::Review,
        title: format!(
            "Review similar atoms: {} and {}",
            primary.title, secondary.title
        ),
        summary: "These atoms appear to substantially overlap.".to_string(),
        reasons,
        evidence: evidence.to_value()?,
        suggested_actions: vec![
            KnowledgeSignalAction {
                id: "review_pair".to_string(),
                label: "Review pair".to_string(),
                kind: "open".to_string(),
            },
            KnowledgeSignalAction {
                id: "open_primary_atom".to_string(),
                label: "Open first atom".to_string(),
                kind: "open".to_string(),
            },
            KnowledgeSignalAction {
                id: "open_secondary_atom".to_string(),
                label: "Open second atom".to_string(),
                kind: "open".to_string(),
            },
            KnowledgeSignalAction {
                id: "keep_separate".to_string(),
                label: "Keep separate".to_string(),
                kind: "dismiss".to_string(),
            },
        ],
        created_at: now.to_string(),
        expires_at: None,
    }))
}

fn source_duplicate_signal(
    candidate: &SourceDuplicateCandidate,
    now: &str,
) -> Result<Option<KnowledgeSignal>, AtomicCoreError> {
    let title_similarity = name_similarity(&candidate.primary.title, &candidate.secondary.title);
    let content_length_ratio = content_length_ratio(
        candidate.primary.content_length,
        candidate.secondary.content_length,
    );
    let group_component = (candidate.duplicate_count as f32 / 4.0).min(1.0);
    let score = (100.0
        * (0.62 + 0.16 * title_similarity + 0.12 * content_length_ratio + 0.10 * group_component))
        .clamp(0.0, 100.0);
    let confidence =
        (0.82 + 0.08 * title_similarity + 0.06 * content_length_ratio + 0.04 * group_component)
            .clamp(0.0, 1.0);

    let (primary, secondary) = order_duplicate_atoms(&candidate.primary, &candidate.secondary);
    let source_url = primary
        .source_url
        .clone()
        .or_else(|| secondary.source_url.clone())
        .unwrap_or_else(|| candidate.normalized_source_url.clone());
    let evidence = SourceDuplicateEvidence {
        primary_atom: primary.into(),
        secondary_atom: secondary.into(),
        source_url,
        normalized_source_url: candidate.normalized_source_url.clone(),
        duplicate_count: candidate.duplicate_count,
        title_similarity,
        content_length_ratio,
    };

    let mut reasons = vec![KnowledgeSignalReason {
        kind: "source_match".to_string(),
        label: "Same source URL".to_string(),
        value: json!(candidate.normalized_source_url),
        contribution: 100.0,
    }];
    if candidate.duplicate_count > 2 {
        reasons.push(KnowledgeSignalReason {
            kind: "duplicate_count".to_string(),
            label: format!("{} captures share this source", candidate.duplicate_count),
            value: json!(candidate.duplicate_count),
            contribution: group_component * 100.0,
        });
    }
    if title_similarity >= 0.65 {
        reasons.push(KnowledgeSignalReason {
            kind: "title_similarity".to_string(),
            label: "Similar title".to_string(),
            value: json!(title_similarity),
            contribution: title_similarity * 100.0,
        });
    }

    Ok(Some(KnowledgeSignal {
        id: source_duplicate_signal_key(&candidate.primary.id, &candidate.secondary.id),
        provider_id: SOURCE_DUPLICATE_PROVIDER_ID.to_string(),
        target: KnowledgeSignalTarget::atom(primary.id.clone(), primary.title.clone()),
        score,
        confidence,
        severity: KnowledgeSignalSeverity::Review,
        title: format!(
            "Review duplicate source captures: {} and {}",
            primary.title, secondary.title
        ),
        summary: "These atoms were captured from the same source URL.".to_string(),
        reasons,
        evidence: evidence.to_value()?,
        suggested_actions: vec![
            KnowledgeSignalAction {
                id: "review_pair".to_string(),
                label: "Review pair".to_string(),
                kind: "open".to_string(),
            },
            KnowledgeSignalAction {
                id: "open_primary_atom".to_string(),
                label: "Open first atom".to_string(),
                kind: "open".to_string(),
            },
            KnowledgeSignalAction {
                id: "open_secondary_atom".to_string(),
                label: "Open second atom".to_string(),
                kind: "open".to_string(),
            },
            KnowledgeSignalAction {
                id: "keep_separate".to_string(),
                label: "Keep separate".to_string(),
                kind: "dismiss".to_string(),
            },
        ],
        created_at: now.to_string(),
        expires_at: None,
    }))
}

fn broken_internal_link_signal(
    candidate: &BrokenInternalLinkCandidate,
    now: &str,
) -> Result<Option<KnowledgeSignal>, AtomicCoreError> {
    let is_missing_atom = candidate.target_kind == "atom_id" && candidate.status == "missing";
    let (score, confidence, title_prefix, summary) = if is_missing_atom {
        (
            72.0,
            0.95,
            "Fix missing atom link",
            "This atom links to an atom id that does not exist.",
        )
    } else {
        (
            42.0,
            0.70,
            "Review unresolved note link",
            "This atom contains a text wikilink that has not been resolved to an atom.",
        )
    };

    let evidence = BrokenInternalLinkEvidence {
        link_id: candidate.link_id.clone(),
        source_atom_id: candidate.source_atom_id.clone(),
        source_atom_title: candidate.source_atom_title.clone(),
        raw_target: candidate.raw_target.clone(),
        label: candidate.label.clone(),
        target_kind: candidate.target_kind.clone(),
        status: candidate.status.clone(),
        start_offset: candidate.start_offset,
        end_offset: candidate.end_offset,
    };

    Ok(Some(KnowledgeSignal {
        id: format!(
            "{}:link:{}",
            BROKEN_INTERNAL_LINK_PROVIDER_ID, candidate.link_id
        ),
        provider_id: BROKEN_INTERNAL_LINK_PROVIDER_ID.to_string(),
        target: KnowledgeSignalTarget::atom(
            candidate.source_atom_id.clone(),
            candidate.source_atom_title.clone(),
        ),
        score,
        confidence,
        severity: KnowledgeSignalSeverity::Review,
        title: format!("{title_prefix}: {}", candidate.raw_target),
        summary: summary.to_string(),
        reasons: vec![
            KnowledgeSignalReason {
                kind: "link_status".to_string(),
                label: if is_missing_atom {
                    "Target atom is missing".to_string()
                } else {
                    "Text link is unresolved".to_string()
                },
                value: json!(candidate.status),
                contribution: score,
            },
            KnowledgeSignalReason {
                kind: "target_kind".to_string(),
                label: candidate.target_kind.replace('_', " "),
                value: json!(candidate.target_kind),
                contribution: if is_missing_atom { 25.0 } else { 10.0 },
            },
        ],
        evidence: evidence.to_value()?,
        suggested_actions: vec![
            KnowledgeSignalAction {
                id: "open_atom".to_string(),
                label: "Open atom".to_string(),
                kind: "open".to_string(),
            },
            KnowledgeSignalAction {
                id: "dismiss".to_string(),
                label: "Dismiss".to_string(),
                kind: "dismiss".to_string(),
            },
        ],
        created_at: now.to_string(),
        expires_at: None,
    }))
}

fn underconnected_atom_signal(
    candidate: &UnderconnectedAtomCandidate,
    now: &str,
) -> Result<Option<KnowledgeSignal>, AtomicCoreError> {
    if candidate.edges_status != "complete" || candidate.captured_atom_count < 8 {
        return Ok(None);
    }

    let edge_isolation = if candidate.strong_edge_count == 0 {
        1.0
    } else {
        (1.0 - (candidate.strong_edge_count as f32 / 3.0)).clamp(0.0, 1.0)
    };
    let similarity_gap = candidate
        .strongest_similarity
        .map(|similarity| (1.0 - (similarity / 0.65)).clamp(0.0, 1.0))
        .unwrap_or(1.0);
    let sparse_tags = match candidate.tag_count {
        0 => 1.0,
        1 => 0.70,
        2 => 0.35,
        _ => 0.0,
    };
    let content_substance = (candidate.content_length as f32 / 1200.0).clamp(0.0, 1.0);
    let db_size_confidence = (candidate.captured_atom_count as f32 / 25.0).clamp(0.0, 1.0);

    let score = (100.0
        * (0.42 * edge_isolation
            + 0.28 * similarity_gap
            + 0.20 * sparse_tags
            + 0.10 * content_substance))
        .clamp(0.0, 100.0);
    let confidence = (0.40 * edge_isolation
        + 0.24 * similarity_gap
        + 0.18 * sparse_tags
        + 0.10 * content_substance
        + 0.08 * db_size_confidence)
        .clamp(0.0, 1.0);

    let evidence = UnderconnectedAtomEvidence {
        atom_id: candidate.atom_id.clone(),
        atom_title: candidate.atom_title.clone(),
        source_url: candidate.source_url.clone(),
        content_length: candidate.content_length,
        tag_count: candidate.tag_count,
        total_edge_count: candidate.total_edge_count,
        strong_edge_count: candidate.strong_edge_count,
        strongest_similarity: candidate.strongest_similarity,
        average_similarity: candidate.average_similarity,
        captured_atom_count: candidate.captured_atom_count,
        edges_status: candidate.edges_status.clone(),
    };

    let mut reasons = vec![KnowledgeSignalReason {
        kind: "strong_edges".to_string(),
        label: match candidate.strong_edge_count {
            0 => "No strong semantic connections".to_string(),
            1 => "Only 1 strong semantic connection".to_string(),
            count => format!("Only {count} strong semantic connections"),
        },
        value: json!(candidate.strong_edge_count),
        contribution: edge_isolation * 100.0,
    }];
    if let Some(strongest) = candidate.strongest_similarity {
        reasons.push(KnowledgeSignalReason {
            kind: "strongest_similarity".to_string(),
            label: format!("Closest note is {:.0}% similar", strongest * 100.0),
            value: json!(strongest),
            contribution: similarity_gap * 100.0,
        });
    }
    if candidate.tag_count <= 1 {
        reasons.push(KnowledgeSignalReason {
            kind: "tag_count".to_string(),
            label: match candidate.tag_count {
                0 => "No tags".to_string(),
                1 => "Only 1 tag".to_string(),
                count => format!("{count} tags"),
            },
            value: json!(candidate.tag_count),
            contribution: sparse_tags * 100.0,
        });
    }

    Ok(Some(KnowledgeSignal {
        id: format!("underconnected_atom:atom:{}", candidate.atom_id),
        provider_id: UNDERCONNECTED_ATOM_PROVIDER_ID.to_string(),
        target: KnowledgeSignalTarget::atom(
            candidate.atom_id.clone(),
            candidate.atom_title.clone(),
        ),
        score,
        confidence,
        severity: KnowledgeSignalSeverity::Review,
        title: format!("Review underconnected atom: {}", candidate.atom_title),
        summary:
            "This atom has little semantic or tag connection to the rest of the knowledge base."
                .to_string(),
        reasons,
        evidence: evidence.to_value()?,
        suggested_actions: vec![
            KnowledgeSignalAction {
                id: "open_atom".to_string(),
                label: "Open atom".to_string(),
                kind: "open".to_string(),
            },
            KnowledgeSignalAction {
                id: "dismiss".to_string(),
                label: "Dismiss".to_string(),
                kind: "dismiss".to_string(),
            },
        ],
        created_at: now.to_string(),
        expires_at: None,
    }))
}

fn limit_missing_tag_signals(
    mut signals: Vec<KnowledgeSignal>,
) -> Result<Vec<KnowledgeSignal>, AtomicCoreError> {
    signals.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| {
                b.confidence
                    .partial_cmp(&a.confidence)
                    .unwrap_or(Ordering::Equal)
            })
    });

    let mut per_atom: HashMap<String, usize> = HashMap::new();
    signals.retain(|signal| {
        let count = per_atom.entry(signal.target.id.clone()).or_default();
        if *count >= 2 {
            return false;
        }
        *count += 1;
        true
    });
    signals.truncate(100);
    Ok(signals)
}

fn limit_near_duplicate_atom_signals(
    mut signals: Vec<KnowledgeSignal>,
) -> Result<Vec<KnowledgeSignal>, AtomicCoreError> {
    signals.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| {
                b.confidence
                    .partial_cmp(&a.confidence)
                    .unwrap_or(Ordering::Equal)
            })
    });

    let mut per_atom: HashMap<String, usize> = HashMap::new();
    signals.retain(|signal| {
        let Some(evidence) = signal.evidence.as_object() else {
            return true;
        };
        let Some(primary_id) = evidence
            .get("primary_atom")
            .and_then(|atom| atom.get("id"))
            .and_then(Value::as_str)
        else {
            return true;
        };
        let Some(secondary_id) = evidence
            .get("secondary_atom")
            .and_then(|atom| atom.get("id"))
            .and_then(Value::as_str)
        else {
            return true;
        };
        let primary_count = *per_atom.get(primary_id).unwrap_or(&0);
        let secondary_count = *per_atom.get(secondary_id).unwrap_or(&0);
        if primary_count >= 3 || secondary_count >= 3 {
            return false;
        }
        *per_atom.entry(primary_id.to_string()).or_default() += 1;
        *per_atom.entry(secondary_id.to_string()).or_default() += 1;
        true
    });
    signals.truncate(100);
    Ok(signals)
}

fn limit_underconnected_atom_signals(
    mut signals: Vec<KnowledgeSignal>,
) -> Result<Vec<KnowledgeSignal>, AtomicCoreError> {
    signals.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| {
                b.confidence
                    .partial_cmp(&a.confidence)
                    .unwrap_or(Ordering::Equal)
            })
    });
    signals.truncate(100);
    Ok(signals)
}

fn limit_broken_internal_link_signals(
    mut signals: Vec<KnowledgeSignal>,
) -> Result<Vec<KnowledgeSignal>, AtomicCoreError> {
    signals.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| {
                b.confidence
                    .partial_cmp(&a.confidence)
                    .unwrap_or(Ordering::Equal)
            })
    });

    let mut per_atom: HashMap<String, usize> = HashMap::new();
    signals.retain(|signal| {
        let count = per_atom.entry(signal.target.id.clone()).or_default();
        if *count >= 3 {
            return false;
        }
        *count += 1;
        true
    });
    signals.truncate(100);
    Ok(signals)
}

fn tag_pair_signal(
    a: &TagCleanupTag,
    b: &TagCleanupTag,
    shared_atom_count: i32,
    centroid_similarity: Option<f32>,
    now: &str,
) -> Result<Option<KnowledgeSignal>, AtomicCoreError> {
    if is_structural_tag(a) || is_structural_tag(b) {
        return Ok(None);
    }

    let min_count = a.atom_count.min(b.atom_count).max(0);
    let union = (a.atom_count + b.atom_count - shared_atom_count).max(1);
    let jaccard = shared_atom_count as f32 / union as f32;
    let containment = if min_count == 0 {
        0.0
    } else {
        shared_atom_count as f32 / min_count as f32
    };
    let name_similarity = name_similarity(&a.name, &b.name);
    let relationship = hierarchy_relationship(a, b);

    if min_count < 5 && !(containment >= 1.0 && name_similarity >= 0.75) {
        return Ok(None);
    }
    if matches!(
        relationship.as_str(),
        "parent_child" | "ancestor_descendant" | "cross_category"
    ) && jaccard < 0.90
    {
        return Ok(None);
    }

    let duplicate_like = jaccard >= 0.80
        || (jaccard >= 0.65
            && name_similarity >= 0.70
            && matches!(relationship.as_str(), "sibling" | "unrelated"));
    let subsumed_like = containment >= 0.85
        && !matches!(
            relationship.as_str(),
            "parent_child" | "ancestor_descendant"
        );

    if !duplicate_like && !subsumed_like {
        return Ok(None);
    }

    let review_posture = if duplicate_like {
        "possible_duplicate"
    } else {
        "possible_subsumption"
    };
    let hierarchy_boost = match relationship.as_str() {
        "sibling" => 0.12,
        "unrelated" => 0.06,
        "cross_category" => -0.20,
        _ => -0.10,
    };
    let semantic = centroid_similarity.unwrap_or(0.0).max(0.0);
    let score = if duplicate_like {
        100.0
            * (0.58 * jaccard
                + 0.16 * containment
                + 0.12 * name_similarity
                + 0.08 * semantic
                + hierarchy_boost)
    } else {
        100.0
            * (0.62 * containment
                + 0.12 * jaccard
                + 0.10 * name_similarity
                + 0.08 * semantic
                + hierarchy_boost)
    }
    .clamp(0.0, 100.0);

    let confidence = (0.45 * jaccard.max(containment)
        + 0.20 * (shared_atom_count as f32 / 12.0).min(1.0)
        + 0.15 * name_similarity
        + 0.10 * semantic
        + 0.10
            * if matches!(relationship.as_str(), "sibling" | "unrelated") {
                1.0
            } else {
                0.4
            })
    .clamp(0.0, 1.0);

    let (primary, secondary) = if a.atom_count >= b.atom_count {
        (a, b)
    } else {
        (b, a)
    };
    let evidence = TagRedundancyEvidence {
        primary_tag: primary.into(),
        secondary_tag: secondary.into(),
        shared_atom_count,
        primary_unique_atom_count: (primary.atom_count - shared_atom_count).max(0),
        secondary_unique_atom_count: (secondary.atom_count - shared_atom_count).max(0),
        jaccard_overlap: jaccard,
        containment_overlap: containment,
        centroid_similarity,
        name_similarity,
        hierarchy_relationship: relationship.clone(),
        review_posture: review_posture.to_string(),
    };

    let title = if duplicate_like {
        format!("Review similar tags: {} and {}", a.name, b.name)
    } else {
        format!("Review overlapping tags: {} and {}", a.name, b.name)
    };
    let mut reasons = vec![
        KnowledgeSignalReason {
            kind: "shared_atoms".to_string(),
            label: format!("{shared_atom_count} shared atoms"),
            value: json!(shared_atom_count),
            contribution: shared_atom_count as f32,
        },
        KnowledgeSignalReason {
            kind: "overlap".to_string(),
            label: format!("{:.0}% overlap", jaccard * 100.0),
            value: json!(jaccard),
            contribution: jaccard * 100.0,
        },
    ];
    if containment >= 0.85 && !duplicate_like {
        reasons.push(KnowledgeSignalReason {
            kind: "containment".to_string(),
            label: "one tag is mostly contained in the other".to_string(),
            value: json!(containment),
            contribution: containment * 100.0,
        });
    }
    if matches!(relationship.as_str(), "sibling" | "unrelated") {
        reasons.push(KnowledgeSignalReason {
            kind: "hierarchy".to_string(),
            label: relationship.replace('_', " "),
            value: json!(relationship),
            contribution: 10.0,
        });
    }

    Ok(Some(KnowledgeSignal {
        id: tag_pair_signal_key(&a.id, &b.id),
        provider_id: TAG_REDUNDANCY_PROVIDER_ID.to_string(),
        target: KnowledgeSignalTarget::tag(primary.id.clone(), primary.name.clone()),
        score,
        confidence,
        severity: KnowledgeSignalSeverity::Review,
        title,
        summary: "These tags share enough atom membership to be worth reviewing together."
            .to_string(),
        reasons,
        evidence: evidence.to_value()?,
        suggested_actions: vec![
            KnowledgeSignalAction {
                id: "review_overlap".to_string(),
                label: "Review overlap".to_string(),
                kind: "open".to_string(),
            },
            KnowledgeSignalAction {
                id: "merge_tags".to_string(),
                label: "Merge tags".to_string(),
                kind: "merge".to_string(),
            },
            KnowledgeSignalAction {
                id: "keep_separate".to_string(),
                label: "Keep separate".to_string(),
                kind: "dismiss".to_string(),
            },
        ],
        created_at: now.to_string(),
        expires_at: None,
    }))
}

fn empty_tag_signal(tag: &TagCleanupTag, now: &str) -> Result<KnowledgeSignal, AtomicCoreError> {
    let evidence = EmptyTagEvidence { tag: tag.into() };
    Ok(KnowledgeSignal {
        id: format!("empty_tag:tag:{}", tag.id),
        provider_id: EMPTY_TAG_PROVIDER_ID.to_string(),
        target: KnowledgeSignalTarget::tag(tag.id.clone(), tag.name.clone()),
        score: 35.0,
        confidence: 1.0,
        severity: KnowledgeSignalSeverity::Review,
        title: format!("Review empty tag: {}", tag.name),
        summary: "This tag has no atoms and no child tags.".to_string(),
        reasons: vec![
            KnowledgeSignalReason {
                kind: "atom_count".to_string(),
                label: "0 atoms".to_string(),
                value: json!(0),
                contribution: 20.0,
            },
            KnowledgeSignalReason {
                kind: "children".to_string(),
                label: "no child tags".to_string(),
                value: json!(0),
                contribution: 15.0,
            },
        ],
        evidence: evidence.to_value()?,
        suggested_actions: vec![
            KnowledgeSignalAction {
                id: "delete_empty_tag".to_string(),
                label: "Delete tag".to_string(),
                kind: "delete".to_string(),
            },
            KnowledgeSignalAction {
                id: "keep".to_string(),
                label: "Keep".to_string(),
                kind: "dismiss".to_string(),
            },
        ],
        created_at: now.to_string(),
        expires_at: None,
    })
}

fn tag_pair_signal_key(a: &str, b: &str) -> String {
    let (left, right) = if a <= b { (a, b) } else { (b, a) };
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(format!("{left}:{right}").as_bytes());
    format!("tag_redundancy:pair:{:x}", digest)
}

fn near_duplicate_atom_signal_key(a: &str, b: &str) -> String {
    let (left, right) = if a <= b { (a, b) } else { (b, a) };
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(format!("{left}:{right}").as_bytes());
    format!("near_duplicate_atom:pair:{:x}", digest)
}

fn source_duplicate_signal_key(a: &str, b: &str) -> String {
    let (left, right) = if a <= b { (a, b) } else { (b, a) };
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(format!("{left}:{right}").as_bytes());
    format!("source_duplicate:pair:{:x}", digest)
}

impl From<&NearDuplicateAtomInfo> for NearDuplicateAtomEvidenceAtom {
    fn from(atom: &NearDuplicateAtomInfo) -> Self {
        Self {
            id: atom.id.clone(),
            title: atom.title.clone(),
            source_url: atom.source_url.clone(),
            content_length: atom.content_length,
            created_at: atom.created_at.clone(),
            updated_at: atom.updated_at.clone(),
        }
    }
}

fn order_duplicate_atoms<'a>(
    a: &'a NearDuplicateAtomInfo,
    b: &'a NearDuplicateAtomInfo,
) -> (&'a NearDuplicateAtomInfo, &'a NearDuplicateAtomInfo) {
    if a.updated_at >= b.updated_at {
        (a, b)
    } else {
        (b, a)
    }
}

fn shared_atom_tags(
    atom_a: &str,
    atom_b: &str,
    atom_tags: &HashMap<String, Vec<AtomTagInfo>>,
) -> Vec<AtomTagInfo> {
    let Some(tags_a) = atom_tags.get(atom_a) else {
        return Vec::new();
    };
    let Some(tags_b) = atom_tags.get(atom_b) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for tag_a in tags_a {
        if tags_b.iter().any(|tag_b| tag_b.id == tag_a.id) {
            out.push(tag_a.clone());
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

fn source_match_status(a: Option<&str>, b: Option<&str>) -> &'static str {
    match (normalize_source_url(a), normalize_source_url(b)) {
        (Some(left), Some(right)) if left == right => "same_source",
        (Some(_), Some(_)) => "different_source",
        _ => "missing_source",
    }
}

fn normalize_source_url(value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    if value.is_empty() {
        return None;
    }
    Some(value.trim_end_matches('/').to_ascii_lowercase())
}

fn content_length_ratio(a: i32, b: i32) -> f32 {
    let min = a.min(b).max(0) as f32;
    let max = a.max(b).max(1) as f32;
    (min / max).clamp(0.0, 1.0)
}

fn is_structural_tag(tag: &TagCleanupTag) -> bool {
    tag.parent_id.is_none()
        && (tag.is_autotag_target
            || matches!(
                tag.name.as_str(),
                "Topics" | "People" | "Locations" | "Organizations" | "Events"
            ))
}

fn tags_are_hierarchically_related(
    a_id: &str,
    b_id: &str,
    tags: &HashMap<String, TagCleanupTag>,
) -> bool {
    a_id == b_id || tag_is_ancestor_of(a_id, b_id, tags) || tag_is_ancestor_of(b_id, a_id, tags)
}

fn tag_is_ancestor_of(
    ancestor_id: &str,
    child_id: &str,
    tags: &HashMap<String, TagCleanupTag>,
) -> bool {
    let mut current = tags.get(child_id).and_then(|tag| tag.parent_id.as_deref());
    while let Some(parent_id) = current {
        if parent_id == ancestor_id {
            return true;
        }
        current = tags.get(parent_id).and_then(|tag| tag.parent_id.as_deref());
    }
    false
}

fn hierarchy_relationship(a: &TagCleanupTag, b: &TagCleanupTag) -> String {
    if a.parent_id.is_some() && a.parent_id == b.parent_id {
        return "sibling".to_string();
    }
    if a.parent_id.as_deref() == Some(&b.id) || b.parent_id.as_deref() == Some(&a.id) {
        return "parent_child".to_string();
    }
    if a.path
        .first()
        .zip(b.path.first())
        .is_some_and(|(x, y)| x != y)
    {
        return "cross_category".to_string();
    }
    "unrelated".to_string()
}

fn name_similarity(a: &str, b: &str) -> f32 {
    let norm_a = normalize_tag_name(a);
    let norm_b = normalize_tag_name(b);
    if norm_a.is_empty() || norm_b.is_empty() {
        return 0.0;
    }
    if norm_a == norm_b {
        return 1.0;
    }
    let tokens_a: std::collections::HashSet<&str> = norm_a.split_whitespace().collect();
    let tokens_b: std::collections::HashSet<&str> = norm_b.split_whitespace().collect();
    let intersection = tokens_a.intersection(&tokens_b).count() as f32;
    let union = tokens_a.union(&tokens_b).count().max(1) as f32;
    let token_score = intersection / union;
    let containment = if norm_a.contains(&norm_b) || norm_b.contains(&norm_a) {
        0.75
    } else {
        0.0
    };
    token_score.max(containment)
}

fn normalize_tag_name(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
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
    use tokio::time::{sleep, Duration as TokioDuration};

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
    async fn dashboard_signal_cache_invalidates_after_feedback() {
        let (core, _temp) = test_core().await;
        let tag = create_child_tag(&core, "Cache Invalidation").await;

        for i in 0..3 {
            core.create_atom(
                CreateAtomRequest {
                    content: long_note(&format!("Cache Invalidation {i}")),
                    source_url: Some(format!("https://example.com/cache/{i}")),
                    tag_ids: vec![tag.id.clone()],
                    ..Default::default()
                },
                |_| {},
            )
            .await
            .unwrap()
            .unwrap();
        }

        let first = list_dashboard_knowledge_signals(&core, 20).await.unwrap();
        let signal_id = first
            .groups
            .iter()
            .flat_map(|group| group.signals.iter())
            .find(|signal| signal.id == format!("wiki_candidate:tag:{}", tag.id))
            .expect("cached dashboard signal")
            .id
            .clone();

        dismiss_signal(&core, &signal_id).await.unwrap();

        let after_dismiss = list_dashboard_knowledge_signals(&core, 20).await.unwrap();
        assert!(!after_dismiss
            .groups
            .iter()
            .flat_map(|group| group.signals.iter())
            .any(|signal| signal.id == signal_id));
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

    #[tokio::test]
    async fn dashboard_signal_listing_honors_provider_visibility_and_weight() {
        let (core, _temp) = test_core().await;
        let tag = create_child_tag(&core, "Database Internals").await;

        core.create_atom(
            CreateAtomRequest {
                content: long_note("Database Internals"),
                source_url: Some("https://example.com/database-internals".to_string()),
                tag_ids: vec![tag.id.clone()],
                ..Default::default()
            },
            |_| {},
        )
        .await
        .unwrap()
        .unwrap();

        let baseline = list_knowledge_signals(
            &core,
            KnowledgeSignalFilter {
                provider_id: Some(WIKI_CANDIDATE_PROVIDER_ID.to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let baseline_score = baseline
            .iter()
            .find(|signal| signal.target.id == tag.id)
            .expect("baseline signal")
            .score;

        set_provider_config(
            &core,
            WIKI_CANDIDATE_PROVIDER_ID,
            KnowledgeSignalProviderConfig {
                provider_id: WIKI_CANDIDATE_PROVIDER_ID.to_string(),
                enabled: true,
                weight: 0.5,
                min_score: 0.0,
                min_confidence: 0.0,
                show_on_dashboard: true,
                include_in_briefing: true,
                config_json: json!({}),
            },
        )
        .await
        .unwrap();

        let weighted = list_knowledge_signals(
            &core,
            KnowledgeSignalFilter {
                provider_id: Some(WIKI_CANDIDATE_PROVIDER_ID.to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let weighted_score = weighted
            .iter()
            .find(|signal| signal.target.id == tag.id)
            .expect("weighted signal")
            .score;
        assert!((weighted_score - baseline_score * 0.5).abs() < 0.01);

        set_provider_config(
            &core,
            WIKI_CANDIDATE_PROVIDER_ID,
            KnowledgeSignalProviderConfig {
                provider_id: WIKI_CANDIDATE_PROVIDER_ID.to_string(),
                enabled: true,
                weight: 1.0,
                min_score: 0.0,
                min_confidence: 0.0,
                show_on_dashboard: false,
                include_in_briefing: true,
                config_json: json!({}),
            },
        )
        .await
        .unwrap();

        let generic_signals = list_knowledge_signals(
            &core,
            KnowledgeSignalFilter {
                provider_id: Some(WIKI_CANDIDATE_PROVIDER_ID.to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(generic_signals
            .iter()
            .any(|signal| signal.target.id == tag.id));

        let dashboard = list_dashboard_knowledge_signals(&core, 20).await.unwrap();
        assert!(!dashboard
            .groups
            .iter()
            .flat_map(|group| group.signals.iter())
            .any(|signal| signal.target.id == tag.id));
    }

    #[tokio::test]
    async fn provider_config_listing_returns_defaults_and_saved_preferences() {
        let (core, _temp) = test_core().await;

        let configs = core.list_knowledge_signal_provider_configs().await.unwrap();
        let wiki_update = configs
            .iter()
            .find(|provider| provider.provider_id == WIKI_UPDATE_PROVIDER_ID)
            .expect("wiki update provider config");
        assert_eq!(wiki_update.name, "Wiki updates");
        assert!(wiki_update.config.enabled);
        assert!(wiki_update.config.include_in_briefing);

        core.set_knowledge_signal_provider_config(
            WIKI_UPDATE_PROVIDER_ID,
            KnowledgeSignalProviderConfig {
                provider_id: WIKI_UPDATE_PROVIDER_ID.to_string(),
                enabled: false,
                weight: 1.0,
                min_score: 25.0,
                min_confidence: 0.4,
                show_on_dashboard: false,
                include_in_briefing: false,
                config_json: json!({}),
            },
        )
        .await
        .unwrap();

        let updated = core.list_knowledge_signal_provider_configs().await.unwrap();
        let wiki_update = updated
            .iter()
            .find(|provider| provider.provider_id == WIKI_UPDATE_PROVIDER_ID)
            .expect("updated wiki update provider config");
        assert!(!wiki_update.config.enabled);
        assert!(!wiki_update.config.show_on_dashboard);
        assert_eq!(wiki_update.config.min_score, 25.0);
    }

    #[tokio::test]
    async fn wiki_update_signal_has_typed_evidence_and_reasons() {
        let (core, _temp) = test_core().await;
        let tag = create_child_tag(&core, "Knowledge Graphs").await;

        core.create_atom(
            CreateAtomRequest {
                content: long_note("Knowledge Graphs baseline"),
                source_url: Some("https://example.com/kg/baseline".to_string()),
                tag_ids: vec![tag.id.clone()],
                ..Default::default()
            },
            |_| {},
        )
        .await
        .unwrap()
        .unwrap();

        let storage = match core.storage() {
            crate::storage::StorageBackend::Sqlite(storage) => storage,
            #[cfg(feature = "postgres")]
            crate::storage::StorageBackend::Postgres(_) => panic!("test uses SQLite storage"),
        };
        storage
            .save_wiki_sync(&tag.id, "Existing wiki", &[], 1)
            .unwrap();

        sleep(TokioDuration::from_millis(5)).await;

        for i in 0..2 {
            core.create_atom(
                CreateAtomRequest {
                    content: long_note(&format!("Knowledge Graphs update {i}")),
                    source_url: Some(format!("https://example.com/kg/update/{i}")),
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
                provider_id: Some(WIKI_UPDATE_PROVIDER_ID.to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let signal = signals
            .iter()
            .find(|signal| signal.target.id == tag.id)
            .expect("wiki update signal");

        assert_eq!(signal.id, format!("wiki_update:tag:{}", tag.id));
        assert_eq!(signal.provider_id, WIKI_UPDATE_PROVIDER_ID);
        assert_eq!(signal.evidence["schema"], "wiki_update");
        assert_eq!(signal.evidence["schema_version"], 1);
        assert_eq!(signal.evidence["tag_id"], tag.id);
        assert_eq!(signal.evidence["tag_name"], "Knowledge Graphs");
        assert_eq!(signal.evidence["article_atom_count"], 1);
        assert_eq!(signal.evidence["current_atom_count"], 3);
        assert_eq!(signal.evidence["new_atom_count"], 2);
        assert!(signal
            .reasons
            .iter()
            .any(|reason| reason.kind == "new_atom_volume"));
        assert!(signal
            .suggested_actions
            .iter()
            .any(|action| action.id == "update_wiki"));
    }

    #[tokio::test]
    async fn wiki_update_signal_includes_retagged_existing_atoms() {
        let (core, _temp) = test_core().await;
        let tag = create_child_tag(&core, "Research").await;

        core.create_atom(
            CreateAtomRequest {
                content: long_note("Research baseline"),
                source_url: Some("https://example.com/research/baseline".to_string()),
                tag_ids: vec![tag.id.clone()],
                ..Default::default()
            },
            |_| {},
        )
        .await
        .unwrap()
        .unwrap();

        let retagged_atom = core
            .create_atom(
                CreateAtomRequest {
                    content: long_note("Research note that was classified later"),
                    source_url: Some("https://example.com/research/retagged".to_string()),
                    tag_ids: vec![],
                    ..Default::default()
                },
                |_| {},
            )
            .await
            .unwrap()
            .unwrap();

        sleep(TokioDuration::from_millis(5)).await;

        let storage = match core.storage() {
            crate::storage::StorageBackend::Sqlite(storage) => storage,
            #[cfg(feature = "postgres")]
            crate::storage::StorageBackend::Postgres(_) => panic!("test uses SQLite storage"),
        };
        storage
            .save_wiki_sync(&tag.id, "Existing wiki", &[], 1)
            .unwrap();

        core.add_tag_to_atom(&retagged_atom.atom.id, &tag.id)
            .await
            .unwrap();

        let signals = list_knowledge_signals(
            &core,
            KnowledgeSignalFilter {
                provider_id: Some(WIKI_UPDATE_PROVIDER_ID.to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let signal = signals
            .iter()
            .find(|signal| signal.target.id == tag.id)
            .expect("wiki update signal for retagged atom");

        assert_eq!(signal.evidence["article_atom_count"], 1);
        assert_eq!(signal.evidence["current_atom_count"], 2);
        assert_eq!(signal.evidence["new_atom_count"], 0);
        assert!(signal.score > 0.0);
        assert!(signal.reasons.iter().any(|reason| {
            reason.kind == "new_atom_volume" && reason.label == "1 atom not reflected"
        }));
    }

    #[tokio::test]
    async fn tag_redundancy_signal_has_typed_evidence_and_merge_action() {
        let (core, _temp) = test_core().await;
        let parent = core.create_tag("Topics", None).await.unwrap();
        let tag_a = core
            .create_tag("AI Agents", Some(&parent.id))
            .await
            .unwrap();
        let tag_b = core
            .create_tag("Agentic AI", Some(&parent.id))
            .await
            .unwrap();

        let mut atom_ids = Vec::new();
        for i in 0..6 {
            let atom = core
                .create_atom(
                    CreateAtomRequest {
                        content: long_note(&format!("Agent systems {i}")),
                        source_url: Some(format!("https://example.com/agents/{i}")),
                        tag_ids: vec![tag_a.id.clone(), tag_b.id.clone()],
                        ..Default::default()
                    },
                    |_| {},
                )
                .await
                .unwrap()
                .unwrap();
            atom_ids.push(atom.atom.id);
        }

        {
            let db = core.database().unwrap();
            let conn = db.conn.lock().unwrap();
            let now = Utc::now().to_rfc3339();
            conn.execute(
                "INSERT INTO wiki_articles (id, tag_id, content, created_at, updated_at, atom_count)
                 VALUES (?1, ?2, 'source article', ?3, ?3, 1)",
                params!["source-wiki", &tag_b.id, &now],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO wiki_articles_fts (id, tag_id, tag_name, content)
                 VALUES (?1, ?2, ?3, 'source article')",
                params!["source-wiki", &tag_b.id, &tag_b.name],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO wiki_citations (id, wiki_article_id, citation_index, atom_id, chunk_index, excerpt)
                 VALUES (?1, 'source-wiki', 1, ?2, NULL, 'excerpt')",
                params!["source-citation", &atom_ids[0]],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO wiki_article_versions (id, tag_id, content, citations_json, atom_count, version_number, created_at)
                 VALUES (?1, ?2, 'old source article', '[]', 1, 1, ?3)",
                params!["source-version", &tag_b.id, &now],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO wiki_proposals (id, tag_id, base_article_id, base_updated_at, content, citations_json, ops_json, new_atom_count, created_at)
                 VALUES (?1, ?2, 'source-wiki', ?3, 'proposal', '[]', '[]', 1, ?3)",
                params!["source-proposal", &tag_b.id, &now],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO wiki_links (id, source_article_id, target_tag_name, target_tag_id, created_at)
                 VALUES (?1, 'source-wiki', ?2, ?3, ?4)",
                params!["source-link", &tag_a.name, &tag_a.id, &now],
            )
            .unwrap();
        }

        let signals = list_knowledge_signals(
            &core,
            KnowledgeSignalFilter {
                provider_id: Some(TAG_REDUNDANCY_PROVIDER_ID.to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let signal = signals
            .iter()
            .find(|signal| {
                signal.evidence["primary_tag"]["id"] == tag_a.id
                    || signal.evidence["secondary_tag"]["id"] == tag_a.id
            })
            .expect("tag redundancy signal");

        assert_eq!(signal.provider_id, TAG_REDUNDANCY_PROVIDER_ID);
        assert!(signal.id.starts_with("tag_redundancy:pair:"));
        assert_eq!(signal.evidence["schema"], "tag_redundancy");
        assert_eq!(signal.evidence["schema_version"], 1);
        assert_eq!(signal.evidence["shared_atom_count"], 6);
        assert_eq!(signal.evidence["jaccard_overlap"], 1.0);
        assert_eq!(signal.evidence["hierarchy_relationship"], "sibling");
        assert!(signal
            .suggested_actions
            .iter()
            .any(|action| action.id == "merge_tags"));

        let result = core.merge_tags(&tag_b.id, &tag_a.id).await.unwrap();
        assert_eq!(result.atoms_retagged, 0);
        assert_eq!(result.children_reparented, 0);
        assert!(result.source_wiki_deleted);

        let remaining = core.get_all_tags().await.unwrap();
        let flat = flatten_test_tags(&remaining);
        assert!(flat.iter().any(|tag| tag.id == tag_a.id));
        assert!(!flat.iter().any(|tag| tag.id == tag_b.id));

        let db = core.database().unwrap();
        let conn = db.conn.lock().unwrap();
        for (table, column) in [
            ("atom_tags", "tag_id"),
            ("wiki_articles", "tag_id"),
            ("wiki_articles_fts", "tag_id"),
            ("wiki_article_versions", "tag_id"),
            ("wiki_proposals", "tag_id"),
        ] {
            let count: i64 = conn
                .query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE {column} = ?1"),
                    [&tag_b.id],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(count, 0, "{table} retained source tag rows");
        }
        let citation_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM wiki_citations WHERE wiki_article_id = 'source-wiki'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(citation_count, 0);
        let link_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM wiki_links WHERE source_article_id = 'source-wiki'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(link_count, 0);
    }

    #[tokio::test]
    async fn empty_tag_signal_is_dashboard_only_cleanup() {
        let (core, _temp) = test_core().await;
        let parent = core.create_tag("Projects", None).await.unwrap();
        let empty = core
            .create_tag("Abandoned Draft", Some(&parent.id))
            .await
            .unwrap();
        let structural = core.create_tag("People", None).await.unwrap();
        core.set_tag_autotag_target(&structural.id, true)
            .await
            .unwrap();

        let signals = list_knowledge_signals(
            &core,
            KnowledgeSignalFilter {
                provider_id: Some(EMPTY_TAG_PROVIDER_ID.to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        assert!(signals.iter().any(|signal| signal.target.id == empty.id));
        assert!(!signals.iter().any(|signal| signal.target.id == parent.id));
        assert!(!signals
            .iter()
            .any(|signal| signal.target.id == structural.id));

        let signal = signals
            .iter()
            .find(|signal| signal.target.id == empty.id)
            .expect("empty tag signal");
        assert_eq!(signal.id, format!("empty_tag:tag:{}", empty.id));
        assert_eq!(signal.evidence["schema"], "empty_tag");
        assert_eq!(signal.evidence["tag"]["id"], empty.id);
        assert!(signal
            .suggested_actions
            .iter()
            .any(|action| action.id == "delete_empty_tag"));
    }

    #[tokio::test]
    async fn missing_tag_overlap_signal_can_add_suggested_tag() {
        let (core, _temp) = test_core().await;
        let parent = core.create_tag("Topics", None).await.unwrap();
        let suggested = core
            .create_tag("Distributed Systems", Some(&parent.id))
            .await
            .unwrap();
        let existing = core
            .create_tag("Databases", Some(&parent.id))
            .await
            .unwrap();

        let target = core
            .create_atom(
                CreateAtomRequest {
                    content: long_note("Consensus overview"),
                    tag_ids: vec![existing.id.clone()],
                    ..Default::default()
                },
                |_| {},
            )
            .await
            .unwrap()
            .unwrap();

        let mut neighbors = Vec::new();
        for i in 0..3 {
            let atom = core
                .create_atom(
                    CreateAtomRequest {
                        content: long_note(&format!("Distributed systems neighbor {i}")),
                        tag_ids: vec![suggested.id.clone()],
                        ..Default::default()
                    },
                    |_| {},
                )
                .await
                .unwrap()
                .unwrap();
            neighbors.push(atom.atom.id);
        }

        let storage = match core.storage() {
            crate::storage::StorageBackend::Sqlite(storage) => storage,
            #[cfg(feature = "postgres")]
            crate::storage::StorageBackend::Postgres(_) => panic!("test uses SQLite storage"),
        };
        {
            let conn = storage.database().conn.lock().unwrap();
            let now = Utc::now().to_rfc3339();
            for (idx, neighbor_id) in neighbors.iter().enumerate() {
                conn.execute(
                    "INSERT INTO semantic_edges
                        (id, source_atom_id, target_atom_id, similarity_score,
                         source_chunk_index, target_chunk_index, created_at)
                     VALUES (?1, ?2, ?3, ?4, 0, 0, ?5)",
                    params![
                        format!("edge-{idx}"),
                        target.atom.id,
                        neighbor_id,
                        0.82_f32,
                        now
                    ],
                )
                .unwrap();
            }
        }

        let signals = list_knowledge_signals(
            &core,
            KnowledgeSignalFilter {
                provider_id: Some(MISSING_TAG_OVERLAP_PROVIDER_ID.to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let signal = signals
            .iter()
            .find(|signal| signal.target.id == target.atom.id)
            .expect("missing tag signal");
        assert_eq!(signal.provider_id, MISSING_TAG_OVERLAP_PROVIDER_ID);
        assert_eq!(signal.evidence["schema"], "missing_tag_overlap");
        assert_eq!(signal.evidence["atom_id"], target.atom.id);
        assert_eq!(signal.evidence["suggested_tag"]["id"], suggested.id);
        assert_eq!(signal.evidence["nearby_tagged_atom_count"], 3);
        assert!(signal
            .suggested_actions
            .iter()
            .any(|action| action.id == "add_tag_to_atom"));

        let updated = core
            .add_tag_to_atom(&target.atom.id, &suggested.id)
            .await
            .unwrap();
        assert!(updated.tags.iter().any(|tag| tag.id == suggested.id));
        assert!(updated.tags.iter().any(|tag| tag.id == existing.id));
    }

    #[tokio::test]
    async fn signal_action_add_tag_is_audited_dismissed_and_undoable() {
        let (core, _temp) = test_core().await;
        let parent = core.create_tag("Topics", None).await.unwrap();
        let suggested = core
            .create_tag("Distributed Systems", Some(&parent.id))
            .await
            .unwrap();
        let existing = core
            .create_tag("Databases", Some(&parent.id))
            .await
            .unwrap();

        let target = core
            .create_atom(
                CreateAtomRequest {
                    content: long_note("Consensus overview"),
                    tag_ids: vec![existing.id.clone()],
                    ..Default::default()
                },
                |_| {},
            )
            .await
            .unwrap()
            .unwrap();

        let mut neighbors = Vec::new();
        for i in 0..3 {
            let atom = core
                .create_atom(
                    CreateAtomRequest {
                        content: long_note(&format!("Distributed systems neighbor {i}")),
                        tag_ids: vec![suggested.id.clone()],
                        ..Default::default()
                    },
                    |_| {},
                )
                .await
                .unwrap()
                .unwrap();
            neighbors.push(atom.atom.id);
        }

        let storage = match core.storage() {
            crate::storage::StorageBackend::Sqlite(storage) => storage,
            #[cfg(feature = "postgres")]
            crate::storage::StorageBackend::Postgres(_) => panic!("test uses SQLite storage"),
        };
        {
            let conn = storage.database().conn.lock().unwrap();
            let now = Utc::now().to_rfc3339();
            for (idx, neighbor_id) in neighbors.iter().enumerate() {
                conn.execute(
                    "INSERT INTO semantic_edges
                        (id, source_atom_id, target_atom_id, similarity_score,
                         source_chunk_index, target_chunk_index, created_at)
                     VALUES (?1, ?2, ?3, ?4, 0, 0, ?5)",
                    params![
                        format!("action-edge-{idx}"),
                        target.atom.id,
                        neighbor_id,
                        0.82_f32,
                        now
                    ],
                )
                .unwrap();
            }
        }

        let signal = list_knowledge_signals(
            &core,
            KnowledgeSignalFilter {
                provider_id: Some(MISSING_TAG_OVERLAP_PROVIDER_ID.to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .into_iter()
        .find(|signal| signal.target.id == target.atom.id)
        .expect("missing tag signal");

        let result = apply_signal_action(
            &core,
            &signal.id,
            KnowledgeSignalActionRequest {
                action: "add_tag_to_atom".to_string(),
                payload: json!({}),
            },
        )
        .await
        .unwrap();
        assert_eq!(result.action, "add_tag_to_atom");
        assert!(result.undo_supported);

        let updated = core.get_atom(&target.atom.id).await.unwrap().unwrap();
        assert!(updated.tags.iter().any(|tag| tag.id == suggested.id));
        assert!(updated.tags.iter().any(|tag| tag.id == existing.id));

        let visible_after_apply = list_knowledge_signals(
            &core,
            KnowledgeSignalFilter {
                provider_id: Some(MISSING_TAG_OVERLAP_PROVIDER_ID.to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(!visible_after_apply.iter().any(|item| item.id == signal.id));

        {
            let conn = storage.database().conn.lock().unwrap();
            let status: String = conn
                .query_row(
                    "SELECT status FROM knowledge_signal_action_log WHERE id = ?1",
                    [&result.action_log_id],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(status, "applied");
        }

        let undo = undo_signal_action(&core, &result.action_log_id)
            .await
            .unwrap();
        assert_eq!(undo.status, "undone");
        let undone = core.get_atom(&target.atom.id).await.unwrap().unwrap();
        assert!(!undone.tags.iter().any(|tag| tag.id == suggested.id));
        assert!(undone.tags.iter().any(|tag| tag.id == existing.id));

        let second_undo = undo_signal_action(&core, &result.action_log_id)
            .await
            .unwrap();
        assert_eq!(second_undo.status, "undone");
    }

    #[tokio::test]
    async fn near_duplicate_atom_signal_has_typed_evidence_and_review_actions() {
        let (core, _temp) = test_core().await;
        let tag = create_child_tag(&core, "LLM Evaluation").await;

        let first = core
            .create_atom(
                CreateAtomRequest {
                    content: long_note("LLM Evaluation Notes"),
                    source_url: Some("https://example.com/evals".to_string()),
                    tag_ids: vec![tag.id.clone()],
                    ..Default::default()
                },
                |_| {},
            )
            .await
            .unwrap()
            .unwrap();
        let second = core
            .create_atom(
                CreateAtomRequest {
                    content: long_note("LLM Evaluation Notes"),
                    source_url: Some("https://example.com/evals/".to_string()),
                    tag_ids: vec![tag.id.clone()],
                    ..Default::default()
                },
                |_| {},
            )
            .await
            .unwrap()
            .unwrap();

        let storage = match core.storage() {
            crate::storage::StorageBackend::Sqlite(storage) => storage,
            #[cfg(feature = "postgres")]
            crate::storage::StorageBackend::Postgres(_) => panic!("test uses SQLite storage"),
        };
        {
            let conn = storage.database().conn.lock().unwrap();
            conn.execute(
                "INSERT INTO semantic_edges
                    (id, source_atom_id, target_atom_id, similarity_score,
                     source_chunk_index, target_chunk_index, created_at)
                 VALUES (?1, ?2, ?3, ?4, 0, 0, ?5)",
                params![
                    "duplicate-edge",
                    first.atom.id,
                    second.atom.id,
                    0.91_f32,
                    Utc::now().to_rfc3339()
                ],
            )
            .unwrap();
        }

        let signals = list_knowledge_signals(
            &core,
            KnowledgeSignalFilter {
                provider_id: Some(NEAR_DUPLICATE_ATOM_PROVIDER_ID.to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let signal = signals
            .iter()
            .find(|signal| {
                signal.evidence["primary_atom"]["id"] == first.atom.id
                    || signal.evidence["secondary_atom"]["id"] == first.atom.id
            })
            .expect("near duplicate atom signal");
        assert_eq!(signal.provider_id, NEAR_DUPLICATE_ATOM_PROVIDER_ID);
        assert!(signal.id.starts_with("near_duplicate_atom:pair:"));
        assert_eq!(signal.evidence["schema"], "near_duplicate_atom");
        assert_eq!(signal.evidence["schema_version"], 1);
        let semantic_similarity = signal.evidence["semantic_similarity"].as_f64().unwrap();
        assert!((semantic_similarity - 0.91).abs() < 0.001);
        assert_eq!(signal.evidence["source_match"], "same_source");
        assert_eq!(signal.evidence["shared_tag_count"], 1);
        assert!(signal
            .suggested_actions
            .iter()
            .any(|action| action.id == "keep_separate"));

        dismiss_signal(&core, &signal.id).await.unwrap();
        let hidden = list_knowledge_signals(
            &core,
            KnowledgeSignalFilter {
                provider_id: Some(NEAR_DUPLICATE_ATOM_PROVIDER_ID.to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(hidden.is_empty());
    }

    #[tokio::test]
    async fn source_duplicate_signal_detects_same_source_url() {
        let (core, _temp) = test_core().await;

        let first = core
            .create_atom(
                CreateAtomRequest {
                    content: long_note("Capture from duplicate source"),
                    source_url: Some("https://example.com/articles/source".to_string()),
                    ..Default::default()
                },
                |_| {},
            )
            .await
            .unwrap()
            .unwrap();
        let second = core
            .create_atom(
                CreateAtomRequest {
                    content: long_note("Capture from duplicate source"),
                    source_url: Some("https://EXAMPLE.com/articles/source/".to_string()),
                    ..Default::default()
                },
                |_| {},
            )
            .await
            .unwrap()
            .unwrap();

        let signals = list_knowledge_signals(
            &core,
            KnowledgeSignalFilter {
                provider_id: Some(SOURCE_DUPLICATE_PROVIDER_ID.to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let signal = signals
            .iter()
            .find(|signal| {
                signal.evidence["primary_atom"]["id"] == first.atom.id
                    || signal.evidence["secondary_atom"]["id"] == first.atom.id
            })
            .expect("source duplicate signal");
        assert_eq!(signal.provider_id, SOURCE_DUPLICATE_PROVIDER_ID);
        assert!(signal.id.starts_with("source_duplicate:pair:"));
        assert_eq!(signal.evidence["schema"], "source_duplicate");
        assert_eq!(signal.evidence["schema_version"], 1);
        assert_eq!(
            signal.evidence["normalized_source_url"],
            "https://example.com/articles/source"
        );
        assert_eq!(signal.evidence["duplicate_count"], 2);
        assert!(
            signal.evidence["primary_atom"]["id"] == second.atom.id
                || signal.evidence["secondary_atom"]["id"] == second.atom.id
        );
        assert!(signal
            .suggested_actions
            .iter()
            .any(|action| action.id == "keep_separate"));
    }

    #[tokio::test]
    async fn broken_internal_link_signal_detects_missing_atom_link() {
        let (core, _temp) = test_core().await;
        let missing_id = "550e8400-e29b-41d4-a716-446655440000";
        let atom = core
            .create_atom(
                CreateAtomRequest {
                    content: format!(
                        "# Broken Links\n\nThis note points to [[{missing_id}|a deleted note]]."
                    ),
                    ..Default::default()
                },
                |_| {},
            )
            .await
            .unwrap()
            .unwrap();

        let signals = list_knowledge_signals(
            &core,
            KnowledgeSignalFilter {
                provider_id: Some(BROKEN_INTERNAL_LINK_PROVIDER_ID.to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let signal = signals
            .iter()
            .find(|signal| signal.target.id == atom.atom.id)
            .expect("broken internal link signal");
        assert_eq!(signal.provider_id, BROKEN_INTERNAL_LINK_PROVIDER_ID);
        assert!(signal.id.starts_with("broken_internal_link:link:"));
        assert_eq!(signal.evidence["schema"], "broken_internal_link");
        assert_eq!(signal.evidence["schema_version"], 1);
        assert_eq!(signal.evidence["source_atom_id"], atom.atom.id);
        assert_eq!(signal.evidence["raw_target"], missing_id);
        assert_eq!(signal.evidence["label"], "a deleted note");
        assert_eq!(signal.evidence["target_kind"], "atom_id");
        assert_eq!(signal.evidence["status"], "missing");
        assert!(signal
            .suggested_actions
            .iter()
            .any(|action| action.id == "open_atom"));
    }

    #[tokio::test]
    async fn underconnected_atom_signal_requires_completed_edges() {
        let (core, _temp) = test_core().await;
        let parent = core.create_tag("Research Areas", None).await.unwrap();
        let tag_a = core
            .create_tag("Connected Systems", Some(&parent.id))
            .await
            .unwrap();
        let tag_b = core
            .create_tag("Architecture", Some(&parent.id))
            .await
            .unwrap();

        let isolated = core
            .create_atom(
                CreateAtomRequest {
                    content: long_note("Standalone architecture note"),
                    tag_ids: vec![],
                    ..Default::default()
                },
                |_| {},
            )
            .await
            .unwrap()
            .unwrap();

        let mut connected = Vec::new();
        for i in 0..7 {
            let atom = core
                .create_atom(
                    CreateAtomRequest {
                        content: long_note(&format!("Connected systems note {i}")),
                        tag_ids: vec![tag_a.id.clone(), tag_b.id.clone()],
                        ..Default::default()
                    },
                    |_| {},
                )
                .await
                .unwrap()
                .unwrap();
            connected.push(atom.atom.id);
        }

        let storage = match core.storage() {
            crate::storage::StorageBackend::Sqlite(storage) => storage,
            #[cfg(feature = "postgres")]
            crate::storage::StorageBackend::Postgres(_) => panic!("test uses SQLite storage"),
        };
        {
            let conn = storage.database().conn.lock().unwrap();
            conn.execute("UPDATE atoms SET edges_status = 'complete'", [])
                .unwrap();
            let now = Utc::now().to_rfc3339();
            for idx in 0..connected.len() - 1 {
                conn.execute(
                    "INSERT INTO semantic_edges
                        (id, source_atom_id, target_atom_id, similarity_score,
                         source_chunk_index, target_chunk_index, created_at)
                     VALUES (?1, ?2, ?3, ?4, 0, 0, ?5)",
                    params![
                        format!("connected-edge-{idx}"),
                        connected[idx],
                        connected[idx + 1],
                        0.82_f32,
                        now
                    ],
                )
                .unwrap();
            }
        }

        let signals = list_knowledge_signals(
            &core,
            KnowledgeSignalFilter {
                provider_id: Some(UNDERCONNECTED_ATOM_PROVIDER_ID.to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let signal = signals
            .iter()
            .find(|signal| signal.target.id == isolated.atom.id)
            .expect("underconnected atom signal");
        assert_eq!(signal.provider_id, UNDERCONNECTED_ATOM_PROVIDER_ID);
        assert_eq!(
            signal.id,
            format!("underconnected_atom:atom:{}", isolated.atom.id)
        );
        assert_eq!(signal.evidence["schema"], "underconnected_atom");
        assert_eq!(signal.evidence["schema_version"], 1);
        assert_eq!(signal.evidence["strong_edge_count"], 0);
        assert_eq!(signal.evidence["tag_count"], 0);
        assert_eq!(signal.evidence["edges_status"], "complete");
        assert!(signal
            .suggested_actions
            .iter()
            .any(|action| action.id == "open_atom"));
        assert!(!signals
            .iter()
            .any(|signal| connected.iter().any(|id| id == &signal.target.id)));
    }

    fn flatten_test_tags(tags: &[crate::TagWithCount]) -> Vec<crate::Tag> {
        let mut out = Vec::new();
        for tag in tags {
            out.push(tag.tag.clone());
            out.extend(flatten_test_tags(&tag.children));
        }
        out
    }
}
