-- Real Postgres+pgvector schema for RAG semantic search (CADS-devsystem#7).
-- One row per chunk (repo-synced file section, manual document, or
-- Unstructured-extracted upload) -- deliberately flat, no separate
-- documents/chunks join, since a chunk's own path/source_kind is enough to
-- group by in a query without a second table this codebase's real query
-- patterns don't need yet.
CREATE EXTENSION IF NOT EXISTS vector;

CREATE TABLE IF NOT EXISTS rag_chunks (
    id BIGSERIAL PRIMARY KEY,
    run_id TEXT NOT NULL,
    -- 'repo_sync' | 'manual_document' | 'unstructured_upload' -- mirrors the
    -- three real ingestion paths in web/src/rag.rs; kept as plain text, not a
    -- Postgres ENUM, so adding a fourth source later is a data migration, not
    -- a schema one.
    source_kind TEXT NOT NULL,
    path TEXT NOT NULL,
    chunk_index INT NOT NULL,
    text TEXT NOT NULL,
    -- text-embedding-3-small's real dimensionality (rag::EMBEDDING_MODEL) --
    -- nullable: a chunk synced with no RAG_EMBEDDING_API_KEY configured is a
    -- real, valid, keyword-only-searchable row, not an error.
    embedding vector(1536),
    created_at BIGINT NOT NULL
);

-- Every real read query in this codebase's search path scopes by run_id
-- first (each run's RAG index is genuinely isolated from every other run's --
-- see owner_authorized()'s per-run scoping elsewhere in this codebase).
CREATE INDEX IF NOT EXISTS rag_chunks_run_id_idx ON rag_chunks (run_id);

-- Real re-sync support: sync_rag replaces a run's repo-synced chunks
-- wholesale on every sync (matches the existing JSON-index behavior), so a
-- fast "delete all repo_sync rows for this run" needs run_id+source_kind
-- together, not just run_id alone.
CREATE INDEX IF NOT EXISTS rag_chunks_run_source_idx ON rag_chunks (run_id, source_kind);

-- Real approximate-nearest-neighbor index, cosine distance (matches
-- rag::cosine_similarity's own metric in the embedded-search fallback path,
-- so the two search strategies agree on what "closer" means). ivfflat over
-- hnsw for v1: cheaper to build/maintain at this deployment's real expected
-- scale (a handful of runs, each capped by MAX_FILES/CHUNK_CHARS -- see
-- rag.rs), and pgvector's own docs recommend ivfflat when the table is small
-- enough that hnsw's better recall-at-scale isn't worth its slower build
-- time. `lists = 100` is pgvector's own general-purpose default, not tuned
-- against real production data yet -- revisit once there's a real row count
-- to tune against.
CREATE INDEX IF NOT EXISTS rag_chunks_embedding_idx ON rag_chunks USING ivfflat (embedding vector_cosine_ops) WITH (lists = 100);
