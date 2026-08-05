//! Real Postgres+pgvector storage for RAG semantic search (CADS-devsystem#7),
//! operator-directed replacement for the embedded in-memory cosine-similarity
//! approach `rag.rs` shipped first -- see that module's own doc comment for
//! why embedded-only was the original call, and this module's own commit
//! history for why the operator overrode it after reviewing
//! `scimbe/cadserv`'s real SurfSense-v2 fork.
//!
//! `DATABASE_URL` at startup selects the real Postgres instance; this module
//! has no opinion on whether that's self-hosted on this deployment's host or
//! a real managed/external service -- that operational decision belongs to
//! whoever provisions the credential, matching the same pattern
//! `RAG_EMBEDDING_API_KEY`/`RAG_UNSTRUCTURED_API_KEY` already established for
//! this module's sibling external dependencies.

use pgvector::Vector;
use sqlx::PgPool;

pub async fn connect(database_url: &str) -> Result<PgPool, sqlx::Error> {
    PgPool::connect(database_url).await
}

/// Real schema migration, `sqlx::migrate!` embeds `migrations/*.sql` into the
/// binary at compile time -- no separate migration step to forget to run
/// against a fresh database, and no drift between what's on disk in this repo
/// and what a running deployment actually applied.
pub async fn run_migrations(pool: &PgPool) -> Result<(), sqlx::migrate::MigrateError> {
    sqlx::migrate!("./migrations").run(pool).await
}

/// One chunk ready to be stored -- `embedding: None` is real and valid (no
/// embedding credential configured for this deployment), not an error state;
/// [`semantic_search`] simply never returns a row with no embedding, the same
/// honest-degrade contract `rag::semantic_search` already established for the
/// embedded path.
#[derive(Debug, Clone)]
pub struct ChunkToStore {
    pub path: String,
    pub chunk_index: i32,
    pub text: String,
    pub embedding: Option<Vec<f32>>,
}

