//! Individual rule evaluators.
//!
//! Each `eval_*` function maps to exactly one `CustomRule` variant and returns
//! a `RawOutcome` (total_considered + flagged_atoms). The dispatcher lives in
//! `mod::evaluate`. Shared helpers are in [`super::helpers`].

use super::helpers::{
    for_each_atom, host_matches, host_of, load_candidates_id_content, preview, push_flag,
    word_count, count_citations, MAX_FLAGGED,
};
use super::types::{DomainMatchMode, FlaggedAtom, RawOutcome};
use crate::error::AtomicCoreError;
use rusqlite::params;
use std::collections::HashMap;

// ==================== Tier 0: core ====================

pub(super) fn eval_tag_requires(
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
    let placeholders_any: String = std::iter::repeat_n("?", any_of.len())
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
        let placeholders: String = std::iter::repeat_n("?", ids.len())
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

pub(super) fn eval_require_source(
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

pub(super) fn eval_content_regex(
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

pub(super) fn eval_require_tag(
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
    let placeholders: String = std::iter::repeat_n("?", ids.len()).collect::<Vec<_>>().join(",");
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

pub(super) fn eval_content_length(
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

pub(super) fn eval_citation_count(
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

pub(super) fn eval_source_domain(
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

pub(super) fn eval_stale_atom(
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

pub(super) fn eval_forbidden_combo(
    conn: &rusqlite::Connection,
    all_of: &[String],
) -> Result<RawOutcome, AtomicCoreError> {
    if all_of.len() < 2 {
        return Ok(RawOutcome { total_considered: 0, flagged_atoms: Vec::new() });
    }
    // Every atom is a candidate. Count atoms that carry every required tag.
    let placeholders: String = std::iter::repeat_n("?", all_of.len()).collect::<Vec<_>>().join(",");
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
        let placeholders: String = std::iter::repeat_n("?", flagged_ids.len()).collect::<Vec<_>>().join(",");
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

pub(super) fn eval_missing_heading(
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

pub(super) fn eval_tag_cardinality(
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
    let placeholders: String = std::iter::repeat_n("?", ids.len()).collect::<Vec<_>>().join(",");
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
