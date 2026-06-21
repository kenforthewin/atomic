-- V8: Store error reasons for failed embeddings/tagging
ALTER TABLE atoms ADD COLUMN IF NOT EXISTS embedding_error TEXT;
ALTER TABLE atoms ADD COLUMN IF NOT EXISTS tagging_error TEXT;

INSERT INTO schema_version (version) VALUES (3);
