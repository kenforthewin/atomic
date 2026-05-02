//! User-defined custom health checks.
//!
//! Atomic ships with opinionated built-in checks, but different knowledge-base
//! workflows have different conventions. Custom checks let users declare rules
//! that reflect *their* conventions ("every atom tagged `paper` must have a
//! source URL", "flag atoms containing TODO markers", etc.) without shipping
//! arbitrary SQL or JavaScript from the UI.
//!
//! # Safety
//!
//! Rules are structured, not free-form: each [`CustomRule`] variant is a
//! hard-coded predicate the Rust evaluator applies. The UI only controls
//! parameters (tag ids, regex patterns) — never the query shape. This avoids
//! the SQL-injection and resource-exhaustion risks that come with arbitrary
//! user-defined SQL, while still covering the workflows users actually ask
//! for.
//!
//! # Storage
//!
//! The full list is persisted per-DB as JSON under the `custom_health_checks`
//! setting key (NOT in registry — see AGENTS.md § Multi-DB Gotchas). Each DB
//! has its own independent rule set.

use super::HealthCheckResult;
use crate::error::AtomicCoreError;
use crate::storage::sqlite::SqliteStorage;
use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;

/// Structural rule a custom check evaluates. One variant per supported
/// predicate shape — keeping this enum small is deliberate: every variant the
/// UI needs requires a Rust implementation, and that pressure keeps the
/// feature safe and predictable.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CustomRule {
    /// Atoms tagged with any of `any_of` must also be tagged with every tag
    /// in `required`. Flags atoms that violate the invariant.
    TagRequires {
        any_of: Vec<String>,
        required: Vec<String>,
    },
    /// Atoms (optionally filtered to those tagged `tag_filter`) must have a
    /// non-empty `source_url`. Flags atoms missing a source.
    RequireSource {
        /// When `Some`, only atoms tagged with this id are checked.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tag_filter: Option<String>,
    },
    /// Atoms whose markdown body matches `pattern` are flagged. When `invert`
    /// is true, atoms that do NOT match are flagged instead.
    ContentRegex {
        pattern: String,
        #[serde(default)]
        invert: bool,
    },
}

/// User-defined health check. `id` is a stable uuid so UI edits don't change
/// the score identity across saves.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CustomCheck {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// 0 = informational (not scored). > 0 contributes at that weight,
    /// normalized alongside built-in checks.
    #[serde(default)]
    pub weight: f64,
    pub rule: CustomRule,
}

fn default_enabled() -> bool {
    true
}

/// Pre-fixed key used to identify custom checks inside the `checks` map of a
/// `HealthReport`. Prevents collisions with built-in check names.
pub const CUSTOM_CHECK_PREFIX: &str = "custom.";

/// Compose the map key a custom check appears under in the report.
pub fn result_key(check_id: &str) -> String {
    format!("{CUSTOM_CHECK_PREFIX}{check_id}")
}

/// Evaluate all enabled custom checks against the database and return a
/// `(map_key, HealthCheckResult)` entry per check.
///
/// Runs on the caller's tokio runtime. Each rule executes one or two bounded
/// queries — no N+1, no full-table scans beyond what the built-in checks
/// already do.
pub fn run_all(
    storage: &SqliteStorage,
    checks: &[CustomCheck],
) -> Result<Vec<(String, HealthCheckResult, CustomCheck)>, AtomicCoreError> {
    let conn = storage
        .db
        .conn
        .lock()
        .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
    let mut out = Vec::with_capacity(checks.len());
    for check in checks.iter().filter(|c| c.enabled) {
        let result = evaluate(&conn, &check.rule)?;
        out.push((result_key(&check.id), finalize(check, result), check.clone()));
    }
    Ok(out)
}

/// Raw per-rule evaluation output before we wrap it as a `HealthCheckResult`.
struct RawOutcome {
    total_considered: i32,
    flagged_atoms: Vec<FlaggedAtom>,
}

#[derive(Serialize)]
struct FlaggedAtom {
    id: String,
    title_preview: String,
}

fn evaluate(
    conn: &rusqlite::Connection,
    rule: &CustomRule,
) -> Result<RawOutcome, AtomicCoreError> {
    match rule {
        CustomRule::TagRequires { any_of, required } => eval_tag_requires(conn, any_of, required),
        CustomRule::RequireSource { tag_filter } => eval_require_source(conn, tag_filter.as_deref()),
        CustomRule::ContentRegex { pattern, invert } => eval_content_regex(conn, pattern, *invert),
    }
}