/// Real, honest replace: deletes every existing row for `(run_id,
/// source_kind)` inside the same transaction as the inserts, so a re-sync
/// (which always re-supplies the *complete* current chunk set for that
/// source) never leaves a stale chunk from a file that was since deleted
/// from the repo -- the same "sync_repo replaces wholesale" contract
/// `rag.rs`'s JSON-index path already has, now against real Postgres rows
/// instead of an in-memory `Vec`.
pub async fn replace_chunks(pool: &PgPool, run_id: &str, source_kind: &str, chunks: &[ChunkToStore], now: i64) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM rag_chunks WHERE run_id = $1 AND source_kind = $2").bind(run_id).bind(source_kind).execute(&mut *tx).await?;
    for chunk in chunks {
        let vector_embedding = chunk.embedding.clone().map(Vector::from);
        sqlx::query(
            "INSERT INTO rag_chunks (run_id, source_kind, path, chunk_index, text, embedding, created_at) VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(run_id)
        .bind(source_kind)
        .bind(&chunk.path)
        .bind(chunk.chunk_index)
        .bind(&chunk.text)
        .bind(vector_embedding)
        .bind(now)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await
}

/// Real per-document delete (mirrors `rag.rs`'s `remove_rag_document`), keyed
/// by `path` within `(run_id, source_kind)` since Postgres rows have no
/// separate document-id column the way the JSON index's `RagDocument.id`
/// does yet -- a real, deliberate v1 simplification: manual-document removal
/// still works, just addressed by its own path rather than a generated id.
pub async fn delete_by_path(pool: &PgPool, run_id: &str, source_kind: &str, path: &str) -> Result<u64, sqlx::Error> {
    let result = sqlx::query("DELETE FROM rag_chunks WHERE run_id = $1 AND source_kind = $2 AND path = $3")
        .bind(run_id)
        .bind(source_kind)
        .bind(path)
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SemanticHit {
    pub path: String,
    pub text: String,
    /// Real cosine similarity (`1 - cosine_distance`), same -1..=1 range and
    /// same metric `rag::cosine_similarity` computes for the embedded path --
    /// the two search strategies agree on what "closer" means, deliberately,
    /// so a caller merging both (as `rag::combined_search` does today for the
    /// embedded path) isn't comparing two different notions of "score."
    /// `f64`, not `f32` -- real, caught by running against a real Postgres,
    /// not assumed: `1 - (embedding <=> $1)` is `double precision` (Postgres
    /// promotes the subtraction), and sqlx's real column-decode is strict
    /// about the mismatch rather than silently narrowing it.
    pub score: f64,
}

/// Real pgvector ANN query via the `<=>` cosine-distance operator (the
/// `rag_chunks_embedding_idx` ivfflat index from the migration makes this a
/// real approximate-nearest-neighbor lookup, not a full scan, once the table
/// has enough rows for Postgres's planner to prefer the index). Only ever
/// returns rows with a real embedding (`embedding IS NOT NULL`) -- a
/// keyword-only chunk from a deployment with no embedding credential
/// configured is silently excluded here, never scored as "close" by
/// accident.
pub async fn semantic_search(pool: &PgPool, run_id: &str, query_embedding: &[f32], limit: i64) -> Result<Vec<SemanticHit>, sqlx::Error> {
    let query_vector = Vector::from(query_embedding.to_vec());
    sqlx::query_as::<_, SemanticHit>(
        "SELECT path, text, 1 - (embedding <=> $1) AS score \
         FROM rag_chunks \
         WHERE run_id = $2 AND embedding IS NOT NULL \
         ORDER BY embedding <=> $1 \
         LIMIT $3",
    )
    .bind(query_vector)
    .bind(run_id)
    .bind(limit)
    .fetch_all(pool)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real local Postgres+pgvector, not mocked -- `RAG_TEST_DATABASE_URL`
    /// must point at a real running instance with the `vector` extension
    /// installable (`CREATE EXTENSION IF NOT EXISTS vector` runs as part of
    /// the real migration). Tests that need it are `#[ignore]`d by default so
    /// `cargo test` still passes hermetically with no real Postgres
    /// available (matching this repo's actual CI, which doesn't run a
    /// Postgres service yet) -- run explicitly with `cargo test -- --ignored`
    /// once `RAG_TEST_DATABASE_URL` is set, e.g. against the real
    /// `pgvector/pgvector:pg16` container this was developed against.
    async fn test_pool() -> PgPool {
        let url = std::env::var("RAG_TEST_DATABASE_URL").expect("RAG_TEST_DATABASE_URL must be set to run these real-Postgres tests");
        let pool = connect(&url).await.expect("connect to real test Postgres");
        run_migrations(&pool).await.expect("run real migrations");
        pool
    }

    /// Real per-test isolation without a fresh database per test (simpler
    /// than wiring `sqlx::test`'s auto-provisioned-database machinery for a
    /// v1): every test scopes all its rows under a run_id unique to that
    /// test, and cleans up its own rows on the way out via `DELETE ... WHERE
    /// run_id = $1` -- genuinely isolated from any other test's real rows in
    /// the same shared test database, real cleanup, not relying on
    /// transaction rollback (the code under test commits its own
    /// transaction, so a wrapping rollback would hide real commit-path bugs).
    async fn cleanup(pool: &PgPool, run_id: &str) {
        let _ = sqlx::query("DELETE FROM rag_chunks WHERE run_id = $1").bind(run_id).execute(pool).await;
    }

    #[tokio::test]
    #[ignore = "needs a real local Postgres+pgvector; run with RAG_TEST_DATABASE_URL set and `cargo test -- --ignored`"]
    async fn replace_chunks_then_semantic_search_finds_the_real_closer_embedding() {
        let pool = test_pool().await;
        let run_id = "vector-store-test-closer-embedding";
        cleanup(&pool, run_id).await;

        let chunks = vec![
            ChunkToStore { path: "close.md".into(), chunk_index: 0, text: "close".into(), embedding: Some(vec![1.0; 1536]) },
            ChunkToStore {
                path: "far.md".into(),
                chunk_index: 0,
                text: "far".into(),
                embedding: Some({
                    let mut v = vec![1.0; 1536];
                    v[0] = -1.0;
                    v
                }),
            },
        ];
        replace_chunks(&pool, run_id, "manual_document", &chunks, 0).await.expect("real insert");

        let query = vec![1.0; 1536];
        let hits = semantic_search(&pool, run_id, &query, 10).await.expect("real search");
        assert_eq!(hits[0].path, "close.md", "the real closer real embedding must rank first");
        assert_eq!(hits[0].text, "close", "the real stored chunk text must round-trip, not just the path");
        assert!(hits[0].score > hits[1].score);

        cleanup(&pool, run_id).await;
    }

    #[tokio::test]
    #[ignore = "needs a real local Postgres+pgvector; run with RAG_TEST_DATABASE_URL set and `cargo test -- --ignored`"]
    async fn semantic_search_never_returns_a_chunk_with_no_real_embedding() {
        let pool = test_pool().await;
        let run_id = "vector-store-test-no-embedding";
        cleanup(&pool, run_id).await;

        let chunks = vec![ChunkToStore { path: "unembedded.md".into(), chunk_index: 0, text: "no embedding here".into(), embedding: None }];
        replace_chunks(&pool, run_id, "manual_document", &chunks, 0).await.expect("real insert");

        let query = vec![1.0; 1536];
        let hits = semantic_search(&pool, run_id, &query, 10).await.expect("real search");
        assert!(hits.is_empty(), "a chunk with no real embedding must never be returned as a semantic hit");

        cleanup(&pool, run_id).await;
    }

    #[tokio::test]
    #[ignore = "needs a real local Postgres+pgvector; run with RAG_TEST_DATABASE_URL set and `cargo test -- --ignored`"]
    async fn replace_chunks_wholesale_removes_a_stale_row_from_a_prior_sync() {
        let pool = test_pool().await;
        let run_id = "vector-store-test-wholesale-replace";
        cleanup(&pool, run_id).await;

        let first_sync = vec![ChunkToStore { path: "deleted-later.md".into(), chunk_index: 0, text: "will be removed from the repo".into(), embedding: None }];
        replace_chunks(&pool, run_id, "repo_sync", &first_sync, 0).await.expect("real first insert");

        let second_sync = vec![ChunkToStore { path: "still-here.md".into(), chunk_index: 0, text: "survives the re-sync".into(), embedding: None }];
        replace_chunks(&pool, run_id, "repo_sync", &second_sync, 1).await.expect("real second insert (wholesale replace)");

        let remaining: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM rag_chunks WHERE run_id = $1 AND source_kind = 'repo_sync'")
            .bind(run_id)
            .fetch_one(&pool)
            .await
            .expect("real count");
        assert_eq!(remaining.0, 1, "a re-sync must wholesale-replace, not accumulate, the prior sync's rows");

        cleanup(&pool, run_id).await;
    }

    #[tokio::test]
    #[ignore = "needs a real local Postgres+pgvector; run with RAG_TEST_DATABASE_URL set and `cargo test -- --ignored`"]
    async fn delete_by_path_removes_only_the_real_matching_row() {
        let pool = test_pool().await;
        let run_id = "vector-store-test-delete-by-path";
        cleanup(&pool, run_id).await;

        let chunks = vec![
            ChunkToStore { path: "keep.txt".into(), chunk_index: 0, text: "stays".into(), embedding: None },
            ChunkToStore { path: "remove.txt".into(), chunk_index: 0, text: "goes".into(), embedding: None },
        ];
        replace_chunks(&pool, run_id, "manual_document", &chunks, 0).await.expect("real insert");

        let removed = delete_by_path(&pool, run_id, "manual_document", "remove.txt").await.expect("real delete");
        assert_eq!(removed, 1);

        let remaining: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM rag_chunks WHERE run_id = $1").bind(run_id).fetch_one(&pool).await.expect("real count");
        assert_eq!(remaining.0, 1, "only the matching path's row should be gone");

        cleanup(&pool, run_id).await;
    }
}
