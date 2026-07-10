//! Per-table copy plan for the SQLite → Postgres migration.
//!
//! Each [`TableSpec`] pairs a SQLite SELECT expression list (producing values
//! in destination column order) with the Postgres columns it feeds. All
//! transforms — status overrides, column renames, FK null-outs — live in the
//! SQLite SQL itself, so the Rust copier stays a uniform pump: read a batch
//! of rows by rowid keyset, bind them as parallel arrays, and
//! `INSERT ... SELECT FROM UNNEST(...)` with the destination `db_id` stamped
//! on every row.
//!
//! Guards (extra WHERE fragments) drop rows that would violate destination
//! foreign keys. SQLite only enforces foreign keys when the pragma is on, so
//! long-lived user databases can contain orphaned child rows; skipping them
//! beats failing the whole migration, and the per-table report surfaces the
//! difference between source and copied row counts.

use crate::error::AtomicCoreError;
use sqlx::PgPool;

/// How a column is read from SQLite and bound to Postgres.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Col {
    /// TEXT (nullable). Bound as `text[]`.
    Text,
    /// INTEGER (nullable). Bound as `int8[]`; Postgres applies assignment
    /// casts down to `int`/`smallint` destination columns.
    Int,
    /// REAL (nullable). Bound as `float8[]`; assignment-cast to `real`.
    Real,
    /// SQLite INTEGER 0/1 destined for a Postgres BOOLEAN column
    /// (only `tags.is_autotag_target` — everything else keeps 0/1 INTEGERs).
    Bool,
}

impl Col {
    fn pg_array_cast(self) -> &'static str {
        match self {
            Col::Text => "text[]",
            Col::Int => "int8[]",
            Col::Real => "float8[]",
            Col::Bool => "bool[]",
        }
    }
}

/// One batch of rows, held column-wise for array binding.
pub(super) enum ColumnData {
    Text(Vec<Option<String>>),
    Int(Vec<Option<i64>>),
    Real(Vec<Option<f64>>),
    Bool(Vec<Option<bool>>),
}

impl ColumnData {
    fn new(col: Col) -> Self {
        match col {
            Col::Text => ColumnData::Text(Vec::new()),
            Col::Int => ColumnData::Int(Vec::new()),
            Col::Real => ColumnData::Real(Vec::new()),
            Col::Bool => ColumnData::Bool(Vec::new()),
        }
    }

    fn push_from_row(&mut self, row: &rusqlite::Row<'_>, idx: usize) -> rusqlite::Result<()> {
        match self {
            ColumnData::Text(v) => v.push(row.get(idx)?),
            ColumnData::Int(v) => v.push(row.get(idx)?),
            ColumnData::Real(v) => v.push(row.get(idx)?),
            ColumnData::Bool(v) => v.push(row.get::<_, Option<i64>>(idx)?.map(|n| n != 0)),
        }
        Ok(())
    }
}

/// Declarative copy recipe for one destination table.
pub(super) struct TableSpec {
    /// Destination Postgres table (also the name used in events and reports).
    pub table: &'static str,
    /// Source SQLite table. Rowid keyset pagination runs against this, so it
    /// must be an ordinary rowid table (all Atomic tables are).
    pub source_table: &'static str,
    /// SELECT expressions in destination column order.
    pub select_exprs: &'static str,
    /// Extra WHERE fragment (ANDed with the keyset predicate). May reference
    /// `?3` — a JSON array of feed URLs to exclude — when `binds_skip_json`.
    pub guard: Option<&'static str>,
    /// Destination columns fed by `select_exprs`, in the same order.
    /// `db_id` is appended automatically by the insert builder.
    pub pg_cols: &'static [&'static str],
    /// Column types, same order as `pg_cols`.
    pub types: &'static [Col],
    /// Bind the skip-URL JSON array as `?3` in the read query.
    pub binds_skip_json: bool,
}

