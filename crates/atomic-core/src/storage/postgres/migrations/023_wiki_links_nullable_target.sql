-- Migration 023: wiki_links.target_tag_id must be nullable
--
-- A NULL target_tag_id is a *dangling* wiki link — the article still names
-- its target ([[link_text]]) but the tag it resolved to no longer exists.
-- SQLite has treated these as first-class since v1 (`target_tag_id TEXT
-- REFERENCES tags(id) ON DELETE SET NULL`), the model is `Option<String>`,
-- and the Postgres read path already LEFT JOINs + COALESCEs to render them.
-- Only the Postgres column disagreed, declared NOT NULL in 001 — which made
-- the save path silently drop dangling links, and broke SQLite → Postgres
-- migration outright (the copier deliberately NULLs unresolvable targets,
-- so any source database with a deleted-tag link failed mid-import).

ALTER TABLE wiki_links ALTER COLUMN target_tag_id DROP NOT NULL;

INSERT INTO schema_version (version) VALUES (23);