/// Preview the first non-empty line of `content` for UI display.
fn preview(content: &str) -> String {
    let trimmed = content
        .lines()
        .map(|l| l.trim())
        .find(|l| !l.is_empty())
        .unwrap_or("");
    const MAX: usize = 80;
    if trimmed.chars().count() > MAX {
        let truncated: String = trimmed.chars().take(MAX).collect();
        format!("{truncated}…")
    } else {
        trimmed.to_string()
    }
}

/// Bound flagged-atom lists so a malformed rule can't blow up the report.
const MAX_FLAGGED: usize = 500;

fn eval_tag_requires(
    conn: &rusqlite::Connection,
    any_of: &[String],
    required: &[String],
) -> Result<RawOutcome, AtomicCoreError> {
    if any_of.is_empty() {
        // Empty filter = nothing to check; treat as passing.
        return Ok(RawOutcome {
            total_considered: 0,
            flagged_atoms: Vec::new(),
        });
    }

    // Candidate atoms: those carrying at least one of `any_of`.
    let placeholders_any: String = std::iter::repeat("?")
        .take(any_of.len())
        .collect::<Vec<_>>()
        .join(",");
    let candidate_sql = format!(
        "SELECT DISTINCT a.id, a.content FROM atoms a \
         JOIN atom_tags at ON at.atom_id = a.id \
         WHERE at.tag_id IN ({placeholders_any})"
    );
    let mut stmt = conn.prepare(&candidate_sql)?;
    let rows = stmt.query_map(
        rusqlite::params_from_iter(any_of.iter()),
        |row| {
            let id: String = row.get(0)?;
            let content: String = row.get(1)?;
            Ok((id, content))
        },
    )?;
    let candidates: Vec<(String, String)> = rows.collect::<Result<_, _>>()?;
    let total_considered = candidates.len() as i32;

    if required.is_empty() {
        // No required tags to check — everything is fine by definition.
        return Ok(RawOutcome {
            total_considered,
            flagged_atoms: Vec::new(),
        });
    }

    // For each candidate, fetch the set of tag ids; flag if any `required`
    // tag is missing. We do a single query to load (atom_id, tag_id) pairs
    // for the candidate set, then bucket in memory — O(N) over result rows
    // rather than one query per atom.
    let ids: Vec<&str> = candidates.iter().map(|(id, _)| id.as_str()).collect();
    let mut tags_by_atom: HashMap<String, std::collections::HashSet<String>> = HashMap::new();
    if !ids.is_empty() {
        let placeholders: String = std::iter::repeat("?")
            .take(ids.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT atom_id, tag_id FROM atom_tags WHERE atom_id IN ({placeholders})"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(ids.iter().copied()), |row| {
            let aid: String = row.get(0)?;
            let tid: String = row.get(1)?;
            Ok((aid, tid))
        })?;
        for row in rows {
            let (aid, tid) = row?;
            tags_by_atom.entry(aid).or_default().insert(tid);
        }
    }

    let mut flagged = Vec::new();
    for (id, content) in candidates {
        let tags = tags_by_atom.get(&id);
        let missing_any = required.iter().any(|r| match tags {
            Some(set) => !set.contains(r),
            None => true,
        });
        if missing_any {
            flagged.push(FlaggedAtom {
                title_preview: preview(&content),
                id,
            });
            if flagged.len() >= MAX_FLAGGED {
                break;
            }
        }
    }

    Ok(RawOutcome {
        total_considered,
        flagged_atoms: flagged,
    })
}

fn eval_require_source(
    conn: &rusqlite::Connection,
    tag_filter: Option<&str>,
) -> Result<RawOutcome, AtomicCoreError> {
    let mut total = 0i32;
    let mut flagged = Vec::new();

    let mut consume = |id: String, content: String, source: Option<String>| {
        total += 1;
        let missing = match source {
            Some(s) => s.trim().is_empty(),
            None => true,
        };
        if missing && flagged.len() < MAX_FLAGGED {
            flagged.push(FlaggedAtom {
                title_preview: preview(&content),
                id,
            });
        }
    };

    match tag_filter {
        Some(tag) => {
            let mut stmt = conn.prepare(
                "SELECT DISTINCT a.id, a.content, a.source_url FROM atoms a \
                 JOIN atom_tags at ON at.atom_id = a.id \
                 WHERE at.tag_id = ?1",
            )?;
            let rows = stmt.query_map(params![tag], |row| {
                let id: String = row.get(0)?;
                let content: String = row.get(1)?;
                let source: Option<String> = row.get(2)?;
                Ok((id, content, source))
            })?;
            for row in rows {
                let (id, content, source) = row?;
                consume(id, content, source);
            }
        }
        None => {
            let mut stmt = conn.prepare("SELECT id, content, source_url FROM atoms")?;
            let rows = stmt.query_map([], |row| {
                let id: String = row.get(0)?;
                let content: String = row.get(1)?;
                let source: Option<String> = row.get(2)?;
                Ok((id, content, source))
            })?;
            for row in rows {
                let (id, content, source) = row?;
                consume(id, content, source);
            }
        }
    }

    Ok(RawOutcome {
        total_considered: total,
        flagged_atoms: flagged,
    })
}

