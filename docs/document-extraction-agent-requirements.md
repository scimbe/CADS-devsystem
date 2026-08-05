# RAG document-extraction agent — requirements for labor-setup.com

Handed off for external implementation (operator's own decision, 2026-08-05), same shape as the
[build-service](android-build-service-requirements.md) and
[emulator-walkthrough](android-emulator-walkthrough-service-requirements.md) hand-offs: this is the
actual role-filler for `devsystem.document_extraction` (issue #7), plus the embeddings/vector-storage
decision that's downstream of it and was deliberately left open until now.

## What exists today

- **The role is declared, not filled.** `devsystem.document_extraction` is a real
  `ServiceType::Custom` role on the `webconference-android` run's live `PipelineSpec` (added via a
  real `devsystem_iterate --remote` submission, `roles_now=4`) — but no agent has bid on or won its
  auction. It's a real, open seat, not a plan.
- **The caller side is real and already built**: `devsystem_document_extraction_client`
  ([CADS-devsystem@d7a5ba9](https://github.com/scimbe/CADS-devsystem/commit/d7a5ba9)) spawns a real
  `ct-agent channel` initiator, sends a document (base64-encoded — the MCP `service/<slug>` schema
  fixes the request to a single string, see `ct_common::mcp::register_service_tools`), and expects
  back `{"status":"extracted","text":"..."}` or `{"status":"error","error":"..."}`. This is the exact
  wire contract a real handler needs to speak.
- **`ct-agent` itself already supports an arbitrary Custom service**, verified directly in
  `scimbe/ct-agent`'s current source (`native/src/channel_run.rs`), not assumed from an older,
  now-stale comment elsewhere in this codebase: `CT_AGENT_SERVICES=devsystem_document_extraction`
  (the exact slug `ct_common::mcp::service_slug` derives from `devsystem.document_extraction`) +
  `CT_AGENT_SERVICE_HANDLER_CMD=<your handler script>` on a `ct-agent channel --serve` process is a
  real, working configuration today — not a hypothetical extension.
- **The M2M report-back path is live** (issues #7/#12, [CADS-Tunnel PR #390](https://github.com/scimbe/CADS-Tunnel/pull/390)):
  once extraction produces real chunks/embeddings, reporting a real iteration back via
  `devsystem_iterate --remote` (with a bearer token, see Credentials below) already works end to end.
- **`web/src/rag.rs`'s existing chunking** (`chunk_text`, ~800-char chunks, `MAX_FILES=60` cap) is
  the real, already-tested downstream consumer of extracted text — whatever this agent returns feeds
  into that, not a new chunking implementation.

## What's being asked for — two real, related pieces

### 1. The actual extraction agent (fills the role)

A real process, on your own infrastructure, that:

1. Bids for `devsystem.document_extraction` via `devsystem_offer` (self-generated ed25519 identity —
   no coordination needed for this part, same "the signature is the authentication" design every
   other real bidder in this system already uses).
2. Serves the role's channel: `ct-agent channel --serve` with `CT_AGENT_SERVICES=devsystem_document_extraction`
   and `CT_AGENT_SERVICE_HANDLER_CMD` pointed at a real handler that:
   - Reads one line of JSON on stdin (`{filename, mime_type, content_base64}`), matching
     `devsystem_document_extraction_client`'s real request shape exactly.
   - Base64-decodes the content and actually extracts text — real PDF/DOCX parsing and real OCR/
     vision for images, not a stub. **Open question, your call, flagging rather than guessing**: hosted
     `api.unstructured.io` (the "lightweight embedded option" branch from the earlier RAG architecture
     decision — moves the resource cost off this host, per that decision's own stated constraint) vs.
     an LLM-vision call vs. something else you have better infra for. Say which and why.
   - Writes exactly one line of JSON to stdout: `{"status":"extracted","text":"..."}` on success,
     `{"status":"error","error":"..."}` on a real failure (unsupported format, corrupt file,
     extraction service unreachable) — never a fabricated `extracted` on failure.
3. Reports real work back via `devsystem_iterate --remote` against the `webconference-android` run
   (or whichever run's requirements actually call for document extraction) using the M2M credential
   below — same round trip already proven for #12's build-service report-back.

### 2. Embeddings/vector-storage architecture (the deliberately-deferred decision)

The original RAG architecture note (issue #7, 2026-08-04) explicitly left this open: *"embeddings
deliberately left as a separate open question, not folded in here."* That question is now yours too,
since you're the one building the agent that would produce them:

- Real, stated options, not guessed: extend `RagChunk`/`RagDocument` with an `embedding: Option<Vec<f32>>`
  field persisted in the existing `rag_index.json` (the "lightweight embedded, no new service" branch
  — brute-force cosine similarity over a few hundred/low-thousand chunks is genuinely sub-millisecond,
  no vector DB needed at this scale), OR a real external vector store if you have a concrete reason
  the embedded approach won't hold up. State which, and why, before building — this is a real
  architecture decision affecting `web/src/rag.rs`, not an implementation detail.
- Embedding provider: same open question the original RAG thread flagged and never resolved (OpenAI
  `text-embedding-3-small` vs. Voyage vs. something you'd rather run yourself) — say which.

## Real constraints, matching every other hand-off so far

- **No new heavy infra on this host** — same rule as #12/#13, this is exactly why it's being handed
  off.
- **No fabricated extraction.** A real handler that can't actually parse a document must return a
  real `error`, never a placeholder string dressed up as extracted text.
- **Preserve the schema-typed service boundary** (#149-A.1's own abuse-mitigation rationale, stated
  in `ct_common::mcp.rs`): the handler only ever receives `{filename, mime_type, content_base64}` on
  stdin — never a free-form instruction slot.
- **Hermetic test coverage for the handler itself**, matching `github_issue_channel_handler.rs`'s
  own precedent (a fake-stdin/stdout test harness, no live channel needed to prove the logic).

## What is explicitly out of scope

- Changing `devsystem_document_extraction_client.rs` or its wire contract — the handler adapts to
  the existing, real, already-tested request/response shape.
- The actual UI/flow for uploading a document that triggers extraction — that's separate,
  already-real work in `web/src/rag.rs`'s existing manual-upload path.
- Deciding embeddings for CADS-devsystem's *other* runs beyond what this agent itself needs to
  operate — scope this to what your agent actually produces.

## Credentials

A dedicated Keycloak M2M service account (`client_credentials` grant, kept separate from the
build-service and emulator-walkthrough identities — one identity per real external service, same
hygiene as those two) will be provisioned for this agent's `devsystem_iterate --remote` report-back.
Say when you're ready and it'll be created and allow-listed; `client_id`/`client_secret` relayed out
of band, never through the issue thread. Your bidding identity (the ed25519 keypair `devsystem_offer`
uses) is self-generated — no coordination needed for that part.

## Deliverable

1. States which extraction approach and which embeddings/vector-storage architecture were chosen,
   and why.
2. A real, running agent that wins `devsystem.document_extraction`'s auction and serves real
   extraction requests.
3. A real `devsystem_iterate --remote` submission reporting genuine work against the role.
4. Hermetic tests for the handler's own request/response logic.
