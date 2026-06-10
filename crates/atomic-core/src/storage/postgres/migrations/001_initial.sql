-- Atomic Postgres schema — mirrors SQLite schema in db.rs
-- Requires: PostgreSQL 16+ with pgvector extension

CREATE EXTENSION IF NOT EXISTS vector;

-- ==================== Core Tables ====================

CREATE TABLE IF NOT EXISTS atoms (
    id TEXT PRIMARY KEY,
    content TEXT NOT NULL DEFAULT '',
    title TEXT NOT NULL DEFAULT '',
    snippet TEXT NOT NULL DEFAULT '',
    source_url TEXT,
    source TEXT,
    published_at TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    embedding_status TEXT NOT NULL DEFAULT 'pending',
    tagging_status TEXT NOT NULL DEFAULT 'pending',
    edges_status TEXT NOT NULL DEFAULT 'pending'
);

CREATE TABLE IF NOT EXISTS tags (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    parent_id TEXT REFERENCES tags(id) ON DELETE SET NULL,
    created_at TEXT NOT NULL,
    atom_count INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS atom_tags (
    atom_id TEXT NOT NULL REFERENCES atoms(id) ON DELETE CASCADE,
    tag_id TEXT NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
    PRIMARY KEY (atom_id, tag_id)
);

-- Chunks + embeddings unified in one table (SQLite uses separate atom_chunks + vec_chunks)
CREATE TABLE IF NOT EXISTS atom_chunks (
    id TEXT PRIMARY KEY,
    atom_id TEXT NOT NULL REFERENCES atoms(id) ON DELETE CASCADE,
    chunk_index INTEGER NOT NULL,
    content TEXT NOT NULL,
    embedding vector,
    token_count INTEGER DEFAULT 0
);

CREATE TABLE IF NOT EXISTS atom_positions (
    atom_id TEXT PRIMARY KEY REFERENCES atoms(id) ON DELETE CASCADE,
    -- DOUBLE PRECISION (not REAL): the Rust model is `f64`, and sqlx's strict
    -- decoding rejects REAL → f64. Fresh installs land at the post-020
    -- column type; existing databases migrate via 020.
    x DOUBLE PRECISION NOT NULL,
    y DOUBLE PRECISION NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS semantic_edges (
    id TEXT PRIMARY KEY,
    source_atom_id TEXT NOT NULL REFERENCES atoms(id) ON DELETE CASCADE,
    target_atom_id TEXT NOT NULL REFERENCES atoms(id) ON DELETE CASCADE,
    similarity_score REAL NOT NULL,
    source_chunk_index INTEGER,
    target_chunk_index INTEGER,
    created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS atom_clusters (
    atom_id TEXT NOT NULL PRIMARY KEY,
    cluster_id INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS tag_embeddings (
    tag_id TEXT PRIMARY KEY REFERENCES tags(id) ON DELETE CASCADE,
    embedding vector
);

-- ==================== Wiki ====================

CREATE TABLE IF NOT EXISTS wiki_articles (
    id TEXT PRIMARY KEY,
    tag_id TEXT NOT NULL UNIQUE,
    content TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    atom_count INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS wiki_citations (
    id TEXT PRIMARY KEY,
    wiki_article_id TEXT NOT NULL REFERENCES wiki_articles(id) ON DELETE CASCADE,
    citation_index INTEGER NOT NULL,
    atom_id TEXT NOT NULL,
    chunk_index INTEGER,
    excerpt TEXT NOT NULL DEFAULT ''
);

CREATE TABLE IF NOT EXISTS wiki_links (
    id TEXT PRIMARY KEY,
    source_article_id TEXT NOT NULL REFERENCES wiki_articles(id) ON DELETE CASCADE,
    target_tag_id TEXT NOT NULL,
    link_text TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS wiki_article_versions (
    id TEXT PRIMARY KEY,
    tag_id TEXT NOT NULL,
    content TEXT NOT NULL,
    atom_count INTEGER NOT NULL DEFAULT 0,
    version_number INTEGER NOT NULL,
    created_at TEXT NOT NULL
);

-- ==================== Chat ====================

CREATE TABLE IF NOT EXISTS conversations (
    id TEXT PRIMARY KEY,
    title TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    is_archived INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS conversation_tags (
    conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    tag_id TEXT NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
    PRIMARY KEY (conversation_id, tag_id)
);

CREATE TABLE IF NOT EXISTS chat_messages (
    id TEXT PRIMARY KEY,
    conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    role TEXT NOT NULL,
    content TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL,
    message_index INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS chat_tool_calls (
    id TEXT PRIMARY KEY,
    message_id TEXT NOT NULL REFERENCES chat_messages(id) ON DELETE CASCADE,
    tool_name TEXT NOT NULL,
    tool_input TEXT NOT NULL DEFAULT '{}',
    tool_result TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS chat_citations (
    id TEXT PRIMARY KEY,
    message_id TEXT NOT NULL REFERENCES chat_messages(id) ON DELETE CASCADE,
    atom_id TEXT NOT NULL,
    chunk_index INTEGER,
    excerpt TEXT NOT NULL DEFAULT '',
    relevance_score REAL
);

-- ==================== Feeds ====================

CREATE TABLE IF NOT EXISTS feeds (
    id TEXT PRIMARY KEY,
    url TEXT NOT NULL UNIQUE,
    title TEXT,
    site_url TEXT,
    poll_interval INTEGER NOT NULL DEFAULT 3600,
    last_polled_at TEXT,
    last_error TEXT,
    created_at TEXT NOT NULL,
    is_paused INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS feed_tags (
    feed_id TEXT NOT NULL REFERENCES feeds(id) ON DELETE CASCADE,
    tag_id TEXT NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
    PRIMARY KEY (feed_id, tag_id)
);

CREATE TABLE IF NOT EXISTS feed_items (
    feed_id TEXT NOT NULL REFERENCES feeds(id) ON DELETE CASCADE,
    guid TEXT NOT NULL,
    atom_id TEXT,
    seen_at TEXT NOT NULL,
    skipped INTEGER NOT NULL DEFAULT 0,
    skip_reason TEXT,
    PRIMARY KEY (feed_id, guid)
);

-- ==================== Settings ====================

CREATE TABLE IF NOT EXISTS settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- ==================== Databases ====================

CREATE TABLE IF NOT EXISTS databases (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    is_default INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    last_opened_at TEXT
);

-- ==================== API Tokens ====================

CREATE TABLE IF NOT EXISTS api_tokens (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    token_hash TEXT NOT NULL,
    token_prefix TEXT NOT NULL,
    created_at TEXT NOT NULL,
    last_used_at TEXT,
    is_revoked INTEGER DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_api_tokens_hash ON api_tokens(token_hash);

-- ==================== Full-Text Search ====================

-- tsvector generated column for FTS on chunk content
ALTER TABLE atom_chunks ADD COLUMN IF NOT EXISTS content_tsv tsvector
    GENERATED ALWAYS AS (to_tsvector('english', content)) STORED;

CREATE INDEX IF NOT EXISTS idx_atom_chunks_fts ON atom_chunks USING GIN(content_tsv);

-- ==================== Indexes ====================

-- Atoms
CREATE INDEX IF NOT EXISTS idx_atoms_updated_id ON atoms(updated_at DESC, id DESC);
CREATE INDEX IF NOT EXISTS idx_atoms_created_id ON atoms(created_at DESC, id DESC);
CREATE INDEX IF NOT EXISTS idx_atoms_source_url ON atoms(source_url) WHERE source_url IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_atoms_source ON atoms(source) WHERE source IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_atoms_embedding_status ON atoms(embedding_status);
CREATE INDEX IF NOT EXISTS idx_atoms_tagging_status ON atoms(tagging_status);
CREATE INDEX IF NOT EXISTS idx_atoms_edges_status ON atoms(edges_status);

-- Atom-tag links
CREATE INDEX IF NOT EXISTS idx_atom_tags_tag_atom ON atom_tags(tag_id, atom_id);

-- Chunks
CREATE INDEX IF NOT EXISTS idx_atom_chunks_atom_id ON atom_chunks(atom_id);

-- Semantic edges
CREATE INDEX IF NOT EXISTS idx_semantic_edges_source ON semantic_edges(source_atom_id);
CREATE INDEX IF NOT EXISTS idx_semantic_edges_target ON semantic_edges(target_atom_id);
CREATE INDEX IF NOT EXISTS idx_semantic_edges_similarity ON semantic_edges(similarity_score DESC);

-- Tags
CREATE INDEX IF NOT EXISTS idx_tags_parent_id ON tags(parent_id);
CREATE INDEX IF NOT EXISTS idx_tags_parent_count ON tags(parent_id, atom_count DESC);
CREATE UNIQUE INDEX IF NOT EXISTS idx_tags_name_parent ON tags(LOWER(name), COALESCE(parent_id, ''));

-- Wiki
CREATE INDEX IF NOT EXISTS idx_wiki_citations_article ON wiki_citations(wiki_article_id);
CREATE INDEX IF NOT EXISTS idx_wiki_links_source ON wiki_links(source_article_id);
CREATE INDEX IF NOT EXISTS idx_wiki_links_target_tag ON wiki_links(target_tag_id);
CREATE INDEX IF NOT EXISTS idx_wiki_versions_tag ON wiki_article_versions(tag_id, version_number);

-- Chat
CREATE INDEX IF NOT EXISTS idx_conversations_updated ON conversations(updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_conversation_tags_conv ON conversation_tags(conversation_id);
CREATE INDEX IF NOT EXISTS idx_conversation_tags_tag ON conversation_tags(tag_id);
CREATE INDEX IF NOT EXISTS idx_chat_messages_conversation ON chat_messages(conversation_id, message_index);
CREATE INDEX IF NOT EXISTS idx_chat_tool_calls_message ON chat_tool_calls(message_id);
CREATE INDEX IF NOT EXISTS idx_chat_citations_message ON chat_citations(message_id);
CREATE INDEX IF NOT EXISTS idx_chat_citations_atom ON chat_citations(atom_id);

-- Feeds
CREATE INDEX IF NOT EXISTS idx_feeds_last_polled ON feeds(is_paused, last_polled_at);
CREATE INDEX IF NOT EXISTS idx_feed_items_feed ON feed_items(feed_id);

-- ==================== Triggers ====================

-- Maintain tags.atom_count on atom_tags insert/delete (mirrors SQLite triggers in db.rs)
CREATE OR REPLACE FUNCTION atom_tags_increment_count() RETURNS TRIGGER AS $$
BEGIN
    UPDATE tags SET atom_count = atom_count + 1 WHERE id = NEW.tag_id;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE OR REPLACE FUNCTION atom_tags_decrement_count() RETURNS TRIGGER AS $$
BEGIN
    UPDATE tags SET atom_count = atom_count - 1 WHERE id = OLD.tag_id;
    RETURN OLD;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS atom_tags_insert_count ON atom_tags;
CREATE TRIGGER atom_tags_insert_count
    AFTER INSERT ON atom_tags
    FOR EACH ROW EXECUTE FUNCTION atom_tags_increment_count();

DROP TRIGGER IF EXISTS atom_tags_delete_count ON atom_tags;
CREATE TRIGGER atom_tags_delete_count
    AFTER DELETE ON atom_tags
    FOR EACH ROW EXECUTE FUNCTION atom_tags_decrement_count();

-- ==================== Schema Version ====================

CREATE TABLE IF NOT EXISTS schema_version (
    version INTEGER NOT NULL,
    applied_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
INSERT INTO schema_version (version) VALUES (1);
