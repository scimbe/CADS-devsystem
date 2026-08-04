# RAG search: real semantic search, Unstructured extraction, Postgres+pgvector

`web/src/rag.rs` + `web/src/vector_store.rs` (CADS-devsystem#7, PR #8). Three
real capabilities, each independently optional and each degrading honestly
(never fabricating a result) when its credential isn't configured:

1. **Keyword search** — always on, no credential needed. Case-insensitive
   term-overlap scoring (`rag::score_chunk`) over whatever a run's repo sync
   (`POST /rag/sync`) and manually-uploaded documents currently hold.
2. **Semantic search** — real embedding-based retrieval, additive to keyword
   search (`match_kind: "keyword" | "semantic"` in every `RagSearchResult`).
3. **Unstructured extraction** — real OCR/document parsing for images,
   PDF, DOCX (and whatever else Unstructured's own `/general/v0/general`
   endpoint supports) via `POST /rag/upload-file`, not just plain-text
   uploads.

## Environment variables

None of these are required for the app to run — every one is `Option`-typed
and reported honestly as "not configured" (never a fabricated result) when
unset. Set what you actually want live.

| Variable | Required for | Default if unset |
|---|---|---|
| `RAG_EMBEDDING_API_KEY` | Semantic search (`rag::embed_texts`) | Unset → search stays keyword-only |
| `RAG_EMBEDDING_API_BASE` | Overriding the embedding provider | `https://api.openai.com/v1` (OpenAI `text-embedding-3-small`, 1536-dim) |
| `RAG_UNSTRUCTURED_API_KEY` | `POST /rag/upload-file` (image/PDF/DOCX extraction) | Unset → that endpoint returns a real `503`, not a silent no-op |
| `RAG_UNSTRUCTURED_API_BASE` | Overriding the Unstructured provider | `https://api.unstructured.io` (hosted, not self-hosted) |
| `DATABASE_URL` | Real Postgres+pgvector-backed semantic search | Unset → semantic search stays on the embedded/JSON-index cosine-similarity fallback in `rag.rs` (still real, just not backed by a real ANN index) |

`DATABASE_URL`, if set, must point at a real Postgres with the `vector`
extension installable (`pgvector/pgvector:pg16` is what this was developed
and tested against — a stock `postgres` image does not have the extension
available). Unlike the other four variables, a **set-but-broken**
`DATABASE_URL` is a hard startup failure (`web/src/main.rs`'s `main()`
`.expect()`s the connect+migrate), not a silent degrade — if you're
configuring real Postgres, it's expected to actually work.

## What happens with nothing configured

Exactly today's original behavior: keyword search only, `POST
/rag/upload-file` reports itself unconfigured (`503`), no real network calls
to any embedding/extraction/database provider ever happen. This is the
default, zero-config state — every environment variable above is additive.

## Storage architecture

`vector_store.rs`'s own module doc explains the real decision (and its
history — an embedded-only, no-new-infra approach shipped first, then was
explicitly reversed by the operator after reviewing `scimbe/cadserv`'s real
SurfSense-v2 fork; see PR #8's commit history for the actual sequence, not
just this document's summary of it): one flat `rag_chunks` table
(`web/migrations/0001_rag_chunks.sql`), a pgvector `vector(1536)` column, an
ivfflat cosine-distance index. Real Postgres is the authoritative store for
semantic search when configured; the in-memory/JSON `RagIndex` (unchanged
from before PR #8) remains the source of truth for keyword search and for
what a run's GUI panel actually displays.

## Testing this without a real credential

`cargo test` (hermetic, no real network calls) covers everything except the
Postgres integration tests, which are `#[ignore]`d and need a real local
Postgres+pgvector instance:

```bash
docker run -d --name rag-pgvector-test -e POSTGRES_PASSWORD=testpass \
  -e POSTGRES_DB=ragtest -p 55432:5432 pgvector/pgvector:pg16
RAG_TEST_DATABASE_URL=postgres://postgres:testpass@127.0.0.1:55432/ragtest \
  cargo test -- --ignored
```

The embedding and Unstructured clients are tested against real local mock
HTTP servers (`spawn_mock_embedding_server`, `spawn_mock_unstructured_server`
in `rag.rs`'s own test module) — real HTTP round trips through the exact
client code, not hand-stubbed function calls, just never hitting the real
OpenAI/Unstructured APIs.

## What's still open

- No real `RAG_EMBEDDING_API_KEY`/`RAG_UNSTRUCTURED_API_KEY` configured on
  the live deployment as of this writing — semantic search and Unstructured
  extraction are real, tested, and (for the Postgres piece) live-smoke-tested
  against a real local database, but not yet verified against the real
  OpenAI/Unstructured APIs in production.
- This repo's CI (`.github/workflows/pipeline-ci.yml`) now has a real
  Postgres service and runs both the hermetic and `--ignored` test suites for
  `web/` — added in the same PR, but not yet confirmed to have actually run
  successfully on GitHub's own infrastructure (only the PR's first commit
  ever triggered a `pull_request`-event run; later pushes haven't, for a
  reason not yet diagnosed).