fn eval_content_regex(
    conn: &rusqlite::Connection,
    pattern: &str,
    invert: bool,
) -> Result<RawOutcome, AtomicCoreError> {
    // Bound pattern size — compiled regex state grows with the input.
    if pattern.len() > 512 {
        return Err(AtomicCoreError::Validation(
            "regex pattern too long (max 512 chars)".to_string(),
        ));
    }
    let re = regex::RegexBuilder::new(pattern)
        .size_limit(1 << 20)
        .dfa_size_limit(1 << 20)
        .build()
        .map_err(|e| AtomicCoreError::Validation(format!("invalid regex: {e}")))?;

    let mut stmt = conn.prepare("SELECT id, content FROM atoms")?;
    let rows = stmt.query_map([], |row| {
        let id: String = row.get(0)?;
        let content: String = row.get(1)?;
        Ok((id, content))
    })?;

    let mut total = 0i32;
    let mut flagged = Vec::new();
    for row in rows {
        let (id, content) = row?;
        total += 1;
        let matches = re.is_match(&content);
        let flag = if invert { !matches } else { matches };
        if flag && flagged.len() < MAX_FLAGGED {
            flagged.push(FlaggedAtom {
                title_preview: preview(&content),
                id,
            });
        }
    }
    Ok(RawOutcome {
        total_considered: total,
        flagged_atoms: flagged,
    })
}

