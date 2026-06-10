//! Postgres helpers for integration tests.
//!
//! Behind the `postgres` feature so consumers that only run SQLite paths
//! don't pull sqlx into their build.

use sqlx::postgres::PgPoolOptions;

/// Wipe per-DB tables on a shared Postgres test instance so consecutive
/// test runs start clean without dropping/recreating the schema.
///
/// Keep this list in sync with any new per-DB tables. The column set
/// mirrors the truncate blocks that used to live inline in atomic-core's
/// and atomic-server's `tests/support/mod.rs`.
pub async fn truncate_postgres_for_test(url: &str) {
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(url)
        .await
        .expect("connect truncate pool");
    let _ = sqlx::raw_sql(
        "TRUNCATE atoms, tags, atom_tags, atom_chunks, atom_positions, atom_pipeline_jobs, \
         semantic_edges, atom_clusters, tag_embeddings, \
         wiki_articles, wiki_citations, wiki_links, wiki_article_versions, atom_links, \
         conversations, conversation_tags, chat_messages, chat_tool_calls, chat_citations, \
         feeds, feed_tags, feed_items, settings, \
         briefing_citations, briefings, oauth_codes, oauth_clients, api_tokens \
         CASCADE",
    )
    .execute(&pool)
    .await;
}