/// Copy order respects destination foreign keys: tags → atoms → atom
/// children → wiki → chat → feeds → reports.
///
/// Deliberately absent (regenerated or out of scope): chunk embeddings,
/// `vec_chunks`/`vec_tags`, `tag_embeddings`, `semantic_edges`,
/// `atom_clusters`, `atom_pipeline_jobs`, `task_runs`, `settings`
/// (instance-global on Postgres), `api_tokens`, and the OAuth tables.
pub(super) const TABLE_SPECS: &[TableSpec] = &[
    // Tags land with NULL parent_id; `link_tag_parents` wires the hierarchy
    // once every row exists, which avoids topological insert ordering.
    // atom_count starts at 0 because destination triggers rebuild it from
    // the atom_tags inserts below.
    TableSpec {
        table: "tags",
        source_table: "tags",
        select_exprs: "id, name, NULL, created_at, 0, COALESCE(is_autotag_target, 0), \
                       COALESCE(autotag_description, '')",
        guard: None,
        pg_cols: &[
            "id",
            "name",
            "parent_id",
            "created_at",
            "atom_count",
            "is_autotag_target",
            "autotag_description",
        ],
        types: &[
            Col::Text,
            Col::Text,
            Col::Text,
            Col::Text,
            Col::Int,
            Col::Bool,
            Col::Text,
        ],
        binds_skip_json: false,
    },
    // embedding_status / edges_status are forced to 'pending' so the
    // destination pipeline re-chunks, re-embeds, and rebuilds semantic edges
    // with its own provider. tagging_status is PRESERVED — the source's tag
    // assignments travel in atom_tags, and re-running auto-tagging over the
    // whole corpus would fight them.
    TableSpec {
        table: "atoms",
        source_table: "atoms",
        select_exprs: "id, content, COALESCE(title, ''), COALESCE(snippet, ''), source_url, \
                       source, published_at, created_at, updated_at, 'pending', \
                       COALESCE(tagging_status, 'pending'), 'pending', NULL, tagging_error, \
                       COALESCE(kind, 'captured')",
        guard: None,
        pg_cols: &[
            "id",
            "content",
            "title",
            "snippet",
            "source_url",
            "source",
            "published_at",
            "created_at",
            "updated_at",
            "embedding_status",
            "tagging_status",
            "edges_status",
            "embedding_error",
            "tagging_error",
            "kind",
        ],
        types: &[
            Col::Text,
            Col::Text,
            Col::Text,
            Col::Text,
            Col::Text,
            Col::Text,
            Col::Text,
            Col::Text,
            Col::Text,
            Col::Text,
            Col::Text,
            Col::Text,
            Col::Text,
            Col::Text,
            Col::Text,
        ],
        binds_skip_json: false,
    },
    TableSpec {
        table: "atom_tags",
        source_table: "atom_tags",
        select_exprs: "atom_id, tag_id, COALESCE(source, 'auto')",
        guard: Some("atom_id IN (SELECT id FROM atoms) AND tag_id IN (SELECT id FROM tags)"),
        pg_cols: &["atom_id", "tag_id", "source"],
        types: &[Col::Text, Col::Text, Col::Text],
        binds_skip_json: false,
    },
    // Chunk TEXT is copied so destination keyword search (the generated
    // content_tsv column) works during the re-embedding backlog; the
    // embedding column is left NULL and every vector-reading query filters
    // `embedding IS NOT NULL`. The pipeline delete+reinserts chunk rows per
    // atom when it re-embeds, replacing these seeds.
    TableSpec {
        table: "atom_chunks",
        source_table: "atom_chunks",
        select_exprs: "id, atom_id, chunk_index, content, 0",
        guard: Some("atom_id IN (SELECT id FROM atoms)"),
        pg_cols: &["id", "atom_id", "chunk_index", "content", "token_count"],
        types: &[Col::Text, Col::Text, Col::Int, Col::Text, Col::Int],
        binds_skip_json: false,
    },
    TableSpec {
        table: "atom_positions",
        source_table: "atom_positions",
        select_exprs: "atom_id, x, y, updated_at",
        guard: Some("atom_id IN (SELECT id FROM atoms)"),
        pg_cols: &["atom_id", "x", "y", "updated_at"],
        types: &[Col::Text, Col::Real, Col::Real, Col::Text],
        binds_skip_json: false,
    },
    TableSpec {
        table: "atom_links",
        source_table: "atom_links",
        select_exprs: "id, source_atom_id, target_atom_id, raw_target, label, target_kind, \
                       status, start_offset, end_offset, created_at, updated_at",
        guard: None,
        pg_cols: &[
            "id",
            "source_atom_id",
            "target_atom_id",
            "raw_target",
            "label",
            "target_kind",
            "status",
            "start_offset",
            "end_offset",
            "created_at",
            "updated_at",
        ],
        types: &[
            Col::Text,
            Col::Text,
            Col::Text,
            Col::Text,
            Col::Text,
            Col::Text,
            Col::Text,
            Col::Int,
            Col::Int,
            Col::Text,
            Col::Text,
        ],
        binds_skip_json: false,
    },
    TableSpec {
        table: "wiki_articles",
        source_table: "wiki_articles",
        select_exprs: "id, tag_id, content, created_at, updated_at, COALESCE(atom_count, 0)",
        guard: Some("tag_id IN (SELECT id FROM tags)"),
        pg_cols: &[
            "id",
            "tag_id",
            "content",
            "created_at",
            "updated_at",
            "atom_count",
        ],
        types: &[
            Col::Text,
            Col::Text,
            Col::Text,
            Col::Text,
            Col::Text,
            Col::Int,
        ],
        binds_skip_json: false,
    },
    TableSpec {
        table: "wiki_citations",
        source_table: "wiki_citations",
        select_exprs: "id, wiki_article_id, citation_index, atom_id, chunk_index, \
                       COALESCE(excerpt, '')",
        guard: Some("wiki_article_id IN (SELECT id FROM wiki_articles)"),
        pg_cols: &[
            "id",
            "wiki_article_id",
            "citation_index",
            "atom_id",
            "chunk_index",
            "excerpt",
        ],
        types: &[
            Col::Text,
            Col::Text,
            Col::Int,
            Col::Text,
            Col::Int,
            Col::Text,
        ],
        binds_skip_json: false,
    },
    // SQLite stores the display name (target_tag_name); Postgres stores it
    // as link_text and resolves the live name from tags at read time.
    TableSpec {
        table: "wiki_links",
        source_table: "wiki_links",
        select_exprs: "id, source_article_id, target_tag_name, \
                       CASE WHEN target_tag_id IN (SELECT id FROM tags) \
                            THEN target_tag_id ELSE NULL END, \
                       created_at",
        guard: Some("source_article_id IN (SELECT id FROM wiki_articles)"),
        pg_cols: &[
            "id",
            "source_article_id",
            "link_text",
            "target_tag_id",
            "created_at",
        ],
        types: &[Col::Text, Col::Text, Col::Text, Col::Text, Col::Text],
        binds_skip_json: false,
    },
    // citations_json is dropped: the Postgres schema doesn't store citations
    // for historical versions, and its read path already returns them empty.
    TableSpec {
        table: "wiki_article_versions",
        source_table: "wiki_article_versions",
        select_exprs: "id, tag_id, content, COALESCE(atom_count, 0), version_number, created_at",
        guard: None,
        pg_cols: &[
            "id",
            "tag_id",
            "content",
            "atom_count",
            "version_number",
            "created_at",
        ],
        types: &[
            Col::Text,
            Col::Text,
            Col::Text,
            Col::Int,
            Col::Int,
            Col::Text,
        ],
        binds_skip_json: false,
    },
    TableSpec {
        table: "wiki_proposals",
        source_table: "wiki_proposals",
        select_exprs: "id, tag_id, base_article_id, base_updated_at, content, citations_json, \
                       ops_json, COALESCE(new_atom_count, 0), created_at",
        guard: None,
        pg_cols: &[
            "id",
            "tag_id",
            "base_article_id",
            "base_updated_at",
            "content",
            "citations_json",
            "ops_json",
            "new_atom_count",
            "created_at",
        ],
        types: &[
            Col::Text,
            Col::Text,
            Col::Text,
            Col::Text,
            Col::Text,
            Col::Text,
            Col::Text,
            Col::Int,
            Col::Text,
        ],
        binds_skip_json: false,
    },
    TableSpec {
        table: "conversations",
        source_table: "conversations",
        select_exprs: "id, title, created_at, updated_at, COALESCE(is_archived, 0)",
        guard: None,
        pg_cols: &["id", "title", "created_at", "updated_at", "is_archived"],
        types: &[Col::Text, Col::Text, Col::Text, Col::Text, Col::Int],
        binds_skip_json: false,
    },
    TableSpec {
        table: "conversation_tags",
        source_table: "conversation_tags",
        select_exprs: "conversation_id, tag_id",
        guard: Some(
            "conversation_id IN (SELECT id FROM conversations) \
             AND tag_id IN (SELECT id FROM tags)",
        ),
        pg_cols: &["conversation_id", "tag_id"],
        types: &[Col::Text, Col::Text],
        binds_skip_json: false,
    },
    TableSpec {
        table: "chat_messages",
        source_table: "chat_messages",
        select_exprs: "id, conversation_id, role, COALESCE(content, ''), created_at, \
                       message_index",
        guard: Some("conversation_id IN (SELECT id FROM conversations)"),
        pg_cols: &[
            "id",
            "conversation_id",
            "role",
            "content",
            "created_at",
            "message_index",
        ],
        types: &[
            Col::Text,
            Col::Text,
            Col::Text,
            Col::Text,
            Col::Text,
            Col::Int,
        ],
        binds_skip_json: false,
    },
    // SQLite's tool_output/status/completed_at collapse into Postgres's
    // tool_result — the read path synthesizes status 'complete' for history.
    TableSpec {
        table: "chat_tool_calls",
        source_table: "chat_tool_calls",
        select_exprs: "id, message_id, tool_name, COALESCE(tool_input, '{}'), \
                       COALESCE(tool_output, ''), created_at",
        guard: Some("message_id IN (SELECT id FROM chat_messages)"),
        pg_cols: &[
            "id",
            "message_id",
            "tool_name",
            "tool_input",
            "tool_result",
            "created_at",
        ],
        types: &[
            Col::Text,
            Col::Text,
            Col::Text,
            Col::Text,
            Col::Text,
            Col::Text,
        ],
        binds_skip_json: false,
    },
    // Postgres has no citation_index column; its read path numbers citations
    // in row order, so rows are inserted in (message_id, citation_index)
    // order — rowid order in SQLite already matches insertion order.
    TableSpec {
        table: "chat_citations",
        source_table: "chat_citations",
        select_exprs: "id, message_id, atom_id, chunk_index, COALESCE(excerpt, ''), \
                       relevance_score",
        guard: Some("message_id IN (SELECT id FROM chat_messages)"),
        pg_cols: &[
            "id",
            "message_id",
            "atom_id",
            "chunk_index",
            "excerpt",
            "relevance_score",
        ],
        types: &[
            Col::Text,
            Col::Text,
            Col::Text,
            Col::Int,
            Col::Text,
            Col::Real,
        ],
        binds_skip_json: false,
    },
    // feeds.url is UNIQUE across the whole Postgres instance (not per-db_id),
    // so feeds whose URL already exists under another database are skipped —
    // together with their feed_tags/feed_items below — and reported.
    TableSpec {
        table: "feeds",
        source_table: "feeds",
        select_exprs: "id, url, title, site_url, COALESCE(poll_interval, 3600), \
                       last_polled_at, last_error, created_at, COALESCE(is_paused, 0)",
        guard: Some("url NOT IN (SELECT value FROM json_each(?3))"),
        pg_cols: &[
            "id",
            "url",
            "title",
            "site_url",
            "poll_interval",
            "last_polled_at",
            "last_error",
            "created_at",
            "is_paused",
        ],
        types: &[
            Col::Text,
            Col::Text,
            Col::Text,
            Col::Text,
            Col::Int,
            Col::Text,
            Col::Text,
            Col::Text,
            Col::Int,
        ],
        binds_skip_json: true,
    },
    TableSpec {
        table: "feed_tags",
        source_table: "feed_tags",
        select_exprs: "feed_id, tag_id",
        guard: Some(
            "feed_id IN (SELECT id FROM feeds \
                         WHERE url NOT IN (SELECT value FROM json_each(?3))) \
             AND tag_id IN (SELECT id FROM tags)",
        ),
        pg_cols: &["feed_id", "tag_id"],
        types: &[Col::Text, Col::Text],
        binds_skip_json: true,
    },
    // The feed-item ledger prevents the destination from re-importing every
    // item on its first poll. atom_id is nulled when the atom is gone (the
    // destination FK is ON DELETE SET NULL; the source may have had foreign
    // keys off).
    TableSpec {
        table: "feed_items",
        source_table: "feed_items",
        select_exprs: "feed_id, guid, \
                       CASE WHEN atom_id IN (SELECT id FROM atoms) \
                            THEN atom_id ELSE NULL END, \
                       seen_at, COALESCE(skipped, 0), skip_reason",
        guard: Some(
            "feed_id IN (SELECT id FROM feeds \
                         WHERE url NOT IN (SELECT value FROM json_each(?3)))",
        ),
        pg_cols: &[
            "feed_id",
            "guid",
            "atom_id",
            "seen_at",
            "skipped",
            "skip_reason",
        ],
        types: &[
            Col::Text,
            Col::Text,
            Col::Text,
            Col::Text,
            Col::Int,
            Col::Text,
        ],
        binds_skip_json: true,
    },
    TableSpec {
        table: "reports",
        source_table: "reports",
        select_exprs: "id, name, description, research_prompt, source_scope_tag_ids, \
                       source_scope_window, source_include_kinds, context_scope_mode, \
                       context_scope_tag_ids, context_scope_window, context_include_kinds, \
                       citation_policy, max_source_atoms, max_source_tokens, \
                       max_tool_iterations, schedule, schedule_tz, COALESCE(enabled, 1), \
                       output_atom_tags, last_run_at, last_finding_atom_id, last_error, \
                       created_at, updated_at",
        guard: None,
        pg_cols: &[
            "id",
            "name",
            "description",
            "research_prompt",
            "source_scope_tag_ids",
            "source_scope_window",
            "source_include_kinds",
            "context_scope_mode",
            "context_scope_tag_ids",
            "context_scope_window",
            "context_include_kinds",
            "citation_policy",
            "max_source_atoms",
            "max_source_tokens",
            "max_tool_iterations",
            "schedule",
            "schedule_tz",
            "enabled",
            "output_atom_tags",
            "last_run_at",
            "last_finding_atom_id",
            "last_error",
            "created_at",
            "updated_at",
        ],
        types: &[
            Col::Text,
            Col::Text,
            Col::Text,
            Col::Text,
            Col::Text,
            Col::Text,
            Col::Text,
            Col::Text,
            Col::Text,
            Col::Text,
            Col::Text,
            Col::Text,
            Col::Int,
            Col::Int,
            Col::Int,
            Col::Text,
            Col::Text,
            Col::Int,
            Col::Text,
            Col::Text,
            Col::Text,
            Col::Text,
            Col::Text,
            Col::Text,
        ],
        binds_skip_json: false,
    },
    TableSpec {
        table: "report_findings",
        source_table: "report_findings",
        select_exprs: "finding_atom_id, \
                       CASE WHEN report_id IN (SELECT id FROM reports) \
                            THEN report_id ELSE NULL END, \
                       run_id, report_name_snapshot, created_at",
        guard: Some("finding_atom_id IN (SELECT id FROM atoms)"),
        pg_cols: &[
            "finding_atom_id",
            "report_id",
            "run_id",
            "report_name_snapshot",
            "created_at",
        ],
        types: &[Col::Text, Col::Text, Col::Text, Col::Text, Col::Text],
        binds_skip_json: false,
    },
    TableSpec {
        table: "report_finding_citations",
        source_table: "report_finding_citations",
        select_exprs: "finding_atom_id, cited_atom_id, position, COALESCE(excerpt, '')",
        guard: Some(
            "finding_atom_id IN (SELECT id FROM atoms) \
             AND cited_atom_id IN (SELECT id FROM atoms)",
        ),
        pg_cols: &["finding_atom_id", "cited_atom_id", "position", "excerpt"],
        types: &[Col::Text, Col::Text, Col::Int, Col::Text],
        binds_skip_json: false,
    },
];