/// Wrap the raw predicate outcome as a `HealthCheckResult` honoring the check's
/// weight semantics (0 → informational, > 0 → contributes to overall score).
fn finalize(check: &CustomCheck, raw: RawOutcome) -> HealthCheckResult {
    let flagged = raw.flagged_atoms.len() as i32;
    let score = if raw.total_considered == 0 {
        100
    } else {
        let bad = flagged.min(raw.total_considered);
        let ratio = 1.0 - (bad as f64 / raw.total_considered as f64);
        (ratio * 100.0).round().clamp(0.0, 100.0) as u32
    };
    let status = if flagged == 0 {
        "ok"
    } else if score >= 80 {
        "warning"
    } else {
        "error"
    }
    .to_string();
    let informational = check.weight <= 0.0;

    HealthCheckResult {
        status,
        score,
        auto_fixable: false,
        requires_review: flagged > 0,
        informational,
        fix_action: None,
        data: json!({
            "custom": true,
            "label": check.label,
            "description": check.description,
            "rule": &check.rule,
            "total_considered": raw.total_considered,
            "flagged_count": flagged,
            "flagged": raw.flagged_atoms,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;
    use std::sync::Arc;

    fn make_storage() -> (SqliteStorage, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("test.db");
        let db = Arc::new(Database::open(&path).unwrap());
        (SqliteStorage::new(db), tmp)
    }

    fn insert_atom(conn: &rusqlite::Connection, id: &str, content: &str, source: Option<&str>) {
        conn.execute(
            "INSERT INTO atoms (id, content, source_url, embedding_status, tagging_status, created_at, updated_at) \
             VALUES (?1, ?2, ?3, 'complete', 'complete', datetime('now'), datetime('now'))",
            params![id, content, source],
        )
        .unwrap();
    }

    fn insert_tag(conn: &rusqlite::Connection, id: &str, name: &str) {
        conn.execute(
            "INSERT INTO tags (id, name, parent_id, created_at, is_autotag_target) VALUES (?1, ?2, NULL, datetime('now'), 0)",
            params![id, name],
        )
        .unwrap();
    }

    fn link(conn: &rusqlite::Connection, atom: &str, tag: &str) {
        conn.execute(
            "INSERT INTO atom_tags (atom_id, tag_id) VALUES (?1, ?2)",
            params![atom, tag],
        )
        .unwrap();
    }

    #[test]
    fn require_source_flags_atoms_without_url() {
        let (storage, _tmp) = make_storage();
        {
            let conn = storage.db.conn.lock().unwrap();
            insert_atom(&conn, "a1", "has source", Some("https://x.com/a"));
            insert_atom(&conn, "a2", "no source", None);
            insert_atom(&conn, "a3", "blank source", Some(""));
        }

        let check = CustomCheck {
            id: "c1".into(),
            label: "needs source".into(),
            description: String::new(),
            enabled: true,
            weight: 0.0,
            rule: CustomRule::RequireSource { tag_filter: None },
        };
        let out = run_all(&storage, &[check.clone()]).unwrap();
        assert_eq!(out.len(), 1);
        let (_, result, _) = &out[0];
        let data = &result.data;
        assert_eq!(data["total_considered"], 3);
        assert_eq!(data["flagged_count"], 2);
        assert_eq!(result.status, "error");
    }

    #[test]
    fn tag_requires_flags_atoms_missing_required_tag() {
        let (storage, _tmp) = make_storage();
        {
            let conn = storage.db.conn.lock().unwrap();
            insert_atom(&conn, "a1", "paper with source", Some("https://x"));
            insert_atom(&conn, "a2", "paper no source", None);
            insert_tag(&conn, "t_paper", "paper");
            insert_tag(&conn, "t_sourced", "sourced");
            link(&conn, "a1", "t_paper");
            link(&conn, "a1", "t_sourced");
            link(&conn, "a2", "t_paper");
        }

        let check = CustomCheck {
            id: "c1".into(),
            label: "papers need sourced".into(),
            description: String::new(),
            enabled: true,
            weight: 0.0,
            rule: CustomRule::TagRequires {
                any_of: vec!["t_paper".into()],
                required: vec!["t_sourced".into()],
            },
        };
        let out = run_all(&storage, &[check.clone()]).unwrap();
        assert_eq!(out.len(), 1);
        let (_, result, _) = &out[0];
        assert_eq!(result.data["total_considered"], 2);
        assert_eq!(result.data["flagged_count"], 1);
        // Only a2 is flagged.
        let flagged = result.data["flagged"].as_array().unwrap();
        assert_eq!(flagged.len(), 1);
        assert_eq!(flagged[0]["id"], "a2");
    }

    #[test]
    fn content_regex_with_invert_flags_atoms_not_matching() {
        let (storage, _tmp) = make_storage();
        {
            let conn = storage.db.conn.lock().unwrap();
            insert_atom(&conn, "a1", "has TODO inside", None);
            insert_atom(&conn, "a2", "no markers here", None);
        }

        let check = CustomCheck {
            id: "c1".into(),
            label: "no TODO in notes".into(),
            description: String::new(),
            enabled: true,
            weight: 0.0,
            rule: CustomRule::ContentRegex {
                pattern: r"TODO".into(),
                invert: false,
            },
        };
        let out = run_all(&storage, &[check.clone()]).unwrap();
        let (_, result, _) = &out[0];
        assert_eq!(result.data["flagged_count"], 1);
        assert_eq!(result.data["flagged"][0]["id"], "a1");

        let inverted = CustomCheck {
            rule: CustomRule::ContentRegex {
                pattern: r"TODO".into(),
                invert: true,
            },
            ..check.clone()
        };
        let out = run_all(&storage, &[inverted]).unwrap();
        let (_, result, _) = &out[0];
        assert_eq!(result.data["flagged_count"], 1);
        assert_eq!(result.data["flagged"][0]["id"], "a2");
    }

    #[test]
    fn disabled_checks_are_skipped() {
        let (storage, _tmp) = make_storage();
        let check = CustomCheck {
            id: "c1".into(),
            label: "anything".into(),
            description: String::new(),
            enabled: false,
            weight: 0.0,
            rule: CustomRule::RequireSource { tag_filter: None },
        };
        let out = run_all(&storage, &[check.clone()]).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn zero_weight_produces_informational_result() {
        let (storage, _tmp) = make_storage();
        {
            let conn = storage.db.conn.lock().unwrap();
            insert_atom(&conn, "a1", "x", None);
        }
        let check = CustomCheck {
            id: "c1".into(),
            label: "l".into(),
            description: String::new(),
            enabled: true,
            weight: 0.0,
            rule: CustomRule::RequireSource { tag_filter: None },
        };
        let out = run_all(&storage, &[check.clone()]).unwrap();
        assert!(out[0].1.informational);

        let scored = CustomCheck {
            weight: 0.2,
            ..check
        };
        let out = run_all(&storage, &[scored]).unwrap();
        assert!(!out[0].1.informational);
    }
}
