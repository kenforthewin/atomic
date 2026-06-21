-- Add search vectors for global keyword search beyond atom chunks.

ALTER TABLE wiki_articles
ADD COLUMN IF NOT EXISTS content_tsv tsvector
    GENERATED ALWAYS AS (to_tsvector('english', content)) STORED;

CREATE INDEX IF NOT EXISTS idx_wiki_articles_fts
    ON wiki_articles USING GIN(content_tsv);

ALTER TABLE chat_messages
ADD COLUMN IF NOT EXISTS content_tsv tsvector
    GENERATED ALWAYS AS (to_tsvector('english', content)) STORED;

CREATE INDEX IF NOT EXISTS idx_chat_messages_fts
    ON chat_messages USING GIN(content_tsv);

INSERT INTO schema_version (version) VALUES (8);
