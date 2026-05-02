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

    // ---- Tier 1 ----

    /// Atoms must carry at least one tag from `any_of`. Catches bare dump
    /// notes. `tag_filter`, when set, restricts the check to atoms already
    /// carrying that tag.
    RequireTag {
        any_of: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tag_filter: Option<String>,
    },
    /// Word-count bounds. 0 on either side means unbounded. Complements the
    /// char-based `content_quality` check: users can opt in to this per
    /// category rather than applying a global length heuristic.
    ContentLength {
        #[serde(default)]
        min_words: u32,
        #[serde(default)]
        max_words: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tag_filter: Option<String>,
    },
    /// Atoms must contain at least `min_citations` inline citations, where
    /// a citation is a markdown link `[text](url)` or a wikilink `[[...]]`.
    CitationCount {
        #[serde(default)]
        min_citations: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tag_filter: Option<String>,
    },
    /// Source URL must be on an allowlist or must NOT be on a blocklist of
    /// domain suffixes. `domains` matches as a suffix so `arxiv.org` covers
    /// `arxiv.org/abs/...`. Atoms without a source_url are skipped — use
    /// `RequireSource` to police that separately.
    SourceDomainMatches {
        domains: Vec<String>,
        #[serde(default)]
        mode: DomainMatchMode,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tag_filter: Option<String>,
    },
    /// Atoms tagged with `tag` and unmodified for longer than `max_age_days`
    /// are flagged.
    StaleAtom {
        tag: String,
        max_age_days: u32,
    },

    // ---- Tier 2 ----

    /// Atoms must NOT carry every tag in `all_of` simultaneously. Models
    /// mutually-exclusive categories.
    ForbiddenTagCombo {
        all_of: Vec<String>,
    },
    /// Markdown atoms with content longer than `min_length_chars` must
    /// contain at least one `#` heading.
    MissingHeading {
        #[serde(default = "default_min_heading_len")]
        min_length_chars: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tag_filter: Option<String>,
    },
    /// Atoms must carry between `min` and `max` (inclusive) total tags.
    /// 0 = unbounded on that side.
    TagCardinality {
        #[serde(default)]
        min: u32,
        #[serde(default)]
        max: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tag_filter: Option<String>,
    },
}

/// How `SourceDomainMatches` interprets the `domains` list.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum DomainMatchMode {
    /// Flag atoms whose source_url domain is NOT in the list.
    #[default]
    Allowlist,
    /// Flag atoms whose source_url domain IS in the list.
    Blocklist,
}

