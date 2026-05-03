//! Shared helpers used by every rule evaluator.
//!
//! Kept separate so new rules can lift any of these without reaching into
//! sibling evaluators. Nothing here is public outside `health::custom`.

use super::types::FlaggedAtom;
use crate::error::AtomicCoreError;
use rusqlite::params;

/// Cap flagged-atom lists so a malformed rule can't blow up the report.
pub(super) const MAX_FLAGGED: usize = 500;

/// Cap on sample atoms returned in the preview. UI doesn't need more.
pub(super) const PREVIEW_SAMPLE: usize = 10;

/// Preview the first non-empty line of `content` for UI display.
pub(super) fn preview(content: &str) -> String {
    let trimmed = content
        .lines()
        .map(|l| l.trim())
        .find(|l| !l.is_empty())
        .unwrap_or("");
    const MAX: usize = 80;
    if trimmed.chars().count() > MAX {
        let truncated: String = trimmed.chars().take(MAX).collect();
        format!("{truncated}\u{2026}")
    } else {
        trimmed.to_string()
    }
}

/// Fast, allocation-free word count. Treats any run of ASCII whitespace as a
/// separator — matches `str::split_whitespace` but avoids allocating an
/// intermediate iterator collection.
pub(super) fn word_count(s: &str) -> u32 {
    s.split_whitespace().count() as u32
}

/// Count markdown links `[text](url)` plus wikilinks `[[...]]`.
pub(super) fn count_citations(content: &str) -> u32 {
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

/// Parse a URL's host. Accepts http/https/ftp/etc. Returns None for empty or
/// malformed URLs — those atoms are skipped (not flagged) since policing the
/// presence of a URL is `RequireSource`'s job.
pub(super) fn host_of(url: &str) -> Option<String> {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let host = rest.split('/').next()?.split('?').next()?.split('#').next()?;
    if host.is_empty() { None } else { Some(host.to_lowercase()) }
}

pub(super) fn host_matches(host: &str, domains: &[String]) -> bool {
    domains.iter().any(|d| {
        let d = d.trim().trim_start_matches("https://").trim_start_matches("http://").to_lowercase();
        if d.is_empty() { return false; }
        host == d || host.ends_with(&format!(".{d}"))
    })
}

/// Load candidate atoms. When `tag_filter` is `Some`, restricts to atoms
/// tagged with that tag id; otherwise all atoms. Standardizes the select-
/// and-iterate boilerplate every evaluator below shares.
pub(super) fn for_each_atom<F>(
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

pub(super) fn push_flag(flagged: &mut Vec<FlaggedAtom>, id: String, content: &str) {
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
pub(super) fn load_candidates_id_content(
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
