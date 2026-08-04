# RAG semantic search + Unstructured API image support — requirements for labor-setup.com

Handed off for external implementation (operator's own decision, 2026-08-04): no embedding
credential is available on this deployment, and the host is already resource-tight (4 CPU /
7.6GB / no swap, running a real fleet of other services — control plane, edge, Keycloak,
devsystem-web itself, multiple demo origins/agents). Rather than guess at scope, this document
grounds the ask in the real, current implementation so the result matches what the operator and
this repo's maintainer both actually mean by it.

## What exists today (`web/src/rag.rs`)

Read the file — it's short (~370 lines) and the doc comments explain every real design decision
already made. Summary:

- **Keyword/full-text search only.** An in-memory inverted index (`RagIndex.chunks: Vec<RagChunk>`),
  serialized to plain JSON per run (`runs/<run_id>/rag_index.json`, gitignored — a re-fetchable
  cache, not source of truth). `score_chunk` is case-insensitive term-overlap counting plus a
  fixed bonus for an exact-phrase substring match. No embeddings, no vectors, no ranking model.
- **Two document sources**, both searched together by `search()`:
  1. **Repo sync** (`sync_repo`, pull-only): given a run's `repo_url` (the same field
     `set_repo_url` already manages — there is no separate "docs repo" concept), fetches the
     repo's default-branch tree via GitHub's REST API (`GET /repos/{owner}/{repo}` then
     `GET .../git/trees/{branch}?recursive=1`), filters to `INDEXABLE_EXTENSIONS` (`md`, `mdx`,
     `txt`, `rst`, `adoc`) plus root basenames (`README`, `LICENSE`, `CHANGELOG`,
     `CONTRIBUTING`), fetches each file's raw body from `raw.githubusercontent.com`, and chunks
     it (`chunk_text`, ~800-char chunks on line boundaries). Hard caps: `MAX_FILES = 60`,
     `MAX_FILE_BYTES = 200_000` — real, reported honestly (`files_seen` vs `files_indexed`), not
     silently truncated.
  2. **Manual documents** (`RagIndex.manual_documents: Vec<RagDocument>`): pasted/uploaded text
     via the GUI, capped at `MAX_RAG_DOCUMENT_BYTES = 500_000`. Chunked at query time (not
     pre-chunked), so both sources score through the identical `chunk_text`/`score_chunk` path.
     Survives a repo re-sync untouched (`sync_repo` never sees or touches this list).
- **HTTP surface** (`web/src/main.rs`): `POST /api/runs/{id}/rag/sync`, `GET
  /api/runs/{id}/rag/search?q=...`, `POST /api/runs/{id}/rag/documents`, `POST
  /api/runs/{id}/rag/documents/{doc_id}/remove` — all owner-scoped via the same
  `owner_authorized()` every other mutating endpoint in this codebase uses.
- **No image support at all.** `INDEXABLE_EXTENSIONS` has no image types; nothing decodes,
  OCRs, or describes an image anywhere in this path.

## What's being asked for

1. **Real semantic search** — embedding-based retrieval (not just keyword overlap) over the same
   two document sources (repo-synced files + manually uploaded documents).
2. **Unstructured API integration** — real document parsing via
   [Unstructured](https://unstructured.io) (or an equivalent), so richer source formats (PDF,
   DOCX, HTML, and — the operator's explicit ask — **images**, i.e. real OCR/vision-based
   extraction of text/content from image files) become real, searchable index entries, not just
   plain-text markdown/README files.