fn default_min_heading_len() -> u32 {
    120
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

/// Preview output for a single (unsaved) rule. Used by the UI to show
/// users "this would flag N atoms" as they tune rule parameters, before
/// persisting the rule.
#[derive(Serialize, Debug)]
pub struct PreviewResult {
    pub total_considered: i32,
    pub flagged_count: i32,
    /// First few flagged atoms (capped at `PREVIEW_SAMPLE`). Each entry
    /// has `id` and `title_preview`.
    pub sample: Vec<serde_json::Value>,
}

/// Cap on sample atoms returned in the preview. UI doesn’t need more.
const PREVIEW_SAMPLE: usize = 10;

/// Evaluate a single rule against the database WITHOUT persisting it or
/// touching the report. Fails when the rule itself is malformed (e.g.
/// invalid regex); the caller surfaces the error so the user can fix it.
pub fn preview_rule(
    storage: &SqliteStorage,
    rule: &CustomRule,
) -> Result<PreviewResult, AtomicCoreError> {
    let conn = storage
        .db
        .conn
        .lock()
        .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
    let raw = evaluate(&conn, rule)?;
    let sample = raw
        .flagged_atoms
        .iter()
        .take(PREVIEW_SAMPLE)
        .map(|f| json!({ "id": f.id, "title_preview": f.title_preview }))
        .collect();
    Ok(PreviewResult {
        total_considered: raw.total_considered,
        flagged_count: raw.flagged_atoms.len() as i32,
        sample,
    })
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
        CustomRule::RequireTag { any_of, tag_filter } => {
            eval_require_tag(conn, any_of, tag_filter.as_deref())
        }
        CustomRule::ContentLength { min_words, max_words, tag_filter } => {
            eval_content_length(conn, *min_words, *max_words, tag_filter.as_deref())
        }
        CustomRule::CitationCount { min_citations, tag_filter } => {
            eval_citation_count(conn, *min_citations, tag_filter.as_deref())
        }
        CustomRule::SourceDomainMatches { domains, mode, tag_filter } => {
            eval_source_domain(conn, domains, *mode, tag_filter.as_deref())
        }
        CustomRule::StaleAtom { tag, max_age_days } => {
            eval_stale_atom(conn, tag, *max_age_days)
        }
        CustomRule::ForbiddenTagCombo { all_of } => eval_forbidden_combo(conn, all_of),
        CustomRule::MissingHeading { min_length_chars, tag_filter } => {
            eval_missing_heading(conn, *min_length_chars, tag_filter.as_deref())
        }
        CustomRule::TagCardinality { min, max, tag_filter } => {
            eval_tag_cardinality(conn, *min, *max, tag_filter.as_deref())
        }
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

// ==================== Tier 1 ====================

/// Load candidate atoms. When `tag_filter` is `Some`, restricts to atoms
/// tagged with that tag id; otherwise all atoms. Standardizes the select-
/// and-iterate boilerplate every evaluator below shares.
fn for_each_atom<F>(
    conn: &rusqlite::Connection,
    tag_filter: Option<&str>,
    cols: &str,
    mut consume: F,
) -> Result<i32, AtomicCoreError>
where
    F: FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<bool>,
{
    let mut total = 0i32;
    match tag_filter {
        Some(tag) => {
            let sql = format!(
                "SELECT DISTINCT {cols} FROM atoms a \
                 JOIN atom_tags at ON at.atom_id = a.id \
                 WHERE at.tag_id = ?1"
            );
            let mut stmt = conn.prepare(&sql)?;
            let mut rows = stmt.query(params![tag])?;
            while let Some(row) = rows.next()? {
                if consume(row)? { total += 1; }
            }
        }
        None => {
            let sql = format!("SELECT {cols} FROM atoms a");
            let mut stmt = conn.prepare(&sql)?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                if consume(row)? { total += 1; }
            }
        }
    }
    Ok(total)
}

fn push_flag(flagged: &mut Vec<FlaggedAtom>, id: String, content: &str) {
    if flagged.len() < MAX_FLAGGED {
        flagged.push(FlaggedAtom {
            title_preview: preview(content),
            id,
        });
    }
}

/// Load `(id, content)` pairs for the candidate atom set. Wrapped as a helper
/// so evaluators that need the full candidate list (not just a streaming
/// callback) don't have to juggle statement lifetimes across match arms.
fn load_candidates_id_content(
    conn: &rusqlite::Connection,
    tag_filter: Option<&str>,
) -> Result<Vec<(String, String)>, AtomicCoreError> {
    match tag_filter {
        Some(tag) => {
            let mut stmt = conn.prepare(
                "SELECT DISTINCT a.id, a.content FROM atoms a \
                 JOIN atom_tags at ON at.atom_id = a.id \
                 WHERE at.tag_id = ?1",
            )?;
            let rows = stmt.query_map(params![tag], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            Ok(rows.collect::<Result<_, _>>()?)
        }
        None => {
            let mut stmt = conn.prepare("SELECT id, content FROM atoms")?;
            let rows = stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            Ok(rows.collect::<Result<_, _>>()?)
        }
    }
}

fn eval_require_tag(
    conn: &rusqlite::Connection,
    any_of: &[String],
    tag_filter: Option<&str>,
) -> Result<RawOutcome, AtomicCoreError> {
    if any_of.is_empty() {
        return Ok(RawOutcome { total_considered: 0, flagged_atoms: Vec::new() });
    }

    // Candidate atoms (scoped by tag_filter if present), each with their
    // full tag-id set. One bulk query, bucket in memory.
    let mut flagged = Vec::new();
    let required: std::collections::HashSet<&str> = any_of.iter().map(|s| s.as_str()).collect();

    let candidates: Vec<(String, String)> = load_candidates_id_content(conn, tag_filter)?;
    let total = candidates.len() as i32;

    if candidates.is_empty() {
        return Ok(RawOutcome { total_considered: 0, flagged_atoms: Vec::new() });
    }
    let ids: Vec<&str> = candidates.iter().map(|(id, _)| id.as_str()).collect();
    let placeholders: String = std::iter::repeat("?").take(ids.len()).collect::<Vec<_>>().join(",");
    let sql = format!("SELECT atom_id, tag_id FROM atom_tags WHERE atom_id IN ({placeholders})");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(ids.iter().copied()), |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut by_atom: HashMap<String, Vec<String>> = HashMap::new();
    for row in rows {
        let (aid, tid) = row?;
        by_atom.entry(aid).or_default().push(tid);
    }

    for (id, content) in candidates {
        let has_any = by_atom
            .get(&id)
            .map(|tags| tags.iter().any(|t| required.contains(t.as_str())))
            .unwrap_or(false);
        if !has_any {
            push_flag(&mut flagged, id, &content);
        }
    }
    Ok(RawOutcome { total_considered: total, flagged_atoms: flagged })
}

/// Fast, allocation-free word count. Treats any run of ASCII whitespace as a
/// separator — matches `str::split_whitespace` but avoids allocating an
/// intermediate iterator collection.
fn word_count(s: &str) -> u32 {
    s.split_whitespace().count() as u32
}

fn eval_content_length(
    conn: &rusqlite::Connection,
    min_words: u32,
    max_words: u32,
    tag_filter: Option<&str>,
) -> Result<RawOutcome, AtomicCoreError> {
    let mut flagged = Vec::new();
    let total = for_each_atom(conn, tag_filter, "a.id, a.content", |row| {
        let id: String = row.get(0)?;
        let content: String = row.get(1)?;
        let n = word_count(&content);
        let too_short = min_words > 0 && n < min_words;
        let too_long  = max_words > 0 && n > max_words;
        if too_short || too_long {
            push_flag(&mut flagged, id, &content);
        }
        Ok(true)
    })?;
    Ok(RawOutcome { total_considered: total, flagged_atoms: flagged })
}

/// Count markdown links `[text](url)` plus wikilinks `[[...]]`.
fn count_citations(content: &str) -> u32 {
    // Cheap linear scan — no regex allocations per atom.
    let mut n = 0u32;
    let bytes = content.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'[' {
            // wikilink: `[[`
            if i + 1 < bytes.len() && bytes[i + 1] == b'[' {
                n += 1;
                i += 2;
                continue;
            }
            // markdown link: `](` following `[...]`
            if let Some(close) = content[i..].find("](") {
                // must not contain a newline between [ and ](
                if !content[i..i + close].contains('\n') {
                    n += 1;
                    i += close + 2;
                    continue;
                }
            }
        }
        i += 1;
    }
    n
}

fn eval_citation_count(
    conn: &rusqlite::Connection,
    min_citations: u32,
    tag_filter: Option<&str>,
) -> Result<RawOutcome, AtomicCoreError> {
    let mut flagged = Vec::new();
    let total = for_each_atom(conn, tag_filter, "a.id, a.content", |row| {
        let id: String = row.get(0)?;
        let content: String = row.get(1)?;
        if count_citations(&content) < min_citations {
            push_flag(&mut flagged, id, &content);
        }
        Ok(true)
    })?;
    Ok(RawOutcome { total_considered: total, flagged_atoms: flagged })
}

/// Parse a URL’s host. Accepts http/https/ftp/etc. Returns None for empty or
/// malformed URLs — those atoms are skipped (not flagged) since policing the
/// presence of a URL is `RequireSource`’s job.
fn host_of(url: &str) -> Option<String> {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let host = rest.split('/').next()?.split('?').next()?.split('#').next()?;
    if host.is_empty() { None } else { Some(host.to_lowercase()) }
}

fn host_matches(host: &str, domains: &[String]) -> bool {
    domains.iter().any(|d| {
        let d = d.trim().trim_start_matches("https://").trim_start_matches("http://").to_lowercase();
        if d.is_empty() { return false; }
        host == d || host.ends_with(&format!(".{d}"))
    })
}

fn eval_source_domain(
    conn: &rusqlite::Connection,
    domains: &[String],
    mode: DomainMatchMode,
    tag_filter: Option<&str>,
) -> Result<RawOutcome, AtomicCoreError> {
    if domains.is_empty() {
        return Ok(RawOutcome { total_considered: 0, flagged_atoms: Vec::new() });
    }
    let mut flagged = Vec::new();
    let mut total = 0i32;
    for_each_atom(conn, tag_filter, "a.id, a.content, a.source_url", |row| {
        let id: String = row.get(0)?;
        let content: String = row.get(1)?;
        let source: Option<String> = row.get(2)?;
        let Some(url) = source.filter(|s| !s.trim().is_empty()) else {
            return Ok(false); // skip — not in the pool this rule polices
        };
        total += 1;
        let host = match host_of(&url) { Some(h) => h, None => return Ok(false) };
        let on_list = host_matches(&host, domains);
        let flag = match mode {
            DomainMatchMode::Allowlist => !on_list,
            DomainMatchMode::Blocklist => on_list,
        };
        if flag { push_flag(&mut flagged, id, &content); }
        Ok(false) // already counted manually
    })?;
    Ok(RawOutcome { total_considered: total, flagged_atoms: flagged })
}

fn eval_stale_atom(
    conn: &rusqlite::Connection,
    tag: &str,
    max_age_days: u32,
) -> Result<RawOutcome, AtomicCoreError> {
    // Compute the RFC3339 cutoff on the Rust side so SQLite string comparison
    // (lexicographic over RFC3339) gives us the right answer.
    let cutoff = chrono::Utc::now() - chrono::Duration::days(max_age_days as i64);
    let cutoff_str = cutoff.to_rfc3339();
    let mut stmt = conn.prepare(
        "SELECT a.id, a.content FROM atoms a \
         JOIN atom_tags at ON at.atom_id = a.id \
         WHERE at.tag_id = ?1",
    )?;
    let mut flagged = Vec::new();
    let mut total = 0i32;
    let rows = stmt.query_map(params![tag], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    // We fetch content + id, then check updated_at via a second single-stmt
    // per atom. SQLite handles this trivially for the expected O(tagged-atoms)
    // sizes; could be inlined with a JOIN on atoms for perf if needed.
    let mut stale_stmt = conn.prepare(
        "SELECT COALESCE(updated_at, created_at) FROM atoms WHERE id = ?1",
    )?;
    for row in rows {
        let (id, content) = row?;
        total += 1;
        let ts: String = stale_stmt.query_row(params![&id], |r| r.get(0))?;
        if ts < cutoff_str {
            push_flag(&mut flagged, id, &content);
        }
    }
    Ok(RawOutcome { total_considered: total, flagged_atoms: flagged })
}

// ==================== Tier 2 ====================

fn eval_forbidden_combo(
    conn: &rusqlite::Connection,
    all_of: &[String],
) -> Result<RawOutcome, AtomicCoreError> {
    if all_of.len() < 2 {
        return Ok(RawOutcome { total_considered: 0, flagged_atoms: Vec::new() });
    }
    // Every atom is a candidate. Count atoms that carry every required tag.
    let placeholders: String = std::iter::repeat("?").take(all_of.len()).collect::<Vec<_>>().join(",");
    let sql = format!(
        "SELECT atom_id, COUNT(DISTINCT tag_id) as matched \
         FROM atom_tags \
         WHERE tag_id IN ({placeholders}) \
         GROUP BY atom_id \
         HAVING matched = ?\
         ORDER BY atom_id"
    );
    // Collect atom ids that have ALL required tags.
    let mut params_vec: Vec<&dyn rusqlite::ToSql> = all_of.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
    let n = all_of.len() as i64;
    params_vec.push(&n);
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_vec.as_slice(), |row| row.get::<_, String>(0))?;
    let flagged_ids: Vec<String> = rows.collect::<Result<_, _>>()?;

    // Total considered = atoms that carry any of the tags (the superset we're policing).
    let total = {
        let sql = format!(
            "SELECT COUNT(DISTINCT atom_id) FROM atom_tags WHERE tag_id IN ({placeholders})"
        );
        let mut stmt = conn.prepare(&sql)?;
        stmt.query_row(rusqlite::params_from_iter(all_of.iter()), |row| row.get::<_, i64>(0))
            .unwrap_or(0) as i32
    };

    let mut flagged = Vec::new();
    if !flagged_ids.is_empty() {
        let placeholders: String = std::iter::repeat("?").take(flagged_ids.len()).collect::<Vec<_>>().join(",");
        let sql = format!("SELECT id, content FROM atoms WHERE id IN ({placeholders})");
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(flagged_ids.iter()), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (id, content) = row?;
            push_flag(&mut flagged, id, &content);
        }
    }
    Ok(RawOutcome { total_considered: total, flagged_atoms: flagged })
}