/// Read one keyset batch from SQLite. Returns the column arrays, the highest
/// rowid seen (the next keyset cursor), and the number of rows read.
pub(super) fn read_batch(
    conn: &rusqlite::Connection,
    spec: &TableSpec,
    skip_json: &str,
    last_rowid: i64,
    limit: i64,
) -> rusqlite::Result<(Vec<ColumnData>, i64, usize)> {
    let guard = spec
        .guard
        .map(|g| format!(" AND ({g})"))
        .unwrap_or_default();
    let sql = format!(
        "SELECT rowid, {} FROM {} WHERE rowid > ?1{} ORDER BY rowid LIMIT ?2",
        spec.select_exprs, spec.source_table, guard
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut cols: Vec<ColumnData> = spec.types.iter().map(|t| ColumnData::new(*t)).collect();
    let mut max_rowid = last_rowid;
    let mut rows_read = 0usize;

    let mut rows = if spec.binds_skip_json {
        stmt.query(rusqlite::params![last_rowid, limit, skip_json])?
    } else {
        stmt.query(rusqlite::params![last_rowid, limit])?
    };
    while let Some(row) = rows.next()? {
        max_rowid = row.get(0)?;
        for (i, col) in cols.iter_mut().enumerate() {
            col.push_from_row(row, i + 1)?;
        }
        rows_read += 1;
    }
    Ok((cols, max_rowid, rows_read))
}

/// Insert one batch into Postgres as parallel arrays:
/// `INSERT INTO t (c1..cn, db_id) SELECT u.c1..u.cn, $n+1 FROM UNNEST(...)`.
pub(super) async fn insert_batch(
    pool: &PgPool,
    db_id: &str,
    spec: &TableSpec,
    cols: Vec<ColumnData>,
) -> Result<(), AtomicCoreError> {
    let col_list = spec.pg_cols.join(", ");
    let unnest_args: Vec<String> = spec
        .types
        .iter()
        .enumerate()
        .map(|(i, t)| format!("${}::{}", i + 1, t.pg_array_cast()))
        .collect();
    let aliases: Vec<String> = (1..=spec.types.len()).map(|i| format!("c{i}")).collect();
    let selects: Vec<String> = aliases.iter().map(|a| format!("u.{a}")).collect();
    let sql = format!(
        "INSERT INTO {} ({col_list}, db_id) SELECT {}, ${} FROM UNNEST({}) AS u({})",
        spec.table,
        selects.join(", "),
        spec.types.len() + 1,
        unnest_args.join(", "),
        aliases.join(", ")
    );

    let mut query = sqlx::query(&sql);
    for col in cols {
        query = match col {
            ColumnData::Text(v) => query.bind(v),
            ColumnData::Int(v) => query.bind(v),
            ColumnData::Real(v) => query.bind(v),
            ColumnData::Bool(v) => query.bind(v),
        };
    }
    query.bind(db_id).execute(pool).await.map_err(|e| {
        AtomicCoreError::DatabaseOperation(format!(
            "migration insert into {} failed: {e}",
            spec.table
        ))
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specs_are_internally_consistent() {
        let mut seen = std::collections::HashSet::new();
        for spec in TABLE_SPECS {
            assert_eq!(
                spec.pg_cols.len(),
                spec.types.len(),
                "column/type arity mismatch for {}",
                spec.table
            );
            assert!(!spec.pg_cols.is_empty(), "{} has no columns", spec.table);
            assert!(seen.insert(spec.table), "duplicate spec for {}", spec.table);
            if let Some(guard) = spec.guard {
                assert_eq!(
                    guard.contains("?3"),
                    spec.binds_skip_json,
                    "{}: guard param and binds_skip_json disagree",
                    spec.table
                );
            } else {
                assert!(
                    !spec.binds_skip_json,
                    "{}: ?3 bound but never used",
                    spec.table
                );
            }
        }
    }
}