3. Referenced by the operator as the shape to match: the "surfsense-v2" idea and prior real
   experimentation in [`scimbe/cadserv`](https://github.com/scimbe/cadserv) — but see the scope
   note below on **not** reusing cadserv's actual infrastructure shape.

## Real constraints the implementation must respect

- **No new heavy infra on this host.** cadserv's own real shape (Postgres+pgvector+Redis+HAProxy)
  was explicitly rejected once already for exactly this reason (see `rag.rs`'s own doc comment,
  design decision #2) — the same constraint still applies. If real vector storage is needed,
  it must be either:
  - a lightweight embedded option (e.g. an on-disk vector index library with no separate server
    process — matching this codebase's existing "no new services" pattern for the keyword index),
    or
  - a real external hosted service (the embedding/vector-store provider itself, not something
    self-hosted on this box) — acceptable specifically because it moves the resource cost off
    this host, not onto it.
  Decide and state which, explicitly, in the implementation PR — don't silently assume.
- **Real embedding credential required, must be provided, never fabricated.** No embedding API
  key exists in this deployment's environment today. Whatever provider is chosen (OpenAI,
  Voyage, Cohere, a local ONNX/GGUF model, etc.), state exactly what credential/config it needs;
  the operator will provision it. Do **not** ship a fallback that silently degrades to fake
  "semantic-looking" results — if the credential is missing/invalid, fail honestly (matching
  every other honest-gap pattern already established in this codebase — e.g. `assistant_status`'s
  `configured`/`reachable` fields, never a guessed value).
- **Preserve the existing keyword-search path.** Don't replace `score_chunk`'s keyword matching —
  either run both and merge/re-rank, or make semantic search additive (a real design decision to
  state explicitly, not silently pick). The existing HTTP contract
  (`GET /api/runs/{id}/rag/search?q=...` → `RagSearchResult { path, score, snippet }`) should stay
  backward compatible for the GUI already calling it; extend the response shape rather than
  breaking it if new fields are needed (e.g. a `match_kind: "keyword" | "semantic"` field, or a
  combined score).
- **Preserve the two-source model.** Both repo-synced chunks and manually uploaded documents must
  be embeddable and searchable — not just one or the other.
- **Preserve owner-scoping and the existing size/count caps** (`MAX_FILES`, `MAX_FILE_BYTES`,
  `MAX_RAG_DOCUMENT_BYTES`) — extend them thoughtfully for the new content types (an image file
  needs its own real, stated size cap) rather than removing the existing bounded-ness this
  codebase has been careful about everywhere else.
- **Real image support means real OCR/vision extraction**, not just "accept image uploads and do
  nothing with them." State which real Unstructured API capability/endpoint is used and verify it
  actually extracts real, checkable text from a real test image before calling this done — the
  same "verified live, not assumed" standard every other feature in this codebase has been held
  to. A stub that accepts an image and stores a placeholder string is not an acceptable outcome.
- **Hermetic test coverage**, matching this codebase's own established gate: `cargo test` under
  `RUSTFLAGS=-D warnings` in a Docker `rust:1-slim` container (see any recent commit in this repo
  for the exact invocation), real HTTP-level tests for new endpoints, and — for anything calling
  a real external API (embeddings, Unstructured, GitHub) — a real local mock server standing in
  for it in tests (the established pattern: see `keycloak_admin.rs`'s tests in the sibling
  `CADS-Tunnel` repo, or `spawn_mock_assistant` in `web/src/main.rs`'s own test module), not a
  hand-wavy assumption that the real call would work.

## What is explicitly out of scope for this request

- Replacing or removing the existing keyword search.
- Any change to the RAG data model's ownership/auth model (`owner_authorized()` stays as-is).
- Any new heavy self-hosted service on the CADS-devsystem/CADS-Tunnel host itself.

## Deliverable

A PR against `scimbe/CADS-devsystem` (this repo) that:
1. States explicitly which embedding provider and which vector-storage approach was chosen, and why.
2. Extends `web/src/rag.rs` (or adds a sibling module) with real semantic search, keeping the
   existing keyword path working.
3. Adds real Unstructured API integration for richer document types, explicitly including images.
4. Documents exactly which new environment variable(s)/credentials the operator needs to set.
5. Includes hermetic tests proving the above, run against the real hermetic gate this repo already uses.
6. Is verified live (not just unit-tested) against a real synced repo and at least one real
   uploaded image, with the actual extracted/embedded content shown, before being called done.