fn eval_missing_heading(
    conn: &rusqlite::Connection,
    min_length_chars: u32,
    tag_filter: Option<&str>,
) -> Result<RawOutcome, AtomicCoreError> {
    let mut flagged = Vec::new();
    let total = for_each_atom(conn, tag_filter, "a.id, a.content", |row| {
        let id: String = row.get(0)?;
        let content: String = row.get(1)?;
        if (content.chars().count() as u32) < min_length_chars {
            return Ok(true); // count toward total, but not flagged
        }
        let has_heading = content.lines().any(|l| l.trim_start().starts_with('#'));
        if !has_heading {
            push_flag(&mut flagged, id, &content);
        }
        Ok(true)
    })?;
    Ok(RawOutcome { total_considered: total, flagged_atoms: flagged })
}

fn eval_tag_cardinality(
    conn: &rusqlite::Connection,
    min: u32,
    max: u32,
    tag_filter: Option<&str>,
) -> Result<RawOutcome, AtomicCoreError> {
    let candidates: Vec<(String, String)> = load_candidates_id_content(conn, tag_filter)?;
    let total = candidates.len() as i32;
    if candidates.is_empty() {
        return Ok(RawOutcome { total_considered: 0, flagged_atoms: Vec::new() });
    }
    let ids: Vec<&str> = candidates.iter().map(|(id, _)| id.as_str()).collect();
    let placeholders: String = std::iter::repeat("?").take(ids.len()).collect::<Vec<_>>().join(",");
    let sql = format!("SELECT atom_id, COUNT(*) FROM atom_tags WHERE atom_id IN ({placeholders}) GROUP BY atom_id");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(ids.iter().copied()), |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as u32))
    })?;
    let mut count_by: HashMap<String, u32> = HashMap::new();
    for row in rows {
        let (id, n) = row?;
        count_by.insert(id, n);
    }
    let mut flagged = Vec::new();
    for (id, content) in candidates {
        let n = count_by.get(&id).copied().unwrap_or(0);
        let too_few = min > 0 && n < min;
        let too_many = max > 0 && n > max;
        if too_few || too_many {
            push_flag(&mut flagged, id, &content);
        }
    }
    Ok(RawOutcome { total_considered: total, flagged_atoms: flagged })
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

    // ---- Tier 1 ----

    fn check_with(rule: CustomRule) -> CustomCheck {
        CustomCheck {
            id: "c1".into(),
            label: "l".into(),
            description: String::new(),
            enabled: true,
            weight: 0.0,
            rule,
        }
    }

    #[test]
    fn require_tag_flags_untagged_atoms() {
        let (storage, _tmp) = make_storage();
        {
            let conn = storage.db.conn.lock().unwrap();
            insert_atom(&conn, "a1", "tagged", None);
            insert_atom(&conn, "a2", "bare", None);
            insert_tag(&conn, "t_topic", "topic");
            link(&conn, "a1", "t_topic");
        }
        let check = check_with(CustomRule::RequireTag {
            any_of: vec!["t_topic".into()],
            tag_filter: None,
        });
        let out = run_all(&storage, &[check]).unwrap();
        let (_, r, _) = &out[0];
        assert_eq!(r.data["flagged_count"], 1);
        assert_eq!(r.data["flagged"][0]["id"], "a2");
    }

    #[test]
    fn content_length_flags_too_short_and_too_long() {
        let (storage, _tmp) = make_storage();
        {
            let conn = storage.db.conn.lock().unwrap();
            insert_atom(&conn, "a1", "one two three four five six", None);  // 6 words, OK
            insert_atom(&conn, "a2", "tiny", None);                         // 1 word
            insert_atom(&conn, "a3", &"w ".repeat(50), None);               // 50 words
        }
        let check = check_with(CustomRule::ContentLength {
            min_words: 5,
            max_words: 30,
            tag_filter: None,
        });
        let out = run_all(&storage, &[check]).unwrap();
        let (_, r, _) = &out[0];
        assert_eq!(r.data["flagged_count"], 2);
        let flagged: Vec<&str> = r.data["flagged"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["id"].as_str().unwrap())
            .collect();
        assert!(flagged.contains(&"a2"));
        assert!(flagged.contains(&"a3"));
    }

    #[test]
    fn citation_count_flags_atoms_with_too_few_links() {
        let (storage, _tmp) = make_storage();
        {
            let conn = storage.db.conn.lock().unwrap();
            insert_atom(&conn, "a1", "one [link](http://a) and [[wiki]]", None);
            insert_atom(&conn, "a2", "no citations here at all", None);
            insert_atom(&conn, "a3", "only [one](http://x) here", None);
        }
        let check = check_with(CustomRule::CitationCount {
            min_citations: 2,
            tag_filter: None,
        });
        let out = run_all(&storage, &[check]).unwrap();
        let (_, r, _) = &out[0];
        assert_eq!(r.data["flagged_count"], 2);
        let flagged: Vec<&str> = r.data["flagged"]
            .as_array().unwrap().iter().map(|v| v["id"].as_str().unwrap()).collect();
        assert!(flagged.contains(&"a2"));
        assert!(flagged.contains(&"a3"));
    }

    #[test]
    fn source_domain_allowlist_flags_off_list_domains() {
        let (storage, _tmp) = make_storage();
        {
            let conn = storage.db.conn.lock().unwrap();
            insert_atom(&conn, "a1", "paper", Some("https://arxiv.org/abs/1"));
            insert_atom(&conn, "a2", "blog", Some("https://random.example/post"));
            insert_atom(&conn, "a3", "no source skipped", None);
        }
        let check = check_with(CustomRule::SourceDomainMatches {
            domains: vec!["arxiv.org".into()],
            mode: DomainMatchMode::Allowlist,
            tag_filter: None,
        });
        let out = run_all(&storage, &[check]).unwrap();
        let (_, r, _) = &out[0];
        // a3 is skipped (no source); a1 on allowlist; a2 off.
        assert_eq!(r.data["total_considered"], 2);
        assert_eq!(r.data["flagged_count"], 1);
        assert_eq!(r.data["flagged"][0]["id"], "a2");
    }

    #[test]
    fn source_domain_blocklist_flags_on_list_domains() {
        let (storage, _tmp) = make_storage();
        {
            let conn = storage.db.conn.lock().unwrap();
            insert_atom(&conn, "a1", "reddit", Some("https://old.reddit.com/r/x"));
            insert_atom(&conn, "a2", "arxiv", Some("https://arxiv.org/abs/1"));
        }
        let check = check_with(CustomRule::SourceDomainMatches {
            domains: vec!["reddit.com".into()],
            mode: DomainMatchMode::Blocklist,
            tag_filter: None,
        });
        let out = run_all(&storage, &[check]).unwrap();
        let (_, r, _) = &out[0];
        assert_eq!(r.data["flagged_count"], 1);
        assert_eq!(r.data["flagged"][0]["id"], "a1");
    }

    #[test]
    fn stale_atom_flags_old_tagged_atoms() {
        let (storage, _tmp) = make_storage();
        {
            let conn = storage.db.conn.lock().unwrap();
            insert_tag(&conn, "t_draft", "draft");
            // Old: 30 days ago
            let old = (chrono::Utc::now() - chrono::Duration::days(30)).to_rfc3339();
            conn.execute(
                "INSERT INTO atoms (id, content, source_url, embedding_status, tagging_status, created_at, updated_at) \
                 VALUES ('a1', 'stale', NULL, 'complete', 'complete', ?1, ?1)",
                params![old],
            ).unwrap();
            // Fresh: now
            insert_atom(&conn, "a2", "fresh", None);
            link(&conn, "a1", "t_draft");
            link(&conn, "a2", "t_draft");
        }
        let check = check_with(CustomRule::StaleAtom {
            tag: "t_draft".into(),
            max_age_days: 14,
        });
        let out = run_all(&storage, &[check]).unwrap();
        let (_, r, _) = &out[0];
        assert_eq!(r.data["total_considered"], 2);
        assert_eq!(r.data["flagged_count"], 1);
        assert_eq!(r.data["flagged"][0]["id"], "a1");
    }

    // ---- Tier 2 ----

    #[test]
    fn forbidden_combo_flags_atoms_carrying_all_forbidden_tags() {
        let (storage, _tmp) = make_storage();
        {
            let conn = storage.db.conn.lock().unwrap();
            insert_atom(&conn, "a1", "both", None);
            insert_atom(&conn, "a2", "only draft", None);
            insert_tag(&conn, "t_draft", "draft");
            insert_tag(&conn, "t_published", "published");
            link(&conn, "a1", "t_draft");
            link(&conn, "a1", "t_published");
            link(&conn, "a2", "t_draft");
        }
        let check = check_with(CustomRule::ForbiddenTagCombo {
            all_of: vec!["t_draft".into(), "t_published".into()],
        });
        let out = run_all(&storage, &[check]).unwrap();
        let (_, r, _) = &out[0];
        assert_eq!(r.data["flagged_count"], 1);
        assert_eq!(r.data["flagged"][0]["id"], "a1");
    }

    #[test]
    fn missing_heading_flags_long_atoms_without_heading() {
        let (storage, _tmp) = make_storage();
        let long = "x".repeat(200);
        let with_h = format!("# Title\n{}", "y".repeat(200));
        {
            let conn = storage.db.conn.lock().unwrap();
            insert_atom(&conn, "short", "too short to flag", None);
            insert_atom(&conn, "no_h", &long, None);
            insert_atom(&conn, "has_h", &with_h, None);
        }
        let check = check_with(CustomRule::MissingHeading {
            min_length_chars: 120,
            tag_filter: None,
        });
        let out = run_all(&storage, &[check]).unwrap();
        let (_, r, _) = &out[0];
        assert_eq!(r.data["flagged_count"], 1);
        assert_eq!(r.data["flagged"][0]["id"], "no_h");
    }

    #[test]
    fn tag_cardinality_flags_over_and_under_tagged() {
        let (storage, _tmp) = make_storage();
        {
            let conn = storage.db.conn.lock().unwrap();
            insert_atom(&conn, "a0", "no tags", None);
            insert_atom(&conn, "a1", "one tag", None);
            insert_atom(&conn, "a2", "two tags", None);
            insert_atom(&conn, "a5", "five tags", None);
            for i in 0..5 {
                insert_tag(&conn, &format!("t{i}"), &format!("t{i}"));
            }
            link(&conn, "a1", "t0");
            link(&conn, "a2", "t0");
            link(&conn, "a2", "t1");
            for i in 0..5 {
                link(&conn, "a5", &format!("t{i}"));
            }
        }
        let check = check_with(CustomRule::TagCardinality {
            min: 1,
            max: 3,
            tag_filter: None,
        });
        let out = run_all(&storage, &[check]).unwrap();
        let (_, r, _) = &out[0];
        let flagged: Vec<&str> = r.data["flagged"]
            .as_array().unwrap().iter().map(|v| v["id"].as_str().unwrap()).collect();
        assert!(flagged.contains(&"a0"));  // under min
        assert!(flagged.contains(&"a5"));  // over max
        assert!(!flagged.contains(&"a1"));
        assert!(!flagged.contains(&"a2"));
    }
}
