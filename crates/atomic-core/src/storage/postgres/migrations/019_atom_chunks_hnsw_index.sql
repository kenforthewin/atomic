-- pgvector HNSW index on atom_chunks.embedding for ANN similarity search.
--
-- HNSW requires a fixed-dimension vector column, but Atomic's embedding
-- dimension is provider-configurable (see ProviderConfig::embedding_dimension
-- in providers/mod.rs). 001_initial leaves the column dimensionless so the
-- runtime can pin it to whatever the configured provider needs.
--
-- This migration creates the index only when the column already has a fixed
-- dimension — i.e. installs that previously hit recreate_vector_index. Fresh
-- installs land here with a dimensionless column; AtomicCore::open_postgres
-- runs the same CREATE INDEX immediately after pinning the column type, so
-- both paths converge on the same end state.
--
-- vector_cosine_ops matches the `<=>` (cosine distance) operator used in
-- postgres/search.rs and postgres/chunks.rs. HNSW is the right default for
-- low-latency point queries at our expected scale (10K–1M chunks per tenant).

DO $$
BEGIN
    IF EXISTS (
        SELECT 1 FROM pg_attribute
        WHERE attrelid = 'atom_chunks'::regclass
          AND attname = 'embedding'
          AND atttypmod > 0
    ) THEN
        CREATE INDEX IF NOT EXISTS atom_chunks_embedding_hnsw_idx
            ON atom_chunks USING hnsw (embedding vector_cosine_ops);
    END IF;
END $$;

INSERT INTO schema_version (version) VALUES (19);
