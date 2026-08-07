//! Real interactive control surface for The Development System's pipeline runs
//! (#382). Every request here goes through the *exact same* `devsystem-pipeline`
//! library functions the CLI tools (`devsystem_iterate`/`devsystem_checkin`) use --
//! no separate/parallel logic path, no fixture data. The pipeline mechanism itself
//! stays project-agnostic: this server lists whatever `runs/<id>/` directories
//! actually exist on disk and lets a human create new ones -- `webconference-android`
//! is just the first one, not a hardcoded case anywhere in this file.

mod rag;
mod vector_store;

use axum::extract::{Path as AxPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json};
use axum::routing::{get, post};
use axum::Router;
use devsystem_pipeline::checkin::render_plan_markdown;
use devsystem_pipeline::envelope::{append_to_memory_log, envelope_from_iteration, govern_memory_entry, read_memory_log};
use devsystem_pipeline::improve::stalled_stages;
use devsystem_pipeline::preflight::{preflight_annotations, process_annotations};
use devsystem_pipeline::runner::{
    checkin_pending, duplicate_of_last_iteration, load_or_init_run, persist_run, push_chat_exchange, qualifying_review_evidence, render_requirements_markdown, run_iteration, toggle_acceptance_criterion,
    toggle_milestone, toggle_requirement, toggle_requirement_auto_judge, valid_run_id, validate_requirement_indices, BacklogItem, CustomPanel,
    Milestone, PendingDeleteRunProposal, PendingIssueProposal, PendingNextStepDraft, PendingPanelEditProposal, PendingPanelProposal, PendingPanelRemovalProposal, PendingStageProposal,
    PlanCanvasAnnotation, Requirement, RoleFillMode, RunOutcome, RunState,
};
use devsystem_pipeline::{
    apply_proposal, contains_bidi_control_char, validate_feedback, validate_proposals, validate_stage, AbortCriteria, IterationRecord, ProposalOutcome,
    StageProposal, ALL_STAGES, MAX_ROLE_UNITS, STAGE_PLAN, STAGE_REVIEW,
};
use ct_common::channel::{CapacityKind, CapacityOffer, ServiceType};
use ct_common::pipeline::SelectionState;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tower_http::services::ServeDir;

#[derive(Clone)]
struct AppState {
    runs_dir: Arc<PathBuf>,
    /// Every mutating handler (create/iterate/criteria/govern) does its own
    /// load-then-persist round trip against `runs/<id>/*.json` with no other
    /// coordination -- two concurrent requests (a double-click, two browser tabs)
    /// could both load the same on-disk state before either writes back, and the
    /// second `persist_run` would silently clobber the first's update. This is a
    /// real single-process control surface, not a multi-tenant service, so one
    /// global write lock (not per-run) is the simplest correct fix: writes are
    /// sub-millisecond, so serializing them costs nothing a human submitting one
    /// form at a time would ever notice.
    write_lock: Arc<tokio::sync::Mutex<()>>,
    /// Real signed `CapacityOffer`s submitted for a run's roles (run_id -> one
    /// current offer per holder pubkey), in-memory only -- deliberately not
    /// persisted to `state.json`: these are live market data with their own
    /// `expires_at`, the same reason CADS-Tunnel core itself never persists
    /// `CapacityOffer`s (see `auction_view`'s doc comment). Follows the proven
    /// CADS-auction-demo pattern: devsystem-web collects real offers itself and
    /// runs the real `PipelineSpec::auction_view` in-process -- CADS-Tunnel's
    /// control plane has no live bid-collection endpoint to call instead
    /// (verified directly against the current checkout, not assumed).
    offers: Arc<tokio::sync::Mutex<HashMap<String, Vec<CapacityOffer>>>>,
    /// Real cross-request `SelectionState` per run (run_id -> the `RoundRobin`
    /// cursor / `LeastCalls` served-counts `PipelineSpec::auction_view` threads
    /// through). Real gap found and fixed 2026-08-05: `view_auction` used to
    /// construct a fresh `SelectionState::default()` on every single call, so
    /// the two stateful policies could never actually behave as designed --
    /// `RoundRobin` never advanced past whichever provider sorts first, and
    /// `LeastCalls` saw every candidate tied at zero served jobs, forever. Only
    /// `LowestFloor` (stateless by design, `SelectionState`'s own doc comment)
    /// was ever unaffected. In-memory only, matching `offers` above -- this is
    /// live scheduling state with the same lifetime as the offers it's
    /// selecting among, not run history worth persisting to `state.json`.
    selection_state: Arc<tokio::sync::Mutex<HashMap<String, SelectionState>>>,
    /// Base URL of a running `devsystem_assistant --serve` bridge (e.g.
    /// `http://host.docker.internal:8791` when the assistant runs as a host
    /// process alongside this container's Docker host -- the real LLM CLI lives
    /// on the host, not in this container). `None` when unconfigured: the
    /// assistant panel then reports a clear "not configured" error rather than
    /// silently doing nothing or fabricating a response.
    assistant_url: Option<Arc<str>>,
    /// A GitHub token with `public_repo` (or `repo`) scope, so an approved issue
    /// proposal can actually be posted (real self-healing, 2026-08-04) -- `None`
    /// on a deployment that hasn't configured one: `approve_issue_proposal` then
    /// reports a clear 503 rather than silently doing nothing or fabricating
    /// success. Never logged, never returned in any response.
    github_token: Option<Arc<str>>,
    http_client: reqwest::Client,
    /// Real embedding credential for RAG semantic search (`rag::embed_texts`),
    /// `RAG_EMBEDDING_API_KEY` at startup -- `None` when unconfigured, the same
    /// honest-degrade contract `assistant_url` already established: `search_rag`
    /// falls back to keyword-only search rather than fabricating a "semantic"
    /// result with no real embedding behind it.
    rag_embedding_api_key: Option<Arc<str>>,
    /// Embedding provider's API base URL, `RAG_EMBEDDING_API_BASE` at startup,
    /// defaulting to OpenAI's -- overridable so a real alternate
    /// OpenAI-compatible provider (or a test's own mock server) never requires
    /// a code change, only a different env var.
    rag_embedding_api_base: Arc<str>,
    /// Real Unstructured API credential (`rag::parse_with_unstructured`),
    /// `RAG_UNSTRUCTURED_API_KEY` at startup -- `None` when unconfigured, same
    /// honest-degrade contract: `upload_rag_file` reports a clear "not
    /// configured" error rather than pretending to extract anything.
    rag_unstructured_api_key: Option<Arc<str>>,
    /// `RAG_UNSTRUCTURED_API_BASE` at startup, defaulting to the real hosted
    /// `api.unstructured.io`.
    rag_unstructured_api_base: Arc<str>,
    /// Real Postgres+pgvector pool for RAG semantic search
    /// (`vector_store::semantic_search`), `DATABASE_URL` at startup --
    /// `None` when unconfigured, same honest-degrade contract as the other
    /// two RAG credentials: `search_rag` falls back to the embedded/JSON
    /// semantic search path rather than erroring.
    rag_db_pool: Option<sqlx::PgPool>,
    /// Real relay configuration for `approve_issue_proposal` (#48 slice 6): when
    /// every piece is configured, an approval shells out to a real
    /// `github_issue_channel_client` subprocess -- which dials the separate,
    /// isolated `github_issue_channel_handler` agent over a real CADS-Tunnel
    /// Agent-Fabric channel -- instead of POSTing to GitHub directly with
    /// `github_token`. The real GitHub credential then lives only on that
    /// separate handler process, never inside devsystem-web at all. `None` when
    /// any one piece is missing on this deployment: an ADDITIVE path, same
    /// honest-degrade contract as `github_token` above -- a deployment that only
    /// ever configured `DEVSYSTEM_GITHUB_TOKEN` keeps working exactly as before
    /// this slice (see `approve_issue_proposal`'s own doc comment for the
    /// priority between the two paths).
    issue_channel: Option<IssueChannelConfig>,
    /// Real fallback extraction path for `upload_rag_file` (#382 goal doc §4.2/
    /// issue #7 & #14, 2026-08-06): when configured, an upload that can't use
    /// `rag_unstructured_api_key` (unset -- no paid API key on this deployment)
    /// shells out to a real `devsystem_document_extraction_client` subprocess,
    /// which dials the real `devsystem.document_extraction` role-filler over a
    /// real CADS-Tunnel Agent-Fabric channel, the same "separate isolated
    /// subprocess holds the real credential/identity, this process never does"
    /// shape `issue_channel` already established. `None` when any piece is
    /// missing -- an ADDITIVE path: a deployment with `rag_unstructured_api_key`
    /// configured is unaffected either way (Unstructured stays tried first,
    /// see `upload_rag_file`'s own doc comment on the real priority), and a
    /// deployment with neither keeps today's honest `503`.
    document_extraction_channel: Option<DocumentExtractionChannelConfig>,
}

/// See `AppState::issue_channel`'s doc comment. Every field here maps directly
/// onto the exact env var `github_issue_channel_client` itself already reads
/// (see that binary's own module doc) -- `client_bin`/`ct_agent_bin` are the
/// two paths (to the client binary itself, and to the real `ct-agent` binary
/// that client spawns as its own child), the rest are that binary's real
/// channel-dialing parameters, this deployment's own private key among them.
#[derive(Clone)]
struct IssueChannelConfig {
    client_bin: Arc<str>,
    ct_agent_bin: Arc<str>,
    addr: Arc<str>,
    noise_key: Arc<str>,
    peer_noise_key: Arc<str>,
    peer_cert_file: Arc<str>,
}

/// See `AppState::document_extraction_channel`'s doc comment. Real
/// broker-mediated shape (issue #14, 2026-08-06 correction) -- NOT the same
/// shape as `IssueChannelConfig`, on purpose: this project has two genuinely
/// different, non-interchangeable Agent-Fabric connection models
/// (direct-address, what `IssueChannelConfig` uses; broker-mediated relay-only,
/// what this one uses), confused once already this session. This channel is
/// grant-gated (`ct-agent channel grant`) -- this deployment's own real signed
/// `SignedChannelGrant` and the real private holder key it was issued to both
/// have to reach the spawned `ct-agent channel` subprocess (inherited via
/// explicit `.env(...)`, matching this file's own "every env var explicit,
/// never left to ambient inheritance" convention -- see `extract_via_channel`).
#[derive(Clone)]
struct DocumentExtractionChannelConfig {
    client_bin: Arc<str>,
    ct_agent_bin: Arc<str>,
    broker: Arc<str>,
    relay: Arc<str>,
    grant_hex: Arc<str>,
    holder_key_hex: Arc<str>,
    noise_key: Arc<str>,
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).expect("system clock before 1970").as_secs()
}

/// `std::env::var` treated the same way every other optional credential/URL in
/// this file already is: unset OR blank both mean "not configured", never an
/// empty-string value silently accepted as real configuration.
fn nonempty_env(name: &str) -> Option<Arc<str>> {
    std::env::var(name).ok().filter(|s| !s.trim().is_empty()).map(Arc::from)
}

/// The real API router, no static-file fallback -- separated out so tests can
/// exercise the exact same routes/handlers `main()` serves, via `tower::ServiceExt`,
/// without binding a real socket or needing a static dir on disk.
///
/// Deliberately no `CorsLayer`: the frontend (`web/static/`) and this API are always
/// served from the same origin -- Caddy reverse-proxies the whole
/// devsystem-demo.bunsenbrenner.org domain to this one process, and `index.html`'s
/// fetch calls are relative (`/api/...`). A permissive CORS layer was present here
/// since the very first commit for no functional reason -- same-origin requests never
/// need CORS headers -- and only widened the attack surface: it let *any* origin's
/// JS read this API's responses in a browser, real risk if a session cookie ever
/// stops being strictly same-site. Removing it is a pure security tightening with no
/// loss of functionality.
fn api_router(state: AppState) -> Router {
    Router::new()
        .route("/api/runs", get(list_runs).post(create_run))
        .route("/api/runs/{id}", get(get_run).delete(delete_run))
        .route("/api/runs/{id}/open-points", get(get_open_points))
        .route("/api/runs/{id}/iterate", post(iterate_run))
        .route("/api/runs/{id}/checkin", get(checkin_run))
        .route("/api/runs/{id}/criteria", post(update_criteria))
        .route("/api/runs/{id}/pause", post(pause_run))
        .route("/api/runs/{id}/resume", post(resume_run))
        .route("/api/runs/{id}/checkin/acknowledge", post(acknowledge_checkin))
        .route("/api/runs/{id}/memory", get(memory_run))
        .route("/api/runs/{id}/memory/{index}/govern", post(govern_memory))
        .route("/api/runs/{id}/backlog", post(add_backlog_item))
        .route("/api/runs/{id}/backlog/{index}/toggle", post(toggle_backlog_item))
        .route("/api/runs/{id}/milestones", post(add_milestone))
        .route("/api/runs/{id}/milestones/{index}/toggle", post(toggle_milestone_handler))
        .route("/api/runs/{id}/requirements", post(add_requirement))
        .route("/api/runs/{id}/requirements/{index}/update", post(update_requirement_handler))
        .route("/api/runs/{id}/requirements/{index}/toggle", post(toggle_requirement_handler))
        .route("/api/runs/{id}/requirements/{index}/auto-judge/toggle", post(toggle_requirement_auto_judge_handler))
        .route("/api/runs/{id}/requirements/{index}/criteria/{criterion_index}/toggle", post(toggle_acceptance_criterion_handler))
        .route("/api/runs/{id}/requirements/export", get(export_requirements))
        .route("/api/runs/{id}/repo", post(set_repo_url))
        .route("/api/runs/{id}/operator-pubkey", post(set_operator_pubkey))
        .route("/api/runs/{id}/roles/{tag}/fill-mode", post(set_role_fill_mode))
        .route("/api/runs/{id}/rag/sync", post(sync_rag))
        .route("/api/runs/{id}/rag/search", get(search_rag))
        .route("/api/runs/{id}/rag/documents", post(add_rag_document))
        .route("/api/runs/{id}/rag/upload-file", post(upload_rag_file))
        .route("/api/runs/{id}/rag/documents/{doc_id}/remove", post(remove_rag_document))
        .route("/api/runs/{id}/panels", post(add_custom_panel))
        .route("/api/runs/{id}/panels/{panel_id}/remove", post(remove_custom_panel))
        .route("/api/runs/{id}/panels/{panel_id}/update", post(update_custom_panel))
        .route("/api/runs/{id}/next-steps/propose", post(propose_next_step))
        .route("/api/runs/{id}/next-steps/{draft_id}/update", post(update_next_step_draft))
        .route("/api/runs/{id}/next-steps/{draft_id}/remove", post(remove_next_step_draft))
        .route("/api/runs/{id}/plan-canvas/annotate", post(plan_canvas_annotate))
        .route("/api/runs/{id}/plan-canvas/annotations/{annotation_id}/remove", post(plan_canvas_remove_annotation))
        .route("/api/runs/{id}/plan-canvas/verdict", post(plan_canvas_verdict))
        .route("/api/runs/{id}/panels/propose", post(propose_custom_panel))
        .route("/api/runs/{id}/panels/proposals/{proposal_id}/approve", post(approve_panel_proposal))
        .route("/api/runs/{id}/panels/proposals/{proposal_id}/reject", post(reject_panel_proposal))
        .route("/api/runs/{id}/panels/{panel_id}/propose-remove", post(propose_panel_removal))
        .route("/api/runs/{id}/panels/removal-proposals/{proposal_id}/approve", post(approve_panel_removal))
        .route("/api/runs/{id}/panels/removal-proposals/{proposal_id}/reject", post(reject_panel_removal))
        .route("/api/runs/{id}/panels/{panel_id}/propose-edit", post(propose_panel_edit))
        .route("/api/runs/{id}/panels/edit-proposals/{proposal_id}/approve", post(approve_panel_edit))
        .route("/api/runs/{id}/panels/edit-proposals/{proposal_id}/reject", post(reject_panel_edit))
        .route("/api/runs/{id}/delete-proposal", post(propose_delete_run))
        .route("/api/runs/{id}/delete-proposal/{proposal_id}/approve", post(approve_delete_run))
        .route("/api/runs/{id}/delete-proposal/{proposal_id}/reject", post(reject_delete_run))
        .route("/api/runs/{id}/stages/propose", post(propose_stage))
        .route("/api/runs/{id}/stages/proposals/{proposal_id}/approve", post(approve_stage_proposal))
        .route("/api/runs/{id}/stages/proposals/{proposal_id}/reject", post(reject_stage_proposal))
        .route("/api/runs/{id}/issues/propose", post(propose_issue))
        .route("/api/runs/{id}/issues/proposals/{proposal_id}/approve", post(approve_issue_proposal))
        .route("/api/runs/{id}/issues/proposals/{proposal_id}/reject", post(reject_issue_proposal))
        .route("/api/runs/{id}/offers/submit", post(submit_offer))
        .route("/api/runs/{id}/offers/quick-submit", post(quick_submit_offer))
        .route("/api/runs/{id}/auction", get(view_auction))
        .route("/api/runs/{id}/assistant", post(ask_assistant))
        .route("/api/assistant/status", get(assistant_status))
        .route("/api/me", get(whoami))
        .route("/api/version", get(version))
        .with_state(state)
}

/// Real gap found live 2026-08-07 (#382 goal doc §8): the existing post-deploy
/// smoke test proves one specific behavior (`duplicate_of_last_iteration`)
/// matches source, not that the WHOLE binary does -- a shared BuildKit cache
/// mount served a genuinely stale binary that passed that one check while a
/// real, unrelated feature (`checkin_cadence_effectively_disabled`) was
/// silently missing, caught only by the full stress harness, not the deploy
/// script itself. `DEVSYSTEM_GIT_SHA` is baked into the image at build time
/// (`web/Dockerfile`'s own `ARG`/`ENV`, set from `git rev-parse HEAD` by
/// `deploy-devsystem-web.sh`) -- comparing this against the real, current
/// `git rev-parse HEAD` after every deploy catches ANY staleness, not just
/// whichever one behavior a smoke test happens to exercise. `"unknown"` when
/// unset (a local `cargo run` outside Docker, or an older image built before
/// this endpoint existed) -- never fabricated.
async fn version() -> impl IntoResponse {
    let git_sha = std::env::var("DEVSYSTEM_GIT_SHA").unwrap_or_else(|_| "unknown".to_string());
    Json(serde_json::json!({ "git_sha": git_sha }))
}

/// Real identity, or honestly none -- never fabricated. Caddy's `forward_auth`
/// gate (demo-site/Caddyfile) copies the real logged-in email from the portal's
/// own `/gate/check` onto `X-Gate-Email` on every request that reaches this
/// process; a direct request that bypasses the gate (local dev, an internal
/// health check) simply has no such header, which this reports as `null`
/// rather than guessing or claiming a session that was never verified.
async fn whoami(headers: axum::http::HeaderMap) -> impl IntoResponse {
    let email = headers.get("x-gate-email").and_then(|v| v.to_str().ok()).map(|s| s.to_string());
    Json(serde_json::json!({ "email": email }))
}

/// Real, live status of the `devsystem.assistant` role's bridge -- the one role
/// in every run's spec that this deployment actually knows how to check, because
/// (unlike every other stage, which is only ever "backed" through the real
/// crew auction) `ask_assistant` talks to a single operator-configured process
/// directly. `configured` is just whether `DEVSYSTEM_ASSISTANT_URL` was set at
/// startup; `reachable` is a genuine live probe (a short-timeout GET against the
/// bridge's own base URL -- any HTTP response at all, even a 404, proves the
/// process is up, without spending real LLM cost on a `/ask` call). `reachable`
/// is `null`, not `false`, when nothing is configured: "unreachable" would imply
/// a real attempt was made. The bridge's own URL is deliberately never returned
/// here -- this endpoint is reachable by anyone who passes the site-wide login
/// gate (the exact "no per-account scoping yet" gap #382 raised), so this stays
/// the minimum honest signal rather than handing out internal network topology.
///
/// `disallowed_tools` answers the GUI's "which ct-agent-connected tools does the
/// assistant have" question honestly: none -- it's the exact
/// [`devsystem_pipeline::ASSISTANT_DISALLOWED_TOOLS`] list the bridge itself
/// passes to `claude -p --disallowedTools`, static configuration rather than a
/// live probe, so it's reported regardless of `configured`/`reachable`.
async fn assistant_status(State(state): State<AppState>) -> impl IntoResponse {
    let Some(url) = state.assistant_url.clone() else {
        return Json(serde_json::json!({
            "configured": false,
            "reachable": null,
            "response_time_ms": null,
            "disallowed_tools": devsystem_pipeline::ASSISTANT_DISALLOWED_TOOLS,
        }))
        .into_response();
    };
    let started = std::time::Instant::now();
    let reachable = state
        .http_client
        .get(url.as_ref())
        .timeout(std::time::Duration::from_secs(2))
        .send()
        .await
        .is_ok();
    // Only a meaningful latency figure when the probe actually completed --
    // a timeout/connection-refused elapsed time is an artifact of the 2s
    // timeout itself, not a real response time, so it's reported as null
    // rather than a number that would misleadingly look like one.
    let response_time_ms = reachable.then(|| started.elapsed().as_millis() as u64);
    Json(serde_json::json!({
        "configured": true,
        "reachable": reachable,
        "response_time_ms": response_time_ms,
        "disallowed_tools": devsystem_pipeline::ASSISTANT_DISALLOWED_TOOLS,
    }))
    .into_response()
}

#[tokio::main]
async fn main() {
    let runs_dir = PathBuf::from(std::env::var("DEVSYSTEM_RUNS_DIR").unwrap_or_else(|_| "runs".to_string()));
    fs::create_dir_all(&runs_dir).expect("create runs dir");
    let assistant_url: Option<Arc<str>> = std::env::var("DEVSYSTEM_ASSISTANT_URL").ok().filter(|s| !s.trim().is_empty()).map(Arc::from);
    if assistant_url.is_none() {
        println!("DEVSYSTEM_ASSISTANT_URL not set -- the Assistant panel will report itself unconfigured");
    }
    let rag_embedding_api_key: Option<Arc<str>> = std::env::var("RAG_EMBEDDING_API_KEY").ok().filter(|s| !s.trim().is_empty()).map(Arc::from);
    if rag_embedding_api_key.is_none() {
        println!("RAG_EMBEDDING_API_KEY not set -- RAG search will stay keyword-only, no semantic results");
    }
    let rag_embedding_api_base: Arc<str> =
        std::env::var("RAG_EMBEDDING_API_BASE").ok().filter(|s| !s.trim().is_empty()).map(Arc::from).unwrap_or_else(|| Arc::from("https://api.openai.com/v1"));
    let rag_unstructured_api_key: Option<Arc<str>> = std::env::var("RAG_UNSTRUCTURED_API_KEY").ok().filter(|s| !s.trim().is_empty()).map(Arc::from);
    if rag_unstructured_api_key.is_none() {
        println!("RAG_UNSTRUCTURED_API_KEY not set -- POST /rag/upload-file will report itself unconfigured");
    }
    let rag_unstructured_api_base: Arc<str> = std::env::var("RAG_UNSTRUCTURED_API_BASE")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(Arc::from)
        .unwrap_or_else(|| Arc::from("https://api.unstructured.io"));
    // Real, not best-effort: if DATABASE_URL is set, this deployment is
    // declaring it wants real Postgres-backed semantic search, so a real
    // connect/migrate failure here is a real startup error, not something to
    // silently degrade past -- unlike the other RAG credentials, an unset
    // DATABASE_URL (rag_db_pool: None) is the honest "not configured" case;
    // a *set-but-broken* one should fail loudly at boot, not at the first
    // real search request.
    let rag_db_pool: Option<sqlx::PgPool> = match std::env::var("DATABASE_URL").ok().filter(|s| !s.trim().is_empty()) {
        Some(url) => {
            let pool = vector_store::connect(&url).await.expect("connect to DATABASE_URL");
            vector_store::run_migrations(&pool).await.expect("run RAG vector_store migrations");
            println!("DATABASE_URL configured -- RAG semantic search backed by real Postgres+pgvector");
            Some(pool)
        }
        None => {
            println!("DATABASE_URL not set -- RAG semantic search stays on the embedded/JSON-index path");
            None
        }
    };
    let github_token: Option<Arc<str>> = std::env::var("DEVSYSTEM_GITHUB_TOKEN").ok().filter(|s| !s.trim().is_empty()).map(Arc::from);
    if github_token.is_none() {
        println!("DEVSYSTEM_GITHUB_TOKEN not set -- approving an issue proposal will fall back to the channel relay if that's configured, else report itself unconfigured");
    }
    // #48 slice 6: all six pieces are required together -- a partially set
    // config (e.g. the client binary path but no channel address) is not a
    // real, usable relay, so it's treated identically to "not configured at
    // all" rather than failing confusingly on the first real approval.
    let issue_channel: Option<IssueChannelConfig> = (|| {
        Some(IssueChannelConfig {
            client_bin: nonempty_env("ISSUE_CHANNEL_CLIENT_BIN")?,
            ct_agent_bin: nonempty_env("CT_AGENT_BIN")?,
            addr: nonempty_env("CT_CHANNEL_ADDR")?,
            noise_key: nonempty_env("CT_CHANNEL_NOISE_KEY")?,
            peer_noise_key: nonempty_env("CT_CHANNEL_PEER_NOISE_KEY")?,
            peer_cert_file: nonempty_env("CT_CHANNEL_PEER_CERT_FILE")?,
        })
    })();
    if issue_channel.is_some() {
        println!("issue-proposal channel relay fully configured -- approving a proposal will dial github_issue_channel_handler over a real channel, not POST to GitHub directly");
    } else {
        println!(
            "issue-proposal channel relay not fully configured (need ISSUE_CHANNEL_CLIENT_BIN, CT_AGENT_BIN, CT_CHANNEL_ADDR, CT_CHANNEL_NOISE_KEY, CT_CHANNEL_PEER_NOISE_KEY, CT_CHANNEL_PEER_CERT_FILE all set) -- approving a proposal will fall back to DEVSYSTEM_GITHUB_TOKEN if that's configured"
        );
    }
    // Same all-or-nothing shape as issue_channel above, same real reason: a
    // partially-set relay is not a real, usable one.
    let document_extraction_channel: Option<DocumentExtractionChannelConfig> = (|| {
        Some(DocumentExtractionChannelConfig {
            client_bin: nonempty_env("DOCUMENT_EXTRACTION_CLIENT_BIN")?,
            ct_agent_bin: nonempty_env("CT_AGENT_BIN")?,
            broker: nonempty_env("CT_CHANNEL_BROKER")?,
            relay: nonempty_env("CT_CHANNEL_RELAY")?,
            grant_hex: nonempty_env("DOCUMENT_EXTRACTION_CHANNEL_GRANT")?,
            holder_key_hex: nonempty_env("DOCUMENT_EXTRACTION_CHANNEL_HOLDER_KEY")?,
            noise_key: nonempty_env("DOCUMENT_EXTRACTION_CHANNEL_NOISE_KEY")?,
        })
    })();
    if document_extraction_channel.is_some() {
        println!("document-extraction channel fully configured -- a RAG upload will fall back to devsystem.document_extraction over a real broker-mediated channel when Unstructured isn't configured");
    } else {
        println!(
            "document-extraction channel not fully configured (need DOCUMENT_EXTRACTION_CLIENT_BIN, CT_AGENT_BIN, CT_CHANNEL_BROKER, CT_CHANNEL_RELAY, DOCUMENT_EXTRACTION_CHANNEL_GRANT, DOCUMENT_EXTRACTION_CHANNEL_HOLDER_KEY, DOCUMENT_EXTRACTION_CHANNEL_NOISE_KEY all set) -- a RAG upload without RAG_UNSTRUCTURED_API_KEY stays a real 503"
        );
    }
    let state = AppState {
        runs_dir: Arc::new(runs_dir),
        write_lock: Arc::new(tokio::sync::Mutex::new(())),
        offers: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        selection_state: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        assistant_url,
        github_token,
        http_client: reqwest::Client::builder().timeout(std::time::Duration::from_secs(90)).build().expect("build http client"),
        rag_embedding_api_key,
        rag_embedding_api_base,
        rag_unstructured_api_key,
        rag_unstructured_api_base,
        rag_db_pool,
        issue_channel,
        document_extraction_channel,
    };

    let static_dir = std::env::var("DEVSYSTEM_STATIC_DIR").unwrap_or_else(|_| "web/static".to_string());

    let app = api_router(state).fallback_service(ServeDir::new(static_dir));

    let addr = "0.0.0.0:8790";
    println!("devsystem-web listening on {addr}");
    let listener = tokio::net::TcpListener::bind(addr).await.expect("bind");
    axum::serve(listener, app).await.expect("serve");
}

/// Defensive cap on backlog/milestone growth -- both persist to state.json on every
/// add, and (unlike history, which only grows one entry per real iteration) nothing
/// stops a client from adding items in a tight loop. Generous enough that no real
/// human workflow hits it, small enough that a runaway script can't grow a run's
/// state.json without bound (matches the host's real, limited disk headroom).
const MAX_LIST_ITEMS: usize = 500;

/// DAU-lens gap found live 2026-08-06 (#382 goal doc §8): every other real free-text
/// field in this codebase has a real length cap (`MAX_REQUIREMENT_STATEMENT_LEN`,
/// `MAX_ACCEPTANCE_CRITERION_LEN`, `MAX_ISSUE_TITLE_LEN`/`MAX_ISSUE_BODY_LEN`) --
/// backlog item text and milestone descriptions were the one real exception,
/// bounded only by axum's generic whole-request body limit (a real 2MB request gets
/// a real, correctly-rejected `413`, but a 500KB single field -- live-confirmed --
/// sailed through as a real `200`). Same reasoning as `MAX_LIST_ITEMS` right above:
/// this persists to state.json on every add, and nothing else stops a client from
/// growing it without bound. Matches `MAX_REQUIREMENT_STATEMENT_LEN`'s own value --
/// both are "one real sentence or two describing what needs to happen," the same
/// shape of field.
const MAX_SHORT_TEXT_LEN: usize = 2_000;

/// Real gap found live 2026-08-06 (stress-test run 42): unlike every per-run list
/// above, `create_run` had no cap at all on the total NUMBER of runs -- confirmed
/// live this deployment already carries 110 real run directories on a host at 91%
/// disk. The sharper real risk isn't disk (each run averages ~15KB): `list_runs`
/// does a real `fs::read_dir` + a full state load for every run on every single
/// `GET /api/runs` call (the Runs panel's own refresh) -- a script hammering
/// `POST /api/runs` with unique ids unboundedly would make that call, and so the
/// whole dashboard, linearly slower for every real user, with zero protection.
/// Same "generous enough no real workflow hits it, small enough a runaway script
/// can't go unbounded" reasoning as `MAX_LIST_ITEMS`; the real delete-run endpoint
/// (run 31) is the intended way to actually stay under it.
const MAX_TOTAL_RUNS: usize = 2000;

fn total_run_count(state: &AppState) -> usize {
    let Ok(entries) = fs::read_dir(state.runs_dir.as_path()) else {
        return 0;
    };
    entries.flatten().filter(|entry| entry.path().join("state.json").exists()).count()
}

fn run_dir(state: &AppState, id: &str) -> PathBuf {
    assert!(valid_run_id(id), "run_dir called with an unvalidated id -- every handler must check valid_run_id(id) first");
    state.runs_dir.join(id)
}

fn run_exists(state: &AppState, id: &str) -> bool {
    run_dir(state, id).join("state.json").exists()
}

/// Real per-run access restriction (#382 #28), narrowly scoped: a signed-in browser
/// user (a real `X-Gate-Email` from the gate) may only view/act on a run they
/// created, UNLESS the run has no recorded owner -- a pre-existing run from before
/// `owner_email` existed, which stays open to any signed-in user rather than locking
/// everyone out of runs nobody "owns" yet.
///
/// A caller with NO gate header at all is deliberately left unrestricted here: every
/// headless CLI tool this whole pipeline mechanism runs on
/// (`devsystem_iterate`/`devsystem_checkin`, this very autonomous loop) has never
/// been gate-authenticated and isn't meant to be -- and `offers/submit` is
/// self-authenticating by real signature (CADS-Tunnel#388), explicitly excluded from
/// the gate at the Caddy layer for exactly that reason. This function only ever
/// narrows *browser* access; it must never be the thing that breaks the headless
/// mechanism.
fn owner_authorized(headers: &axum::http::HeaderMap, run_state: &devsystem_pipeline::runner::RunState) -> bool {
    let Some(caller) = headers.get("x-gate-email").and_then(|v| v.to_str().ok()) else {
        return true;
    };
    match &run_state.owner_email {
        None => true,
        Some(owner) => owner == caller,
    }
}

/// Real gap #10 (#382 goal doc §8, fourteenth stress-test run, 2026-08-06):
/// `devsystem.assistant`'s own `apply_action` calls this exact same
/// `/requirements/{index}/toggle` endpoint a human's GUI click does, with
/// nothing server-side distinguishing the two -- live-verified the assistant
/// will sometimes genuinely mark a requirement verified from a plain chat
/// request, based purely on the implementer's own self-reported feedback,
/// with zero independent evidence and zero mechanical bar, on any run that
/// hasn't declared a `review` role (most runs, by default -- gap #2's own
/// mandatory gate only bites once a run opts in). `X-Actor: devsystem.assistant`
/// is the real signal `apply_action` now sends on every request it makes so
/// this handler can tell the two callers apart -- see
/// `toggle_requirement_handler`'s own use of this for the actual gate.
fn is_assistant_actor(headers: &axum::http::HeaderMap) -> bool {
    headers.get("x-actor").and_then(|v| v.to_str().ok()) == Some("devsystem.assistant")
}

#[derive(Serialize)]
struct RunSummary {
    run_id: String,
    iterations: usize,
    roles: usize,
    added_stages: Vec<String>,
    stalled_stages: Vec<String>,
    risk_count: usize,
    needs_attention: bool,
    /// Real count of proposals (custom panel / pipeline stage / GitHub issue)
    /// genuinely awaiting a human decision on this run -- gap found 2026-08-04:
    /// a run could sit with e.g. a stage proposal unresolved for many loop
    /// firings without ever surfacing in the Runs list, only visible after
    /// opening that run's Pipeline panel specifically. Each of these already
    /// has its own real "propose, human approves" gate (#38/#39/#45); this is
    /// just making the fact that one is WAITING visible where a human
    /// actually looks first, not a new decision mechanism.
    pending_reviews: usize,
    paused: bool,
    /// Real gap found live 2026-08-06 (stress-test run 50): the Runs list already
    /// surfaced `paused`, but never why -- nothing here distinguished a run that
    /// stopped because it needs a look from one that's simply idle between real
    /// work. See `devsystem_pipeline::runner::RunState::pause_reason`'s own doc
    /// comment.
    pause_reason: Option<String>,
    owner_email: Option<String>,
}

/// True when a run is close enough to its own bound that a human should notice it
/// before opening the run, not just after -- the same danger/warn thresholds the
/// GUI's health panel already uses -- OR a real proposal is genuinely waiting on a
/// human decision (`pending_reviews`), evaluated once here so the run list can
/// surface either case (matches the stalled-stage badge precedent: proactive, not
/// only-on-click).
fn needs_attention(health: &RunHealth, pending_reviews: usize) -> bool {
    pending_reviews > 0 || health.consecutive_failures + 1 >= health.criteria.max_consecutive_failures || health.iterations_until_checkin <= 1 || health.checkin_pending
}

async fn list_runs(State(state): State<AppState>, headers: axum::http::HeaderMap) -> impl IntoResponse {
    let mut runs = Vec::new();
    let Ok(entries) = fs::read_dir(state.runs_dir.as_path()) else {
        return Json(runs).into_response();
    };
    for entry in entries.flatten() {
        let id = entry.file_name().to_string_lossy().to_string();
        if !run_exists(&state, &id) {
            continue;
        }
        if let Ok((spec, run_state)) = load_or_init_run(&run_dir(&state, &id), &id) {
            if !owner_authorized(&headers, &run_state) {
                continue;
            }
            let stalled = stalled_stages(&run_state);
            let risk_count = preflight_annotations(&run_state).len() + process_annotations(&spec, &run_state).len();
            let health = run_health(&run_state);
            // Real gap found live 2026-08-06 (stress-test run 50): this only ever
            // summed three of the five real proposal queues -- the exact same
            // undercounting shape already found and fixed once for the Pipeline
            // panel's own chip badge, but this is a genuinely separate call site
            // that never got the same fix. Live-confirmed before touching anything:
            // a real pending panel-removal proposal showed pending_reviews: 0 and
            // needs_attention: false in the actual Runs list, completely invisible
            // where a human looks first.
            let pending_reviews = run_state.pending_panel_proposals.len()
                + run_state.pending_panel_removal_proposals.len()
                + run_state.pending_panel_edit_proposals.len()
                + run_state.pending_stage_proposals.len()
                + run_state.pending_issue_proposals.len()
                // §7.2 gap #2's newest instance (2026-08-07): added to this exact
                // count in the same commit that introduced the field, not left for a
                // future firing to rediscover this same undercounting bug class a
                // sixth queue.
                + run_state.pending_delete_run_proposal.is_some() as usize;
            let alert = needs_attention(&health, pending_reviews);
            let paused = run_state.paused;
            let pause_reason = run_state.pause_reason.clone();
            let owner_email = run_state.owner_email.clone();
            runs.push(RunSummary {
                run_id: id,
                iterations: run_state.history.len(),
                roles: spec.roles.len(),
                added_stages: run_state.added_stages,
                stalled_stages: stalled,
                risk_count,
                needs_attention: alert,
                pending_reviews,
                paused,
                pause_reason,
                owner_email,
            });
        }
    }
    // Real gap found live 2026-08-06 (stress-test run 52): purely alphabetical, no
    // priority at all -- confirmed against the actual deployment before touching
    // anything, the real flagship `webconference-android` run (genuinely paused,
    // needing a real human decision) sat at position 105 of 110 in the Runs list,
    // behind over a hundred alphabetically-earlier scratch runs with nothing
    // outstanding. The same real urgency order this list's own badge logic already
    // uses (GUI: paused > pending review > needs attention > stalled > risk) now
    // drives the sort too, so the run that most needs a look is the one a human
    // actually sees first -- alphabetical only as the tie-break within a tier, not
    // the whole ordering.
    runs.sort_by(|a, b| attention_priority(a).cmp(&attention_priority(b)).then_with(|| a.run_id.cmp(&b.run_id)));
    Json(runs).into_response()
}

/// Lower = more urgent. Mirrors the GUI's own real badge precedence exactly (see
/// `index.html`'s own comment on the run-list badge) so the run at the top of the
/// list is always the one showing the most urgent badge, never a mismatch between
/// what's sorted first and what's visually flagged first.
fn attention_priority(r: &RunSummary) -> u8 {
    if r.paused {
        0
    } else if r.pending_reviews > 0 {
        1
    } else if r.needs_attention {
        2
    } else if !r.stalled_stages.is_empty() {
        3
    } else if r.risk_count > 0 {
        4
    } else {
        5
    }
}

async fn create_run(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Json(body): Json<CreateRunRequest>,
) -> impl IntoResponse {
    let id = body.run_id.trim();
    if !valid_run_id(id) {
        return (StatusCode::BAD_REQUEST, "run_id must be non-empty alphanumeric/-/_ only").into_response();
    }
    let _guard = state.write_lock.lock().await;
    if run_exists(&state, id) {
        return (StatusCode::CONFLICT, "run already exists").into_response();
    }
    if total_run_count(&state) >= MAX_TOTAL_RUNS {
        return (
            StatusCode::BAD_REQUEST,
            format!(
                "this deployment is at its defensive cap of {MAX_TOTAL_RUNS} total runs -- \
                 delete a scratch/finished run first (DELETE /api/runs/{{id}}, or the trash-can \
                 button in the Runs panel) before creating a new one"
            ),
        )
            .into_response();
    }
    let dir = run_dir(&state, id);
    match load_or_init_run(&dir, id) {
        Ok((spec, mut run_state)) => {
            // The same real, gate-verified identity `/api/me` reports -- not trusted
            // from anywhere else, and honestly `None` (not a guess) when the gate
            // header isn't present. A label for "who created this," not an access
            // check (#382 gap: no per-run authorization exists yet).
            run_state.owner_email = headers.get("x-gate-email").and_then(|v| v.to_str().ok()).map(|s| s.to_string());
            match persist_run(&dir, &spec, &run_state) {
                Ok(()) => (StatusCode::CREATED, Json(serde_json::json!({"run_id": id, "roles": spec.roles.len()}))).into_response(),
                Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("persist failed: {e}")).into_response(),
            }
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("init failed: {e}")).into_response(),
    }
}

#[derive(Deserialize)]
struct CreateRunRequest {
    run_id: String,
}

async fn get_run(State(state): State<AppState>, AxPath(id): AxPath<String>, headers: axum::http::HeaderMap) -> impl IntoResponse {
    if !valid_run_id(&id) {
        return (StatusCode::BAD_REQUEST, "run_id must be non-empty alphanumeric/-/_ only").into_response();
    }
    if !run_exists(&state, &id) {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    }
    match load_or_init_run(&run_dir(&state, &id), &id) {
        Ok((spec, run_state)) => {
            if !owner_authorized(&headers, &run_state) {
                return (StatusCode::FORBIDDEN, "this run belongs to a different account").into_response();
            }
            let stalled = stalled_stages(&run_state);
            let health = run_health(&run_state);
            let mut risks = preflight_annotations(&run_state);
            risks.extend(process_annotations(&spec, &run_state));
            Json(serde_json::json!({
                "spec": spec,
                "state": run_state,
                "stalled_stages": stalled,
                "health": health,
                "risks": risks,
                // Real evaluator finding, issue #51: the "+ New Project" dialog claims a
                // "generic 7-stage pipeline template", but a new run deliberately only ever
                // seeds `devsystem.plan` -- speculatively pre-declaring all seven as
                // auction-backed roles would contradict the self-optimizing design (#382's
                // own reframing: "let the system inform itself about the task" rather than
                // build the whole pipeline up front). That left the New Iteration stage
                // dropdown offering only the plan role, forcing every other real stage name
                // to be hand-typed into the unvalidated free-text box -- confirmed live to
                // produce exactly the kind of typo `validate_stage` (issue #49) now rejects
                // (`devsystem.reveiw`), but the GUI still made hand-typing the only path to
                // 6 of 7 real stages. `ALL_STAGES` (single source of truth, same constant
                // `validate_stage` itself checks) lets the GUI offer all seven canonical
                // names as real, pickable options -- without pretending they're declared
                // roles, since `validate_stage` already accepts any of them regardless of
                // `spec.roles`.
                "canonical_stages": ALL_STAGES,
            }))
            .into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response(),
    }
}

/// A single real, actionable item this run is waiting on a human to decide --
/// the data a future guided "stack mode" (operator ask, 2026-08-06: step
/// through every open point one at a time, guided by `devsystem.assistant`,
/// instead of a human having to notice each one live across five separate
/// panels) will actually consume. Deliberately a read-only projection over
/// state that already exists elsewhere (the six real pending-proposal
/// queues -- a fifth grew to six with `pending_delete_run_proposal`,
/// 2026-08-07 -- plus `paused`/`pause_reason`) -- this invents no new state
/// of its own, it only orders and summarizes what's already real. Matches
/// `pending_reviews`'s own established definition of "needs a human
/// decision" exactly (same six queues, same order) plus the one further
/// case that already has its own honest "here's why, a human decides" story:
/// a paused run, e.g. `webconference-android`'s own real M1 checkpoint.
/// Deliberately does NOT include unverified requirements or stalled stages --
/// both are normal, common run states on their own, not a stalled decision
/// nothing can proceed without; folding them in would drown the real
/// open points in noise.
#[derive(Debug, Clone, Serialize)]
struct OpenPoint {
    /// A stable, GUI-switchable kind, not a user-facing label by itself.
    kind: &'static str,
    /// The real id needed to act on this item via its own existing endpoint
    /// (a proposal id, or the literal `"paused"` for the one paused-checkpoint
    /// entry a run can have at most one of).
    id: String,
    summary: String,
    proposed_at: Option<u64>,
    /// The real panel title Approve would destroy or overwrite, structured
    /// data for the GUI's own confirm() gate (2026-08-07) -- NOT parsed out
    /// of `summary`'s human-readable text, the same "no invented signal"
    /// discipline `RiskAnnotation::fix_target` already established. `Some`
    /// only for `panel_removal_proposal`/`panel_edit_proposal` (the two
    /// kinds whose Approve is a real, permanent destructive step on an
    /// EXISTING panel); `None` for every other kind, including
    /// `panel_proposal` (approving that only ever ADDS a panel, nothing
    /// existing to destroy).
    #[serde(skip_serializing_if = "Option::is_none")]
    approve_destroys_panel_title: Option<String>,
}

fn open_points(run_state: &RunState) -> Vec<OpenPoint> {
    let mut points = Vec::new();
    if run_state.paused {
        points.push(OpenPoint {
            kind: "paused_checkpoint",
            id: "paused".to_string(),
            summary: run_state.pause_reason.clone().unwrap_or_else(|| "paused, no reason recorded".to_string()),
            proposed_at: None,
            approve_destroys_panel_title: None,
        });
    }
    for p in &run_state.pending_panel_proposals {
        points.push(OpenPoint { kind: "panel_proposal", id: p.id.clone(), summary: format!("new panel \"{}\"", p.title), proposed_at: Some(p.proposed_at), approve_destroys_panel_title: None });
    }
    for p in &run_state.pending_panel_removal_proposals {
        points.push(OpenPoint {
            kind: "panel_removal_proposal",
            id: p.id.clone(),
            summary: format!("remove panel \"{}\"", p.panel_title),
            proposed_at: Some(p.proposed_at),
            approve_destroys_panel_title: Some(p.panel_title.clone()),
        });
    }
    for p in &run_state.pending_panel_edit_proposals {
        points.push(OpenPoint {
            kind: "panel_edit_proposal",
            id: p.id.clone(),
            summary: format!("edit panel \"{}\" -> \"{}\"", p.old_title, p.new_title),
            proposed_at: Some(p.proposed_at),
            approve_destroys_panel_title: Some(p.old_title.clone()),
        });
    }
    for p in &run_state.pending_stage_proposals {
        points.push(OpenPoint {
            kind: "stage_proposal",
            id: p.id.clone(),
            summary: format!("new stage \"{}\": {}", p.proposal.stage_id, p.proposal.rationale),
            proposed_at: Some(p.proposed_at),
            approve_destroys_panel_title: None,
        });
    }
    for p in &run_state.pending_issue_proposals {
        points.push(OpenPoint { kind: "issue_proposal", id: p.id.clone(), summary: format!("file issue on {}: {}", p.repo, p.title), proposed_at: Some(p.proposed_at), approve_destroys_panel_title: None });
    }
    // §7.2 gap #2's newest instance (2026-08-07): same "real pending-proposal
    // queue, surfaced here so a human sees it without hunting" treatment as
    // the five queues above.
    if let Some(p) = &run_state.pending_delete_run_proposal {
        points.push(OpenPoint { kind: "delete_run_proposal", id: p.id.clone(), summary: format!("delete this run: {}", p.rationale), proposed_at: Some(p.proposed_at), approve_destroys_panel_title: None });
    }
    // Real gap found live 2026-08-07, the same firing after the check-in-pending
    // gate itself shipped: this endpoint's own stated purpose ("every real item
    // this run is actually waiting on a human to decide") never gained a real
    // entry for it -- `checkin_pending` reached the Runs list badge and the
    // per-run health object, but not the one panel purpose-built to be "every
    // real open item, one queue." No `id`/`proposed_at` -- unlike every other
    // kind here, this isn't a discrete record with its own creation timestamp,
    // it's a real, derived fact about the run's own history vs. its criteria.
    // Real gap found live by a non-technical evaluator, issue #48, the same fix that
    // made a fired check-in actually pause the run (above): once that lands, a
    // pending check-in is represented TWICE here whenever it's also the reason the
    // run is paused -- the paused_checkpoint entry above (real pause_reason:
    // "check-in due -- ...") and this one, both naming the identical real fact.
    // Only add this separate entry when the check-in ISN'T already covered by a
    // paused_checkpoint for that same reason -- still needed for the real, reachable
    // case where checkin_pending is true but the run isn't currently paused for it
    // (e.g. a manual resume past an unacknowledged check-in, or legacy state that
    // predates this fix).
    let checkin_already_shown_as_paused_checkpoint =
        run_state.paused && run_state.pause_reason.as_deref().is_some_and(|r| r.starts_with("check-in due"));
    if checkin_pending(run_state) && !checkin_already_shown_as_paused_checkpoint {
        points.push(OpenPoint {
            kind: "checkin_due",
            id: "checkin".to_string(),
            summary: "this run crossed its own check-in cadence and hasn't been acknowledged yet".to_string(),
            proposed_at: None,
            approve_destroys_panel_title: None,
        });
    }
    // Real gap, live-found 2026-08-06: while paused, a next-step draft is
    // shown nested under the paused_checkpoint entry above (see the GUI's own
    // renderOpenPointsPanel) -- but resuming the run makes that entry vanish
    // from this list entirely, and nothing else ever surfaces
    // pending_next_step_drafts. Live-confirmed before fixing: a draft added
    // while paused silently survived a resume in the real backend state, with
    // zero remaining GUI path to see, edit, or delete it -- exactly the
    // "declared but not accessible" pattern this project keeps finding and
    // closing elsewhere. Not deleting the draft on resume -- the operator's
    // own explicit ask was "the user can delete, change and manipulate" a
    // draft directly, never that resuming should silently discard one. Only
    // added here when NOT paused, so a paused run's drafts still nest under
    // its one checkpoint card (comparing 2-3 options together) rather than
    // also appearing as separate entries -- no duplication either way.
    if !run_state.paused {
        for d in &run_state.pending_next_step_drafts {
            points.push(OpenPoint { kind: "next_step_draft", id: d.id.clone(), summary: d.text.clone(), proposed_at: Some(d.proposed_at), approve_destroys_panel_title: None });
        }
    }
    points
}

/// `GET /api/runs/{id}/open-points` -- the real, ordered queue behind "stack
/// mode": every item this run is actually waiting on a human to decide,
/// paused-checkpoint first (the single highest-urgency real state a run can
/// be in, matching `attention_priority`'s own precedence), then the six real
/// pending-proposal queues in `pending_reviews`'s own established order, then
/// a real, unacknowledged fired check-in (2026-08-07 -- see
/// `checkin_pending`'s own doc comment), then any leftover next-step draft.
async fn get_open_points(State(state): State<AppState>, AxPath(id): AxPath<String>, headers: axum::http::HeaderMap) -> impl IntoResponse {
    if !valid_run_id(&id) {
        return (StatusCode::BAD_REQUEST, "run_id must be non-empty alphanumeric/-/_ only").into_response();
    }
    if !run_exists(&state, &id) {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    }
    match load_or_init_run(&run_dir(&state, &id), &id) {
        Ok((_spec, run_state)) => {
            if !owner_authorized(&headers, &run_state) {
                return (StatusCode::FORBIDDEN, "this run belongs to a different account").into_response();
            }
            Json(open_points(&run_state)).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response(),
    }
}

/// Real gap, found live 2026-08-06: the Runs list has grown to 112 real entries on
/// this actual deployment -- almost all of them throwaway scratch/verification runs
/// this project's own stress-test methodology creates on every firing -- with no way
/// to ever remove one. `run_dir`'s own storage model is a plain directory per run, so
/// deleting one is a real, permanent `fs::remove_dir_all`, not a soft "hide" flag --
/// matching the honesty of this codebase's other "this deletes it for real" actions
/// (custom panels, RAG documents), not a fabricated undo. No server-side confirmation
/// param, matching every other destructive endpoint here (`remove_custom_panel`,
/// `remove_rag_document`) -- the GUI's own `confirm()` dialog is the real gate, kept
/// consistent with existing precedent rather than inventing a second mechanism.
async fn delete_run(State(state): State<AppState>, AxPath(id): AxPath<String>, headers: axum::http::HeaderMap) -> impl IntoResponse {
    if !valid_run_id(&id) {
        return (StatusCode::BAD_REQUEST, "run_id must be non-empty alphanumeric/-/_ only").into_response();
    }
    if !run_exists(&state, &id) {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    }
    let _guard = state.write_lock.lock().await;
    // Re-check existence under the lock -- a concurrent delete could have already won
    // the race between the check above and acquiring it.
    if !run_exists(&state, &id) {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    }
    let dir = run_dir(&state, &id);
    match load_or_init_run(&dir, &id) {
        Ok((_spec, run_state)) => {
            if !owner_authorized(&headers, &run_state) {
                return (StatusCode::FORBIDDEN, "this run belongs to a different account").into_response();
            }
        }
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response(),
    }
    match fs::remove_dir_all(&dir) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("failed to remove run directory: {e}")).into_response(),
    }
}

#[derive(serde::Deserialize)]
struct ProposeDeleteRunRequest {
    rationale: String,
}

/// `POST /api/runs/{id}/delete-proposal` -- §7.2 gap #2's newest closed
/// instance (#382 goal doc, 2026-08-07): `devsystem.assistant`'s own
/// assistant-facing half of the propose-then-approve trust model
/// `pending_panel_removal_proposals` already established, applied to deleting
/// a whole run. Records the proposal only -- `delete_run` above (the real,
/// permanent `fs::remove_dir_all`) never runs until a human explicitly
/// approves it below. An `Option`, not a queue: a second real proposal
/// replaces the first rather than accumulating redundant requests to delete
/// the same one run.
async fn propose_delete_run(
    State(state): State<AppState>,
    AxPath(id): AxPath<String>,
    headers: axum::http::HeaderMap,
    Json(body): Json<ProposeDeleteRunRequest>,
) -> impl IntoResponse {
    if !valid_run_id(&id) {
        return (StatusCode::BAD_REQUEST, "run_id must be non-empty alphanumeric/-/_ only").into_response();
    }
    if !run_exists(&state, &id) {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    }
    let rationale = body.rationale.trim().to_string();
    if rationale.is_empty() {
        return (StatusCode::BAD_REQUEST, "rationale must not be empty -- a run disappearing for good deserves a real, stated reason").into_response();
    }
    // Same real gap `validate_proposals` closes for the embedded-proposal path,
    // applied here for the identical reason `propose_stage`'s own rationale check is:
    // a human approving from the open-points queue trusts this text at face value.
    if contains_bidi_control_char(&rationale) {
        return (
            StatusCode::BAD_REQUEST,
            "rationale contains a Unicode bidi control character (e.g. a right-to-left override) \
             -- these can make the visually displayed text not match what's actually stored"
                .to_string(),
        )
            .into_response();
    }
    let _guard = state.write_lock.lock().await;
    let dir = run_dir(&state, &id);
    let (spec, mut run_state) = match load_or_init_run(&dir, &id) {
        Ok(v) => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("load failed: {e}")).into_response(),
    };
    if !owner_authorized(&headers, &run_state) {
        return (StatusCode::FORBIDDEN, "this run belongs to a different account").into_response();
    }
    let proposal = PendingDeleteRunProposal { id: format!("{:016x}", rand::random::<u64>()), rationale, proposed_at: unix_now() };
    run_state.pending_delete_run_proposal = Some(proposal.clone());
    match persist_run(&dir, &spec, &run_state) {
        Ok(()) => Json(proposal).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("persist failed: {e}")).into_response(),
    }
}

/// `POST /api/runs/{id}/delete-proposal/{proposal_id}/approve` -- the actual
/// destructive step: real `fs::remove_dir_all`, identical to `delete_run`'s
/// own. Unlike `approve_panel_removal`, there is no "already removed some
/// other way, clear the stale proposal anyway" case to handle gracefully --
/// once this run is gone, there is no longer a `state.json` to persist a
/// cleared proposal into, so a `204` with no body is the only honest
/// response (matching `delete_run`'s own).
async fn approve_delete_run(State(state): State<AppState>, AxPath((id, proposal_id)): AxPath<(String, String)>, headers: axum::http::HeaderMap) -> impl IntoResponse {
    if !valid_run_id(&id) {
        return (StatusCode::BAD_REQUEST, "run_id must be non-empty alphanumeric/-/_ only").into_response();
    }
    if !run_exists(&state, &id) {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    }
    let _guard = state.write_lock.lock().await;
    let dir = run_dir(&state, &id);
    match load_or_init_run(&dir, &id) {
        Ok((_spec, run_state)) => {
            if !owner_authorized(&headers, &run_state) {
                return (StatusCode::FORBIDDEN, "this run belongs to a different account").into_response();
            }
            let Some(proposal) = &run_state.pending_delete_run_proposal else {
                return (StatusCode::NOT_FOUND, format!("no pending delete proposal with id {proposal_id:?}")).into_response();
            };
            if proposal.id != proposal_id {
                return (StatusCode::NOT_FOUND, format!("no pending delete proposal with id {proposal_id:?}")).into_response();
            }
        }
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response(),
    }
    match fs::remove_dir_all(&dir) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("failed to remove run directory: {e}")).into_response(),
    }
}

/// `POST /api/runs/{id}/delete-proposal/{proposal_id}/reject` -- the safe
/// direction: the run was never touched, so this only ever drops the
/// pending proposal.
async fn reject_delete_run(State(state): State<AppState>, AxPath((id, proposal_id)): AxPath<(String, String)>, headers: axum::http::HeaderMap) -> impl IntoResponse {
    if !valid_run_id(&id) {
        return (StatusCode::BAD_REQUEST, "run_id must be non-empty alphanumeric/-/_ only").into_response();
    }
    if !run_exists(&state, &id) {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    }
    let _guard = state.write_lock.lock().await;
    let dir = run_dir(&state, &id);
    let (spec, mut run_state) = match load_or_init_run(&dir, &id) {
        Ok(v) => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("load failed: {e}")).into_response(),
    };
    if !owner_authorized(&headers, &run_state) {
        return (StatusCode::FORBIDDEN, "this run belongs to a different account").into_response();
    }
    let matches = matches!(&run_state.pending_delete_run_proposal, Some(p) if p.id == proposal_id);
    if !matches {
        return (StatusCode::NOT_FOUND, format!("no pending delete proposal with id {proposal_id:?}")).into_response();
    }
    run_state.pending_delete_run_proposal = None;
    match persist_run(&dir, &spec, &run_state) {
        Ok(()) => Json(serde_json::json!({"rejected": proposal_id})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("persist failed: {e}")).into_response(),
    }
}

/// The same [`AbortCriteria::default`] every real `iterate_run` call folds a record
/// against, plus how close this run currently is to each bound -- so a human can see
/// risk (an imminent abort, an overdue check-in) at a glance instead of only after
/// `run_iteration` has already decided it.
#[derive(Serialize)]
struct RunHealth {
    criteria: AbortCriteria,
    consecutive_failures: u32,
    iterations_completed: u32,
    iterations_until_checkin: u32,
    iterations_until_ceiling: u32,
    /// Real, persistent signal a one-time toast can never give -- see
    /// [`devsystem_pipeline::runner::checkin_pending`]'s own doc comment.
    checkin_pending: bool,
}

fn run_health(run_state: &devsystem_pipeline::runner::RunState) -> RunHealth {
    let criteria = run_state.criteria;
    let completed = run_state.history.len() as u32;
    // Real bug found live by the incompetent-agent stress test (#382 goal
    // doc §8/§9, 2026-08-06): `checkin_every: 0` has no validation anywhere
    // (update_criteria only rejects max_iterations/max_consecutive_failures
    // at zero) and `should_checkin`'s own real fallback for it means the
    // mandatory cadence is effectively disabled -- only the hard
    // `max_iterations` ceiling still forces a check-in. Reporting a
    // hardcoded `0` here used to actively misrepresent that as "due right
    // now" -- and, worse, fed straight into `needs_attention`'s own `<= 1`
    // threshold below, permanently false-flagging such a run as needing
    // attention for a reason that was never real. Report the real distance
    // to the actual next check-in event (the ceiling) instead.
    // Real gap found live by a non-technical evaluator, issue #48: this used to
    // compute purely off `completed % checkin_every`, with no idea whether the
    // LAST real boundary it crossed was ever acknowledged
    // (`checkin_acknowledged_through`) -- so a run sitting on an overdue,
    // unacknowledged check-in (`checkin_pending: true`) could still report
    // `iterations_until_checkin: 1`, counting down toward a future boundary as if
    // the one that already fired didn't matter. `checkin_pending` is the real,
    // authoritative "is one actually due right now" signal; when it's true, the
    // honest distance is `0` (due now), not a countdown to the next one.
    let pending = checkin_pending(run_state);
    let until_checkin = if pending {
        0
    } else if criteria.checkin_every == 0 {
        criteria.max_iterations.saturating_sub(completed)
    } else {
        let rem = completed % criteria.checkin_every;
        if rem == 0 { criteria.checkin_every } else { criteria.checkin_every - rem }
    };
    RunHealth {
        criteria,
        consecutive_failures: run_state.consecutive_failures,
        iterations_completed: completed,
        iterations_until_checkin: until_checkin,
        iterations_until_ceiling: criteria.max_iterations.saturating_sub(completed),
        checkin_pending: pending,
    }
}

#[derive(Deserialize)]
struct IterateRequest {
    stage: String,
    feedback: String,
    #[serde(default = "default_true")]
    succeeded: bool,
    #[serde(default)]
    proposals: Vec<StageProposal>,
    /// Real requirement traceability (#47 follow-up slice) -- which
    /// `state.requirements` indices this iteration claims to address.
    /// `#[serde(default)]` so every existing caller (nothing claimed any
    /// requirement before this field existed) keeps working unchanged.
    #[serde(default)]
    requirement_indices: Vec<usize>,
}

fn default_true() -> bool {
    true
}

async fn iterate_run(
    State(state): State<AppState>,
    AxPath(id): AxPath<String>,
    headers: axum::http::HeaderMap,
    Json(body): Json<IterateRequest>,
) -> impl IntoResponse {
    if !valid_run_id(&id) {
        return (StatusCode::BAD_REQUEST, "run_id must be non-empty alphanumeric/-/_ only").into_response();
    }
    let _guard = state.write_lock.lock().await;
    let dir = run_dir(&state, &id);
    let (mut spec, mut run_state) = match load_or_init_run(&dir, &id) {
        Ok(v) => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("load failed: {e}")).into_response(),
    };
    if !owner_authorized(&headers, &run_state) {
        return (StatusCode::FORBIDDEN, "this run belongs to a different account").into_response();
    }
    if run_state.paused {
        return (StatusCode::CONFLICT, "run is paused -- resume it first (POST /api/runs/{id}/resume)").into_response();
    }
    // Real gap found live by a non-technical evaluator, issue #46: `paused` alone
    // isn't a real bound -- `POST /resume` clears it without re-checking whether the
    // ceiling that caused the pause is still true, so a submission right after
    // Resume was accepted and recorded past max_iterations/max_consecutive_failures
    // every time, one Resume click at a time. Checked independently of `paused` so
    // it refuses regardless of how `paused` got cleared.
    if let Some(reason) = devsystem_pipeline::runner::ceiling_already_reached(&run_state, &run_state.criteria, body.succeeded) {
        return (StatusCode::CONFLICT, reason).into_response();
    }
    // DAU-lens gap found live 2026-08-06 (#382 goal doc §8), same lens and shape as the
    // add_requirement/validate_proposals fixes: this used to `find` and reject on the
    // FIRST out-of-range index in the batch only. `requirement_indices` is a real batch
    // (a role-filler can claim several requirements addressed in one iteration), so a
    // submission with more than one bad index needed one resubmit per additional
    // mistake to discover them all. Live-confirmed before fixing: `[99, 150]` against a
    // run with zero requirements only ever named 99.
    //
    // Now calls the shared `validate_requirement_indices` (pipeline crate) instead of
    // keeping this logic duplicated inline -- see its own doc comment: the local,
    // non-`--remote` `devsystem_iterate` CLI path calls `run_iteration` directly with no
    // HTTP layer to share this check through any other way, and had zero protection
    // until that shared function existed.
    if let Err(e) = validate_requirement_indices(&run_state, &body.requirement_indices) {
        return (StatusCode::BAD_REQUEST, e).into_response();
    }
    // Real gap found live 2026-08-06 (stress-test run 45): confirmed live a real
    // `succeeded: true` iteration with `""` or `"   "` as its feedback got a real
    // 200 -- see `validate_feedback`'s own doc comment (pipeline crate) for why this
    // matters and why the check lives there, shared with the local CLI's identical
    // real entry point, not duplicated inline here.
    if let Err(e) = validate_feedback(&body.feedback) {
        return (StatusCode::BAD_REQUEST, e).into_response();
    }
    // Real gap found live by the incompetent-agent stress test (#382 goal doc §8,
    // 2026-08-06): a role-filler's own embedded `proposals` -- applied *immediately*
    // to the live PipelineSpec, no human review step at all (see "How the pipeline
    // proposes and grows its own stages") -- had zero content validation here. Its
    // twelfth run found the fix itself was incomplete: this check used to live
    // inline, right here, duplicated from (and drifting independently of)
    // `propose_stage`'s own equivalent check -- and the local `devsystem_iterate`
    // CLI's non-`--remote` mode, which calls `run_iteration` directly with no HTTP
    // layer at all, still had no protection whatsoever. Moved to
    // `devsystem_pipeline::validate_proposals` so every real entry point shares the
    // identical gate instead of each needing to remember its own copy.
    if let Err(e) = validate_proposals(&body.proposals) {
        return (StatusCode::BAD_REQUEST, e).into_response();
    }
    // Real evaluator finding, issue #49: the mandatory review gate's entire notion
    // of "a review happened" is keyed on this exact field, and until this fix it was
    // the one free-text field in this whole API with no validation of any kind --
    // see `validate_stage`'s own doc comment (pipeline crate) for the full live
    // repro (empty/whitespace/5,000-char stages, and an undeclared role accepted
    // identically to a real one).
    if let Err(e) = validate_stage(&body.stage, &spec, &body.proposals) {
        return (StatusCode::BAD_REQUEST, e).into_response();
    }
    // Real idempotency guard, found necessary live (2026-08-05): a same-day window of
    // overlapping devsystem-web container instances during a redeploy let two
    // functionally-identical iterations both land with the same computed iteration
    // number (two "iteration: 8" entries, no "9" -- confirmed directly in this run's
    // own real history). `write_lock` already serializes concurrent requests *within*
    // one process; it can't help against two separate process instances each running
    // their own independent lock. This is process-external, not stage-specific -- a
    // submission that's byte-identical (stage/feedback/succeeded/proposals/
    // requirement_indices) to the run's own immediately-preceding entry is rejected
    // outright, regardless of *why* a duplicate arrived (a client retry, an overlapping
    // deploy, two callers doing the same real work independently).
    //
    // Now calls the shared `duplicate_of_last_iteration` (pipeline crate) instead of
    // keeping this comparison duplicated inline -- see its own doc comment: this was a
    // fourth "two real entry points, one bug class" gap, and `devsystem_iterate`'s
    // local, non-`--remote` CLI path had zero protection against a re-run/retried
    // record.json until that shared function existed.
    if let Some(dup_iteration) = duplicate_of_last_iteration(
        &run_state.history,
        &body.stage,
        &body.feedback,
        body.succeeded,
        &body.proposals,
        &body.requirement_indices,
    ) {
        return (
            StatusCode::CONFLICT,
            format!("this submission is byte-identical to iteration {dup_iteration}, the run's own immediately-preceding entry -- refusing to record it as a distinct, new iteration"),
        )
            .into_response();
    }

    let iteration = run_state.history.len() as u32 + 1;
    // Real identity (GitHub issue #38: the exact same iteration got submitted
    // twice, byte-for-byte, into webconference-android's real history, with
    // no field on the record to tell the two apart or say which one was
    // real). Server-generated, the same `format!("{:016x}",
    // rand::random::<u64>())` convention every other real id in this
    // codebase already uses -- deliberately never accepted from the request
    // body, so a role-filler/client cannot forge or collide it.
    // Real evaluator finding, issue #40: the same real, gate-verified identity
    // `/api/me` and `owner_email` already use -- deliberately never trusted from the
    // request body, so a client cannot claim to be someone else. Honestly `None` for
    // an M2M/`--remote` bearer-token caller (no browser session, no x-gate-email
    // header): a service-account credential is not a human identity, so leaving this
    // absent is more honest than fabricating one.
    let submitted_by = headers.get("x-gate-email").and_then(|v| v.to_str().ok()).map(|s| s.to_string());
    let record = IterationRecord {
        run_id: id.clone(),
        stage: body.stage,
        iteration,
        feedback: body.feedback,
        succeeded: body.succeeded,
        proposals: body.proposals,
        requirement_indices: body.requirement_indices,
        id: Some(format!("{:016x}", rand::random::<u64>())),
        submitted_at: Some(unix_now()),
        submitted_by,
    };

    let memory_path = dir.join("memory.jsonl");
    let envelope = envelope_from_iteration(&record, &run_state.requirements);
    if let Err(e) = append_to_memory_log(&memory_path, &envelope) {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("memory log failed: {e}")).into_response();
    }

    let criteria = run_state.criteria;
    let outcome = run_iteration(&mut spec, &mut run_state, record, &criteria);

    if let Err(e) = persist_run(&dir, &spec, &run_state) {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("persist failed: {e}")).into_response();
    }

    let outcome_str = match outcome {
        RunOutcome::Continue => "Continue",
        RunOutcome::CheckinDue => "CheckinDue",
        RunOutcome::Abort => "Abort",
    };
    // Real gap found live by a non-technical evaluator, issues #46/#47/#48: this
    // response never carried WHY an Abort or CheckinDue fired -- consecutive
    // failures vs. the iteration ceiling vs. the check-in cadence collapse into the
    // identical outcome string, even though `run_state.pause_reason` (set by
    // `run_iteration`, same match order it itself checks the real conditions in)
    // has always distinguished them correctly server-side. The GUI's own status
    // line had to fall back to a generic "too many consecutive failures, or the
    // iteration ceiling was reached" -- unable to tell an operator which one
    // actually happened even though the server already knew. Included here so the
    // one real, authoritative reason reaches the surface that reports it, instead
    // of the client re-deriving or guessing at it.
    Json(serde_json::json!({
        "outcome": outcome_str,
        "iteration": iteration,
        "roles_now": spec.roles.len(),
        "added_stages": run_state.added_stages,
        "pause_reason": run_state.pause_reason,
    }))
    .into_response()
}

async fn checkin_run(State(state): State<AppState>, AxPath(id): AxPath<String>, headers: axum::http::HeaderMap) -> impl IntoResponse {
    if !valid_run_id(&id) {
        return (StatusCode::BAD_REQUEST, "run_id must be non-empty alphanumeric/-/_ only").into_response();
    }
    if !run_exists(&state, &id) {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    }
    let (_spec, run_state) = match load_or_init_run(&run_dir(&state, &id), &id) {
        Ok(v) => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response(),
    };
    if !owner_authorized(&headers, &run_state) {
        return (StatusCode::FORBIDDEN, "this run belongs to a different account").into_response();
    }
    match render_plan_markdown(&run_state) {
        Some(markdown) => Json(serde_json::json!({"markdown": markdown})).into_response(),
        None => (StatusCode::NOT_FOUND, "no iteration history yet").into_response(),
    }
}

/// `devsystem.remember`'s durable log (`memory.jsonl`) was write-only until now --
/// `iterate_run` has appended a real envelope every iteration since the mechanism
/// was built, but nothing could ever read it back. Real data, not a stub: whatever
/// this run's actual history produced.
async fn memory_run(State(state): State<AppState>, AxPath(id): AxPath<String>, headers: axum::http::HeaderMap) -> impl IntoResponse {
    if !valid_run_id(&id) {
        return (StatusCode::BAD_REQUEST, "run_id must be non-empty alphanumeric/-/_ only").into_response();
    }
    if !run_exists(&state, &id) {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    }
    match load_or_init_run(&run_dir(&state, &id), &id) {
        Ok((_spec, run_state)) if !owner_authorized(&headers, &run_state) => {
            return (StatusCode::FORBIDDEN, "this run belongs to a different account").into_response();
        }
        Ok(_) => {}
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response(),
    }
    let memory_path = run_dir(&state, &id).join("memory.jsonl");
    match read_memory_log(&memory_path) {
        Ok(entries) => Json(entries).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response(),
    }
}

/// The only place `Trust::Governed` should ever get set: a human, through the GUI,
/// explicitly marking one memory entry as reviewed. Never automatic.
async fn govern_memory(
    State(state): State<AppState>,
    AxPath((id, index)): AxPath<(String, usize)>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    if !valid_run_id(&id) {
        return (StatusCode::BAD_REQUEST, "run_id must be non-empty alphanumeric/-/_ only").into_response();
    }
    if !run_exists(&state, &id) {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    }
    match load_or_init_run(&run_dir(&state, &id), &id) {
        Ok((_spec, run_state)) if !owner_authorized(&headers, &run_state) => {
            return (StatusCode::FORBIDDEN, "this run belongs to a different account").into_response();
        }
        Ok(_) => {}
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response(),
    }
    let _guard = state.write_lock.lock().await;
    let memory_path = run_dir(&state, &id).join("memory.jsonl");
    match govern_memory_entry(&memory_path, index) {
        Ok(entries) => Json(entries).into_response(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (StatusCode::NOT_FOUND, e.to_string()).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
struct UpdateCriteriaRequest {
    max_iterations: u32,
    max_consecutive_failures: u32,
    checkin_every: u32,
}

/// Real, generous-but-finite ceiling on any single `AbortCriteria` field (#382
/// goal doc §8, stress-test finding, 2026-08-06). `update_criteria` already
/// rejected `0` (an immediately-dead run), but had no upper bound at all --
/// live-verified before this fix: `{"max_iterations": 4294967295, ...}` (u32::MAX)
/// got a real `200`, turning this run's "bounded super loop" -- #382's own
/// stated, central architectural principle -- into one that's unbounded for any
/// practical purpose. Ten thousand is deliberately generous (real runs in this
/// project use single- or low-double-digit values), not a tight arbitrary
/// limit -- a run that ever genuinely needed more than this would already have
/// needed real human reconfiguration long before hitting it.
const MAX_ABORT_CRITERIA_VALUE: u32 = 10_000;

/// The "customize" half of "control and customize the pipeline for actual use" (#382):
/// every run starts on [`AbortCriteria::default`], but a run that's earned trust (or
/// needs a tighter leash) can have its own bounded-loop criteria tuned here, persisted
/// on `RunState` itself so `run_iteration`/`run_health` immediately pick it up.
async fn update_criteria(
    State(state): State<AppState>,
    AxPath(id): AxPath<String>,
    headers: axum::http::HeaderMap,
    Json(body): Json<UpdateCriteriaRequest>,
) -> impl IntoResponse {
    if !valid_run_id(&id) {
        return (StatusCode::BAD_REQUEST, "run_id must be non-empty alphanumeric/-/_ only").into_response();
    }
    if !run_exists(&state, &id) {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    }
    if body.max_iterations == 0 || body.max_consecutive_failures == 0 {
        return (StatusCode::BAD_REQUEST, "max_iterations and max_consecutive_failures must be at least 1").into_response();
    }
    if body.max_iterations > MAX_ABORT_CRITERIA_VALUE || body.max_consecutive_failures > MAX_ABORT_CRITERIA_VALUE || body.checkin_every > MAX_ABORT_CRITERIA_VALUE {
        return (
            StatusCode::BAD_REQUEST,
            format!(
                "max_iterations, max_consecutive_failures, and checkin_every must each be at most \
                 {MAX_ABORT_CRITERIA_VALUE} -- this run's bounded super loop needs a real, finite bound, \
                 not a number so large it's unbounded in practice"
            ),
        )
            .into_response();
    }
    let _guard = state.write_lock.lock().await;
    let dir = run_dir(&state, &id);
    let (spec, mut run_state) = match load_or_init_run(&dir, &id) {
        Ok(v) => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("load failed: {e}")).into_response(),
    };
    if !owner_authorized(&headers, &run_state) {
        return (StatusCode::FORBIDDEN, "this run belongs to a different account").into_response();
    }
    run_state.criteria = AbortCriteria {
        max_iterations: body.max_iterations,
        max_consecutive_failures: body.max_consecutive_failures,
        checkin_every: body.checkin_every,
    };
    match persist_run(&dir, &spec, &run_state) {
        Ok(()) => Json(serde_json::json!({"criteria": run_state.criteria})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("persist failed: {e}")).into_response(),
    }
}

/// Real "stop, let me correct something" control -- operator feedback: "ich weiss
/// nicht... wie ich es anhalten kann um es zu korrigieren." Sets `RunState::paused`;
/// `iterate_run` refuses new iterations while it's set.
async fn pause_run(State(state): State<AppState>, AxPath(id): AxPath<String>, headers: axum::http::HeaderMap) -> impl IntoResponse {
    set_paused(state, id, true, headers).await
}

async fn resume_run(State(state): State<AppState>, AxPath(id): AxPath<String>, headers: axum::http::HeaderMap) -> impl IntoResponse {
    set_paused(state, id, false, headers).await
}

async fn set_paused(state: AppState, id: String, paused: bool, headers: axum::http::HeaderMap) -> axum::response::Response {
    if !valid_run_id(&id) {
        return (StatusCode::BAD_REQUEST, "run_id must be non-empty alphanumeric/-/_ only").into_response();
    }
    if !run_exists(&state, &id) {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    }
    let _guard = state.write_lock.lock().await;
    let dir = run_dir(&state, &id);
    let (spec, mut run_state) = match load_or_init_run(&dir, &id) {
        Ok(v) => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("load failed: {e}")).into_response(),
    };
    if !owner_authorized(&headers, &run_state) {
        return (StatusCode::FORBIDDEN, "this run belongs to a different account").into_response();
    }
    run_state.paused = paused;
    // Real "why paused" distinction (RunState::pause_reason's own doc comment, run
    // 49): a direct pause/resume through this exact route is the one real trigger
    // that's always a deliberate human action, never automatic -- reflects that
    // honestly rather than leaving a stale reason from an earlier auto-pause
    // showing after a human explicitly took over, and clears it on resume so a
    // later auto-pause doesn't inherit a stale human-set one either.
    run_state.pause_reason = if paused { Some("paused manually".to_string()) } else { None };
    match persist_run(&dir, &spec, &run_state) {
        Ok(()) => Json(serde_json::json!({"paused": run_state.paused, "pause_reason": run_state.pause_reason})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("persist failed: {e}")).into_response(),
    }
}

/// Real, explicit acknowledgment that a human has actually reviewed this run's
/// most recently fired check-in -- see [`RunState::checkin_acknowledged_through`]'s
/// own doc comment for the real gap this closes. A visible, deliberate action
/// (matches this project's own "never let state change silently" discipline),
/// not something inferred from merely viewing the check-in markdown -- a
/// background refresh or an automated poll must never count as a real human
/// review. Idempotent: acknowledging when nothing is pending is a real, cheap
/// no-op, not an error -- a careless double-click must never fail.
async fn acknowledge_checkin(State(state): State<AppState>, AxPath(id): AxPath<String>, headers: axum::http::HeaderMap) -> impl IntoResponse {
    if !valid_run_id(&id) {
        return (StatusCode::BAD_REQUEST, "run_id must be non-empty alphanumeric/-/_ only").into_response();
    }
    if !run_exists(&state, &id) {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    }
    let _guard = state.write_lock.lock().await;
    let dir = run_dir(&state, &id);
    let (spec, mut run_state) = match load_or_init_run(&dir, &id) {
        Ok(v) => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("load failed: {e}")).into_response(),
    };
    if !owner_authorized(&headers, &run_state) {
        return (StatusCode::FORBIDDEN, "this run belongs to a different account").into_response();
    }
    run_state.checkin_acknowledged_through = run_state.history.len() as u32;
    // Real gap found live by a non-technical evaluator, issue #48: `run_iteration`
    // now actually pauses the run when a check-in is due (it always should have --
    // see `RunOutcome::CheckinDue`'s own doc comment), reusing the same real
    // `paused` mechanism every other pause reason uses. But a cadence check-in is
    // conceptually a review CHECKPOINT, not a stop like a milestone or a ceiling --
    // its own AbortCriteria field literally means "pause at least this often for a
    // human to look," not "pause forever until a separate deliberate decision."
    // Acknowledging it IS that decision. Only resumes when the real, current pause
    // reason is a check-in pause specifically (the tag `run_iteration` itself
    // writes) -- a milestone/ceiling/manual pause that happens to coincide is left
    // exactly as it was; acknowledging a check-in must never silently wave through
    // an unrelated, still-real reason to stay stopped.
    if run_state.paused && run_state.pause_reason.as_deref().is_some_and(|r| r.starts_with("check-in due")) {
        run_state.paused = false;
        run_state.pause_reason = None;
    }
    match persist_run(&dir, &spec, &run_state) {
        Ok(()) => Json(serde_json::json!({"checkin_acknowledged_through": run_state.checkin_acknowledged_through})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("persist failed: {e}")).into_response(),
    }
}

#[derive(Deserialize)]
struct AddBacklogItemRequest {
    text: String,
}

/// A run's real backlog -- operator feedback: "ich möchte die Liste der
/// Taskliste... ein echtes Backlog pro Run." Human-editable today; the natural
/// place a future `devsystem.assistant` role would read/write from once it exists,
/// but the data + API need to exist first regardless of who ends up populating it.
async fn add_backlog_item(
    State(state): State<AppState>,
    AxPath(id): AxPath<String>,
    headers: axum::http::HeaderMap,
    Json(body): Json<AddBacklogItemRequest>,
) -> impl IntoResponse {
    if !valid_run_id(&id) {
        return (StatusCode::BAD_REQUEST, "run_id must be non-empty alphanumeric/-/_ only").into_response();
    }
    if !run_exists(&state, &id) {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    }
    let text = body.text.trim().to_string();
    if text.is_empty() {
        return (StatusCode::BAD_REQUEST, "text must not be empty").into_response();
    }
    if text.len() > MAX_SHORT_TEXT_LEN {
        return (StatusCode::BAD_REQUEST, format!("text must be under {MAX_SHORT_TEXT_LEN} characters")).into_response();
    }
    if contains_bidi_control_char(&text) {
        return (
            StatusCode::BAD_REQUEST,
            "text contains a Unicode bidi control character (e.g. a right-to-left override) -- \
             these can make the visually displayed text not match what's actually stored"
                .to_string(),
        )
            .into_response();
    }
    let _guard = state.write_lock.lock().await;
    let dir = run_dir(&state, &id);
    let (spec, mut run_state) = match load_or_init_run(&dir, &id) {
        Ok(v) => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("load failed: {e}")).into_response(),
    };
    if !owner_authorized(&headers, &run_state) {
        return (StatusCode::FORBIDDEN, "this run belongs to a different account").into_response();
    }
    if run_state.backlog.len() >= MAX_LIST_ITEMS {
        return (StatusCode::BAD_REQUEST, format!("backlog is at its defensive cap of {MAX_LIST_ITEMS} items")).into_response();
    }
    run_state.backlog.push(BacklogItem { text, done: false });
    match persist_run(&dir, &spec, &run_state) {
        Ok(()) => Json(serde_json::json!({"backlog": run_state.backlog})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("persist failed: {e}")).into_response(),
    }
}

async fn toggle_backlog_item(
    State(state): State<AppState>,
    AxPath((id, index)): AxPath<(String, usize)>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    if !valid_run_id(&id) {
        return (StatusCode::BAD_REQUEST, "run_id must be non-empty alphanumeric/-/_ only").into_response();
    }
    if !run_exists(&state, &id) {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    }
    let _guard = state.write_lock.lock().await;
    let dir = run_dir(&state, &id);
    let (spec, mut run_state) = match load_or_init_run(&dir, &id) {
        Ok(v) => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("load failed: {e}")).into_response(),
    };
    if !owner_authorized(&headers, &run_state) {
        return (StatusCode::FORBIDDEN, "this run belongs to a different account").into_response();
    }
    let Some(item) = run_state.backlog.get_mut(index) else {
        return (StatusCode::NOT_FOUND, format!("no backlog item at index {index}")).into_response();
    };
    item.done = !item.done;
    match persist_run(&dir, &spec, &run_state) {
        Ok(()) => Json(serde_json::json!({"backlog": run_state.backlog})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("persist failed: {e}")).into_response(),
    }
}

#[derive(Deserialize)]
struct AddMilestoneRequest {
    description: String,
}

/// Operator feedback: "ich möchte nicht nur Iterationen, sondern auch Milestones
/// als Abbruchkriterium definieren können." See `Milestone`/`toggle_milestone` in
/// devsystem-pipeline for the real semantics (the achieved transition auto-pauses
/// the run).
async fn add_milestone(
    State(state): State<AppState>,
    AxPath(id): AxPath<String>,
    headers: axum::http::HeaderMap,
    Json(body): Json<AddMilestoneRequest>,
) -> impl IntoResponse {
    if !valid_run_id(&id) {
        return (StatusCode::BAD_REQUEST, "run_id must be non-empty alphanumeric/-/_ only").into_response();
    }
    if !run_exists(&state, &id) {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    }
    let description = body.description.trim().to_string();
    if description.is_empty() {
        return (StatusCode::BAD_REQUEST, "description must not be empty").into_response();
    }
    if description.len() > MAX_SHORT_TEXT_LEN {
        return (StatusCode::BAD_REQUEST, format!("description must be under {MAX_SHORT_TEXT_LEN} characters")).into_response();
    }
    if contains_bidi_control_char(&description) {
        return (
            StatusCode::BAD_REQUEST,
            "description contains a Unicode bidi control character (e.g. a right-to-left override) \
             -- these can make the visually displayed text not match what's actually stored, which \
             matters most here since achieving a milestone auto-pauses the run as a real checkpoint"
                .to_string(),
        )
            .into_response();
    }
    let _guard = state.write_lock.lock().await;
    let dir = run_dir(&state, &id);
    let (spec, mut run_state) = match load_or_init_run(&dir, &id) {
        Ok(v) => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("load failed: {e}")).into_response(),
    };
    if !owner_authorized(&headers, &run_state) {
        return (StatusCode::FORBIDDEN, "this run belongs to a different account").into_response();
    }
    if run_state.milestones.len() >= MAX_LIST_ITEMS {
        return (StatusCode::BAD_REQUEST, format!("milestones is at its defensive cap of {MAX_LIST_ITEMS} items")).into_response();
    }
    run_state.milestones.push(Milestone { description, achieved: false });
    match persist_run(&dir, &spec, &run_state) {
        Ok(()) => Json(serde_json::json!({"milestones": run_state.milestones})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("persist failed: {e}")).into_response(),
    }
}

async fn toggle_milestone_handler(
    State(state): State<AppState>,
    AxPath((id, index)): AxPath<(String, usize)>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    if !valid_run_id(&id) {
        return (StatusCode::BAD_REQUEST, "run_id must be non-empty alphanumeric/-/_ only").into_response();
    }
    if !run_exists(&state, &id) {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    }
    let _guard = state.write_lock.lock().await;
    let dir = run_dir(&state, &id);
    let (spec, mut run_state) = match load_or_init_run(&dir, &id) {
        Ok(v) => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("load failed: {e}")).into_response(),
    };
    if !owner_authorized(&headers, &run_state) {
        return (StatusCode::FORBIDDEN, "this run belongs to a different account").into_response();
    }
    if let Err(e) = toggle_milestone(&mut run_state, index) {
        return (StatusCode::NOT_FOUND, e).into_response();
    }
    match persist_run(&dir, &spec, &run_state) {
        Ok(()) => Json(serde_json::json!({"milestones": run_state.milestones, "paused": run_state.paused})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("persist failed: {e}")).into_response(),
    }
}

#[derive(Deserialize)]
struct AddRequirementRequest {
    statement: String,
    #[serde(default)]
    acceptance_criteria: Vec<String>,
    /// Real provenance (#382 goal doc, gap #1): `None`/absent means a human is adding
    /// this directly (the GUI's own Requirements panel never sends this field); a
    /// non-empty value names the stage that proposed it (`devsystem_assistant` sends
    /// `"devsystem.assistant"`). Trimmed and empty-string-normalized to `None` below,
    /// same convention as every other string field on this request.
    #[serde(default)]
    proposed_by: Option<String>,
}

const MAX_REQUIREMENT_STATEMENT_LEN: usize = 2_000;
const MAX_ACCEPTANCE_CRITERIA: usize = 20;
const MAX_ACCEPTANCE_CRITERION_LEN: usize = 500;
const MIN_ACCEPTANCE_CRITERION_ALNUM_CHARS: usize = 5;

/// Real, structured requirement management (2026-08-04 operator ask, grounded
/// in researched industry practice -- EARS notation, spec-driven-development's
/// "the spec is the prompt", traceSDD/Spec Kit-style acceptance criteria) --
/// see `Requirement`'s own doc comment for why this is a distinct kind of run
/// state from milestones/backlog. A requirement with no acceptance criteria
/// defeats its entire point (nothing to actually check), so unlike
/// milestones/backlog this rejects an empty `acceptance_criteria` list, not
/// just an empty statement.
/// Shared statement/acceptance-criteria validation for both adding a new
/// requirement and (#382 goal doc, issue #37) correcting one in place --
/// extracted out of what used to be `add_requirement`'s own inline body so
/// `update_requirement_handler` enforces the identical real rules rather than
/// a second, separately-maintained copy that could quietly drift. Returns the
/// trimmed statement and the cleaned acceptance-criteria list on success, or
/// the exact `(StatusCode, message)` response the caller should return as-is.
fn validate_requirement_fields(statement: &str, acceptance_criteria: &[String]) -> Result<(String, Vec<String>), (StatusCode, String)> {
    let statement = statement.trim().to_string();
    if statement.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "statement must not be empty".to_string()));
    }
    if statement.len() > MAX_REQUIREMENT_STATEMENT_LEN {
        return Err((StatusCode::BAD_REQUEST, format!("statement must be under {MAX_REQUIREMENT_STATEMENT_LEN} characters")));
    }
    if contains_bidi_control_char(&statement) {
        return Err((
            StatusCode::BAD_REQUEST,
            "statement contains a Unicode bidi control character (e.g. a right-to-left override) \
             -- these can make the visually displayed text not match what's actually stored, which \
             a reviewer relies on reading correctly"
                .to_string(),
        ));
    }
    // Real gap found live by the incompetent-agent stress test (#382 goal doc
    // §8, 2026-08-05, DAU lens): a completely non-EARS statement like "asdf"
    // got a real 200 -- nothing checked that a "requirement" even attempted
    // the format this whole feature is built around. "SHALL" is the one
    // universal, defining keyword across every real EARS requirement type
    // (WHEN/IF/WHILE-triggered or ubiquitous with no trigger clause at all --
    // requiring "WHEN" specifically would wrongly reject legitimate EARS
    // variants), so checking for it alone is a safe, low-false-positive proxy,
    // same crude-but-honest mechanical convention as every other check in this
    // codebase -- not real NLP, just "did you even attempt the format."
    //
    // The stress test's thirteenth real run (2026-08-06) found this exact
    // check had the SAME false-positive shape as a plain substring search
    // always has: `.contains("shall")` matches inside completely unrelated
    // words -- "shallow", "Marshall", "installshall" -- not just the real
    // EARS keyword. Live-verified before this fix: "Do a shallow
    // implementation of the login flow for now" (zero trigger/behavior
    // clause, not remotely EARS-shaped) got a real 200, purely because
    // "shallow" contains "shall" as a raw substring. Fixed by splitting on
    // non-alphanumeric boundaries and requiring "shall" as an exact WORD,
    // same word-splitting convention `distinct_word_count` already
    // established elsewhere in this codebase (case-insensitive,
    // punctuation-collapsing) -- "shallow"/"marshall" no longer match,
    // "SHALL," / "shall." / "shall/could" still correctly do.
    let has_shall_as_a_real_word = statement.to_lowercase().split(|c: char| !c.is_alphanumeric()).any(|w| w == "shall");
    if !has_shall_as_a_real_word {
        return Err((
            StatusCode::BAD_REQUEST,
            "statement doesn't look like a real EARS requirement -- expected something containing \
             \"SHALL\" (e.g. \"WHEN <trigger>, THE SYSTEM SHALL <behavior>\"), not a free-form note."
                .to_string(),
        ));
    }
    let acceptance_criteria: Vec<String> = acceptance_criteria.iter().map(|c| c.trim().to_string()).filter(|c| !c.is_empty()).collect();
    if acceptance_criteria.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "at least one non-empty acceptance criterion is required".to_string()));
    }
    if acceptance_criteria.len() > MAX_ACCEPTANCE_CRITERIA {
        return Err((StatusCode::BAD_REQUEST, format!("acceptance_criteria is at its defensive cap of {MAX_ACCEPTANCE_CRITERIA} items")));
    }
    // Real gap found and closed by the incompetent-agent stress test (#382 goal
    // doc §8, 2026-08-05): a live round-trip proved criteria like "ok", ".", and
    // "done" -- plus a criterion that was ONLY a zero-width space (U+200B), which
    // .trim() doesn't strip (it's Unicode category Cf/Format, not White_Space, so
    // it renders as apparently blank in the GUI while passing the "not empty"
    // check) -- all sailed through as real, checkable criteria. Same mechanical-
    // check convention as MIN_REVIEW_FEEDBACK_LEN: requiring a minimum count of
    // alphanumeric characters (not just total length) catches both problems with
    // one rule -- a criterion with too few real letters/digits to be checkable,
    // and one with none at all (which is what an invisible-character-only string
    // actually is under this count).
    //
    // DAU-lens gap found live 2026-08-06, applying the same "does this hide a
    // real, distinct, actionable thing" lens that found the multi-finding bugs
    // in preflight.rs's own risk checks: these two rules used to each `find`
    // and reject on the FIRST bad criterion only. A careless human/role-filler
    // submitting three simultaneously-bad criteria got told about exactly one,
    // fixed it, resubmitted, got told about the next -- a real, avoidable
    // round-trip per extra mistake. Now reports every bad criterion in the one
    // real request that actually has them, not one repeated retry-and-learn
    // cycle.
    let bad_criteria: Vec<String> = acceptance_criteria
        .iter()
        .filter_map(|c| {
            if c.len() > MAX_ACCEPTANCE_CRITERION_LEN {
                Some(format!("\"{c}\" is over {MAX_ACCEPTANCE_CRITERION_LEN} characters"))
            } else if c.chars().filter(|ch| ch.is_alphanumeric()).count() < MIN_ACCEPTANCE_CRITERION_ALNUM_CHARS {
                Some(format!(
                    "\"{c}\" doesn't have enough real content to be checkable (minimum \
                     {MIN_ACCEPTANCE_CRITERION_ALNUM_CHARS} letters/digits) -- \"ok\", \".\", or an \
                     invisible character aren't real acceptance criteria"
                ))
            } else if contains_bidi_control_char(c) {
                Some(format!(
                    "\"{c}\" contains a Unicode bidi control character (e.g. a right-to-left \
                     override) -- these can make the visually displayed text not match what's \
                     actually stored, which a reviewer relies on reading correctly"
                ))
            } else {
                None
            }
        })
        .collect();
    if !bad_criteria.is_empty() {
        return Err((StatusCode::BAD_REQUEST, format!("acceptance criteria: {}", bad_criteria.join("; "))));
    }
    Ok((statement, acceptance_criteria))
}

async fn add_requirement(
    State(state): State<AppState>,
    AxPath(id): AxPath<String>,
    headers: axum::http::HeaderMap,
    Json(body): Json<AddRequirementRequest>,
) -> impl IntoResponse {
    if !valid_run_id(&id) {
        return (StatusCode::BAD_REQUEST, "run_id must be non-empty alphanumeric/-/_ only").into_response();
    }
    if !run_exists(&state, &id) {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    }
    let (statement, acceptance_criteria) = match validate_requirement_fields(&body.statement, &body.acceptance_criteria) {
        Ok(v) => v,
        Err((status, msg)) => return (status, msg).into_response(),
    };
    let _guard = state.write_lock.lock().await;
    let dir = run_dir(&state, &id);
    let (spec, mut run_state) = match load_or_init_run(&dir, &id) {
        Ok(v) => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("load failed: {e}")).into_response(),
    };
    if !owner_authorized(&headers, &run_state) {
        return (StatusCode::FORBIDDEN, "this run belongs to a different account").into_response();
    }
    if run_state.requirements.len() >= MAX_LIST_ITEMS {
        return (StatusCode::BAD_REQUEST, format!("requirements is at its defensive cap of {MAX_LIST_ITEMS} items")).into_response();
    }
    let proposed_by = body.proposed_by.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    // Real evaluator finding, issue #55: the same real, gate-verified identity
    // `/api/me`/`owner_email`/`submitted_by` already use -- genuinely separate from
    // `proposed_by` just above (that field answers human-vs-LLM-authored, never which
    // real account). `None` for a header-less call, same honesty convention as every
    // other real actor field in this codebase.
    let created_by = headers.get("x-gate-email").and_then(|v| v.to_str().ok()).map(|s| s.to_string());
    run_state.requirements.push(Requirement {
        statement,
        acceptance_criteria,
        verified: false,
        verified_criteria: Vec::new(),
        auto_judge: false,
        proposed_by,
        created_by,
    });
    match persist_run(&dir, &spec, &run_state) {
        Ok(()) => Json(serde_json::json!({"requirements": run_state.requirements})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("persist failed: {e}")).into_response(),
    }
}

/// `POST /api/runs/{id}/requirements/{index}/update` -- issue #37 (#382 goal doc):
/// until this, a requirement could be added but never corrected. A wrong or
/// unsatisfiable statement (the issue's own live example: `webconference-android`
/// requirement #1 conflating iteration-succeeded with requirement-verified) was
/// permanently load-bearing for the review gate and coverage, with the only
/// "fix" being deleting and recreating the entire run.
///
/// Deliberately an update-in-place (the issue's own suggested resolution #1),
/// not a remove -- `requirement_indices` on iterations is positional, so a
/// remove would renumber every existing iteration's references (the exact
/// problem #35 already documents for the Markdown export's numbering). Update
/// avoids that entirely.
///
/// Resets `verified`/`verified_criteria` to their unconfirmed state: the
/// specific text a human previously verified may no longer be what's being
/// asked, so carrying old confirmations forward against changed criteria would
/// misrepresent them as still applying. `proposed_by`/`created_by` (who
/// originally authored this requirement) are left untouched -- correcting the
/// text doesn't change who first wrote it.
async fn update_requirement_handler(
    State(state): State<AppState>,
    AxPath((id, index)): AxPath<(String, usize)>,
    headers: axum::http::HeaderMap,
    Json(body): Json<AddRequirementRequest>,
) -> impl IntoResponse {
    if !valid_run_id(&id) {
        return (StatusCode::BAD_REQUEST, "run_id must be non-empty alphanumeric/-/_ only").into_response();
    }
    if !run_exists(&state, &id) {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    }
    let (statement, acceptance_criteria) = match validate_requirement_fields(&body.statement, &body.acceptance_criteria) {
        Ok(v) => v,
        Err((status, msg)) => return (status, msg).into_response(),
    };
    let _guard = state.write_lock.lock().await;
    let dir = run_dir(&state, &id);
    let (spec, mut run_state) = match load_or_init_run(&dir, &id) {
        Ok(v) => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("load failed: {e}")).into_response(),
    };
    if !owner_authorized(&headers, &run_state) {
        return (StatusCode::FORBIDDEN, "this run belongs to a different account").into_response();
    }
    let Some(requirement) = run_state.requirements.get_mut(index) else {
        return (StatusCode::NOT_FOUND, format!("no requirement at index {index}")).into_response();
    };
    requirement.statement = statement;
    requirement.acceptance_criteria = acceptance_criteria;
    requirement.verified = false;
    requirement.verified_criteria = Vec::new();
    match persist_run(&dir, &spec, &run_state) {
        Ok(()) => Json(serde_json::json!({"requirements": run_state.requirements})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("persist failed: {e}")).into_response(),
    }
}

async fn toggle_requirement_handler(
    State(state): State<AppState>,
    AxPath((id, index)): AxPath<(String, usize)>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    if !valid_run_id(&id) {
        return (StatusCode::BAD_REQUEST, "run_id must be non-empty alphanumeric/-/_ only").into_response();
    }
    if !run_exists(&state, &id) {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    }
    let _guard = state.write_lock.lock().await;
    let dir = run_dir(&state, &id);
    let (spec, mut run_state) = match load_or_init_run(&dir, &id) {
        Ok(v) => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("load failed: {e}")).into_response(),
    };
    if !owner_authorized(&headers, &run_state) {
        return (StatusCode::FORBIDDEN, "this run belongs to a different account").into_response();
    }
    // Real gap #10 (#382 goal doc §8, fourteenth stress-test run): when the
    // assistant relay is the caller AND this toggle would mark the
    // requirement verified (not un-verify it -- that direction stays
    // unconditionally safe, same as the human path), require the same real
    // evidence the review gate already enforces, UNCONDITIONALLY -- not just
    // on runs that happen to have declared `review`. A human's own direct
    // click is unaffected; this only closes the chat-driven path a plain
    // instruction could otherwise talk the assistant into.
    if is_assistant_actor(&headers) {
        if let Some(requirement) = run_state.requirements.get(index) {
            if !requirement.verified {
                if let Err(e) = qualifying_review_evidence(&run_state, index) {
                    return (
                        StatusCode::CONFLICT,
                        format!(
                            "{e} This check applies unconditionally to devsystem.assistant-driven \
                             verification, regardless of whether this run declares a review role -- a \
                             human's own direct click in the Requirements panel is not affected."
                        ),
                    )
                        .into_response();
                }
            }
        }
    }
    if let Err(e) = toggle_requirement(&spec, &mut run_state, index) {
        let status = if e.contains("no requirement at index") { StatusCode::NOT_FOUND } else { StatusCode::CONFLICT };
        return (status, e).into_response();
    }
    match persist_run(&dir, &spec, &run_state) {
        Ok(()) => Json(serde_json::json!({"requirements": run_state.requirements})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("persist failed: {e}")).into_response(),
    }
}

/// `GET /api/runs/{id}/requirements/export` -- real requirements export (#382 goal
/// doc §4.4, gap #7): a real Markdown document, not just the raw JSON `GET
/// /api/runs/{id}` already exposes. `owner_authorized` gates this the same as every
/// other run-scoped read that isn't the top-level listing (matching
/// `toggle_requirement_handler`'s own gating), since a run's requirements can be a
/// real, sensitive spec.
async fn export_requirements(State(state): State<AppState>, AxPath(id): AxPath<String>, headers: axum::http::HeaderMap) -> impl IntoResponse {
    if !valid_run_id(&id) {
        return (StatusCode::BAD_REQUEST, "run_id must be non-empty alphanumeric/-/_ only").into_response();
    }
    if !run_exists(&state, &id) {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    }
    match load_or_init_run(&run_dir(&state, &id), &id) {
        Ok((_spec, run_state)) => {
            if !owner_authorized(&headers, &run_state) {
                return (StatusCode::FORBIDDEN, "this run belongs to a different account").into_response();
            }
            let md = render_requirements_markdown(&id, &run_state.requirements, &run_state.history);
            (
                [
                    (axum::http::header::CONTENT_TYPE, "text/markdown; charset=utf-8"),
                    (axum::http::header::CONTENT_DISPOSITION, "attachment; filename=\"requirements.md\""),
                ],
                md,
            )
                .into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response(),
    }
}

/// `POST /api/runs/{id}/requirements/{index}/auto-judge/toggle` -- lets a requirement's
/// owner opt it into (or back out of) LLM judgment, per `Requirement::auto_judge`'s own
/// doc comment (operator decision 2026-08-05: human by default, explicit opt-in for
/// "automode"). Toggling this alone never changes `verified`/`verified_criteria` --
/// it only authorizes future judgment, doesn't perform any itself.
async fn toggle_requirement_auto_judge_handler(
    State(state): State<AppState>,
    AxPath((id, index)): AxPath<(String, usize)>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    if !valid_run_id(&id) {
        return (StatusCode::BAD_REQUEST, "run_id must be non-empty alphanumeric/-/_ only").into_response();
    }
    if !run_exists(&state, &id) {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    }
    let _guard = state.write_lock.lock().await;
    let dir = run_dir(&state, &id);
    let (spec, mut run_state) = match load_or_init_run(&dir, &id) {
        Ok(v) => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("load failed: {e}")).into_response(),
    };
    if !owner_authorized(&headers, &run_state) {
        return (StatusCode::FORBIDDEN, "this run belongs to a different account").into_response();
    }
    if let Err(e) = toggle_requirement_auto_judge(&mut run_state, index) {
        return (StatusCode::NOT_FOUND, e).into_response();
    }
    match persist_run(&dir, &spec, &run_state) {
        Ok(()) => Json(serde_json::json!({"requirements": run_state.requirements})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("persist failed: {e}")).into_response(),
    }
}

/// Real, purely additive follow-up (2026-08-05) to `toggle_requirement_handler`
/// -- see `Requirement::verified_criteria`'s own doc comment for why this is a
/// separate signal from the whole-requirement `verified` flag.
async fn toggle_acceptance_criterion_handler(
    State(state): State<AppState>,
    AxPath((id, req_index, criterion_index)): AxPath<(String, usize, usize)>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    if !valid_run_id(&id) {
        return (StatusCode::BAD_REQUEST, "run_id must be non-empty alphanumeric/-/_ only").into_response();
    }
    if !run_exists(&state, &id) {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    }
    let _guard = state.write_lock.lock().await;
    let dir = run_dir(&state, &id);
    let (spec, mut run_state) = match load_or_init_run(&dir, &id) {
        Ok(v) => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("load failed: {e}")).into_response(),
    };
    if !owner_authorized(&headers, &run_state) {
        return (StatusCode::FORBIDDEN, "this run belongs to a different account").into_response();
    }
    // Real evaluator finding, issue #55: the platform's own highest-stakes verdict
    // signal -- a *confirmed* criterion, not just a filed report -- used to carry no
    // provenance at all. Same real, gate-verified identity every other real actor
    // field in this codebase uses; honestly None for a header-less caller.
    let confirmed_by = headers.get("x-gate-email").and_then(|v| v.to_str().ok()).map(|s| s.to_string());
    if let Err(e) = toggle_acceptance_criterion(&mut run_state, req_index, criterion_index, confirmed_by, unix_now()) {
        return (StatusCode::NOT_FOUND, e).into_response();
    }
    match persist_run(&dir, &spec, &run_state) {
        Ok(()) => Json(serde_json::json!({"requirements": run_state.requirements})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("persist failed: {e}")).into_response(),
    }
}

#[derive(Deserialize)]
struct SetRepoUrlRequest {
    repo_url: String,
}

#[derive(Deserialize)]
struct SetOperatorPubkeyRequest {
    operator_pubkey_hex: String,
}

/// `POST /api/runs/{id}/operator-pubkey` -- a real gap found 2026-08-05: `PipelineSpec.operator_pubkey_hex`
/// (the Agent-Fabric channel operator key gating every real `SignedChannelGrant` for this run's
/// roles, see `pipeline/src/lib.rs`) was only ever settable via `full_spec`/`plan_only_spec` at
/// run-creation time -- no way to set it on an existing run through the GUI/API at all, discovered
/// while trying to unblock CADS-devsystem#7's real channel-discovery proposal. Mutates `spec`, not
/// `run_state` (unlike `set_repo_url`) -- `operator_pubkey_hex` genuinely lives on the pipeline spec.
/// Validates it's a real, well-formed ed25519 public key (64 lowercase hex chars = 32 bytes) rather
/// than accepting an arbitrary string that would only fail much later at grant-verification time.
async fn set_operator_pubkey(
    State(state): State<AppState>,
    AxPath(id): AxPath<String>,
    headers: axum::http::HeaderMap,
    Json(body): Json<SetOperatorPubkeyRequest>,
) -> impl IntoResponse {
    if !valid_run_id(&id) {
        return (StatusCode::BAD_REQUEST, "run_id must be non-empty alphanumeric/-/_ only").into_response();
    }
    if !run_exists(&state, &id) {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    }
    let trimmed = body.operator_pubkey_hex.trim().to_lowercase();
    if !trimmed.is_empty() && (trimmed.len() != 64 || !trimmed.chars().all(|c| c.is_ascii_hexdigit())) {
        return (
            StatusCode::BAD_REQUEST,
            "operator_pubkey_hex must be exactly 64 lowercase hex chars (a real 32-byte ed25519 public key), or empty to clear it",
        )
            .into_response();
    }
    let _guard = state.write_lock.lock().await;
    let dir = run_dir(&state, &id);
    let (mut spec, run_state) = match load_or_init_run(&dir, &id) {
        Ok(v) => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("load failed: {e}")).into_response(),
    };
    if !owner_authorized(&headers, &run_state) {
        return (StatusCode::FORBIDDEN, "this run belongs to a different account").into_response();
    }
    spec.operator_pubkey_hex = if trimmed.is_empty() { None } else { Some(trimmed) };
    match persist_run(&dir, &spec, &run_state) {
        Ok(()) => Json(serde_json::json!({"operator_pubkey_hex": spec.operator_pubkey_hex})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("persist failed: {e}")).into_response(),
    }
}

/// Operator feedback: "ich möchte Zugang zu aktuellem Code." This is the one
/// place a human tells the pipeline which real repository a run is actually
/// building -- devsystem-web itself never infers or hardcodes one (#382's own
/// project-agnostic promise). An empty `repo_url` clears it. Only a basic
/// `https://` sanity check here; the GUI does the real work of talking to
/// GitHub's API, client-side, against whatever URL a human actually confirms.
async fn set_repo_url(
    State(state): State<AppState>,
    AxPath(id): AxPath<String>,
    headers: axum::http::HeaderMap,
    Json(body): Json<SetRepoUrlRequest>,
) -> impl IntoResponse {
    if !valid_run_id(&id) {
        return (StatusCode::BAD_REQUEST, "run_id must be non-empty alphanumeric/-/_ only").into_response();
    }
    if !run_exists(&state, &id) {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    }
    let trimmed = body.repo_url.trim().to_string();
    if !trimmed.is_empty() && !trimmed.starts_with("https://") {
        return (StatusCode::BAD_REQUEST, "repo_url must start with https:// (or be empty to clear it)").into_response();
    }
    // DAU-lens gap found live 2026-08-06 (#382 goal doc §8), the same class just
    // closed for backlog/milestone text: repo_url had no length cap at all, unlike
    // every sibling free-text field. Live-confirmed before this fix: a real
    // 500,000-character repo_url got a real 200. A genuine GitHub URL is nowhere
    // near this length; reusing the same generous-but-finite MAX_SHORT_TEXT_LEN
    // every other short free-text field already uses.
    if trimmed.len() > MAX_SHORT_TEXT_LEN {
        return (StatusCode::BAD_REQUEST, format!("repo_url must be under {MAX_SHORT_TEXT_LEN} characters")).into_response();
    }
    let _guard = state.write_lock.lock().await;
    let dir = run_dir(&state, &id);
    let (spec, mut run_state) = match load_or_init_run(&dir, &id) {
        Ok(v) => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("load failed: {e}")).into_response(),
    };
    if !owner_authorized(&headers, &run_state) {
        return (StatusCode::FORBIDDEN, "this run belongs to a different account").into_response();
    }
    run_state.repo_url = if trimmed.is_empty() { None } else { Some(trimmed) };
    match persist_run(&dir, &spec, &run_state) {
        Ok(()) => Json(serde_json::json!({"repo_url": run_state.repo_url})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("persist failed: {e}")).into_response(),
    }
}

#[derive(Deserialize)]
struct AcceptedBidReq {
    holder_label: String,
    price: u64,
}

#[derive(Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
enum SetRoleFillModeRequest {
    Auction,
    Dedicated {
        label: String,
        #[serde(default)]
        accepted_bid: Option<AcceptedBidReq>,
    },
}

/// `POST /api/runs/{id}/roles/{tag}/fill-mode` (#382 Roles panel ask 1/4, extended by
/// the operator's direct-accept ask): switch one role between `Auction` (today's
/// default, unchanged) and `Dedicated` -- either a plain hand-typed label, or a real
/// bid accepted directly from the live auction view (`accepted_bid`, a point-in-time
/// snapshot of `holder_label`/`price`; see [`RoleFillMode`]'s own doc comment).
/// `:tag` is validated against the run's real live spec, not accepted blindly -- an
/// unknown role tag is a real `400`, not silently stored as dead data nobody's
/// `spec.roles` will ever match.
async fn set_role_fill_mode(
    State(state): State<AppState>,
    AxPath((id, tag)): AxPath<(String, String)>,
    headers: axum::http::HeaderMap,
    Json(body): Json<SetRoleFillModeRequest>,
) -> impl IntoResponse {
    if !valid_run_id(&id) {
        return (StatusCode::BAD_REQUEST, "run_id must be non-empty alphanumeric/-/_ only").into_response();
    }
    if !run_exists(&state, &id) {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    }
    if let SetRoleFillModeRequest::Dedicated { label, accepted_bid } = &body {
        if label.trim().is_empty() {
            return (StatusCode::BAD_REQUEST, "label must be non-empty for a dedicated role").into_response();
        }
        // Real gap found live by the incompetent-agent stress test (#382 goal doc
        // §8, 2026-08-06): the same bidi-control-character (Trojan Source) class
        // already closed for requirement statement/criteria, milestones, backlog,
        // custom-panel title, and stage-proposal rationale -- never checked here,
        // even though `label` is exactly the same shape (short, human-typed,
        // displayed and trusted in the Roles panel's own badge/popover, per
        // `RoleFillMode::Dedicated`'s own doc comment: a real, deliberate choice
        // of who fills a role, not decorative). Live-confirmed before fixing: a
        // label reading "Trusted Agent" + U+202E + reversed text sailed through
        // untouched, visually hiding "This is a really malicious agent" behind an
        // apparently-trustworthy label a human relies on to decide who to trust
        // with a role.
        if contains_bidi_control_char(label) {
            return (
                StatusCode::BAD_REQUEST,
                "label contains a Unicode bidi control character (e.g. a right-to-left override) -- \
                 these can make the visually displayed text not match what's actually stored"
                    .to_string(),
            )
                .into_response();
        }
        // Real gap found live 2026-08-06 (stress-test run 53): the outer `label`
        // above was already validated non-empty, but `accepted_bid.holder_label` --
        // a real identity record of who actually won the bid being accepted, not
        // decorative -- had zero validation. Live-confirmed before this fix: both a
        // byte-empty and a whitespace-only holder_label got a real 200, silently
        // recorded as if a real bidder's name.
        if let Some(bid) = accepted_bid {
            if bid.holder_label.trim().is_empty() {
                return (StatusCode::BAD_REQUEST, "accepted_bid.holder_label must be non-empty").into_response();
            }
            if contains_bidi_control_char(&bid.holder_label) {
                return (
                    StatusCode::BAD_REQUEST,
                    "accepted_bid.holder_label contains a Unicode bidi control character (e.g. a \
                     right-to-left override) -- these can make the visually displayed text not match \
                     what's actually stored"
                        .to_string(),
                )
                    .into_response();
            }
        }
    }
    let _guard = state.write_lock.lock().await;
    let dir = run_dir(&state, &id);
    let (spec, mut run_state) = match load_or_init_run(&dir, &id) {
        Ok(v) => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("load failed: {e}")).into_response(),
    };
    if !owner_authorized(&headers, &run_state) {
        return (StatusCode::FORBIDDEN, "this run belongs to a different account").into_response();
    }
    if !spec.roles.iter().any(|r| r.tag == tag) {
        return (StatusCode::BAD_REQUEST, format!("no role tagged {tag:?} in this run's live spec")).into_response();
    }
    // Real gap found live (#382 goal doc §7.2/§8, 2026-08-07): `no_price_ceiling`
    // (preflight.rs) has always flagged a role as unbounded, but nothing anywhere
    // actually enforced a real ceiling once it WAS set -- confirmed by reading
    // every real call site before this fix, exactly as that risk's own doc comment
    // says. This is the one real, local, non-financial-guessing place to close
    // part of that gap: accepting a bid directly (skipping the auction) is a real,
    // one-click decision a human or the assistant makes right here, so this is
    // where a genuine ceiling should actually bind. Auction-cleared bids still
    // aren't checked anywhere -- not claimed solved, see the updated risk evidence.
    if let SetRoleFillModeRequest::Dedicated { accepted_bid: Some(bid), .. } = &body {
        let stage_id = spec.roles.iter().find(|r| r.tag == tag).and_then(|r| match &r.service {
            ServiceType::Custom(s) => Some(s.clone()),
            _ => None,
        });
        if let Some(stage_id) = stage_id {
            if let Some(ceiling) = devsystem_pipeline::runner::price_ceiling_for(&run_state, &stage_id) {
                if bid.price > ceiling {
                    return (
                        StatusCode::BAD_REQUEST,
                        format!(
                            "this role's own real price_ceiling is {ceiling} -- accepting this bid at {} would exceed it; accept a lower bid, or raise the ceiling first by re-proposing this stage with a higher price_ceiling",
                            bid.price
                        ),
                    )
                        .into_response();
                }
            }
        }
    }
    let mode = match body {
        SetRoleFillModeRequest::Auction => RoleFillMode::Auction,
        SetRoleFillModeRequest::Dedicated { label, accepted_bid } => RoleFillMode::Dedicated {
            label: label.trim().to_string(),
            accepted_bid: accepted_bid.map(|b| devsystem_pipeline::runner::AcceptedBid { holder_label: b.holder_label, price: b.price }),
        },
    };
    // `Auction` is the implicit default for a tag absent from the map -- storing it
    // explicitly would just be dead weight that never affects behavior, so switching
    // back to auction removes the entry instead.
    match &mode {
        RoleFillMode::Auction => {
            run_state.role_fill_modes.remove(&tag);
        }
        RoleFillMode::Dedicated { .. } => {
            run_state.role_fill_modes.insert(tag.clone(), mode);
        }
    }
    match persist_run(&dir, &spec, &run_state) {
        Ok(()) => Json(serde_json::json!({"tag": tag, "fill_mode": run_state.role_fill_modes.get(&tag)})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("persist failed: {e}")).into_response(),
    }
}

fn rag_index_path(state: &AppState, id: &str) -> PathBuf {
    run_dir(state, id).join("rag_index.json")
}

fn load_rag_index(state: &AppState, id: &str) -> Option<rag::RagIndex> {
    fs::read_to_string(rag_index_path(state, id)).ok().and_then(|s| serde_json::from_str(&s).ok())
}

fn persist_rag_index(state: &AppState, id: &str, index: &rag::RagIndex) -> Result<(), String> {
    let s = serde_json::to_string_pretty(index).map_err(|e| e.to_string())?;
    fs::write(rag_index_path(state, id), s).map_err(|e| e.to_string())
}

/// Real batch embedding of whatever in `index` doesn't have one yet -- chunks
/// from a fresh `sync_repo` always lack one (real chunks are never embedded
/// twice), manual documents already carrying an embedding (preserved across a
/// re-sync) are skipped so a re-sync doesn't re-spend real embedding cost on
/// unchanged text. One real batched API call for however many texts need it,
/// not one call per chunk -- `embed_texts` already accepts a batch. No-ops
/// (and costs nothing) when no embedding credential is configured, or when
/// there's genuinely nothing new to embed. Logs, never panics, on a real
/// provider failure -- a sync/upload still succeeds with keyword-only search
/// for the new content rather than failing the whole operation over a
/// secondary feature.
async fn embed_index_in_place(state: &AppState, index: &mut rag::RagIndex) {
    let Some(api_key) = state.rag_embedding_api_key.clone() else {
        return;
    };
    let chunk_idxs: Vec<usize> = index.chunks.iter().enumerate().filter(|(_, c)| c.embedding.is_none()).map(|(i, _)| i).collect();
    let doc_idxs: Vec<usize> = index.manual_documents.iter().enumerate().filter(|(_, d)| d.embedding.is_none()).map(|(i, _)| i).collect();
    if chunk_idxs.is_empty() && doc_idxs.is_empty() {
        return;
    }
    let texts: Vec<String> =
        chunk_idxs.iter().map(|&i| index.chunks[i].text.clone()).chain(doc_idxs.iter().map(|&i| index.manual_documents[i].text.clone())).collect();
    match rag::embed_texts(&state.http_client, &state.rag_embedding_api_base, &api_key, &texts).await {
        Ok(embeddings) => {
            for (offset, &i) in chunk_idxs.iter().enumerate() {
                index.chunks[i].embedding = Some(embeddings[offset].clone());
            }
            for (offset, &i) in doc_idxs.iter().enumerate() {
                index.manual_documents[i].embedding = Some(embeddings[chunk_idxs.len() + offset].clone());
            }
        }
        Err(e) => eprintln!("devsystem-web: RAG embedding failed for {} chunk(s)/{} document(s), continuing keyword-only for them: {e}", chunk_idxs.len(), doc_idxs.len()),
    }
}

/// Mirrors the current in-memory `index` into real Postgres (when
/// `DATABASE_URL` is configured), source-kind by source-kind so
/// [`vector_store::replace_chunks`]'s wholesale-replace semantics apply
/// independently to repo-synced chunks vs. manual/uploaded documents --
/// re-syncing a repo must not wipe uploaded documents from Postgres either,
/// the same real invariant `sync_rag`'s own JSON-index splicing already
/// protects. A real Postgres write failure is logged, never panics or fails
/// the caller's request: Postgres here is a real second index alongside the
/// JSON one, not (yet) the sole source of truth, so a transient DB error
/// degrades to "semantic search via Postgres missed this update," not "the
/// whole upload/sync failed."
async fn sync_vector_store(state: &AppState, run_id: &str, index: &rag::RagIndex) {
    let Some(pool) = &state.rag_db_pool else {
        return;
    };
    let now = unix_now() as i64;
    let repo_chunks: Vec<vector_store::ChunkToStore> = index
        .chunks
        .iter()
        .map(|c| vector_store::ChunkToStore { path: c.path.clone(), chunk_index: c.index as i32, text: c.text.clone(), embedding: c.embedding.clone() })
        .collect();
    if let Err(e) = vector_store::replace_chunks(pool, run_id, "repo_sync", &repo_chunks, now).await {
        eprintln!("devsystem-web: could not mirror repo-synced chunks into Postgres for run {run_id}: {e}");
    }
    let manual_chunks: Vec<vector_store::ChunkToStore> = index
        .manual_documents
        .iter()
        .map(|d| vector_store::ChunkToStore { path: d.path.clone(), chunk_index: 0, text: d.text.clone(), embedding: d.embedding.clone() })
        .collect();
    if let Err(e) = vector_store::replace_chunks(pool, run_id, "manual_document", &manual_chunks, now).await {
        eprintln!("devsystem-web: could not mirror manual documents into Postgres for run {run_id}: {e}");
    }
}

/// `POST /api/runs/{id}/rag/sync` (#382 task 29): real fetch + chunk + index of
/// whatever `RunState.repo_url` currently names, via [`rag::sync_repo`]. Owner-
/// restricted like every other GUI mutation -- this hits GitHub's real API on the
/// caller's behalf, a real (if small) cost, not something any signed-in user
/// should be able to trigger against a run they don't own.
async fn sync_rag(State(state): State<AppState>, AxPath(id): AxPath<String>, headers: axum::http::HeaderMap) -> impl IntoResponse {
    if !valid_run_id(&id) {
        return (StatusCode::BAD_REQUEST, "run_id must be non-empty alphanumeric/-/_ only").into_response();
    }
    if !run_exists(&state, &id) {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    }
    let run_state = match load_or_init_run(&run_dir(&state, &id), &id) {
        Ok((_spec, s)) => s,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response(),
    };
    if !owner_authorized(&headers, &run_state) {
        return (StatusCode::FORBIDDEN, "this run belongs to a different account").into_response();
    }
    let Some(repo_url) = run_state.repo_url.clone() else {
        return (StatusCode::BAD_REQUEST, "this run has no repo_url set yet -- set one first (POST /api/runs/{id}/repo)").into_response();
    };
    // Preserve any manually-uploaded documents across a re-sync -- sync_repo
    // only ever knows about the GitHub side, so it can't preserve them itself;
    // splicing them back in here is what actually keeps a re-sync from
    // silently deleting something a human added by hand.
    let existing_manual = load_rag_index(&state, &id).map(|i| i.manual_documents).unwrap_or_default();
    match rag::sync_repo(&state.http_client, &repo_url, unix_now()).await {
        Ok(mut index) => {
            index.manual_documents = existing_manual;
            embed_index_in_place(&state, &mut index).await;
            sync_vector_store(&state, &id, &index).await;
            let summary = serde_json::json!({
                "repo_url": index.repo_url,
                "branch": index.branch,
                "synced_at": index.synced_at,
                "files_seen": index.files_seen,
                "files_indexed": index.files_indexed,
                "chunks": index.chunks.len(),
            });
            match persist_rag_index(&state, &id, &index) {
                Ok(()) => Json(summary).into_response(),
                Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("could not persist the index: {e}")).into_response(),
            }
        }
        Err(e) => (StatusCode::BAD_GATEWAY, format!("sync failed: {e}")).into_response(),
    }
}

#[derive(Deserialize)]
struct RagSearchQuery {
    q: String,
}

/// `GET /api/runs/{id}/rag/search?q=...` -- real keyword search over whatever the
/// last `sync_rag` persisted. `configured: false` (not a 404) when nothing has
/// been synced yet, honestly distinguishing "never synced" from "synced but no
/// match," the same `null`-vs-`false` honesty precedent `assistant_status` set.
async fn search_rag(
    State(state): State<AppState>,
    AxPath(id): AxPath<String>,
    Query(q): Query<RagSearchQuery>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    if !valid_run_id(&id) {
        return (StatusCode::BAD_REQUEST, "run_id must be non-empty alphanumeric/-/_ only").into_response();
    }
    if !run_exists(&state, &id) {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    }
    let run_state = match load_or_init_run(&run_dir(&state, &id), &id) {
        Ok((_spec, s)) => s,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response(),
    };
    if !owner_authorized(&headers, &run_state) {
        return (StatusCode::FORBIDDEN, "this run belongs to a different account").into_response();
    }
    let Some(index) = load_rag_index(&state, &id) else {
        return Json(serde_json::json!({"configured": false, "results": [], "manual_documents": []})).into_response();
    };
    // Real semantic search only when a real embedding credential is configured
    // -- an embedding failure (bad key, provider outage) degrades to
    // keyword-only results rather than failing the whole search, since the
    // keyword path never depended on this credential and shouldn't start
    // failing because of it.
    let query_embedding = match &state.rag_embedding_api_key {
        Some(key) => match rag::embed_texts(&state.http_client, &state.rag_embedding_api_base, key, &[q.q.trim().to_string()]).await {
            Ok(mut embeddings) => embeddings.pop(),
            Err(e) => {
                eprintln!("devsystem-web: RAG query embedding failed, falling back to keyword-only search: {e}");
                None
            }
        },
        None => None,
    };
    let mut results = rag::combined_search(&index, q.q.trim(), query_embedding.as_deref(), 10);
    // Real Postgres semantic search, additive to the embedded/JSON path
    // above -- when both are configured, Postgres is the real source of
    // truth for "closest embedding" (a real ANN index, not a brute-force
    // scan of whatever's in this request's in-memory RagIndex), but a
    // Postgres error degrades to whatever the embedded path already found
    // rather than failing the whole search.
    if let (Some(pool), Some(qe)) = (&state.rag_db_pool, &query_embedding) {
        match vector_store::semantic_search(pool, &id, qe, 10).await {
            Ok(hits) => {
                for hit in hits {
                    let snippet: String = hit.text.trim().chars().take(400).collect();
                    let score = (hit.score * 100.0).round() as u32;
                    match results.iter_mut().find(|r| r.path == hit.path && r.snippet == snippet) {
                        Some(existing) if score > existing.score => {
                            existing.score = score;
                            existing.match_kind = rag::MatchKind::Semantic;
                        }
                        Some(_) => {}
                        None => results.push(rag::RagSearchResult { path: hit.path, score, snippet, match_kind: rag::MatchKind::Semantic }),
                    }
                }
                results.sort_by_key(|b| std::cmp::Reverse(b.score));
                results.truncate(10);
            }
            Err(e) => eprintln!("devsystem-web: real Postgres semantic search failed for run {id}, falling back to the embedded results already found: {e}"),
        }
    }
    // Metadata only (id/path/added_at) -- the real text is only ever returned
    // via a search match's snippet, not the management listing, so the GUI's
    // document list doesn't have to fetch (and the network doesn't have to
    // carry) potentially-large uploaded text just to render a title.
    let manual_documents: Vec<_> =
        index.manual_documents.iter().map(|d| serde_json::json!({"id": d.id, "path": d.path, "added_at": d.added_at})).collect();
    Json(serde_json::json!({
        "configured": true,
        "synced_at": index.synced_at,
        "branch": index.branch,
        "files_indexed": index.files_indexed,
        "results": results,
        "manual_documents": manual_documents,
    }))
    .into_response()
}

#[derive(Deserialize)]
struct AddRagDocumentRequest {
    path: String,
    text: String,
}

/// Real cap, same reasoning as `MAX_CUSTOM_PANEL_HTML_BYTES`: a run's RAG
/// index is loaded into memory on every search; nothing here should let one
/// pasted document make that unreasonably large. Well above any real doc
/// (README-sized text is a few KB); this is a sanity ceiling, not a design
/// target.
const MAX_RAG_DOCUMENT_BYTES: usize = 500_000;

/// `POST /api/runs/{id}/rag/documents` (#382 RAG slice 2): a real, human-
/// uploaded document -- paste text or upload a small file through the GUI,
/// no GitHub repo involved. Owner-restricted like every other GUI mutation.
/// Creates the index if this run has never synced a repo at all -- upload
/// doesn't require `repo_url` to be set first, unlike `sync_rag`.
async fn add_rag_document(
    State(state): State<AppState>,
    AxPath(id): AxPath<String>,
    headers: axum::http::HeaderMap,
    Json(body): Json<AddRagDocumentRequest>,
) -> impl IntoResponse {
    if !valid_run_id(&id) {
        return (StatusCode::BAD_REQUEST, "run_id must be non-empty alphanumeric/-/_ only").into_response();
    }
    if !run_exists(&state, &id) {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    }
    let path = body.path.trim().to_string();
    if path.is_empty() {
        return (StatusCode::BAD_REQUEST, "path must not be empty").into_response();
    }
    if body.text.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "text must not be empty").into_response();
    }
    if body.text.len() > MAX_RAG_DOCUMENT_BYTES {
        return (StatusCode::BAD_REQUEST, format!("text must be under {MAX_RAG_DOCUMENT_BYTES} bytes")).into_response();
    }
    let _guard = state.write_lock.lock().await;
    let run_state = match load_or_init_run(&run_dir(&state, &id), &id) {
        Ok((_spec, s)) => s,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response(),
    };
    if !owner_authorized(&headers, &run_state) {
        return (StatusCode::FORBIDDEN, "this run belongs to a different account").into_response();
    }
    let mut index = load_rag_index(&state, &id).unwrap_or_else(|| rag::RagIndex {
        repo_url: run_state.repo_url.clone().unwrap_or_default(),
        synced_at: 0,
        branch: String::new(),
        files_seen: 0,
        files_indexed: 0,
        chunks: Vec::new(),
        manual_documents: Vec::new(),
    });
    let doc = rag::RagDocument { id: format!("{:016x}", rand::random::<u64>()), path, text: body.text, added_at: unix_now(), embedding: None };
    index.manual_documents.push(doc.clone());
    embed_index_in_place(&state, &mut index).await;
    sync_vector_store(&state, &id, &index).await;
    match persist_rag_index(&state, &id, &index) {
        Ok(()) => Json(serde_json::json!({"id": doc.id, "path": doc.path, "added_at": doc.added_at})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("could not persist the index: {e}")).into_response(),
    }
}

/// Real cap on a raw upload's bytes before it's even sent to Unstructured --
/// the requirements doc's own ask ("an image file needs its own real, stated
/// size cap"), separate from [`rag::MAX_UNSTRUCTURED_EXTRACTED_CHARS`] (which
/// caps the *extracted text*, not the upload itself). 10MB covers a real
/// scanned page or a typical PDF without letting an unbounded upload spend
/// unbounded real Unstructured API cost on this operator's behalf.
const MAX_RAG_UPLOAD_BYTES: usize = 10_000_000;

/// `POST /api/runs/{id}/rag/upload-file` -- real image/PDF/DOCX upload,
/// `multipart/form-data`, one field named `file`. Owner-restricted like every
/// other GUI mutation. Two real extraction paths, tried in this order (same
/// priority shape `approve_issue_proposal` already established for its own
/// two real posting paths):
///
/// 1. The real Unstructured API (`rag::parse_with_unstructured`), when
///    `RAG_UNSTRUCTURED_API_KEY` is configured -- the operator's explicit
///    image-OCR ask from CADS-devsystem#7, and the only path that handles
///    images at all. Unchanged behavior from before this fallback existed.
/// 2. The real `devsystem.document_extraction` channel
///    (`AppState::document_extraction_channel`), when Unstructured isn't
///    configured but this is -- PDF, DOCX, legacy DOC, and plain
///    text/markdown as of this writing (see that role's own handler,
///    updated for issue #14's PR #17), never images. A real, free,
///    already-auctioned capability instead of leaving every deployment
///    without a paid Unstructured key permanently unable to upload at all.
///
/// A real `503` (not a silent no-op), naming which path(s) were checked,
/// when neither is configured -- matching `ask_assistant`'s own "not
/// configured" precedent rather than pretending to accept a file it can't
/// actually process.
async fn upload_rag_file(State(state): State<AppState>, AxPath(id): AxPath<String>, headers: axum::http::HeaderMap, mut multipart: axum::extract::Multipart) -> impl IntoResponse {
    if !valid_run_id(&id) {
        return (StatusCode::BAD_REQUEST, "run_id must be non-empty alphanumeric/-/_ only").into_response();
    }
    if !run_exists(&state, &id) {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    }
    if state.rag_unstructured_api_key.is_none() && state.document_extraction_channel.is_none() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "neither RAG_UNSTRUCTURED_API_KEY nor the devsystem.document_extraction channel is configured on this deployment",
        )
            .into_response();
    }
    let _guard = state.write_lock.lock().await;
    let run_state = match load_or_init_run(&run_dir(&state, &id), &id) {
        Ok((_spec, s)) => s,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response(),
    };
    if !owner_authorized(&headers, &run_state) {
        return (StatusCode::FORBIDDEN, "this run belongs to a different account").into_response();
    }
    let mut filename = String::new();
    let mut bytes: Vec<u8> = Vec::new();
    loop {
        let field = match multipart.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => return (StatusCode::BAD_REQUEST, format!("malformed multipart upload: {e}")).into_response(),
        };
        if field.name() != Some("file") {
            continue;
        }
        filename = field.file_name().unwrap_or("upload").to_string();
        bytes = match field.bytes().await {
            Ok(b) => b.to_vec(),
            Err(e) => return (StatusCode::BAD_REQUEST, format!("could not read the uploaded file: {e}")).into_response(),
        };
    }
    if bytes.is_empty() {
        return (StatusCode::BAD_REQUEST, "no file field found in the upload (expected a multipart field named 'file')").into_response();
    }
    if bytes.len() > MAX_RAG_UPLOAD_BYTES {
        return (StatusCode::BAD_REQUEST, format!("upload must be under {MAX_RAG_UPLOAD_BYTES} bytes")).into_response();
    }
    let (text, truncated, elements_extracted, via) = if let Some(api_key) = state.rag_unstructured_api_key.clone() {
        let elements = match rag::parse_with_unstructured(&state.http_client, &state.rag_unstructured_api_base, &api_key, &filename, bytes).await {
            Ok(e) => e,
            Err(e) => return (StatusCode::BAD_GATEWAY, format!("Unstructured extraction failed: {e}")).into_response(),
        };
        let (text, truncated) = rag::elements_to_text(&elements);
        (text, truncated, elements.len(), "unstructured")
    } else {
        let cfg = state.document_extraction_channel.clone().expect("checked Some above -- Unstructured branch is the only other option");
        match extract_via_channel(&cfg, &filename, &bytes).await {
            Ok(text) => (text, false, 0, "document_extraction_channel"),
            Err(e) => return (StatusCode::BAD_GATEWAY, format!("devsystem.document_extraction channel extraction failed: {e}")).into_response(),
        }
    };
    if text.trim().is_empty() {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({"error": format!("{via} extracted no text from this file"), "elements": elements_extracted})),
        )
            .into_response();
    }
    let mut index = load_rag_index(&state, &id).unwrap_or_else(|| rag::RagIndex {
        repo_url: run_state.repo_url.clone().unwrap_or_default(),
        synced_at: 0,
        branch: String::new(),
        files_seen: 0,
        files_indexed: 0,
        chunks: Vec::new(),
        manual_documents: Vec::new(),
    });
    let doc = rag::RagDocument { id: format!("{:016x}", rand::random::<u64>()), path: filename, text, added_at: unix_now(), embedding: None };
    index.manual_documents.push(doc.clone());
    embed_index_in_place(&state, &mut index).await;
    sync_vector_store(&state, &id, &index).await;
    match persist_rag_index(&state, &id, &index) {
        Ok(()) => Json(serde_json::json!({
            "id": doc.id,
            "path": doc.path,
            "added_at": doc.added_at,
            "elements_extracted": elements_extracted,
            "extracted_text_truncated": truncated,
            "extracted_via": via,
        }))
        .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("could not persist the index: {e}")).into_response(),
    }
}

/// `POST /api/runs/{id}/rag/documents/{doc_id}/remove` -- real per-document
/// delete, the other half of the operator's "kann ich die Dokumente
/// verwalten" ask. Only ever removes from `manual_documents`; a repo-synced
/// file is removed by it no longer existing in the repo at the next sync,
/// not by an individual-delete action here.
async fn remove_rag_document(
    State(state): State<AppState>,
    AxPath((id, doc_id)): AxPath<(String, String)>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    if !valid_run_id(&id) {
        return (StatusCode::BAD_REQUEST, "run_id must be non-empty alphanumeric/-/_ only").into_response();
    }
    if !run_exists(&state, &id) {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    }
    let _guard = state.write_lock.lock().await;
    let run_state = match load_or_init_run(&run_dir(&state, &id), &id) {
        Ok((_spec, s)) => s,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response(),
    };
    if !owner_authorized(&headers, &run_state) {
        return (StatusCode::FORBIDDEN, "this run belongs to a different account").into_response();
    }
    let Some(mut index) = load_rag_index(&state, &id) else {
        return (StatusCode::NOT_FOUND, "no RAG index for this run yet").into_response();
    };
    let removed_path = index.manual_documents.iter().find(|d| d.id == doc_id).map(|d| d.path.clone());
    let before = index.manual_documents.len();
    index.manual_documents.retain(|d| d.id != doc_id);
    if index.manual_documents.len() == before {
        return (StatusCode::NOT_FOUND, format!("no manual document with id {doc_id:?}")).into_response();
    }
    if let (Some(pool), Some(path)) = (&state.rag_db_pool, &removed_path) {
        if let Err(e) = vector_store::delete_by_path(pool, &id, "manual_document", path).await {
            eprintln!("devsystem-web: could not remove {path:?} from Postgres for run {id}: {e}");
        }
    }
    match persist_rag_index(&state, &id, &index) {
        Ok(()) => Json(serde_json::json!({"removed": doc_id})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("could not persist the index: {e}")).into_response(),
    }
}

#[derive(Deserialize)]
struct AddCustomPanelRequest {
    title: String,
    html: String,
}

/// Real cap on a custom panel's `html` size -- a run's `state.json` is loaded
/// into memory on every request; nothing here should let one careless panel
/// bloat that file into something the rest of the GUI pays for.
const MAX_CUSTOM_PANEL_HTML_BYTES: usize = 100_000;

/// `POST /api/runs/{id}/panels` (#382, custom panels slice 1): add a real,
/// human-written GUI panel. Owner-restricted. `html` is stored as-is -- no
/// sanitization here, because sanitizing-then-trusting is exactly the mistake
/// this feature's real safety instead comes from: the GUI renders it inside a
/// sandboxed iframe (`<iframe sandbox="allow-scripts">`), never the main page,
/// so nothing server-side needs to guess what's "safe enough" to inject.
async fn add_custom_panel(
    State(state): State<AppState>,
    AxPath(id): AxPath<String>,
    headers: axum::http::HeaderMap,
    Json(body): Json<AddCustomPanelRequest>,
) -> impl IntoResponse {
    if !valid_run_id(&id) {
        return (StatusCode::BAD_REQUEST, "run_id must be non-empty alphanumeric/-/_ only").into_response();
    }
    if !run_exists(&state, &id) {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    }
    let title = body.title.trim().to_string();
    if title.is_empty() {
        return (StatusCode::BAD_REQUEST, "title must not be empty").into_response();
    }
    // Real gap found live by the incompetent-agent stress test (#382 goal doc
    // §8, 2026-08-06), closing out this class's last two noted candidates (see
    // devsystem_pipeline::BIDI_CONTROL_CHARS's own doc comment): a panel's
    // `title` is real, trusted
    // UI chrome -- shown in the panel list, and interpolated raw into this
    // panel's own confirm() dialogs -- unlike `html`, which is deliberately
    // untrusted-by-design (rendered only inside a sandboxed iframe, per this
    // function's own doc comment). Live-confirmed before this fix: a title
    // like "Safe Panel" + U+202E + reversed text sailed through untouched,
    // displaying as an apparently-safe title while hiding real content after
    // it. Closed at all four real title entry points in one sweep, matching
    // the html-empty-check fix's own precedent.
    if contains_bidi_control_char(&title) {
        return (
            StatusCode::BAD_REQUEST,
            "title contains a Unicode bidi control character (e.g. a right-to-left override) -- \
             these can make the visually displayed text not match what's actually stored"
                .to_string(),
        )
            .into_response();
    }
    if body.html.len() > MAX_CUSTOM_PANEL_HTML_BYTES {
        return (StatusCode::BAD_REQUEST, format!("html must be under {MAX_CUSTOM_PANEL_HTML_BYTES} bytes")).into_response();
    }
    // DAU-lens gap found live 2026-08-06 (#382 goal doc §8): every other real
    // free-text field in this codebase (milestones, backlog, requirement
    // statements, stage proposals) already rejects whitespace-only content --
    // a custom panel's own `html` was the one exception, at all four real entry
    // points. Live-confirmed before this fix: `{"title":"x","html":""}` got a
    // real 200, creating a genuinely blank, useless panel with nothing telling
    // the human anything went wrong.
    if body.html.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "html must not be empty").into_response();
    }
    let _guard = state.write_lock.lock().await;
    let dir = run_dir(&state, &id);
    let (spec, mut run_state) = match load_or_init_run(&dir, &id) {
        Ok(v) => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("load failed: {e}")).into_response(),
    };
    if !owner_authorized(&headers, &run_state) {
        return (StatusCode::FORBIDDEN, "this run belongs to a different account").into_response();
    }
    // Real gap found live by the incompetent-agent stress test (#382 goal doc
    // §8, 2026-08-06): MAX_LIST_ITEMS's own doc comment gives a real reason
    // ("nothing stops a client from adding items in a tight loop... matches the
    // host's real, limited disk headroom") that applies just as much to
    // custom_panels and every pending-proposal queue as it does to backlog/
    // milestones/requirements -- but only the latter three ever got the check.
    // Live-confirmed before this fix: 510 real panels added in a row, zero
    // rejections, no cap anywhere.
    if run_state.custom_panels.len() >= MAX_LIST_ITEMS {
        return (StatusCode::BAD_REQUEST, format!("custom_panels is at its defensive cap of {MAX_LIST_ITEMS} items")).into_response();
    }
    let panel = CustomPanel {
        id: format!("{:016x}", rand::random::<u64>()),
        title,
        html: body.html,
        source: None,
        created_at: unix_now(),
    };
    run_state.custom_panels.push(panel.clone());
    match persist_run(&dir, &spec, &run_state) {
        Ok(()) => Json(panel).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("persist failed: {e}")).into_response(),
    }
}

/// `POST /api/runs/{id}/panels/{panel_id}/remove` -- the operator's own "aber
/// auch einfach wieder entfernen kann." Owner-restricted, same as adding one.
async fn remove_custom_panel(
    State(state): State<AppState>,
    AxPath((id, panel_id)): AxPath<(String, String)>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    if !valid_run_id(&id) {
        return (StatusCode::BAD_REQUEST, "run_id must be non-empty alphanumeric/-/_ only").into_response();
    }
    if !run_exists(&state, &id) {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    }
    let _guard = state.write_lock.lock().await;
    let dir = run_dir(&state, &id);
    let (spec, mut run_state) = match load_or_init_run(&dir, &id) {
        Ok(v) => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("load failed: {e}")).into_response(),
    };
    if !owner_authorized(&headers, &run_state) {
        return (StatusCode::FORBIDDEN, "this run belongs to a different account").into_response();
    }
    let before = run_state.custom_panels.len();
    run_state.custom_panels.retain(|p| p.id != panel_id);
    if run_state.custom_panels.len() == before {
        return (StatusCode::NOT_FOUND, format!("no custom panel with id {panel_id:?}")).into_response();
    }
    match persist_run(&dir, &spec, &run_state) {
        Ok(()) => Json(serde_json::json!({"removed": panel_id})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("persist failed: {e}")).into_response(),
    }
}

#[derive(Deserialize)]
struct UpdateCustomPanelRequest {
    title: String,
    html: String,
}

/// `POST /api/runs/{id}/panels/{panel_id}/update` -- the last real piece of gap
/// #4 (#382 goal doc §7.2): a human could already add and remove a panel
/// directly, but editing one meant remove-then-re-add (losing the original
/// `created_at`/`source`/`id` for no real reason). Applies immediately, same
/// trust level as the human's own direct Remove button (their own content,
/// their own call) -- same title/html validation as `add_custom_panel`.
async fn update_custom_panel(
    State(state): State<AppState>,
    AxPath((id, panel_id)): AxPath<(String, String)>,
    headers: axum::http::HeaderMap,
    Json(body): Json<UpdateCustomPanelRequest>,
) -> impl IntoResponse {
    if !valid_run_id(&id) {
        return (StatusCode::BAD_REQUEST, "run_id must be non-empty alphanumeric/-/_ only").into_response();
    }
    if !run_exists(&state, &id) {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    }
    let title = body.title.trim().to_string();
    if title.is_empty() {
        return (StatusCode::BAD_REQUEST, "title must not be empty").into_response();
    }
    // Same real gap as add_custom_panel -- see its own doc comment (#382 goal
    // doc §8, 2026-08-06). This is the direct-edit path.
    if contains_bidi_control_char(&title) {
        return (
            StatusCode::BAD_REQUEST,
            "title contains a Unicode bidi control character (e.g. a right-to-left override) -- \
             these can make the visually displayed text not match what's actually stored"
                .to_string(),
        )
            .into_response();
    }
    if body.html.len() > MAX_CUSTOM_PANEL_HTML_BYTES {
        return (StatusCode::BAD_REQUEST, format!("html must be under {MAX_CUSTOM_PANEL_HTML_BYTES} bytes")).into_response();
    }
    // DAU-lens gap found live 2026-08-06 (#382 goal doc §8) -- see add_custom_panel's
    // own doc comment for the full finding; the same gap existed at all four real
    // panel-html entry points, this being the direct-edit one.
    if body.html.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "html must not be empty").into_response();
    }
    let _guard = state.write_lock.lock().await;
    let dir = run_dir(&state, &id);
    let (spec, mut run_state) = match load_or_init_run(&dir, &id) {
        Ok(v) => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("load failed: {e}")).into_response(),
    };
    if !owner_authorized(&headers, &run_state) {
        return (StatusCode::FORBIDDEN, "this run belongs to a different account").into_response();
    }
    let Some(panel) = run_state.custom_panels.iter_mut().find(|p| p.id == panel_id) else {
        return (StatusCode::NOT_FOUND, format!("no custom panel with id {panel_id:?}")).into_response();
    };
    panel.title = title;
    panel.html = body.html;
    let updated = panel.clone();
    match persist_run(&dir, &spec, &run_state) {
        Ok(()) => Json(updated).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("persist failed: {e}")).into_response(),
    }
}

#[derive(Deserialize)]
struct ProposePanelRequest {
    title: String,
    html: String,
}

/// `POST /api/runs/{id}/panels/propose` -- the assistant-facing half of the
/// operator's original trust-model decision for custom panels: "proposes, human
/// clicks install," never assistant-installs-directly. Shape mirrors
/// `add_custom_panel` exactly (same title/html/size validation), but writes to
/// `pending_panel_proposals`, not the live `custom_panels` a run's GUI actually
/// renders -- a proposal has zero effect on what anyone sees until a human
/// approves it below. Same headless-caller-unrestricted `owner_authorized` as
/// every other assistant-driven write (#35) -- the assistant bridge has no gate
/// header and was never meant to.
async fn propose_custom_panel(
    State(state): State<AppState>,
    AxPath(id): AxPath<String>,
    headers: axum::http::HeaderMap,
    Json(body): Json<ProposePanelRequest>,
) -> impl IntoResponse {
    if !valid_run_id(&id) {
        return (StatusCode::BAD_REQUEST, "run_id must be non-empty alphanumeric/-/_ only").into_response();
    }
    if !run_exists(&state, &id) {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    }
    let title = body.title.trim().to_string();
    if title.is_empty() {
        return (StatusCode::BAD_REQUEST, "title must not be empty").into_response();
    }
    // Same real gap as add_custom_panel -- see its own doc comment (#382 goal
    // doc §8, 2026-08-06). This is the assistant-proposal path -- the highest-
    // stakes of the four, since a human approving from the review queue is
    // trusting exactly this title at face value.
    if contains_bidi_control_char(&title) {
        return (
            StatusCode::BAD_REQUEST,
            "title contains a Unicode bidi control character (e.g. a right-to-left override) -- \
             these can make the visually displayed text not match what's actually stored"
                .to_string(),
        )
            .into_response();
    }
    if body.html.len() > MAX_CUSTOM_PANEL_HTML_BYTES {
        return (StatusCode::BAD_REQUEST, format!("html must be under {MAX_CUSTOM_PANEL_HTML_BYTES} bytes")).into_response();
    }
    // Same real gap as add_custom_panel -- see its own doc comment (#382 goal doc §8,
    // 2026-08-06). This is the assistant-proposal path; without this check a proposed
    // panel could sit in the human's real review queue as something to approve that's
    // already known to be blank.
    if body.html.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "html must not be empty").into_response();
    }
    let _guard = state.write_lock.lock().await;
    let dir = run_dir(&state, &id);
    let (spec, mut run_state) = match load_or_init_run(&dir, &id) {
        Ok(v) => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("load failed: {e}")).into_response(),
    };
    if !owner_authorized(&headers, &run_state) {
        return (StatusCode::FORBIDDEN, "this run belongs to a different account").into_response();
    }
    // Same real defensive cap as add_custom_panel -- see its own doc comment.
    if run_state.pending_panel_proposals.len() >= MAX_LIST_ITEMS {
        return (StatusCode::BAD_REQUEST, format!("pending_panel_proposals is at its defensive cap of {MAX_LIST_ITEMS} items")).into_response();
    }
    let proposal = PendingPanelProposal { id: format!("{:016x}", rand::random::<u64>()), title, html: body.html, proposed_at: unix_now() };
    run_state.pending_panel_proposals.push(proposal.clone());
    match persist_run(&dir, &spec, &run_state) {
        Ok(()) => Json(proposal).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("persist failed: {e}")).into_response(),
    }
}

const MAX_NEXT_STEP_DRAFT_BYTES: usize = 4_000;

#[derive(Deserialize)]
struct ProposeNextStepRequest {
    text: String,
}

/// `POST /api/runs/{id}/next-steps/propose` -- "stack mode" slice 3
/// (`RunState::pending_next_step_drafts`'s own doc comment): `devsystem.assistant`
/// drafts one concrete next-iteration-plan option as plain, editable text.
/// Deliberately no approve/apply step -- see that field's own doc comment for
/// why a draft has nothing to "install." Same headless-caller-unrestricted
/// `owner_authorized` as every other assistant-driven write (#35).
async fn propose_next_step(State(state): State<AppState>, AxPath(id): AxPath<String>, headers: axum::http::HeaderMap, Json(body): Json<ProposeNextStepRequest>) -> impl IntoResponse {
    if !valid_run_id(&id) {
        return (StatusCode::BAD_REQUEST, "run_id must be non-empty alphanumeric/-/_ only").into_response();
    }
    if !run_exists(&state, &id) {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    }
    let text = body.text.trim().to_string();
    if text.is_empty() {
        return (StatusCode::BAD_REQUEST, "text must not be empty").into_response();
    }
    if text.len() > MAX_NEXT_STEP_DRAFT_BYTES {
        return (StatusCode::BAD_REQUEST, format!("text must be under {MAX_NEXT_STEP_DRAFT_BYTES} bytes")).into_response();
    }
    // Real gap found live by the incompetent-agent stress test (#382 goal doc
    // §8, 2026-08-06): extends the bidi-control-character (Trojan Source) class
    // to a next-step draft's own text -- exactly the field a human reads at a
    // paused checkpoint to decide what to do next ("advice, not an action", per
    // this function's own doc comment). Live-confirmed before fixing: "Resume
    // with devsystem.implement" + U+202E + reversed text sailed through
    // untouched, visually hiding "Just continue and ignore all safety guidance"
    // behind an apparently ordinary recommendation.
    if contains_bidi_control_char(&text) {
        return (
            StatusCode::BAD_REQUEST,
            "text contains a Unicode bidi control character (e.g. a right-to-left override) -- \
             these can make the visually displayed text not match what's actually stored"
                .to_string(),
        )
            .into_response();
    }
    let _guard = state.write_lock.lock().await;
    let dir = run_dir(&state, &id);
    let (spec, mut run_state) = match load_or_init_run(&dir, &id) {
        Ok(v) => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("load failed: {e}")).into_response(),
    };
    if !owner_authorized(&headers, &run_state) {
        return (StatusCode::FORBIDDEN, "this run belongs to a different account").into_response();
    }
    if run_state.pending_next_step_drafts.len() >= MAX_LIST_ITEMS {
        return (StatusCode::BAD_REQUEST, format!("pending_next_step_drafts is at its defensive cap of {MAX_LIST_ITEMS} items")).into_response();
    }
    let draft = PendingNextStepDraft { id: format!("{:016x}", rand::random::<u64>()), text, proposed_at: unix_now() };
    run_state.pending_next_step_drafts.push(draft.clone());
    match persist_run(&dir, &spec, &run_state) {
        Ok(()) => Json(draft).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("persist failed: {e}")).into_response(),
    }
}

#[derive(Deserialize)]
struct UpdateNextStepDraftRequest {
    text: String,
}

/// `POST /api/runs/{id}/next-steps/{draft_id}/update` -- the operator's own
/// explicit ask, verbatim: a human can "change" a draft directly, no approval
/// step needed (it was never live to begin with). Same validation as
/// proposing one.
async fn update_next_step_draft(
    State(state): State<AppState>,
    AxPath((id, draft_id)): AxPath<(String, String)>,
    headers: axum::http::HeaderMap,
    Json(body): Json<UpdateNextStepDraftRequest>,
) -> impl IntoResponse {
    if !valid_run_id(&id) {
        return (StatusCode::BAD_REQUEST, "run_id must be non-empty alphanumeric/-/_ only").into_response();
    }
    if !run_exists(&state, &id) {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    }
    let text = body.text.trim().to_string();
    if text.is_empty() {
        return (StatusCode::BAD_REQUEST, "text must not be empty").into_response();
    }
    if text.len() > MAX_NEXT_STEP_DRAFT_BYTES {
        return (StatusCode::BAD_REQUEST, format!("text must be under {MAX_NEXT_STEP_DRAFT_BYTES} bytes")).into_response();
    }
    // Same real gap as propose_next_step -- see its own doc comment.
    if contains_bidi_control_char(&text) {
        return (
            StatusCode::BAD_REQUEST,
            "text contains a Unicode bidi control character (e.g. a right-to-left override) -- \
             these can make the visually displayed text not match what's actually stored"
                .to_string(),
        )
            .into_response();
    }
    let _guard = state.write_lock.lock().await;
    let dir = run_dir(&state, &id);
    let (spec, mut run_state) = match load_or_init_run(&dir, &id) {
        Ok(v) => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("load failed: {e}")).into_response(),
    };
    if !owner_authorized(&headers, &run_state) {
        return (StatusCode::FORBIDDEN, "this run belongs to a different account").into_response();
    }
    let Some(draft) = run_state.pending_next_step_drafts.iter_mut().find(|d| d.id == draft_id) else {
        return (StatusCode::NOT_FOUND, format!("no next-step draft with id {draft_id:?}")).into_response();
    };
    draft.text = text;
    let updated = draft.clone();
    match persist_run(&dir, &spec, &run_state) {
        Ok(()) => Json(updated).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("persist failed: {e}")).into_response(),
    }
}

/// `POST /api/runs/{id}/next-steps/{draft_id}/remove` -- the operator's own
/// explicit ask, verbatim: a human can "delete" a draft directly. Same real,
/// permanent, no-undo shape as `remove_custom_panel` -- no server-side
/// confirmation param, the GUI's own `confirm()` is the real gate, same
/// existing precedent.
async fn remove_next_step_draft(State(state): State<AppState>, AxPath((id, draft_id)): AxPath<(String, String)>, headers: axum::http::HeaderMap) -> impl IntoResponse {
    if !valid_run_id(&id) {
        return (StatusCode::BAD_REQUEST, "run_id must be non-empty alphanumeric/-/_ only").into_response();
    }
    if !run_exists(&state, &id) {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    }
    let _guard = state.write_lock.lock().await;
    let dir = run_dir(&state, &id);
    let (spec, mut run_state) = match load_or_init_run(&dir, &id) {
        Ok(v) => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("load failed: {e}")).into_response(),
    };
    if !owner_authorized(&headers, &run_state) {
        return (StatusCode::FORBIDDEN, "this run belongs to a different account").into_response();
    }
    let before = run_state.pending_next_step_drafts.len();
    run_state.pending_next_step_drafts.retain(|d| d.id != draft_id);
    if run_state.pending_next_step_drafts.len() == before {
        return (StatusCode::NOT_FOUND, format!("no next-step draft with id {draft_id:?}")).into_response();
    }
    match persist_run(&dir, &spec, &run_state) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("persist failed: {e}")).into_response(),
    }
}

const MAX_PLAN_CANVAS_ANCHOR_SNIPPET_BYTES: usize = 300;
const MAX_PLAN_CANVAS_ANNOTATION_TEXT_BYTES: usize = 2_000;

#[derive(Deserialize)]
struct PlanCanvasAnnotateRequest {
    anchor_snippet: String,
    text: String,
}

fn validate_plan_canvas_field(value: &str, field_name: &str, max_bytes: usize) -> Result<String, String> {
    let trimmed = value.trim().to_string();
    if trimmed.is_empty() {
        return Err(format!("{field_name} must not be empty"));
    }
    if trimmed.len() > max_bytes {
        return Err(format!("{field_name} must be under {max_bytes} bytes"));
    }
    if contains_bidi_control_char(&trimmed) {
        return Err(format!(
            "{field_name} contains a Unicode bidi control character (e.g. a right-to-left override) -- \
             these can make the visually displayed text not match what's actually stored"
        ));
    }
    Ok(trimmed)
}

/// `POST /api/runs/{id}/plan-canvas/annotate` -- the real "point at it" half of
/// [`RunState::plan_canvas_annotations`]'s own doc comment: a reviewer names the
/// exact block of the plan they mean (`anchor_snippet`, the GUI's own real
/// excerpt of what was clicked) plus their actual comment, instead of retyping
/// the whole review as free prose.
async fn plan_canvas_annotate(State(state): State<AppState>, AxPath(id): AxPath<String>, headers: axum::http::HeaderMap, Json(body): Json<PlanCanvasAnnotateRequest>) -> impl IntoResponse {
    if !valid_run_id(&id) {
        return (StatusCode::BAD_REQUEST, "run_id must be non-empty alphanumeric/-/_ only").into_response();
    }
    if !run_exists(&state, &id) {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    }
    let anchor_snippet = match validate_plan_canvas_field(&body.anchor_snippet, "anchor_snippet", MAX_PLAN_CANVAS_ANCHOR_SNIPPET_BYTES) {
        Ok(v) => v,
        Err(e) => return (StatusCode::BAD_REQUEST, e).into_response(),
    };
    let text = match validate_plan_canvas_field(&body.text, "text", MAX_PLAN_CANVAS_ANNOTATION_TEXT_BYTES) {
        Ok(v) => v,
        Err(e) => return (StatusCode::BAD_REQUEST, e).into_response(),
    };
    let _guard = state.write_lock.lock().await;
    let dir = run_dir(&state, &id);
    let (spec, mut run_state) = match load_or_init_run(&dir, &id) {
        Ok(v) => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("load failed: {e}")).into_response(),
    };
    if !owner_authorized(&headers, &run_state) {
        return (StatusCode::FORBIDDEN, "this run belongs to a different account").into_response();
    }
    if run_state.plan_canvas_annotations.len() >= MAX_LIST_ITEMS {
        return (StatusCode::BAD_REQUEST, format!("plan_canvas_annotations is at its defensive cap of {MAX_LIST_ITEMS} items")).into_response();
    }
    let annotation = PlanCanvasAnnotation { id: format!("{:016x}", rand::random::<u64>()), anchor_snippet, text, created_at: unix_now() };
    run_state.plan_canvas_annotations.push(annotation.clone());
    match persist_run(&dir, &spec, &run_state) {
        Ok(()) => Json(annotation).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("persist failed: {e}")).into_response(),
    }
}

/// `POST /api/runs/{id}/plan-canvas/annotations/{annotation_id}/remove` -- undo a
/// mis-anchored or no-longer-wanted annotation before delivering a verdict. Same
/// real, permanent, no-undo shape as `remove_next_step_draft`.
async fn plan_canvas_remove_annotation(State(state): State<AppState>, AxPath((id, annotation_id)): AxPath<(String, String)>, headers: axum::http::HeaderMap) -> impl IntoResponse {
    if !valid_run_id(&id) {
        return (StatusCode::BAD_REQUEST, "run_id must be non-empty alphanumeric/-/_ only").into_response();
    }
    if !run_exists(&state, &id) {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    }
    let _guard = state.write_lock.lock().await;
    let dir = run_dir(&state, &id);
    let (spec, mut run_state) = match load_or_init_run(&dir, &id) {
        Ok(v) => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("load failed: {e}")).into_response(),
    };
    if !owner_authorized(&headers, &run_state) {
        return (StatusCode::FORBIDDEN, "this run belongs to a different account").into_response();
    }
    let before = run_state.plan_canvas_annotations.len();
    run_state.plan_canvas_annotations.retain(|a| a.id != annotation_id);
    if run_state.plan_canvas_annotations.len() == before {
        return (StatusCode::NOT_FOUND, format!("no plan-canvas annotation with id {annotation_id:?}")).into_response();
    }
    match persist_run(&dir, &spec, &run_state) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("persist failed: {e}")).into_response(),
    }
}

#[derive(Deserialize)]
struct PlanCanvasVerdictRequest {
    verdict: String,
}

/// `POST /api/runs/{id}/plan-canvas/verdict` -- delivers the real review
/// decision the whole panel exists for. `approve` folds the session into a
/// real, substantive `devsystem.review` iteration -- through the exact same
/// gates a normal `/iterate` call goes through (paused/ceiling/duplicate-guard),
/// not a separate, less-guarded path -- and clears the annotations (the
/// session concluded). `request_changes` requires at least one real annotation
/// (asking for changes with nothing pointed at is not an actionable signal) and
/// deliberately does NOT record a review iteration or clear the annotations --
/// they stay visible, real, structured feedback for the plan's own next
/// author, not summarized away into free prose.
async fn plan_canvas_verdict(State(state): State<AppState>, AxPath(id): AxPath<String>, headers: axum::http::HeaderMap, Json(body): Json<PlanCanvasVerdictRequest>) -> impl IntoResponse {
    if !valid_run_id(&id) {
        return (StatusCode::BAD_REQUEST, "run_id must be non-empty alphanumeric/-/_ only").into_response();
    }
    if body.verdict != "approve" && body.verdict != "request_changes" {
        return (StatusCode::BAD_REQUEST, "verdict must be \"approve\" or \"request_changes\"").into_response();
    }
    let _guard = state.write_lock.lock().await;
    let dir = run_dir(&state, &id);
    let (mut spec, mut run_state) = match load_or_init_run(&dir, &id) {
        Ok(v) => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("load failed: {e}")).into_response(),
    };
    if !owner_authorized(&headers, &run_state) {
        return (StatusCode::FORBIDDEN, "this run belongs to a different account").into_response();
    }
    if run_state.history.iter().rev().find(|r| r.stage == STAGE_PLAN).is_none() {
        return (StatusCode::BAD_REQUEST, "this run has no devsystem.plan iteration yet -- nothing real to review").into_response();
    }
    if body.verdict == "request_changes" {
        if run_state.plan_canvas_annotations.is_empty() {
            return (
                StatusCode::BAD_REQUEST,
                "request_changes needs at least one real annotation -- point at what needs to change, an empty request gives the plan's own next author nothing to act on",
            )
                .into_response();
        }
        let summary = format!(
            "Plan Canvas: request changes -- {} annotation(s): {}",
            run_state.plan_canvas_annotations.len(),
            run_state
                .plan_canvas_annotations
                .iter()
                .map(|a| format!("[{}] {}", truncate_for_summary(&a.anchor_snippet, 40), truncate_for_summary(&a.text, 80)))
                .collect::<Vec<_>>()
                .join("; ")
        );
        run_state.backlog.push(BacklogItem { text: summary, done: false });
        return match persist_run(&dir, &spec, &run_state) {
            Ok(()) => Json(serde_json::json!({"verdict": "request_changes", "annotations": run_state.plan_canvas_annotations})).into_response(),
            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("persist failed: {e}")).into_response(),
        };
    }

    // approve: the same real checks a normal /iterate submission goes through --
    // not a separate, less-guarded path just because it originated from this panel.
    if run_state.paused {
        return (StatusCode::CONFLICT, "run is paused -- resume it first (POST /api/runs/{id}/resume)").into_response();
    }
    if let Some(reason) = devsystem_pipeline::runner::ceiling_already_reached(&run_state, &run_state.criteria, true) {
        return (StatusCode::CONFLICT, reason).into_response();
    }
    let feedback = if run_state.plan_canvas_annotations.is_empty() {
        "Plan approved via Plan Canvas with no specific annotations -- reviewed as written.".to_string()
    } else {
        format!(
            "Plan approved via Plan Canvas ({} annotation(s) addressed): {}",
            run_state.plan_canvas_annotations.len(),
            run_state
                .plan_canvas_annotations
                .iter()
                .map(|a| format!("[{}] {}", truncate_for_summary(&a.anchor_snippet, 40), truncate_for_summary(&a.text, 80)))
                .collect::<Vec<_>>()
                .join("; ")
        )
    };
    if let Err(e) = validate_feedback(&feedback) {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("synthesized feedback failed its own validation: {e}")).into_response();
    }
    if let Some(dup_iteration) = duplicate_of_last_iteration(&run_state.history, STAGE_REVIEW, &feedback, true, &[], &[]) {
        return (
            StatusCode::CONFLICT,
            format!("this approval is byte-identical to iteration {dup_iteration}, the run's own immediately-preceding entry -- refusing to record it as a distinct, new iteration"),
        )
            .into_response();
    }
    let iteration = run_state.history.len() as u32 + 1;
    let submitted_by = headers.get("x-gate-email").and_then(|v| v.to_str().ok()).map(|s| s.to_string());
    let record = IterationRecord {
        run_id: id.clone(),
        stage: STAGE_REVIEW.to_string(),
        iteration,
        feedback,
        succeeded: true,
        proposals: vec![],
        requirement_indices: vec![],
        id: Some(format!("{:016x}", rand::random::<u64>())),
        submitted_at: Some(unix_now()),
        submitted_by,
    };
    let memory_path = dir.join("memory.jsonl");
    let envelope = envelope_from_iteration(&record, &run_state.requirements);
    if let Err(e) = append_to_memory_log(&memory_path, &envelope) {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("memory log failed: {e}")).into_response();
    }
    let criteria = run_state.criteria;
    let outcome = run_iteration(&mut spec, &mut run_state, record, &criteria);
    run_state.plan_canvas_annotations.clear();
    match persist_run(&dir, &spec, &run_state) {
        Ok(()) => Json(serde_json::json!({"verdict": "approve", "outcome": format!("{outcome:?}"), "iteration": iteration})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("persist failed: {e}")).into_response(),
    }
}

fn truncate_for_summary(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max_chars).collect::<String>())
    }
}

/// `POST /api/runs/{id}/panels/proposals/{proposal_id}/approve` -- the actual
/// human-install step: moves a pending proposal into the real `custom_panels`
/// list (`source: Some("assistant")`, honestly distinguishing it from a
/// hand-written one), removing it from the pending list. Owner-restricted like
/// every other real mutation.
async fn approve_panel_proposal(
    State(state): State<AppState>,
    AxPath((id, proposal_id)): AxPath<(String, String)>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    if !valid_run_id(&id) {
        return (StatusCode::BAD_REQUEST, "run_id must be non-empty alphanumeric/-/_ only").into_response();
    }
    if !run_exists(&state, &id) {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    }
    let _guard = state.write_lock.lock().await;
    let dir = run_dir(&state, &id);
    let (spec, mut run_state) = match load_or_init_run(&dir, &id) {
        Ok(v) => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("load failed: {e}")).into_response(),
    };
    if !owner_authorized(&headers, &run_state) {
        return (StatusCode::FORBIDDEN, "this run belongs to a different account").into_response();
    }
    let Some(pos) = run_state.pending_panel_proposals.iter().position(|p| p.id == proposal_id) else {
        return (StatusCode::NOT_FOUND, format!("no pending proposal with id {proposal_id:?}")).into_response();
    };
    let proposal = run_state.pending_panel_proposals.remove(pos);
    let panel = CustomPanel { id: proposal.id, title: proposal.title, html: proposal.html, source: Some("assistant".to_string()), created_at: unix_now() };
    run_state.custom_panels.push(panel.clone());
    match persist_run(&dir, &spec, &run_state) {
        Ok(()) => Json(panel).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("persist failed: {e}")).into_response(),
    }
}

/// `POST /api/runs/{id}/panels/proposals/{proposal_id}/reject` -- discards a
/// pending proposal outright; nothing was ever live, so there's nothing to undo
/// beyond removing it from the pending list.
async fn reject_panel_proposal(
    State(state): State<AppState>,
    AxPath((id, proposal_id)): AxPath<(String, String)>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    if !valid_run_id(&id) {
        return (StatusCode::BAD_REQUEST, "run_id must be non-empty alphanumeric/-/_ only").into_response();
    }
    if !run_exists(&state, &id) {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    }
    let _guard = state.write_lock.lock().await;
    let dir = run_dir(&state, &id);
    let (spec, mut run_state) = match load_or_init_run(&dir, &id) {
        Ok(v) => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("load failed: {e}")).into_response(),
    };
    if !owner_authorized(&headers, &run_state) {
        return (StatusCode::FORBIDDEN, "this run belongs to a different account").into_response();
    }
    let before = run_state.pending_panel_proposals.len();
    run_state.pending_panel_proposals.retain(|p| p.id != proposal_id);
    if run_state.pending_panel_proposals.len() == before {
        return (StatusCode::NOT_FOUND, format!("no pending proposal with id {proposal_id:?}")).into_response();
    }
    match persist_run(&dir, &spec, &run_state) {
        Ok(()) => Json(serde_json::json!({"rejected": proposal_id})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("persist failed: {e}")).into_response(),
    }
}

/// `POST /api/runs/{id}/panels/{panel_id}/propose-remove` -- the other real
/// half of gap #4 (#382 goal doc §7.2). Mirrors `propose_custom_panel`'s own
/// trust model exactly, inverted: the assistant can already propose ADDING a
/// panel (pending until a human approves); this lets it propose REMOVING an
/// existing one, equally pending until a human approves -- never applied
/// directly, since removal is destructive and irreversible the same way the
/// human's own Remove button already gets a real confirm() dialog for.
async fn propose_panel_removal(
    State(state): State<AppState>,
    AxPath((id, panel_id)): AxPath<(String, String)>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    if !valid_run_id(&id) {
        return (StatusCode::BAD_REQUEST, "run_id must be non-empty alphanumeric/-/_ only").into_response();
    }
    if !run_exists(&state, &id) {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    }
    let _guard = state.write_lock.lock().await;
    let dir = run_dir(&state, &id);
    let (spec, mut run_state) = match load_or_init_run(&dir, &id) {
        Ok(v) => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("load failed: {e}")).into_response(),
    };
    if !owner_authorized(&headers, &run_state) {
        return (StatusCode::FORBIDDEN, "this run belongs to a different account").into_response();
    }
    // Same real defensive cap as add_custom_panel -- see its own doc comment.
    if run_state.pending_panel_removal_proposals.len() >= MAX_LIST_ITEMS {
        return (StatusCode::BAD_REQUEST, format!("pending_panel_removal_proposals is at its defensive cap of {MAX_LIST_ITEMS} items")).into_response();
    }
    let Some(panel) = run_state.custom_panels.iter().find(|p| p.id == panel_id) else {
        return (StatusCode::NOT_FOUND, format!("no custom panel with id {panel_id:?}")).into_response();
    };
    let proposal = PendingPanelRemovalProposal {
        id: format!("{:016x}", rand::random::<u64>()),
        panel_id: panel_id.clone(),
        panel_title: panel.title.clone(),
        proposed_at: unix_now(),
    };
    run_state.pending_panel_removal_proposals.push(proposal.clone());
    match persist_run(&dir, &spec, &run_state) {
        Ok(()) => Json(proposal).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("persist failed: {e}")).into_response(),
    }
}

/// `POST /api/runs/{id}/panels/removal-proposals/{proposal_id}/approve` --
/// the actual destructive step: removes the real panel from `custom_panels`
/// for real, and drops the proposal. If the panel was already removed some
/// other way in the meantime (e.g. a human used the direct Remove button),
/// this still clears the now-stale proposal rather than leaving it dangling,
/// but honestly reports that nothing was actually removed by this call.
async fn approve_panel_removal(
    State(state): State<AppState>,
    AxPath((id, proposal_id)): AxPath<(String, String)>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    if !valid_run_id(&id) {
        return (StatusCode::BAD_REQUEST, "run_id must be non-empty alphanumeric/-/_ only").into_response();
    }
    if !run_exists(&state, &id) {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    }
    let _guard = state.write_lock.lock().await;
    let dir = run_dir(&state, &id);
    let (spec, mut run_state) = match load_or_init_run(&dir, &id) {
        Ok(v) => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("load failed: {e}")).into_response(),
    };
    if !owner_authorized(&headers, &run_state) {
        return (StatusCode::FORBIDDEN, "this run belongs to a different account").into_response();
    }
    let Some(pos) = run_state.pending_panel_removal_proposals.iter().position(|p| p.id == proposal_id) else {
        return (StatusCode::NOT_FOUND, format!("no pending removal proposal with id {proposal_id:?}")).into_response();
    };
    let proposal = run_state.pending_panel_removal_proposals.remove(pos);
    let before = run_state.custom_panels.len();
    run_state.custom_panels.retain(|p| p.id != proposal.panel_id);
    let actually_removed = run_state.custom_panels.len() != before;
    match persist_run(&dir, &spec, &run_state) {
        Ok(()) => Json(serde_json::json!({"removed": actually_removed, "panel_id": proposal.panel_id})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("persist failed: {e}")).into_response(),
    }
}

/// `POST /api/runs/{id}/panels/removal-proposals/{proposal_id}/reject` --
/// the safe direction: the panel was never touched, so this only ever drops
/// the pending proposal.
async fn reject_panel_removal(
    State(state): State<AppState>,
    AxPath((id, proposal_id)): AxPath<(String, String)>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    if !valid_run_id(&id) {
        return (StatusCode::BAD_REQUEST, "run_id must be non-empty alphanumeric/-/_ only").into_response();
    }
    if !run_exists(&state, &id) {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    }
    let _guard = state.write_lock.lock().await;
    let dir = run_dir(&state, &id);
    let (spec, mut run_state) = match load_or_init_run(&dir, &id) {
        Ok(v) => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("load failed: {e}")).into_response(),
    };
    if !owner_authorized(&headers, &run_state) {
        return (StatusCode::FORBIDDEN, "this run belongs to a different account").into_response();
    }
    let before = run_state.pending_panel_removal_proposals.len();
    run_state.pending_panel_removal_proposals.retain(|p| p.id != proposal_id);
    if run_state.pending_panel_removal_proposals.len() == before {
        return (StatusCode::NOT_FOUND, format!("no pending removal proposal with id {proposal_id:?}")).into_response();
    }
    match persist_run(&dir, &spec, &run_state) {
        Ok(()) => Json(serde_json::json!({"rejected": proposal_id})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("persist failed: {e}")).into_response(),
    }
}

#[derive(Deserialize)]
struct ProposePanelEditRequest {
    title: String,
    html: String,
}

/// `POST /api/runs/{id}/panels/{panel_id}/propose-edit` -- the last real piece
/// of gap #4 (#382 goal doc §7.2): the assistant could already propose ADDING
/// or REMOVING a custom panel; this lets it propose EDITING an existing one,
/// gated exactly the same way -- overwriting real content is exactly as
/// irreversible as removing it, so it never applies directly.
async fn propose_panel_edit(
    State(state): State<AppState>,
    AxPath((id, panel_id)): AxPath<(String, String)>,
    headers: axum::http::HeaderMap,
    Json(body): Json<ProposePanelEditRequest>,
) -> impl IntoResponse {
    if !valid_run_id(&id) {
        return (StatusCode::BAD_REQUEST, "run_id must be non-empty alphanumeric/-/_ only").into_response();
    }
    if !run_exists(&state, &id) {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    }
    let new_title = body.title.trim().to_string();
    if new_title.is_empty() {
        return (StatusCode::BAD_REQUEST, "title must not be empty").into_response();
    }
    // Same real gap as add_custom_panel -- see its own doc comment (#382 goal doc §8,
    // 2026-08-06). This is the assistant-edit-proposal path -- also the one that
    // shows an old_title/new_title diff in the review queue, so a spoofed
    // new_title is exactly what a reviewer is trying to compare against.
    if contains_bidi_control_char(&new_title) {
        return (
            StatusCode::BAD_REQUEST,
            "title contains a Unicode bidi control character (e.g. a right-to-left override) -- \
             these can make the visually displayed text not match what's actually stored"
                .to_string(),
        )
            .into_response();
    }
    // Same real gap as add_custom_panel -- see its own doc comment (#382 goal doc §8,
    // 2026-08-06). This is the assistant-edit-proposal path.
    if body.html.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "html must not be empty").into_response();
    }
    if body.html.len() > MAX_CUSTOM_PANEL_HTML_BYTES {
        return (StatusCode::BAD_REQUEST, format!("html must be under {MAX_CUSTOM_PANEL_HTML_BYTES} bytes")).into_response();
    }
    let _guard = state.write_lock.lock().await;
    let dir = run_dir(&state, &id);
    let (spec, mut run_state) = match load_or_init_run(&dir, &id) {
        Ok(v) => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("load failed: {e}")).into_response(),
    };
    if !owner_authorized(&headers, &run_state) {
        return (StatusCode::FORBIDDEN, "this run belongs to a different account").into_response();
    }
    // Same real defensive cap as add_custom_panel -- see its own doc comment.
    if run_state.pending_panel_edit_proposals.len() >= MAX_LIST_ITEMS {
        return (StatusCode::BAD_REQUEST, format!("pending_panel_edit_proposals is at its defensive cap of {MAX_LIST_ITEMS} items")).into_response();
    }
    let Some(panel) = run_state.custom_panels.iter().find(|p| p.id == panel_id) else {
        return (StatusCode::NOT_FOUND, format!("no custom panel with id {panel_id:?}")).into_response();
    };
    let proposal = PendingPanelEditProposal {
        id: format!("{:016x}", rand::random::<u64>()),
        panel_id: panel_id.clone(),
        old_title: panel.title.clone(),
        new_title,
        new_html: body.html,
        proposed_at: unix_now(),
    };
    run_state.pending_panel_edit_proposals.push(proposal.clone());
    match persist_run(&dir, &spec, &run_state) {
        Ok(()) => Json(proposal).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("persist failed: {e}")).into_response(),
    }
}

/// `POST /api/runs/{id}/panels/edit-proposals/{proposal_id}/approve` -- the
/// actual destructive step: overwrites the real panel's title/html for real,
/// and drops the proposal. If the panel was removed some other way in the
/// meantime, this honestly reports that nothing was actually updated, rather
/// than resurrecting it or silently dropping the now-stale proposal.
async fn approve_panel_edit(
    State(state): State<AppState>,
    AxPath((id, proposal_id)): AxPath<(String, String)>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    if !valid_run_id(&id) {
        return (StatusCode::BAD_REQUEST, "run_id must be non-empty alphanumeric/-/_ only").into_response();
    }
    if !run_exists(&state, &id) {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    }
    let _guard = state.write_lock.lock().await;
    let dir = run_dir(&state, &id);
    let (spec, mut run_state) = match load_or_init_run(&dir, &id) {
        Ok(v) => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("load failed: {e}")).into_response(),
    };
    if !owner_authorized(&headers, &run_state) {
        return (StatusCode::FORBIDDEN, "this run belongs to a different account").into_response();
    }
    let Some(pos) = run_state.pending_panel_edit_proposals.iter().position(|p| p.id == proposal_id) else {
        return (StatusCode::NOT_FOUND, format!("no pending edit proposal with id {proposal_id:?}")).into_response();
    };
    let proposal = run_state.pending_panel_edit_proposals.remove(pos);
    let updated = if let Some(panel) = run_state.custom_panels.iter_mut().find(|p| p.id == proposal.panel_id) {
        panel.title = proposal.new_title;
        panel.html = proposal.new_html;
        true
    } else {
        false
    };
    match persist_run(&dir, &spec, &run_state) {
        Ok(()) => Json(serde_json::json!({"updated": updated, "panel_id": proposal.panel_id})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("persist failed: {e}")).into_response(),
    }
}

/// `POST /api/runs/{id}/panels/edit-proposals/{proposal_id}/reject` -- the
/// safe direction: the panel's real content was never touched, so this only
/// ever drops the pending proposal.
async fn reject_panel_edit(
    State(state): State<AppState>,
    AxPath((id, proposal_id)): AxPath<(String, String)>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    if !valid_run_id(&id) {
        return (StatusCode::BAD_REQUEST, "run_id must be non-empty alphanumeric/-/_ only").into_response();
    }
    if !run_exists(&state, &id) {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    }
    let _guard = state.write_lock.lock().await;
    let dir = run_dir(&state, &id);
    let (spec, mut run_state) = match load_or_init_run(&dir, &id) {
        Ok(v) => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("load failed: {e}")).into_response(),
    };
    if !owner_authorized(&headers, &run_state) {
        return (StatusCode::FORBIDDEN, "this run belongs to a different account").into_response();
    }
    let before = run_state.pending_panel_edit_proposals.len();
    run_state.pending_panel_edit_proposals.retain(|p| p.id != proposal_id);
    if run_state.pending_panel_edit_proposals.len() == before {
        return (StatusCode::NOT_FOUND, format!("no pending edit proposal with id {proposal_id:?}")).into_response();
    }
    match persist_run(&dir, &spec, &run_state) {
        Ok(()) => Json(serde_json::json!({"rejected": proposal_id})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("persist failed: {e}")).into_response(),
    }
}

#[derive(Deserialize)]
struct ProposeStageRequest {
    stage_id: String,
    tag: String,
    rationale: String,
    #[serde(default)]
    use_existing_service: Option<String>,
    #[serde(default = "default_units")]
    units: u64,
    #[serde(default)]
    price_ceiling: Option<u64>,
}

/// `POST /api/runs/{id}/stages/propose` -- the assistant-facing half of the
/// self-optimizing-pipeline mandate (#382), gated the same way custom panels are
/// (#38): a real role-filler's own mid-iteration `StageProposal` (attached to a
/// real `IterationRecord`) still applies immediately via `run_iteration`,
/// unchanged -- this is specifically for the advisory chat assistant's
/// speculative suggestions, which never touch the live spec on their own.
/// `proposed_by` is always "devsystem.assistant" here, never client-supplied --
/// the accountability trail this field exists for (see `StageProposal`'s own
/// doc comment) would be meaningless if a caller could claim to be any stage.
async fn propose_stage(
    State(state): State<AppState>,
    AxPath(id): AxPath<String>,
    headers: axum::http::HeaderMap,
    Json(body): Json<ProposeStageRequest>,
) -> impl IntoResponse {
    if !valid_run_id(&id) {
        return (StatusCode::BAD_REQUEST, "run_id must be non-empty alphanumeric/-/_ only").into_response();
    }
    if !run_exists(&state, &id) {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    }
    let stage_id = body.stage_id.trim().to_string();
    let tag = body.tag.trim().to_string();
    let rationale = body.rationale.trim().to_string();
    if stage_id.is_empty() || tag.is_empty() || rationale.is_empty() {
        return (StatusCode::BAD_REQUEST, "stage_id, tag, and rationale must not be empty").into_response();
    }
    // Same real gap `validate_proposals` closes for the embedded-proposal path
    // (devsystem_pipeline::lib.rs) -- see its own doc comment. This is the
    // assistant-facing pending-review path: a human approving from the queue
    // trusts exactly this rationale to justify the proposal at face value.
    if contains_bidi_control_char(&rationale) {
        return (
            StatusCode::BAD_REQUEST,
            "rationale contains a Unicode bidi control character (e.g. a right-to-left override) \
             -- these can make the visually displayed text not match what's actually stored"
                .to_string(),
        )
            .into_response();
    }
    if body.units == 0 {
        return (StatusCode::BAD_REQUEST, "units must be at least 1").into_response();
    }
    if body.units > MAX_ROLE_UNITS {
        return (StatusCode::BAD_REQUEST, format!("units must be at most {MAX_ROLE_UNITS}")).into_response();
    }
    let _guard = state.write_lock.lock().await;
    let dir = run_dir(&state, &id);
    let (spec, mut run_state) = match load_or_init_run(&dir, &id) {
        Ok(v) => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("load failed: {e}")).into_response(),
    };
    if !owner_authorized(&headers, &run_state) {
        return (StatusCode::FORBIDDEN, "this run belongs to a different account").into_response();
    }
    // Same real defensive cap as add_custom_panel -- see its own doc comment.
    if run_state.pending_stage_proposals.len() >= MAX_LIST_ITEMS {
        return (StatusCode::BAD_REQUEST, format!("pending_stage_proposals is at its defensive cap of {MAX_LIST_ITEMS} items")).into_response();
    }
    let proposal = PendingStageProposal {
        id: format!("{:016x}", rand::random::<u64>()),
        proposal: StageProposal {
            proposed_by: "devsystem.assistant".to_string(),
            stage_id,
            tag,
            rationale,
            use_existing_service: body.use_existing_service,
            units: body.units,
            price_ceiling: body.price_ceiling,
        },
        proposed_at: unix_now(),
    };
    run_state.pending_stage_proposals.push(proposal.clone());
    match persist_run(&dir, &spec, &run_state) {
        Ok(()) => Json(proposal).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("persist failed: {e}")).into_response(),
    }
}

/// `POST /api/runs/{id}/stages/proposals/{proposal_id}/approve` -- applies the
/// pending proposal to the *live* spec for real, via the exact same
/// `apply_proposal` a real role-filler's iteration-time proposal goes through
/// (idempotent: `AlreadyPresent` if some other path already added this
/// `stage_id`, still removed from pending either way since there's nothing
/// left to approve).
async fn approve_stage_proposal(
    State(state): State<AppState>,
    AxPath((id, proposal_id)): AxPath<(String, String)>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    if !valid_run_id(&id) {
        return (StatusCode::BAD_REQUEST, "run_id must be non-empty alphanumeric/-/_ only").into_response();
    }
    if !run_exists(&state, &id) {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    }
    let _guard = state.write_lock.lock().await;
    let dir = run_dir(&state, &id);
    let (mut spec, mut run_state) = match load_or_init_run(&dir, &id) {
        Ok(v) => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("load failed: {e}")).into_response(),
    };
    if !owner_authorized(&headers, &run_state) {
        return (StatusCode::FORBIDDEN, "this run belongs to a different account").into_response();
    }
    let Some(pos) = run_state.pending_stage_proposals.iter().position(|p| p.id == proposal_id) else {
        return (StatusCode::NOT_FOUND, format!("no pending proposal with id {proposal_id:?}")).into_response();
    };
    let pending = run_state.pending_stage_proposals.remove(pos);
    let outcome = apply_proposal(&mut spec, &pending.proposal);
    if outcome == ProposalOutcome::Added {
        run_state.added_stages.push(pending.proposal.stage_id.clone());
    }
    // Real gap found live by the stress test, twenty-fifth run, 2026-08-06 (see
    // RunState::approved_stage_proposals' own doc comment): this path used to
    // discard `pending.proposal` here entirely, permanently losing its real
    // price_ceiling the moment it was approved. Pushed unconditionally (not
    // gated on `Added`) since the twenty-seventh run: a real re-proposal of an
    // already-live role (a human trying to *fix* an unbounded one) correctly
    // gets `AlreadyPresent` here, but that real, newer price_ceiling
    // information must not be silently discarded either -- see
    // `no_price_ceiling`'s own doc comment for the "latest entry wins" read side.
    run_state.approved_stage_proposals.push(pending.proposal.clone());
    match persist_run(&dir, &spec, &run_state) {
        Ok(()) => Json(serde_json::json!({
            "stage_id": pending.proposal.stage_id,
            "outcome": if outcome == ProposalOutcome::Added { "added" } else { "already_present" },
            "roles_now": spec.roles.len(),
        }))
        .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("persist failed: {e}")).into_response(),
    }
}

/// `POST /api/runs/{id}/stages/proposals/{proposal_id}/reject` -- discards a
/// pending stage proposal; the live spec was never touched, so there's nothing
/// to undo beyond removing it from the pending list.
async fn reject_stage_proposal(
    State(state): State<AppState>,
    AxPath((id, proposal_id)): AxPath<(String, String)>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    if !valid_run_id(&id) {
        return (StatusCode::BAD_REQUEST, "run_id must be non-empty alphanumeric/-/_ only").into_response();
    }
    if !run_exists(&state, &id) {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    }
    let _guard = state.write_lock.lock().await;
    let dir = run_dir(&state, &id);
    let (spec, mut run_state) = match load_or_init_run(&dir, &id) {
        Ok(v) => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("load failed: {e}")).into_response(),
    };
    if !owner_authorized(&headers, &run_state) {
        return (StatusCode::FORBIDDEN, "this run belongs to a different account").into_response();
    }
    let before = run_state.pending_stage_proposals.len();
    run_state.pending_stage_proposals.retain(|p| p.id != proposal_id);
    if run_state.pending_stage_proposals.len() == before {
        return (StatusCode::NOT_FOUND, format!("no pending proposal with id {proposal_id:?}")).into_response();
    }
    match persist_run(&dir, &spec, &run_state) {
        Ok(()) => Json(serde_json::json!({"rejected": proposal_id})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("persist failed: {e}")).into_response(),
    }
}

const MAX_ISSUE_TITLE_LEN: usize = 300;
const MAX_ISSUE_BODY_LEN: usize = 20_000;
/// `owner/repo` this deployment will actually let a proposal target -- the
/// operator's own explicit ask names CADS-webconference-demo; kept as a real
/// allowlist rather than a free-form field so a proposal (assistant-authored
/// text) can never point a real GitHub write at an arbitrary repo.
const ISSUE_PROPOSAL_REPO_ALLOWLIST: &[&str] = &["scimbe/CADS-webconference-demo"];

#[derive(Deserialize)]
struct ProposeIssueRequest {
    repo: String,
    title: String,
    body: String,
}

/// `POST /api/runs/{id}/issues/propose` -- real "self-healing" (operator ask,
/// 2026-08-04): the assistant notices a genuine gap/error and drafts a real
/// GitHub issue, but this alone never reaches GitHub -- see
/// `RunState::pending_issue_proposals`'s doc comment for the trust-model
/// reasoning (same gate as custom panels/stage proposals).
async fn propose_issue(
    State(state): State<AppState>,
    AxPath(id): AxPath<String>,
    headers: axum::http::HeaderMap,
    Json(body): Json<ProposeIssueRequest>,
) -> impl IntoResponse {
    if !valid_run_id(&id) {
        return (StatusCode::BAD_REQUEST, "run_id must be non-empty alphanumeric/-/_ only").into_response();
    }
    if !run_exists(&state, &id) {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    }
    let repo = body.repo.trim().to_string();
    let title = body.title.trim().to_string();
    let issue_body = body.body.trim().to_string();
    if !ISSUE_PROPOSAL_REPO_ALLOWLIST.contains(&repo.as_str()) {
        return (
            StatusCode::BAD_REQUEST,
            format!("repo must be one of {ISSUE_PROPOSAL_REPO_ALLOWLIST:?} -- proposing against an arbitrary repo isn't allowed"),
        )
            .into_response();
    }
    if title.is_empty() || issue_body.is_empty() {
        return (StatusCode::BAD_REQUEST, "title and body must not be empty").into_response();
    }
    if title.len() > MAX_ISSUE_TITLE_LEN {
        return (StatusCode::BAD_REQUEST, format!("title must be under {MAX_ISSUE_TITLE_LEN} characters")).into_response();
    }
    if issue_body.len() > MAX_ISSUE_BODY_LEN {
        return (StatusCode::BAD_REQUEST, format!("body must be under {MAX_ISSUE_BODY_LEN} characters")).into_response();
    }
    // Same bidi-control-character (Trojan Source) class already closed for
    // requirement statement/criteria, milestones, backlog, custom-panel title,
    // stage-proposal rationale, role fill-mode label, and next-step draft text
    // (#382 goal doc §8, 2026-08-06) -- a human approving this proposal from the
    // review queue trusts exactly this title/body, and approving it files a
    // real GitHub issue with whatever content is actually stored.
    if contains_bidi_control_char(&title) || contains_bidi_control_char(&issue_body) {
        return (
            StatusCode::BAD_REQUEST,
            "title/body contains a Unicode bidi control character (e.g. a right-to-left override) -- \
             these can make the visually displayed text not match what's actually stored"
                .to_string(),
        )
            .into_response();
    }
    let _guard = state.write_lock.lock().await;
    let dir = run_dir(&state, &id);
    let (spec, mut run_state) = match load_or_init_run(&dir, &id) {
        Ok(v) => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("load failed: {e}")).into_response(),
    };
    if !owner_authorized(&headers, &run_state) {
        return (StatusCode::FORBIDDEN, "this run belongs to a different account").into_response();
    }
    // Same real defensive cap as add_custom_panel -- see its own doc comment.
    if run_state.pending_issue_proposals.len() >= MAX_LIST_ITEMS {
        return (StatusCode::BAD_REQUEST, format!("pending_issue_proposals is at its defensive cap of {MAX_LIST_ITEMS} items")).into_response();
    }
    let proposal = PendingIssueProposal { id: format!("{:016x}", rand::random::<u64>()), repo, title, body: issue_body, proposed_at: unix_now() };
    run_state.pending_issue_proposals.push(proposal.clone());
    match persist_run(&dir, &spec, &run_state) {
        Ok(()) => Json(proposal).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("persist failed: {e}")).into_response(),
    }
}

#[derive(Deserialize)]
struct CreatedGithubIssue {
    number: u64,
    html_url: String,
}

/// What either real posting path (direct-POST or channel-relay) produces on
/// success -- `number` is `None` for the channel path's "already filed"
/// outcome (`github_issue_channel_handler`'s own real dedup memory reporting
/// back a prior URL, not a freshly created issue with a real new number).
struct PostedIssue {
    number: Option<u64>,
    html_url: String,
    already_filed: bool,
}

fn issue_error(status: StatusCode, error: impl Into<String>) -> (StatusCode, Json<serde_json::Value>) {
    (status, Json(serde_json::json!({ "error": error.into() })))
}

/// The original, still-supported path: this process's own `github_token`
/// POSTs straight to the GitHub REST API. See `approve_issue_proposal`'s doc
/// comment for when this is used instead of the channel relay.
async fn post_issue_directly(client: &reqwest::Client, token: &str, repo: &str, title: &str, body: &str) -> Result<PostedIssue, (StatusCode, Json<serde_json::Value>)> {
    let url = format!("https://api.github.com/repos/{repo}/issues");
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "devsystem-web-issue-proposal/1 (+https://github.com/scimbe/CADS-devsystem)")
        .json(&serde_json::json!({"title": title, "body": body}))
        .send()
        .await;
    match resp {
        Ok(r) if r.status().is_success() => match r.json::<CreatedGithubIssue>().await {
            Ok(created) => Ok(PostedIssue { number: Some(created.number), html_url: created.html_url, already_filed: false }),
            Err(e) => Err(issue_error(StatusCode::BAD_GATEWAY, format!("GitHub returned a real issue but the response couldn't be parsed: {e}"))),
        },
        Ok(r) => {
            let status = r.status();
            let text = r.text().await.unwrap_or_default();
            Err(issue_error(StatusCode::BAD_GATEWAY, format!("GitHub rejected the issue create: HTTP {status}: {text}")))
        }
        Err(e) => Err(issue_error(StatusCode::BAD_GATEWAY, format!("could not reach GitHub: {e}"))),
    }
}

/// Parses `github_issue_channel_client`'s own real stdout contract (see that
/// binary's `main()`): `"created: #<n> <url>"` or `"already filed: <url>"`,
/// on whichever line actually matches -- deliberately not JSON, matching the
/// exact plain-text shape that binary really prints, not a guessed one.
fn parse_issue_channel_client_stdout(stdout: &str) -> Option<PostedIssue> {
    stdout.lines().find_map(|line| {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("created: #") {
            let (num_str, url) = rest.split_once(' ')?;
            let number = num_str.trim().parse::<u64>().ok()?;
            return Some(PostedIssue { number: Some(number), html_url: url.trim().to_string(), already_filed: false });
        }
        line.strip_prefix("already filed: ").map(|url| PostedIssue { number: None, html_url: url.trim().to_string(), already_filed: true })
    })
}

/// The new #48 slice 6 path: shells out to a real `github_issue_channel_client`
/// subprocess, which itself dials the real, isolated `github_issue_channel_handler`
/// agent over a real CADS-Tunnel Agent-Fabric channel and holds the real GitHub
/// token -- never this process. Every env var this subprocess needs is set
/// explicitly here (not left to ambient inheritance) so the exact contract is
/// visible in one place and independently testable, matching
/// `github_issue_channel_client.rs`'s own tests' `fake_ct_agent` pattern (a
/// fake binary standing in for the real one, here a fake client binary
/// standing in for the real `github_issue_channel_client`).
///
/// Exit status IS meaningful for this specific subprocess (unlike the
/// handler's own always-exit-0 contract, see that binary's module doc): a
/// non-zero exit means the channel round trip itself failed, or the handler
/// reported a real `IssueResponse::Error` (that binary prints it to stderr and
/// exits non-zero) -- either way, a real failure, surfaced honestly via
/// stderr, never treated as success.
async fn post_issue_via_channel(cfg: &IssueChannelConfig, repo: &str, title: &str, body: &str) -> Result<PostedIssue, (StatusCode, Json<serde_json::Value>)> {
    let output = tokio::process::Command::new(cfg.client_bin.as_ref())
        .arg(repo)
        .arg(title)
        .arg(body)
        .env("CT_AGENT_BIN", cfg.ct_agent_bin.as_ref())
        .env("CT_CHANNEL_ADDR", cfg.addr.as_ref())
        .env("CT_CHANNEL_NOISE_KEY", cfg.noise_key.as_ref())
        .env("CT_CHANNEL_PEER_NOISE_KEY", cfg.peer_noise_key.as_ref())
        .env("CT_CHANNEL_PEER_CERT_FILE", cfg.peer_cert_file.as_ref())
        .stdin(std::process::Stdio::null())
        .output()
        .await;
    match output {
        Ok(out) if out.status.success() => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            parse_issue_channel_client_stdout(&stdout)
                .ok_or_else(|| issue_error(StatusCode::BAD_GATEWAY, format!("github_issue_channel_client exited 0 but printed no line this deployment recognizes: {stdout:?}")))
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            Err(issue_error(StatusCode::BAD_GATEWAY, format!("github_issue_channel_client exited with {}: {}", out.status, stderr.trim())))
        }
        Err(e) => Err(issue_error(StatusCode::BAD_GATEWAY, format!("could not run the issue-channel client ({}): {e}", cfg.client_bin))),
    }
}

/// See `AppState::document_extraction_channel`'s doc comment. Same real
/// subprocess-shape as `post_issue_via_channel`, two real differences:
/// (1) `devsystem_document_extraction_client`'s own CLI contract is a real
/// file *path* (it reads the file itself, to derive `mime_type` from the
/// real extension -- see that binary's own `mime_type_for`), not stdin/argv
/// content, so the real uploaded bytes are written to a real temp file
/// first, named to preserve the original extension, and always cleaned up
/// (`defer`-style via the `_cleanup` guard) whether the call succeeds or
/// fails. (2) This is the broker-mediated relay-only connection model, not
/// direct-address -- see `DocumentExtractionChannelConfig`'s own doc comment
/// for why these two models aren't interchangeable.
async fn extract_via_channel(cfg: &DocumentExtractionChannelConfig, filename: &str, bytes: &[u8]) -> Result<String, String> {
    let ext = std::path::Path::new(filename).extension().and_then(|e| e.to_str()).unwrap_or("bin");
    let tmp_path = std::env::temp_dir().join(format!("devsystem-rag-upload-{:016x}.{ext}", rand::random::<u64>()));
    if let Err(e) = tokio::fs::write(&tmp_path, bytes).await {
        return Err(format!("could not write a real temp file for the extraction client: {e}"));
    }
    struct CleanupOnDrop(std::path::PathBuf);
    impl Drop for CleanupOnDrop {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let _cleanup = CleanupOnDrop(tmp_path.clone());

    let output = tokio::process::Command::new(cfg.client_bin.as_ref())
        .arg(&tmp_path)
        .env("CT_AGENT_BIN", cfg.ct_agent_bin.as_ref())
        .env("CT_CHANNEL_BROKER", cfg.broker.as_ref())
        .env("CT_CHANNEL_RELAY", cfg.relay.as_ref())
        .env("CT_CHANNEL_GRANT", cfg.grant_hex.as_ref())
        .env("CT_CHANNEL_HOLDER_KEY", cfg.holder_key_hex.as_ref())
        .env("CT_CHANNEL_NOISE_KEY", cfg.noise_key.as_ref())
        .stdin(std::process::Stdio::null())
        .output()
        .await;
    match output {
        Ok(out) if out.status.success() => Ok(String::from_utf8_lossy(&out.stdout).trim_end_matches('\n').to_string()),
        Ok(out) => Err(format!("devsystem_document_extraction_client exited with {}: {}", out.status, String::from_utf8_lossy(&out.stderr).trim())),
        Err(e) => Err(format!("could not run the document-extraction channel client ({}): {e}", cfg.client_bin)),
    }
}

/// `POST /api/runs/{id}/issues/proposals/{proposal_id}/approve` -- the actual
/// "eingebaut nach meiner Zustimmung" (built in after my approval) step: files
/// the real issue. Two real posting paths, tried in this order:
///
/// 1. The channel relay (`issue_channel`, #48 slice 6), when fully configured --
///    the GitHub credential lives only on the separate `github_issue_channel_handler`
///    process this dials over a real channel, never in this process. This is the
///    intended production path going forward.
/// 2. `github_token`, POSTing straight to the GitHub REST API -- the original
///    path, kept working unmodified for any deployment that only ever
///    configured `DEVSYSTEM_GITHUB_TOKEN` and hasn't set up the channel relay.
///
/// Honest `503` (never a silent no-op) when NEITHER is configured -- the
/// drafted title/body/repo are still in the response either way, so the
/// operator can copy-paste and post it by hand as a fallback, same pattern as
/// CADS-Tunnel's `ensure_user` relaying a temp password out of band when
/// there's no real email mechanism.
async fn approve_issue_proposal(
    State(state): State<AppState>,
    AxPath((id, proposal_id)): AxPath<(String, String)>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    if !valid_run_id(&id) {
        return (StatusCode::BAD_REQUEST, "run_id must be non-empty alphanumeric/-/_ only").into_response();
    }
    if !run_exists(&state, &id) {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    }
    let _guard = state.write_lock.lock().await;
    let dir = run_dir(&state, &id);
    let (spec, mut run_state) = match load_or_init_run(&dir, &id) {
        Ok(v) => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("load failed: {e}")).into_response(),
    };
    if !owner_authorized(&headers, &run_state) {
        return (StatusCode::FORBIDDEN, "this run belongs to a different account").into_response();
    }
    let Some(pos) = run_state.pending_issue_proposals.iter().position(|p| p.id == proposal_id) else {
        return (StatusCode::NOT_FOUND, format!("no pending proposal with id {proposal_id:?}")).into_response();
    };

    let issue_channel = state.issue_channel.clone();
    let github_token = state.github_token.clone();
    if issue_channel.is_none() && github_token.is_none() {
        let proposal = &run_state.pending_issue_proposals[pos];
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "neither the issue-proposal channel relay nor DEVSYSTEM_GITHUB_TOKEN is configured on this deployment -- post it by hand instead",
                "repo": proposal.repo,
                "title": proposal.title,
                "body": proposal.body,
            })),
        )
            .into_response();
    }

    let proposal = run_state.pending_issue_proposals[pos].clone();
    let posted = if let Some(cfg) = issue_channel {
        post_issue_via_channel(&cfg, &proposal.repo, &proposal.title, &proposal.body).await
    } else {
        post_issue_directly(&state.http_client, github_token.as_deref().expect("checked above: one of the two is Some"), &proposal.repo, &proposal.title, &proposal.body).await
    };
    let posted = match posted {
        Ok(p) => p,
        Err((status, body)) => return (status, body).into_response(),
    };

    run_state.pending_issue_proposals.remove(pos);
    match persist_run(&dir, &spec, &run_state) {
        Ok(()) => Json(serde_json::json!({"number": posted.number, "html_url": posted.html_url, "already_filed": posted.already_filed})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("issue was posted ({}) but persisting the local state failed: {e}", posted.html_url)).into_response(),
    }
}

async fn reject_issue_proposal(
    State(state): State<AppState>,
    AxPath((id, proposal_id)): AxPath<(String, String)>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    if !valid_run_id(&id) {
        return (StatusCode::BAD_REQUEST, "run_id must be non-empty alphanumeric/-/_ only").into_response();
    }
    if !run_exists(&state, &id) {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    }
    let _guard = state.write_lock.lock().await;
    let dir = run_dir(&state, &id);
    let (spec, mut run_state) = match load_or_init_run(&dir, &id) {
        Ok(v) => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("load failed: {e}")).into_response(),
    };
    if !owner_authorized(&headers, &run_state) {
        return (StatusCode::FORBIDDEN, "this run belongs to a different account").into_response();
    }
    let before = run_state.pending_issue_proposals.len();
    run_state.pending_issue_proposals.retain(|p| p.id != proposal_id);
    if run_state.pending_issue_proposals.len() == before {
        return (StatusCode::NOT_FOUND, format!("no pending proposal with id {proposal_id:?}")).into_response();
    }
    match persist_run(&dir, &spec, &run_state) {
        Ok(()) => Json(serde_json::json!({"rejected": proposal_id})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("persist failed: {e}")).into_response(),
    }
}

/// Real offer intake -- the counterpart to CADS-Tunnel core deliberately having no
/// live bid-collection endpoint (verified directly against the checkout, not
/// assumed). Any process, on any host, that holds a real ed25519 key can sign a
/// `CapacityOffer` (`ct_common::channel::CapacityOffer::sign_new_with_services`,
/// the exact same constructor `devsystem-pipeline`'s own tests use) and POST it
/// here -- that's the whole "minimum agent": no channel/network infra of its own,
/// just this one HTTP call. Rejects an invalid signature or an already-expired
/// offer outright (`CapacityOffer::is_valid`, real cryptographic verification, not
/// a shape check) rather than silently accepting garbage. One current offer per
/// holder pubkey per run -- a resubmission replaces the holder's prior offer.
///
/// Deliberately no `MAX_ROLE_UNITS` check here, despite the auction view
/// rendering this offer's `units_available` under an `"units"` JSON key that
/// reads identically to `StageProposal.units` -- investigated live 2026-08-06
/// after `devsystem_offer --units 18446744073709551615`/`--units 0` both got a
/// real `200` here with no rejection, initially suspected as the same
/// unbounded-`units` bug class `MAX_ROLE_UNITS` already closed at
/// `propose_stage`/`quick_submit_offer`/`validate_proposals`. It isn't: checked
/// directly against `ct_common::channel::CapacityOffer` at this crate's own
/// pinned tag (`v0.4.13`) rather than assumed from the JSON key name alone --
/// `units_available` is "tokens for cloud quota, job-units for local hardware,"
/// a bidder's own real capacity claim, semantically unrelated to
/// `StageProposal.units` ("how many bidder slots a role needs"). Capping a real
/// bidder's declared capacity at 100 would incorrectly reject any legitimately
/// large real-world provider. CADS-Tunnel core's own `CapacityOffer::is_valid`
/// doesn't bound it either (checked directly, not assumed) -- this is the
/// underlying protocol's own design, not a gap in this endpoint.
async fn submit_offer(State(state): State<AppState>, AxPath(id): AxPath<String>, Json(offer): Json<CapacityOffer>) -> impl IntoResponse {
    if !valid_run_id(&id) {
        return (StatusCode::BAD_REQUEST, "run_id must be non-empty alphanumeric/-/_ only").into_response();
    }
    if !run_exists(&state, &id) {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    }
    if !offer.is_valid(unix_now()) {
        return (StatusCode::BAD_REQUEST, "offer signature is invalid or it is already expired").into_response();
    }
    let mut offers = state.offers.lock().await;
    let run_offers = offers.entry(id).or_default();
    run_offers.retain(|o| o.holder_pubkey != offer.holder_pubkey);
    run_offers.push(offer);
    (StatusCode::OK, Json(serde_json::json!({"accepted": true}))).into_response()
}

#[derive(Deserialize)]
struct QuickOfferReq {
    stage_id: String,
    price: u64,
    #[serde(default = "default_units")]
    units: u64,
}
fn default_units() -> u64 {
    1
}

/// The browser-driven equivalent of running `devsystem_offer` by hand -- a real
/// signed `CapacityOffer`, not a fixture, built server-side with a fresh,
/// ephemeral ed25519 key generated per submission (a real CSPRNG, `rand::rngs::OsRng`
/// -- the same one `devsystem_offer --key-file` uses to mint a first-time
/// identity). Each submission is a genuinely new bidder identity, matching how
/// this run's roles have been staffed by hand via the CLI so far: an honest
/// "someone just bid this," not a persistent account -- account-scoped
/// resource ownership is a separate, larger increment (see the Architecture
/// panel's own note on this). Goes through the exact same acceptance path as
/// a real external agent's offer (`submit_offer`'s validation), just called
/// directly instead of round-tripping through HTTP to itself.
async fn quick_submit_offer(State(state): State<AppState>, AxPath(id): AxPath<String>, Json(body): Json<QuickOfferReq>) -> impl IntoResponse {
    if !valid_run_id(&id) {
        return (StatusCode::BAD_REQUEST, "run_id must be non-empty alphanumeric/-/_ only").into_response();
    }
    if !run_exists(&state, &id) {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    }
    if body.stage_id.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "stage_id is required").into_response();
    }
    if body.units == 0 {
        return (StatusCode::BAD_REQUEST, "units must be at least 1").into_response();
    }
    if body.units > MAX_ROLE_UNITS {
        return (StatusCode::BAD_REQUEST, format!("units must be at most {MAX_ROLE_UNITS}")).into_response();
    }

    let mut csprng = rand::rngs::OsRng;
    let signing_key = ed25519_dalek::SigningKey::generate(&mut csprng);
    let now = unix_now();
    let offer = CapacityOffer::sign_new_with_services(
        &signing_key,
        CapacityKind::CloudApiQuota,
        vec!["devsystem-web-quick-offer".to_string()],
        body.units,
        body.price,
        "usd".to_string(),
        now,
        now + 300,
        vec![ServiceType::Custom(body.stage_id.clone())],
    );
    let holder_hex: String = signing_key.verifying_key().to_bytes().iter().take(4).map(|b| format!("{b:02x}")).collect();

    let mut offers = state.offers.lock().await;
    let run_offers = offers.entry(id).or_default();
    run_offers.retain(|o| o.holder_pubkey != offer.holder_pubkey);
    run_offers.push(offer);
    (StatusCode::OK, Json(serde_json::json!({"accepted": true, "holder": holder_hex}))).into_response()
}

/// The real auction, not a fixture: runs `PipelineSpec::auction_view` (the same
/// primitive CADS-auction-demo proves out) over whatever real, currently-valid
/// offers have been submitted for this run's roles. Expired offers are excluded by
/// `auction_view` itself (`is_valid(now)`); a role with no qualifying offer makes
/// the whole call fail with `PipelineError::UnfilledRole` -- surfaced as a real,
/// honest per-role "no offers yet" rather than a fabricated empty bid list.
async fn view_auction(State(state): State<AppState>, AxPath(id): AxPath<String>) -> impl IntoResponse {
    if !valid_run_id(&id) {
        return (StatusCode::BAD_REQUEST, "run_id must be non-empty alphanumeric/-/_ only").into_response();
    }
    if !run_exists(&state, &id) {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    }
    let (spec, _run_state) = match load_or_init_run(&run_dir(&state, &id), &id) {
        Ok(v) => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response(),
    };
    let offers = state.offers.lock().await;
    let run_offers = offers.get(&id).cloned().unwrap_or_default();
    // Real, persisted per-run SelectionState (2026-08-05 fix) -- a fresh
    // SelectionState::default() every call meant RoundRobin/LeastCalls could
    // never actually accumulate cross-request state; holding the lock for the
    // mutable borrow below means auction_view's real mutation (the round-robin
    // cursor advancing, a served count incrementing) lands directly in the map,
    // not a value that's discarded the moment this handler returns.
    let mut selection_state_guard = state.selection_state.lock().await;
    let selection_state = selection_state_guard.entry(id.clone()).or_default();
    // No real identity-resolution registry is wired up yet (a real, honest gap,
    // not fabricated) -- label by a short hex prefix of the holder pubkey so bids
    // from different holders are at least distinguishable.
    let label = |holder: &[u8; 32]| holder.iter().take(4).map(|b| format!("{b:02x}")).collect::<String>();
    // RoleBidView (ct_common::pipeline) deliberately doesn't carry issued_at/
    // expires_at -- devsystem-web already holds the real CapacityOffers it collected
    // itself, so it enriches its own auction view with real bid freshness rather
    // than leaving the operator to guess whether a winning bid is fresh or about to
    // expire. Real groundwork for any future staleness-aware policy (penalize a
    // non-responsive bidder), not itself an enforcement decision.
    let freshness_by_label: std::collections::HashMap<String, (u64, u64)> =
        run_offers.iter().map(|o| (label(&o.holder_pubkey), (o.issued_at, o.expires_at))).collect();
    match spec.auction_view(&run_offers, unix_now(), spec.selection_policy, selection_state, label) {
        Ok(views) => {
            let now = unix_now();
            let mut roles_json = serde_json::to_value(&views).unwrap_or(serde_json::json!([]));
            if let Some(roles) = roles_json.as_array_mut() {
                for role in roles {
                    let Some(bids) = role.get_mut("bids").and_then(|b| b.as_array_mut()) else { continue };
                    for bid in bids {
                        let Some(who) = bid.get("who").and_then(|w| w.as_str()).map(str::to_string) else { continue };
                        if let Some(&(issued_at, expires_at)) = freshness_by_label.get(&who) {
                            if let Some(obj) = bid.as_object_mut() {
                                obj.insert("issued_at".to_string(), serde_json::json!(issued_at));
                                obj.insert("expires_at".to_string(), serde_json::json!(expires_at));
                                obj.insert("seconds_since_issued".to_string(), serde_json::json!(now.saturating_sub(issued_at)));
                            }
                        }
                    }
                }
            }
            Json(serde_json::json!({"roles": roles_json})).into_response()
        }
        Err(e) => (StatusCode::OK, Json(serde_json::json!({"error": e.to_string()}))).into_response(),
    }
}

#[derive(Deserialize)]
struct AssistantAsk {
    instruction: String,
}

/// Proxies to a real `devsystem_assistant --serve` bridge -- a separate process
/// (possibly a separate host: the real LLM CLI + its auth live wherever an
/// operator has them, not necessarily co-located with this container). This
/// handler never calls an LLM itself and never fabricates a response: with no
/// bridge configured, or if the bridge is unreachable, it says so plainly.
async fn ask_assistant(
    State(state): State<AppState>,
    AxPath(id): AxPath<String>,
    headers: axum::http::HeaderMap,
    Json(body): Json<AssistantAsk>,
) -> impl IntoResponse {
    if !valid_run_id(&id) {
        return (StatusCode::BAD_REQUEST, "run_id must be non-empty alphanumeric/-/_ only").into_response();
    }
    // Real gap found live 2026-08-06, stress-test run 34: every OTHER per-run
    // handler in this file returns a real 404 immediately for a run that
    // doesn't exist -- this one used to be the one exception, silently falling
    // through to a real network round-trip to the assistant bridge, which then
    // made its OWN round-trip back to this exact `GET /api/runs/{id}` (also a
    // 404), and only THEN surfaced a confusing wrapped `502`
    // (`"could not fetch run context from ...: HTTP 404 Not Found"`) -- a real,
    // reachable case now that a run can genuinely disappear mid-session (run 31's
    // delete-run feature): a chat message sent to a run deleted from another tab
    // got this exact confusing error, not a clean "no such run". Matching every
    // other handler's own convention closes this for real, not just for this one
    // symptom -- and saves a wasted round-trip to a process that was always going
    // to fail the identical way.
    if !run_exists(&state, &id) {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    }
    match load_or_init_run(&run_dir(&state, &id), &id) {
        Ok((_spec, run_state)) if !owner_authorized(&headers, &run_state) => {
            return (StatusCode::FORBIDDEN, "this run belongs to a different account").into_response();
        }
        Ok(_) => {}
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response(),
    }
    if body.instruction.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "instruction is required"}))).into_response();
    }
    let Some(assistant_url) = state.assistant_url.clone() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "devsystem.assistant is not configured on this deployment (DEVSYSTEM_ASSISTANT_URL unset)"})),
        )
            .into_response();
    };
    let url = format!("{}/ask", assistant_url.trim_end_matches('/'));
    let resp = state
        .http_client
        .post(&url)
        .json(&serde_json::json!({"run_id": id, "instruction": body.instruction}))
        .send()
        .await;
    match resp {
        Ok(r) => {
            let status = StatusCode::from_u16(r.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let text = r.text().await.unwrap_or_else(|e| serde_json::json!({"error": format!("could not read assistant response body: {e}")}).to_string());
            // Real usage accounting (#382 goal doc §7.3, gap #5) AND real chat
            // history (#382 goal doc §4.2, gap #6): this bridge call already
            // computes real token/cost usage (devsystem_assistant.rs's
            // parse_llm_json_output) and a real reply, both forwarded straight
            // to the caller below and, until now, never persisted anywhere --
            // a run's real cumulative assistant spend AND every past exchange
            // were unrecoverable once the browser tab closed. Best-effort: a
            // missing/malformed field never blocks the real reply below from
            // reaching the caller.
            if status.is_success() {
                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&text) {
                    let usage = parsed.get("usage");
                    let response_text = parsed.get("response").and_then(|v| v.as_str());
                    // Real per-requirement chat attribution (#382 goal doc
                    // §4.2, gap #6): the bridge already computed this from its
                    // own real, structured Action dispatch -- see
                    // devsystem_assistant.rs's requirement_indices_touched.
                    // Missing/malformed here just means no attribution, same
                    // honest fallback as usage/response_text above.
                    let requirement_indices: Vec<usize> = parsed
                        .get("requirement_indices")
                        .and_then(|v| v.as_array())
                        .map(|arr| arr.iter().filter_map(|v| v.as_u64()).map(|n| n as usize).collect())
                        .unwrap_or_default();
                    if usage.is_some() || response_text.is_some() {
                        persist_assistant_call(&state, &id, &body.instruction, response_text, usage, &requirement_indices).await;
                    }
                }
            }
            (status, [(axum::http::header::CONTENT_TYPE, "application/json")], text).into_response()
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({"error": format!("could not reach devsystem.assistant bridge at {url}: {e}")})),
        )
            .into_response(),
    }
}

/// See [`ask_assistant`]'s own doc comment for why this exists. Silently does
/// nothing if the run doesn't exist (nothing real to persist into) or if the
/// load/persist round-trip fails for any reason -- this accounting is real, but
/// it must never be the reason a real assistant reply fails to reach the caller.
/// One load/persist round-trip for both usage and chat history, not two --
/// they're always updated together, from the exact same real `/ask` call.
async fn persist_assistant_call(
    state: &AppState,
    id: &str,
    instruction: &str,
    response_text: Option<&str>,
    usage: Option<&serde_json::Value>,
    requirement_indices: &[usize],
) {
    if !run_exists(state, id) {
        return;
    }
    let _guard = state.write_lock.lock().await;
    let dir = run_dir(state, id);
    let Ok((spec, mut run_state)) = load_or_init_run(&dir, id) else { return };
    if let Some(usage) = usage {
        run_state.assistant_usage.add_call(usage);
    }
    if let Some(response) = response_text {
        push_chat_exchange(&mut run_state, instruction.to_string(), response.to_string(), unix_now(), requirement_indices.to_vec());
    }
    let _ = persist_run(&dir, &spec, &run_state);
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode as SC};
    use tower::ServiceExt;

    fn test_state() -> (AppState, tempfile::TempDir) {
        test_state_with_assistant(None)
    }

    fn test_state_with_assistant(assistant_url: Option<&str>) -> (AppState, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = AppState {
            runs_dir: Arc::new(dir.path().to_path_buf()),
            write_lock: Arc::new(tokio::sync::Mutex::new(())),
            offers: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            selection_state: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            assistant_url: assistant_url.map(Arc::from),
            github_token: None,
            http_client: reqwest::Client::builder().timeout(std::time::Duration::from_secs(5)).build().expect("build http client"),
            rag_embedding_api_key: None,
            rag_embedding_api_base: Arc::from("https://api.openai.com/v1"),
            rag_unstructured_api_key: None,
            rag_unstructured_api_base: Arc::from("https://api.unstructured.io"),
            rag_db_pool: None,
            issue_channel: None,
            document_extraction_channel: None,
        };
        (state, dir)
    }

    fn test_state_with_rag_embedding(api_key: &str, api_base: &str) -> (AppState, tempfile::TempDir) {
        let (mut state, dir) = test_state();
        state.rag_embedding_api_key = Some(Arc::from(api_key));
        state.rag_embedding_api_base = Arc::from(api_base);
        (state, dir)
    }

    fn test_state_with_rag_unstructured(api_key: &str, api_base: &str) -> (AppState, tempfile::TempDir) {
        let (mut state, dir) = test_state();
        state.rag_unstructured_api_key = Some(Arc::from(api_key));
        state.rag_unstructured_api_base = Arc::from(api_base);
        (state, dir)
    }

    /// A fake `github_issue_channel_client` standing in for the real
    /// subprocess -- same real reason `github_issue_channel_client.rs`'s own
    /// tests fake `ct-agent` rather than the real binary: proves
    /// `post_issue_via_channel`'s own env-passing/argv-passing/output-parsing
    /// logic without a real channel or a real `ct-agent`. The actual real
    /// channel round trip (this deployment's `github_issue_channel_client`
    /// talking to the real, separately-hosted `github_issue_channel_handler`)
    /// is live-verified by hand against the real deployment, same "hermetic
    /// test the logic, live-verify the real transport by hand" precedent
    /// `github_issue_channel_client.rs` itself already established.
    fn fake_issue_channel_client(dir: &std::path::Path, script: &str) -> String {
        let path = dir.join("fake-issue-channel-client.sh");
        std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        path.to_string_lossy().to_string()
    }

    fn test_state_with_issue_channel(client_script: &str) -> (AppState, tempfile::TempDir) {
        let (mut state, dir) = test_state();
        let client_bin = fake_issue_channel_client(dir.path(), client_script);
        state.issue_channel = Some(IssueChannelConfig {
            client_bin: Arc::from(client_bin),
            ct_agent_bin: Arc::from("fake-ct-agent"),
            addr: Arc::from("127.0.0.1:1"),
            noise_key: Arc::from("fake-noise-priv"),
            peer_noise_key: Arc::from("fake-noise-peer-pub"),
            peer_cert_file: Arc::from("/nonexistent/fake-cert-file"),
        });
        (state, dir)
    }

    /// A fake `devsystem_document_extraction_client` standing in for the real
    /// subprocess -- same real reason `fake_issue_channel_client` fakes that
    /// binary rather than a real `ct-agent`/real channel: proves
    /// `extract_via_channel`'s own temp-file/env-passing/output-parsing logic,
    /// not a real channel round trip (that's live-verified by hand against the
    /// real deployment, same "hermetic test the logic, live-verify the real
    /// transport by hand" precedent this file's own issue-channel tests
    /// already established).
    fn fake_extraction_client(dir: &std::path::Path, script: &str) -> String {
        let path = dir.join("fake-extraction-client.sh");
        std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        path.to_string_lossy().to_string()
    }

    fn test_state_with_document_extraction_channel(client_script: &str) -> (AppState, tempfile::TempDir) {
        let (mut state, dir) = test_state();
        let client_bin = fake_extraction_client(dir.path(), client_script);
        state.document_extraction_channel = Some(DocumentExtractionChannelConfig {
            client_bin: Arc::from(client_bin),
            ct_agent_bin: Arc::from("fake-ct-agent"),
            broker: Arc::from("127.0.0.1:4435"),
            relay: Arc::from("127.0.0.1:4436"),
            grant_hex: Arc::from("fake-grant-hex"),
            holder_key_hex: Arc::from("fake-holder-key-hex"),
            noise_key: Arc::from("fake-noise-priv"),
        });
        (state, dir)
    }

    /// A real signed CapacityOffer -- the exact constructor devsystem-pipeline's own
    /// tests use (`pipeline/src/lib.rs`), not a hand-rolled fixture.
    fn real_offer(seed: u8, service: &str, price: u64) -> CapacityOffer {
        let sk = ed25519_dalek::SigningKey::from_bytes(&[seed; 32]);
        CapacityOffer::sign_new_with_services(
            &sk,
            ct_common::channel::CapacityKind::CloudApiQuota,
            vec!["claude".into()],
            1,
            price,
            "usd".into(),
            0,
            u64::MAX,
            vec![ct_common::channel::ServiceType::Custom(service.to_string())],
        )
    }

    async fn body_json(response: axum::response::Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.expect("read body");
        serde_json::from_slice(&bytes).expect("valid json body")
    }

    fn json_request(method: &str, uri: &str, body: serde_json::Value) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .expect("build request")
    }

    #[tokio::test]
    async fn list_runs_empty_dir_returns_empty_array() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        let response = app
            .oneshot(Request::builder().uri("/api/runs").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK);
        assert_eq!(body_json(response).await, serde_json::json!([]));
    }

    #[tokio::test]
    async fn no_cors_headers_leak_to_a_cross_origin_request() {
        // The API and frontend are always same-origin (Caddy reverse-proxies the
        // whole domain to this one process); a permissive CorsLayer here would only
        // let a foreign origin's JS read responses in a real browser. Proves the
        // actual security property directly: even with a hostile-looking Origin
        // header, no access-control-allow-* header comes back.
        let (state, _dir) = test_state();
        let app = api_router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/runs")
                    .header("origin", "https://evil.example")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK);
        assert!(
            response.headers().get("access-control-allow-origin").is_none(),
            "no CORS layer should mean no access-control-allow-origin header, regardless of the request's Origin"
        );
    }

    #[tokio::test]
    async fn whoami_reports_the_real_gate_header_when_present() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        let response = app
            .oneshot(Request::builder().uri("/api/me").header("x-gate-email", "operator@example.com").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK);
        let body = body_json(response).await;
        assert_eq!(body["email"], "operator@example.com");
    }

    #[tokio::test]
    async fn whoami_reports_none_honestly_when_the_gate_header_is_absent() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        let response = app.oneshot(Request::builder().uri("/api/me").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(response.status(), SC::OK);
        let body = body_json(response).await;
        assert!(body["email"].is_null(), "no header reaching this process must never be papered over with a fabricated identity");
    }

    #[tokio::test]
    /// Real gap this endpoint closes -- see `version`'s own doc comment. The
    /// "set" case (a real deploy always sets DEVSYSTEM_GIT_SHA) is exercised
    /// by the actual deploy script's own post-deploy check against a real
    /// running container, not here -- mutating a process-global env var in a
    /// multi-threaded test binary would race unpredictably with any other
    /// test reading it. This proves the one thing safe and worth proving
    /// hermetically: an unset value is reported honestly, never fabricated.
    async fn version_reports_unknown_honestly_when_git_sha_is_unset() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        let response = app.oneshot(Request::builder().uri("/api/version").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(response.status(), SC::OK);
        let body = body_json(response).await;
        assert_eq!(body["git_sha"], "unknown");
    }

    #[tokio::test]
    async fn list_runs_surfaces_risk_count_and_needs_attention() {
        let (state, _dir) = test_state();
        let app = api_router(state);

        app.clone()
            .oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "list-health-run"})))
            .await
            .unwrap();

        // A fresh run: no risks, and not yet close to any bound.
        let response = app.clone().oneshot(Request::builder().uri("/api/runs").body(Body::empty()).unwrap()).await.unwrap();
        let body = body_json(response).await;
        assert_eq!(body[0]["risk_count"], 0);
        assert_eq!(body[0]["needs_attention"], false);
        assert_eq!(body[0]["pending_reviews"], 0);

        // Push it to 2 consecutive failures against the default max of 3 -- one away
        // from the abort bound, so the list should flag it now. Distinct feedback per
        // attempt: two byte-identical submissions in a row are now a real, deliberate
        // 409 (the idempotency guard, found necessary live 2026-08-05), not a way to
        // simulate two separate failures.
        for i in 0..2 {
            app.clone()
                .oneshot(json_request(
                    "POST",
                    "/api/runs/list-health-run/iterate",
                    serde_json::json!({
                        "stage": "devsystem.implement",
                        "feedback": format!("attempt {i}: wired the real session handshake and key material"),
                        "succeeded": false
                    }),
                ))
                .await
                .unwrap();
        }

        let response = app.oneshot(Request::builder().uri("/api/runs").body(Body::empty()).unwrap()).await.unwrap();
        let body = body_json(response).await;
        assert!(body[0]["risk_count"].as_u64().unwrap() > 0, "the security-keyword feedback should register as a real risk");
        assert_eq!(body[0]["needs_attention"], true, "2/3 consecutive failures should already flag needs_attention");
    }

    #[tokio::test]
    /// Real process-level check (#382 goal doc §4.3/§9, gap #9): a run with real
    /// progress but no devsystem.review role declared surfaces as a real risk in
    /// both GET /api/runs (risk_count) and GET /api/runs/{id} (risks), not just in
    /// the pipeline crate's own unit tests.
    async fn a_run_with_real_progress_and_no_review_role_is_flagged_as_a_process_risk() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "process-risk-run"}))).await.unwrap();

        for i in 0..3 {
            // Only the first submission needs to declare the custom stage -- once
            // `apply_proposal` adds it to the live spec, every later iterate call under
            // the same stage name is already role-declared.
            let mut body = serde_json::json!({"stage": "devsystem.android_native_bridge", "feedback": format!("real work {i}"), "succeeded": true});
            if i == 0 {
                body["proposals"] = serde_json::json!([{
                    "proposed_by": "devsystem.improve",
                    "stage_id": "devsystem.android_native_bridge",
                    "tag": "android_native_bridge",
                    "rationale": "real android-specific work needs its own stage",
                    "use_existing_service": null,
                    "units": 1,
                    "price_ceiling": null
                }]);
            }
            let response = app.clone().oneshot(json_request("POST", "/api/runs/process-risk-run/iterate", body)).await.unwrap();
            assert_eq!(response.status(), SC::OK);
        }

        let response = app.clone().oneshot(Request::builder().uri("/api/runs/process-risk-run").body(Body::empty()).unwrap()).await.unwrap();
        let body = body_json(response).await;
        let risks = body["risks"].as_array().unwrap();
        assert!(
            risks.iter().any(|r| r["label"] == "no review role declared despite real progress"),
            "3 successful iterations with no review role declared must be flagged: {risks:?}"
        );

        let response = app.oneshot(Request::builder().uri("/api/runs").body(Body::empty()).unwrap()).await.unwrap();
        let body = body_json(response).await;
        assert!(body[0]["risk_count"].as_u64().unwrap() > 0, "the same process risk must also count toward the list view's risk_count");
    }

    #[tokio::test]
    async fn list_runs_flags_needs_attention_for_a_real_pending_proposal_even_when_health_is_fine() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "list-pending-run"}))).await.unwrap();

        // A fresh, healthy run -- must NOT flag attention on health grounds.
        let response = app.clone().oneshot(Request::builder().uri("/api/runs").body(Body::empty()).unwrap()).await.unwrap();
        let body = body_json(response).await;
        assert_eq!(body[0]["needs_attention"], false);
        assert_eq!(body[0]["pending_reviews"], 0);

        app.clone()
            .oneshot(json_request(
                "POST",
                "/api/runs/list-pending-run/stages/propose",
                serde_json::json!({"stage_id": "devsystem.android_emulator_test", "tag": "android_emulator_test", "rationale": "need real emulator coverage"}),
            ))
            .await
            .unwrap();

        // Same fresh, healthy run, but now with one real pending proposal --
        // must surface in the run LIST, not just be discoverable after
        // opening that run's own Pipeline panel.
        let response = app.oneshot(Request::builder().uri("/api/runs").body(Body::empty()).unwrap()).await.unwrap();
        let body = body_json(response).await;
        assert_eq!(body[0]["pending_reviews"], 1);
        assert_eq!(body[0]["needs_attention"], true, "a real pending proposal must flag needs_attention on its own, independent of health thresholds");
    }

    #[tokio::test]
    /// Real gap found live 2026-08-06 (stress-test run 50): list_runs's own
    /// pending_reviews only ever summed three of the five real proposal queues --
    /// the exact same undercounting shape already found and fixed once for the
    /// Pipeline panel's own chip badge (2026-08-04), but this call site never got
    /// the same fix. Live-confirmed before touching anything: a real pending
    /// panel-removal proposal showed pending_reviews: 0 in the actual Runs list.
    async fn list_runs_pending_reviews_counts_panel_removal_and_edit_proposals_too() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "undercount-run"}))).await.unwrap();
        let create = app
            .clone()
            .oneshot(json_request("POST", "/api/runs/undercount-run/panels", serde_json::json!({"title": "Real Panel", "html": "<p>x</p>"})))
            .await
            .unwrap();
        let panel_id = body_json(create).await["id"].as_str().unwrap().to_string();

        let response = app
            .clone()
            .oneshot(Request::builder().method("POST").uri(format!("/api/runs/undercount-run/panels/{panel_id}/propose-remove")).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK);

        let response = app.oneshot(Request::builder().uri("/api/runs").body(Body::empty()).unwrap()).await.unwrap();
        let body = body_json(response).await;
        assert_eq!(body[0]["pending_reviews"], 1, "a real pending panel-removal proposal must count toward pending_reviews, not be silently invisible");
        assert_eq!(body[0]["needs_attention"], true);
    }

    #[tokio::test]
    /// Real gap found live 2026-08-06 (stress-test run 52): list_runs sorted purely
    /// alphabetically, no priority at all. Live-confirmed before touching anything:
    /// the real flagship webconference-android run (genuinely paused) sat at
    /// position 105 of 110 in the actual Runs list, behind well over a hundred
    /// alphabetically-earlier scratch runs with nothing outstanding. Proves the real
    /// fix with run_ids deliberately chosen to sort the WRONG way alphabetically if
    /// the old behavior regressed -- "z-paused-run" must still land before
    /// "a-healthy-run" despite the letters saying otherwise.
    async fn list_runs_sorts_the_most_urgent_run_first_not_purely_alphabetically() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "a-healthy-run"}))).await.unwrap();
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "z-paused-run"}))).await.unwrap();
        app.clone()
            .oneshot(Request::builder().method("POST").uri("/api/runs/z-paused-run/pause").body(Body::empty()).unwrap())
            .await
            .unwrap();

        let response = app.oneshot(Request::builder().uri("/api/runs").body(Body::empty()).unwrap()).await.unwrap();
        let body = body_json(response).await;
        assert_eq!(body[0]["run_id"], "z-paused-run", "the paused run must sort first despite losing alphabetically");
        assert_eq!(body[1]["run_id"], "a-healthy-run");
    }

    #[tokio::test]
    async fn create_run_success_then_duplicate_conflicts() {
        let (state, _dir) = test_state();
        let app = api_router(state);

        let response = app
            .clone()
            .oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "demo-run"})))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::CREATED);

        let response = app
            .oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "demo-run"})))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::CONFLICT);
    }

    #[tokio::test]
    async fn create_run_records_the_real_gate_email_as_owner_when_present() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        let req = Request::builder()
            .method("POST")
            .uri("/api/runs")
            .header("content-type", "application/json")
            .header("x-gate-email", "scimbe@gmail.com")
            .body(Body::from(serde_json::json!({"run_id": "owned-run"}).to_string()))
            .expect("build request");
        let response = app.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), SC::CREATED);

        let response = app.oneshot(Request::builder().uri("/api/runs/owned-run").body(Body::empty()).unwrap()).await.unwrap();
        let body = body_json(response).await;
        assert_eq!(body["state"]["owner_email"], "scimbe@gmail.com");
    }

    #[tokio::test]
    async fn create_run_leaves_owner_honestly_absent_without_the_gate_header() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "unowned-run"}))).await.unwrap();

        let response = app.oneshot(Request::builder().uri("/api/runs/unowned-run").body(Body::empty()).unwrap()).await.unwrap();
        let body = body_json(response).await;
        assert!(body["state"]["owner_email"].is_null(), "no gate header present -- must not guess an owner");
    }

    fn gate_request(method: &str, uri: &str, email: &str, body: Option<serde_json::Value>) -> Request<Body> {
        let mut builder = Request::builder().method(method).uri(uri).header("x-gate-email", email);
        let body = match body {
            Some(v) => {
                builder = builder.header("content-type", "application/json");
                Body::from(v.to_string())
            }
            None => Body::empty(),
        };
        builder.body(body).expect("build request")
    }

    #[tokio::test]
    async fn a_different_signed_in_account_cannot_view_someone_elses_run() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone()
            .oneshot(gate_request("POST", "/api/runs", "owner@example.com", Some(serde_json::json!({"run_id": "owned-view-run"}))))
            .await
            .unwrap();

        // A different real, signed-in identity is turned away with a real 403.
        let response = app.clone().oneshot(gate_request("GET", "/api/runs/owned-view-run", "someone-else@example.com", None)).await.unwrap();
        assert_eq!(response.status(), SC::FORBIDDEN);

        // The real owner still gets through.
        let response = app.oneshot(gate_request("GET", "/api/runs/owned-view-run", "owner@example.com", None)).await.unwrap();
        assert_eq!(response.status(), SC::OK);
    }

    #[tokio::test]
    async fn a_headless_caller_with_no_gate_header_is_never_restricted_by_ownership() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone()
            .oneshot(gate_request("POST", "/api/runs", "owner@example.com", Some(serde_json::json!({"run_id": "headless-access-run"}))))
            .await
            .unwrap();

        // No x-gate-email at all -- devsystem_iterate/devsystem_checkin's own real
        // calling shape, and CADS-Tunnel#388's headless offer-submission callers --
        // must reach an owned run exactly as before this restriction existed.
        let response = app
            .oneshot(Request::builder().uri("/api/runs/headless-access-run").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK);
    }

    #[tokio::test]
    async fn any_signed_in_account_can_still_reach_an_unowned_run() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        // Created with no gate header -- a legacy/pre-existing run, owner_email stays None.
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "legacy-run"}))).await.unwrap();

        let response = app.oneshot(gate_request("GET", "/api/runs/legacy-run", "anyone@example.com", None)).await.unwrap();
        assert_eq!(response.status(), SC::OK, "an unowned run must stay open to any signed-in user, not lock everyone out");
    }

    #[tokio::test]
    async fn a_different_account_cannot_mutate_someone_elses_run() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone()
            .oneshot(gate_request("POST", "/api/runs", "owner@example.com", Some(serde_json::json!({"run_id": "owned-mutate-run"}))))
            .await
            .unwrap();

        let response = app
            .oneshot(gate_request(
                "POST",
                "/api/runs/owned-mutate-run/roles/plan/fill-mode",
                "someone-else@example.com",
                Some(serde_json::json!({"mode": "dedicated", "label": "X"})),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::FORBIDDEN);
    }

    #[tokio::test]
    async fn rag_document_add_then_remove_round_trips_and_becomes_searchable() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "rag-doc-run"}))).await.unwrap();

        // No repo ever synced -- upload must work anyway (doesn't require repo_url).
        let response = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/runs/rag-doc-run/rag/documents",
                serde_json::json!({"path": "notes.txt", "text": "a real note about Agent-Fabric channel joins"}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK);
        let created = body_json(response).await;
        let doc_id = created["id"].as_str().unwrap().to_string();
        assert_eq!(created["path"], "notes.txt");

        let response = app
            .clone()
            .oneshot(Request::builder().uri("/api/runs/rag-doc-run/rag/search?q=Agent-Fabric").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body = body_json(response).await;
        assert_eq!(body["configured"], true);
        assert_eq!(body["results"][0]["path"], "notes.txt");
        assert_eq!(body["manual_documents"].as_array().unwrap().len(), 1);
        assert_eq!(body["manual_documents"][0]["path"], "notes.txt");

        let response = app
            .clone()
            .oneshot(json_request("POST", &format!("/api/runs/rag-doc-run/rag/documents/{doc_id}/remove"), serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK);

        let response = app
            .oneshot(Request::builder().uri("/api/runs/rag-doc-run/rag/search?q=Agent-Fabric").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body = body_json(response).await;
        assert!(body["results"].as_array().unwrap().is_empty(), "removed document must no longer be searchable");
        assert!(body["manual_documents"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn rag_document_rejects_empty_fields_and_an_oversized_upload() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "rag-doc-bad-run"}))).await.unwrap();

        let response = app
            .clone()
            .oneshot(json_request("POST", "/api/runs/rag-doc-bad-run/rag/documents", serde_json::json!({"path": "  ", "text": "x"})))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::BAD_REQUEST);

        let huge = "x".repeat(MAX_RAG_DOCUMENT_BYTES + 1);
        let response = app
            .oneshot(json_request("POST", "/api/runs/rag-doc-bad-run/rag/documents", serde_json::json!({"path": "a.txt", "text": huge})))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::BAD_REQUEST);
    }

    #[tokio::test]
    async fn removing_an_unknown_rag_document_id_is_a_real_404() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "rag-doc-404-run"}))).await.unwrap();

        let response = app
            .oneshot(json_request("POST", "/api/runs/rag-doc-404-run/rag/documents/does-not-exist/remove", serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_different_account_cannot_upload_a_rag_document_to_someone_elses_run() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone()
            .oneshot(gate_request("POST", "/api/runs", "owner@example.com", Some(serde_json::json!({"run_id": "rag-doc-owned-run"}))))
            .await
            .unwrap();

        let response = app
            .oneshot(gate_request(
                "POST",
                "/api/runs/rag-doc-owned-run/rag/documents",
                "someone-else@example.com",
                Some(serde_json::json!({"path": "a.txt", "text": "hello"})),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::FORBIDDEN);
    }

    #[tokio::test]
    async fn custom_panel_add_then_remove_round_trips_through_real_state() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "panels-run"}))).await.unwrap();

        let response = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/runs/panels-run/panels",
                serde_json::json!({"title": "My Custom Panel", "html": "<h1>hi</h1>"}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK);
        let created = body_json(response).await;
        let panel_id = created["id"].as_str().unwrap().to_string();
        assert_eq!(created["title"], "My Custom Panel");
        assert_eq!(created["html"], "<h1>hi</h1>");
        assert!(created["source"].is_null(), "a hand-written panel has no marketplace source");

        let response = app.clone().oneshot(Request::builder().uri("/api/runs/panels-run").body(Body::empty()).unwrap()).await.unwrap();
        let body = body_json(response).await;
        assert_eq!(body["state"]["custom_panels"].as_array().unwrap().len(), 1);

        let response = app
            .clone()
            .oneshot(json_request("POST", &format!("/api/runs/panels-run/panels/{panel_id}/remove"), serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK);

        let response = app.oneshot(Request::builder().uri("/api/runs/panels-run").body(Body::empty()).unwrap()).await.unwrap();
        let body = body_json(response).await;
        assert!(body["state"]["custom_panels"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn custom_panel_rejects_an_empty_title_and_an_oversized_html_payload() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "panels-bad-run"}))).await.unwrap();

        let response = app
            .clone()
            .oneshot(json_request("POST", "/api/runs/panels-bad-run/panels", serde_json::json!({"title": "  ", "html": "<p>x</p>"})))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::BAD_REQUEST);

        let huge = "x".repeat(MAX_CUSTOM_PANEL_HTML_BYTES + 1);
        let response = app
            .clone()
            .oneshot(json_request("POST", "/api/runs/panels-bad-run/panels", serde_json::json!({"title": "T", "html": huge})))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::BAD_REQUEST);
    }

    #[tokio::test]
    /// DAU-lens gap found live 2026-08-06 (#382 goal doc §8): every other real free-text
    /// field in this codebase already rejects whitespace-only content -- a custom
    /// panel's own `html` was the one exception, at all four real entry points
    /// (add, update, propose, propose-edit). Live-confirmed before this fix:
    /// `{"title":"x","html":""}` got a real 200, creating a genuinely blank panel.
    async fn custom_panel_html_rejects_empty_or_whitespace_only_content_at_all_four_real_entry_points() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "panels-blank-html-run"}))).await.unwrap();

        for bad_html in ["", "   ", "\n\t"] {
            let response = app
                .clone()
                .oneshot(json_request("POST", "/api/runs/panels-blank-html-run/panels", serde_json::json!({"title": "T", "html": bad_html})))
                .await
                .unwrap();
            assert_eq!(response.status(), SC::BAD_REQUEST, "add_custom_panel must reject {bad_html:?}");

            let response = app
                .clone()
                .oneshot(json_request("POST", "/api/runs/panels-blank-html-run/panels/propose", serde_json::json!({"title": "T", "html": bad_html})))
                .await
                .unwrap();
            assert_eq!(response.status(), SC::BAD_REQUEST, "propose_custom_panel must reject {bad_html:?}");
        }

        let real = app
            .clone()
            .oneshot(json_request("POST", "/api/runs/panels-blank-html-run/panels", serde_json::json!({"title": "T", "html": "<p>real content</p>"})))
            .await
            .unwrap();
        let panel_id = body_json(real).await["id"].as_str().unwrap().to_string();

        for bad_html in ["", "   "] {
            let response = app
                .clone()
                .oneshot(json_request(
                    "POST",
                    &format!("/api/runs/panels-blank-html-run/panels/{panel_id}/update"),
                    serde_json::json!({"title": "T", "html": bad_html}),
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), SC::BAD_REQUEST, "update_custom_panel must reject {bad_html:?}");

            let response = app
                .clone()
                .oneshot(json_request(
                    "POST",
                    &format!("/api/runs/panels-blank-html-run/panels/{panel_id}/propose-edit"),
                    serde_json::json!({"title": "T", "html": bad_html}),
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), SC::BAD_REQUEST, "propose_panel_edit must reject {bad_html:?}");
        }
    }

    #[tokio::test]
    async fn removing_an_unknown_custom_panel_id_is_a_real_404() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "panels-404-run"}))).await.unwrap();

        let response = app
            .oneshot(json_request("POST", "/api/runs/panels-404-run/panels/does-not-exist/remove", serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_different_account_cannot_add_a_panel_to_someone_elses_run() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone()
            .oneshot(gate_request("POST", "/api/runs", "owner@example.com", Some(serde_json::json!({"run_id": "panels-owned-run"}))))
            .await
            .unwrap();

        let response = app
            .oneshot(gate_request(
                "POST",
                "/api/runs/panels-owned-run/panels",
                "someone-else@example.com",
                Some(serde_json::json!({"title": "T", "html": "<p>x</p>"})),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::FORBIDDEN);
    }

    #[tokio::test]
    async fn create_run_rejects_invalid_characters() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        let response = app
            .oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "not a valid id!"})))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::BAD_REQUEST);
    }

    #[tokio::test]
    async fn memory_run_reads_back_every_real_iteration_appended() {
        let (state, _dir) = test_state();
        let app = api_router(state);

        app.clone()
            .oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "memory-run"})))
            .await
            .unwrap();

        for feedback in ["first real finding", "second real finding"] {
            app.clone()
                .oneshot(json_request(
                    "POST",
                    "/api/runs/memory-run/iterate",
                    serde_json::json!({"stage": "devsystem.implement", "feedback": feedback, "succeeded": true}),
                ))
                .await
                .unwrap();
        }

        let response = app
            .oneshot(Request::builder().uri("/api/runs/memory-run/memory").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK);
        let body = body_json(response).await;
        let entries = body.as_array().expect("memory log is an array");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["envelope"]["key_findings"][0], "first real finding");
        assert_eq!(entries[1]["envelope"]["key_findings"][0], "second real finding");
    }

    #[tokio::test]
    async fn memory_run_404_for_nonexistent_run() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        let response = app
            .oneshot(Request::builder().uri("/api/runs/does-not-exist/memory").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), SC::NOT_FOUND);
    }

    #[tokio::test]
    async fn govern_memory_promotes_the_targeted_entry_and_persists_it() {
        let (state, _dir) = test_state();
        let app = api_router(state);

        app.clone()
            .oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "govern-run"})))
            .await
            .unwrap();
        for feedback in ["first", "second"] {
            app.clone()
                .oneshot(json_request(
                    "POST",
                    "/api/runs/govern-run/iterate",
                    serde_json::json!({"stage": "devsystem.implement", "feedback": feedback, "succeeded": true}),
                ))
                .await
                .unwrap();
        }

        let response = app
            .clone()
            .oneshot(Request::builder().method("POST").uri("/api/runs/govern-run/memory/0/govern").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK);
        let body = body_json(response).await;
        assert_eq!(body[0]["trust"], "governed");
        assert_eq!(body[1]["trust"], "unreviewed", "only the targeted entry should be promoted");

        // Re-fetching independently proves it actually persisted to disk, not just
        // the response of the call that made the change.
        let response = app
            .oneshot(Request::builder().uri("/api/runs/govern-run/memory").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let reread = body_json(response).await;
        assert_eq!(reread[0]["trust"], "governed");
    }

    #[tokio::test]
    async fn govern_memory_404_for_an_out_of_range_index() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone()
            .oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "govern-oob-run"})))
            .await
            .unwrap();

        let response = app
            .oneshot(Request::builder().method("POST").uri("/api/runs/govern-oob-run/memory/9/govern").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), SC::NOT_FOUND);
    }

    #[tokio::test]
    async fn get_run_surfaces_real_preflight_risk_findings() {
        let (state, _dir) = test_state();
        let app = api_router(state);

        app.clone()
            .oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "risk-run"})))
            .await
            .unwrap();
        app.clone()
            .oneshot(json_request(
                "POST",
                "/api/runs/risk-run/iterate",
                serde_json::json!({
                    "stage": "devsystem.implement",
                    "feedback": "wired the real session handshake and key material",
                    "succeeded": true
                }),
            ))
            .await
            .unwrap();

        let response = app
            .oneshot(Request::builder().uri("/api/runs/risk-run").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body = body_json(response).await;
        let risks = body["risks"].as_array().expect("risks is an array");
        assert!(risks.iter().any(|r| r["label"] == "touches auth/security"), "expected a real preflight finding, got {risks:?}");
    }

    #[tokio::test]
    async fn repo_url_can_be_set_then_cleared_and_persists() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone()
            .oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "repo-run"})))
            .await
            .unwrap();

        let response = app
            .clone()
            .oneshot(json_request("POST", "/api/runs/repo-run/repo", serde_json::json!({"repo_url": "https://github.com/scimbe/CADS-webconference-android"})))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK);

        let response = app.clone().oneshot(Request::builder().uri("/api/runs/repo-run").body(Body::empty()).unwrap()).await.unwrap();
        let body = body_json(response).await;
        assert_eq!(body["state"]["repo_url"], "https://github.com/scimbe/CADS-webconference-android");

        let response = app
            .clone()
            .oneshot(json_request("POST", "/api/runs/repo-run/repo", serde_json::json!({"repo_url": ""})))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK);
        let response = app.oneshot(Request::builder().uri("/api/runs/repo-run").body(Body::empty()).unwrap()).await.unwrap();
        let body = body_json(response).await;
        assert!(body["state"]["repo_url"].is_null(), "an empty repo_url must clear it");
    }

    #[tokio::test]
    async fn set_repo_url_rejects_a_non_https_value() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone()
            .oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "repo-bad-run"})))
            .await
            .unwrap();

        let response = app
            .oneshot(json_request("POST", "/api/runs/repo-bad-run/repo", serde_json::json!({"repo_url": "javascript:alert(1)"})))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::BAD_REQUEST);
    }

    #[tokio::test]
    /// DAU-lens gap found live 2026-08-06 (#382 goal doc §8), the same class just
    /// closed for backlog/milestone text: repo_url had no length cap at all.
    /// Live-confirmed before fixing: a real 500,000-character repo_url got a real 200.
    async fn set_repo_url_rejects_an_absurdly_long_value() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "repo-long-run"}))).await.unwrap();

        let huge = format!("https://{}", "x".repeat(MAX_SHORT_TEXT_LEN + 1));
        let response = app
            .clone()
            .oneshot(json_request("POST", "/api/runs/repo-long-run/repo", serde_json::json!({"repo_url": huge})))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::BAD_REQUEST, "an absurdly long repo_url must be rejected");

        let response = app
            .oneshot(json_request(
                "POST",
                "/api/runs/repo-long-run/repo",
                serde_json::json!({"repo_url": "https://github.com/scimbe/CADS-webconference-android"}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK, "a genuine, real-sized repo_url must not be rejected");
    }

    #[tokio::test]
    async fn set_operator_pubkey_accepts_a_real_valid_hex_key_and_persists_it() {
        let (state, dir) = test_state();
        let app = api_router(state);
        app.clone()
            .oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "op-key-run"})))
            .await
            .unwrap();

        let key = "7e59d28d596978c27eb5faa4afa4759b7e9fc1948c7172d7ca66f6abc97d92cb";
        let response = app
            .oneshot(json_request("POST", "/api/runs/op-key-run/operator-pubkey", serde_json::json!({"operator_pubkey_hex": key})))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK);

        let spec_path = dir.path().join("op-key-run").join("spec.json");
        let spec: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(spec_path).unwrap()).unwrap();
        assert_eq!(spec["operator_pubkey_hex"], key);
    }

    #[tokio::test]
    async fn set_operator_pubkey_rejects_malformed_hex() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone()
            .oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "op-key-bad-run"})))
            .await
            .unwrap();

        for bad in ["not-hex-at-all", "7e59d28d", "zzzz59d28d596978c27eb5faa4afa4759b7e9fc1948c7172d7ca66f6abc97d92c"] {
            let response = app
                .clone()
                .oneshot(json_request(
                    "POST",
                    "/api/runs/op-key-bad-run/operator-pubkey",
                    serde_json::json!({"operator_pubkey_hex": bad}),
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), SC::BAD_REQUEST, "expected {bad:?} to be rejected");
        }
    }

    #[tokio::test]
    async fn set_operator_pubkey_can_be_cleared_with_an_empty_string() {
        let (state, dir) = test_state();
        let app = api_router(state);
        app.clone()
            .oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "op-key-clear-run"})))
            .await
            .unwrap();
        let key = "7e59d28d596978c27eb5faa4afa4759b7e9fc1948c7172d7ca66f6abc97d92cb";
        app.clone()
            .oneshot(json_request("POST", "/api/runs/op-key-clear-run/operator-pubkey", serde_json::json!({"operator_pubkey_hex": key})))
            .await
            .unwrap();

        let response = app
            .oneshot(json_request("POST", "/api/runs/op-key-clear-run/operator-pubkey", serde_json::json!({"operator_pubkey_hex": ""})))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK);

        let spec_path = dir.path().join("op-key-clear-run").join("spec.json");
        let spec: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(spec_path).unwrap()).unwrap();
        assert_eq!(spec["operator_pubkey_hex"], serde_json::Value::Null);
    }

    #[tokio::test]
    async fn role_fill_mode_defaults_to_auction_and_can_switch_to_dedicated_and_back() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "fillmode-run"}))).await.unwrap();

        // Default: no entry in the map at all -- Auction is implicit, not a fabricated explicit value.
        let response = app.clone().oneshot(Request::builder().uri("/api/runs/fillmode-run").body(Body::empty()).unwrap()).await.unwrap();
        let body = body_json(response).await;
        assert!(body["state"]["role_fill_modes"].as_object().unwrap().is_empty());

        // Switch the real "plan" role (present in the default spec) to Dedicated.
        let response = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/runs/fillmode-run/roles/plan/fill-mode",
                serde_json::json!({"mode": "dedicated", "label": "Compass-1"}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK);
        let body = body_json(response).await;
        assert_eq!(body["fill_mode"]["mode"], "dedicated");
        assert_eq!(body["fill_mode"]["label"], "Compass-1");

        let response = app.clone().oneshot(Request::builder().uri("/api/runs/fillmode-run").body(Body::empty()).unwrap()).await.unwrap();
        let body = body_json(response).await;
        assert_eq!(body["state"]["role_fill_modes"]["plan"]["mode"], "dedicated");

        // Switch back to Auction -- the entry is removed, not stored as an explicit "auction" value.
        let response = app
            .clone()
            .oneshot(json_request("POST", "/api/runs/fillmode-run/roles/plan/fill-mode", serde_json::json!({"mode": "auction"})))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK);
        let response = app.oneshot(Request::builder().uri("/api/runs/fillmode-run").body(Body::empty()).unwrap()).await.unwrap();
        let body = body_json(response).await;
        assert!(body["state"]["role_fill_modes"].as_object().unwrap().is_empty());
    }

    #[tokio::test]
    async fn role_fill_mode_can_directly_accept_a_real_bid_with_its_price_snapshot() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "accept-bid-run"}))).await.unwrap();

        let response = app
            .oneshot(json_request(
                "POST",
                "/api/runs/accept-bid-run/roles/plan/fill-mode",
                serde_json::json!({"mode": "dedicated", "label": "Compass-1", "accepted_bid": {"holder_label": "abc123", "price": 8}}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK);
        let body = body_json(response).await;
        assert_eq!(body["fill_mode"]["accepted_bid"]["holder_label"], "abc123");
        assert_eq!(body["fill_mode"]["accepted_bid"]["price"], 8);
    }

    #[tokio::test]
    /// Real gap found live 2026-08-07 (#382 goal doc §7.2/§8): `price_ceiling` was
    /// stored and shown, never actually compared against a real bid's price
    /// anywhere -- this is the fix for the one real, local, direct-accept path.
    async fn accepting_a_bid_over_the_roles_own_real_price_ceiling_is_rejected() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "ceiling-enforce-run"}))).await.unwrap();
        let propose_response = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/runs/ceiling-enforce-run/stages/propose",
                serde_json::json!({"stage_id": "devsystem.bounded_role", "tag": "bounded", "rationale": "a real reason", "units": 1, "price_ceiling": 50}),
            ))
            .await
            .unwrap();
        let proposal = body_json(propose_response).await;
        let proposal_id = proposal["id"].as_str().unwrap();
        let approve_response = app
            .clone()
            .oneshot(json_request("POST", &format!("/api/runs/ceiling-enforce-run/stages/proposals/{proposal_id}/approve"), serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(approve_response.status(), SC::OK);

        // Over the real ceiling -- rejected, not silently accepted.
        let over_response = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/runs/ceiling-enforce-run/roles/bounded/fill-mode",
                serde_json::json!({"mode": "dedicated", "label": "Compass-1", "accepted_bid": {"holder_label": "abc123", "price": 51}}),
            ))
            .await
            .unwrap();
        assert_eq!(over_response.status(), SC::BAD_REQUEST, "a bid priced over the role's own real price_ceiling must be rejected");

        // At the real ceiling, exactly -- allowed.
        let at_response = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/runs/ceiling-enforce-run/roles/bounded/fill-mode",
                serde_json::json!({"mode": "dedicated", "label": "Compass-1", "accepted_bid": {"holder_label": "abc123", "price": 50}}),
            ))
            .await
            .unwrap();
        assert_eq!(at_response.status(), SC::OK, "a bid priced exactly at the real ceiling must be allowed");

        // A role with no real ceiling (the default spec's own "plan" role, never proposed
        // via a real StageProposal) -- any price is still allowed, matching the honest
        // "0/unset both mean nothing to enforce" semantics.
        let unbounded_response = app
            .oneshot(json_request(
                "POST",
                "/api/runs/ceiling-enforce-run/roles/plan/fill-mode",
                serde_json::json!({"mode": "dedicated", "label": "Compass-1", "accepted_bid": {"holder_label": "abc123", "price": 999_999}}),
            ))
            .await
            .unwrap();
        assert_eq!(unbounded_response.status(), SC::OK, "a role with no real price_ceiling must not be blocked -- there's nothing real to enforce yet");
    }

    #[tokio::test]
    /// Real gap found live 2026-08-06 (stress-test run 53): the outer `label` was
    /// already validated non-empty for a dedicated role, but `accepted_bid`'s own
    /// `holder_label` -- a real identity record of who actually won the bid, not
    /// decorative -- had zero validation. Live-confirmed before this fix: both a
    /// byte-empty and a whitespace-only holder_label got a real 200.
    async fn accepting_a_bid_with_an_empty_or_whitespace_only_holder_label_is_rejected() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "empty-holder-run"}))).await.unwrap();

        for holder_label in ["", "   "] {
            let response = app
                .clone()
                .oneshot(json_request(
                    "POST",
                    "/api/runs/empty-holder-run/roles/plan/fill-mode",
                    serde_json::json!({"mode": "dedicated", "label": "Compass-1", "accepted_bid": {"holder_label": holder_label, "price": 8}}),
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), SC::BAD_REQUEST, "holder_label {holder_label:?} must be rejected, not silently accepted");
        }
    }

    #[tokio::test]
    /// Real gap found live by the incompetent-agent stress test (#382 goal doc
    /// §8, 2026-08-06): extends the bidi-control-character (Trojan Source)
    /// class -- already closed for requirement statement/criteria, milestones,
    /// backlog, custom-panel title, and stage-proposal rationale -- to a field
    /// with the exact same shape that was never checked: a dedicated role's
    /// `label` (and its `accepted_bid.holder_label`), both short, human-typed,
    /// displayed and trusted in the Roles panel. Live-confirmed before fixing:
    /// "Trusted Agent" + U+202E + reversed text sailed through untouched.
    async fn role_fill_mode_rejects_bidi_control_characters_in_label_and_holder_label() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "bidi-fillmode-run"}))).await.unwrap();

        let bidi_label = "Trusted Agent\u{202e} tnega suoicilam a yllaer si sihT";
        let response = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/runs/bidi-fillmode-run/roles/plan/fill-mode",
                serde_json::json!({"mode": "dedicated", "label": bidi_label}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::BAD_REQUEST, "a bidi-laced label must be rejected");

        let response = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/runs/bidi-fillmode-run/roles/plan/fill-mode",
                serde_json::json!({"mode": "dedicated", "label": "Compass-1", "accepted_bid": {"holder_label": bidi_label, "price": 8}}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::BAD_REQUEST, "a bidi-laced holder_label must be rejected");

        let response = app
            .oneshot(json_request(
                "POST",
                "/api/runs/bidi-fillmode-run/roles/plan/fill-mode",
                serde_json::json!({"mode": "dedicated", "label": "a real, clean label"}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK, "a clean label must not be rejected");
    }

    #[tokio::test]
    async fn role_fill_mode_rejects_an_unknown_role_tag() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "fillmode-bad-run"}))).await.unwrap();

        let response = app
            .oneshot(json_request(
                "POST",
                "/api/runs/fillmode-bad-run/roles/not-a-real-role/fill-mode",
                serde_json::json!({"mode": "dedicated", "label": "X"}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::BAD_REQUEST);
    }

    #[tokio::test]
    async fn role_fill_mode_rejects_an_empty_dedicated_label() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "fillmode-empty-run"}))).await.unwrap();

        let response = app
            .oneshot(json_request(
                "POST",
                "/api/runs/fillmode-empty-run/roles/plan/fill-mode",
                serde_json::json!({"mode": "dedicated", "label": "   "}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::BAD_REQUEST);
    }

    #[tokio::test]
    async fn achieving_a_milestone_via_the_api_auto_pauses_the_run() {
        let (state, _dir) = test_state();
        let app = api_router(state);

        app.clone()
            .oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "milestone-run"})))
            .await
            .unwrap();

        let response = app
            .clone()
            .oneshot(json_request("POST", "/api/runs/milestone-run/milestones", serde_json::json!({"description": "APK builds and installs"})))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK);
        let body = body_json(response).await;
        assert_eq!(body["milestones"][0]["achieved"], false);

        let response = app
            .clone()
            .oneshot(Request::builder().method("POST").uri("/api/runs/milestone-run/milestones/0/toggle").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK);
        let body = body_json(response).await;
        assert_eq!(body["milestones"][0]["achieved"], true);
        assert_eq!(body["paused"], true, "achieving a milestone must auto-pause the run");

        // Independently confirms it actually persisted and that iterate_run now
        // really refuses, not just that the toggle response claimed paused.
        let response = app
            .oneshot(json_request(
                "POST",
                "/api/runs/milestone-run/iterate",
                serde_json::json!({"stage": "devsystem.plan", "feedback": "should be refused", "succeeded": true}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::CONFLICT);
    }

    #[tokio::test]
    async fn toggling_an_out_of_range_milestone_404s() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone()
            .oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "milestone-oob-run"})))
            .await
            .unwrap();

        let response = app
            .oneshot(Request::builder().method("POST").uri("/api/runs/milestone-oob-run/milestones/9/toggle").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), SC::NOT_FOUND);
    }

    #[tokio::test]
    async fn requirements_can_be_added_and_toggled_and_never_auto_pause() {
        let (state, _dir) = test_state();
        let app = api_router(state);

        app.clone()
            .oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "req-run"})))
            .await
            .unwrap();

        let response = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/runs/req-run/requirements",
                serde_json::json!({
                    "statement": "WHEN a user sends a text message over an established channel, THE SYSTEM SHALL persist it locally before confirming delivery to the UI",
                    "acceptance_criteria": ["message survives an app restart", "UI shows \"sent\" only after local persistence succeeds"],
                }),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK);
        let body = body_json(response).await;
        assert_eq!(body["requirements"][0]["verified"], false);
        assert_eq!(body["requirements"][0]["acceptance_criteria"].as_array().unwrap().len(), 2);

        let response = app
            .clone()
            .oneshot(Request::builder().method("POST").uri("/api/runs/req-run/requirements/0/toggle").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK);
        let body = body_json(response).await;
        assert_eq!(body["requirements"][0]["verified"], true);
        assert!(body.get("paused").is_none(), "unlike a milestone, a requirement toggle response has no paused field to fake-imply auto-pause");

        // Independently confirms it did NOT auto-pause the run (a real behavioral
        // check, not just absence of a field in the toggle response).
        let response = app
            .oneshot(json_request(
                "POST",
                "/api/runs/req-run/iterate",
                serde_json::json!({"stage": "devsystem.plan", "feedback": "should still be allowed", "succeeded": true}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK, "verifying a requirement must never block the next iteration");
    }

    #[tokio::test]
    async fn acceptance_criteria_can_be_toggled_independently_of_the_whole_requirement() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "criteria-run"}))).await.unwrap();
        app.clone()
            .oneshot(json_request(
                "POST",
                "/api/runs/criteria-run/requirements",
                serde_json::json!({"statement": "WHEN ..., THE SYSTEM SHALL ...", "acceptance_criteria": ["criterion A", "criterion B"]}),
            ))
            .await
            .unwrap();

        let mut request = Request::builder().method("POST").uri("/api/runs/criteria-run/requirements/0/criteria/1/toggle").body(Body::empty()).unwrap();
        request.headers_mut().insert("x-gate-email", "scimbe@gmail.com".parse().unwrap());
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), SC::OK);
        let body = body_json(response).await;
        assert!(body["requirements"][0]["verified_criteria"][0].is_null(), "must grow with a real, honest null, not just record index 1 alone");
        assert_eq!(body["requirements"][0]["verified_criteria"][1]["confirmed_by"], "scimbe@gmail.com", "real evaluator finding, issue #55: the real gate-verified account must be recorded, not a bare true");
        assert!(body["requirements"][0]["verified_criteria"][1]["confirmed_at"].as_u64().unwrap() > 1_700_000_000, "a real, current timestamp must be recorded");
        assert_eq!(body["requirements"][0]["verified"], false, "toggling one criterion must never silently flip the independent whole-requirement flag");

        // Independently confirms it actually persisted, not just the response.
        let response = app.oneshot(Request::builder().uri("/api/runs/criteria-run").body(Body::empty()).unwrap()).await.unwrap();
        let body = body_json(response).await;
        assert!(body["state"]["requirements"][0]["verified_criteria"][0].is_null());
        assert_eq!(body["state"]["requirements"][0]["verified_criteria"][1]["confirmed_by"], "scimbe@gmail.com");
    }

    #[tokio::test]
    async fn toggling_an_out_of_range_acceptance_criterion_404s() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "criteria-oob-run"}))).await.unwrap();
        app.clone()
            .oneshot(json_request(
                "POST",
                "/api/runs/criteria-oob-run/requirements",
                serde_json::json!({"statement": "WHEN ..., THE SYSTEM SHALL ...", "acceptance_criteria": ["only one criterion"]}),
            ))
            .await
            .unwrap();

        let response = app
            .clone()
            .oneshot(Request::builder().method("POST").uri("/api/runs/criteria-oob-run/requirements/0/criteria/5/toggle").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), SC::NOT_FOUND);

        let response = app
            .oneshot(Request::builder().method("POST").uri("/api/runs/criteria-oob-run/requirements/9/criteria/0/toggle").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), SC::NOT_FOUND, "an out-of-range requirement index must also 404, not panic");
    }

    #[tokio::test]
    /// Real evaluator finding, issue #55: a criterion toggle with no gate header at
    /// all (the local CLI/M2M path) must honestly record no actor, not fabricate one
    /// -- but the confirmation itself, and its real timestamp, still land.
    async fn toggling_a_criterion_with_no_gate_header_honestly_records_no_actor() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "criteria-no-session-run"}))).await.unwrap();
        app.clone()
            .oneshot(json_request(
                "POST",
                "/api/runs/criteria-no-session-run/requirements",
                serde_json::json!({"statement": "WHEN ..., THE SYSTEM SHALL ...", "acceptance_criteria": ["only one criterion"]}),
            ))
            .await
            .unwrap();

        let response = app
            .oneshot(Request::builder().method("POST").uri("/api/runs/criteria-no-session-run/requirements/0/criteria/0/toggle").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK);
        let body = body_json(response).await;
        assert!(body["requirements"][0]["verified_criteria"][0]["confirmed_by"].is_null(), "no gate header means an honest null actor, never a fabricated one");
        assert!(body["requirements"][0]["verified_criteria"][0]["confirmed_at"].as_u64().unwrap() > 1_700_000_000, "the real confirmation timestamp must still land even with no actor");
    }

    #[tokio::test]
    /// Un-toggling (confirmed -> unconfirmed) must clear the whole real record, not
    /// leave a stale confirmed_by/confirmed_at around claiming a confirmation that no
    /// longer holds.
    async fn untoggling_a_confirmed_criterion_clears_its_real_record() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "criteria-untoggle-run"}))).await.unwrap();
        app.clone()
            .oneshot(json_request(
                "POST",
                "/api/runs/criteria-untoggle-run/requirements",
                serde_json::json!({"statement": "WHEN ..., THE SYSTEM SHALL ...", "acceptance_criteria": ["only one criterion"]}),
            ))
            .await
            .unwrap();

        let mut confirm_req = Request::builder().method("POST").uri("/api/runs/criteria-untoggle-run/requirements/0/criteria/0/toggle").body(Body::empty()).unwrap();
        confirm_req.headers_mut().insert("x-gate-email", "scimbe@gmail.com".parse().unwrap());
        app.clone().oneshot(confirm_req).await.unwrap();

        let unconfirm_req = Request::builder().method("POST").uri("/api/runs/criteria-untoggle-run/requirements/0/criteria/0/toggle").body(Body::empty()).unwrap();
        let response = app.oneshot(unconfirm_req).await.unwrap();
        assert_eq!(response.status(), SC::OK);
        let body = body_json(response).await;
        assert!(body["requirements"][0]["verified_criteria"][0].is_null(), "un-confirming must clear the whole record back to a real, honest null");
    }

    #[tokio::test]
    /// Real evaluator finding, issue #55: `created_by` is a genuinely separate
    /// signal from `proposed_by` (which answers human-vs-LLM-authored, not which
    /// real account). Stamped from the same real, gate-verified session
    /// owner_email/submitted_by already use; a client-claimed value in the body
    /// is never trusted.
    async fn add_requirement_stamps_the_real_gate_verified_creator_never_a_client_claimed_one() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "req-creator-run"}))).await.unwrap();

        let mut request = json_request(
            "POST",
            "/api/runs/req-creator-run/requirements",
            serde_json::json!({"statement": "WHEN ..., THE SYSTEM SHALL ...", "acceptance_criteria": ["a real checkable criterion"], "created_by": "client-forged@example.com"}),
        );
        request.headers_mut().insert("x-gate-email", "real-human@example.com".parse().unwrap());
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), SC::OK);
        let body = body_json(response).await;
        assert_eq!(body["requirements"][0]["created_by"], "real-human@example.com", "the real gate header identity must be stamped, never a client-claimed value in the body");

        // No gate header at all -- honestly None, matching owner_email/submitted_by's
        // own established convention.
        let response = app
            .oneshot(json_request(
                "POST",
                "/api/runs/req-creator-run/requirements",
                serde_json::json!({"statement": "WHEN ..., THE SYSTEM SHALL ...", "acceptance_criteria": ["another real checkable criterion"]}),
            ))
            .await
            .unwrap();
        let body = body_json(response).await;
        assert!(body["requirements"][1]["created_by"].is_null(), "no gate header means an honest null creator, never a fabricated one");
    }

    #[tokio::test]
    async fn add_requirement_rejects_an_empty_statement_or_no_acceptance_criteria() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone()
            .oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "req-validate-run"})))
            .await
            .unwrap();

        let response = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/runs/req-validate-run/requirements",
                serde_json::json!({"statement": "  ", "acceptance_criteria": ["something"]}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::BAD_REQUEST);

        let response = app
            .oneshot(json_request(
                "POST",
                "/api/runs/req-validate-run/requirements",
                serde_json::json!({"statement": "WHEN a user does X, THE SYSTEM SHALL do Y (a real statement)", "acceptance_criteria": []}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::BAD_REQUEST, "a requirement with no checkable acceptance criteria must be rejected");
    }

    #[tokio::test]
    /// Real gap found and closed by the incompetent-agent stress test (#382
    /// goal doc §8, 2026-08-05, DAU lens): a live round-trip proved
    /// `{"statement":"asdf",...}` got a real 200 -- nothing checked that a
    /// "requirement" even attempted EARS format.
    async fn add_requirement_rejects_a_statement_that_does_not_look_like_ears() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone()
            .oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "req-ears-run"})))
            .await
            .unwrap();

        let response = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/runs/req-ears-run/requirements",
                serde_json::json!({"statement": "asdf", "acceptance_criteria": ["a real criterion"]}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::BAD_REQUEST, "a statement with no \"SHALL\" isn't even attempting EARS format");

        // Case-insensitive, and doesn't require the "WHEN" trigger clause --
        // a real ubiquitous EARS requirement has none.
        let response = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/runs/req-ears-run/requirements",
                serde_json::json!({"statement": "THE SYSTEM shall always encrypt messages at rest", "acceptance_criteria": ["a real criterion"]}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK, "a real ubiquitous EARS requirement (no WHEN clause) must still be accepted");
    }

    #[tokio::test]
    /// The stress test's thirteenth real run (#382 goal doc §8, 2026-08-06): a
    /// plain `.contains("shall")` matches inside completely unrelated words,
    /// not just the real EARS keyword -- "shallow", "Marshall", etc.
    async fn add_requirement_rejects_shall_only_as_a_substring_of_an_unrelated_word() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "req-ears-substring-run"}))).await.unwrap();

        for garbage in [
            "Do a shallow implementation of the login flow for now",
            "Ask Marshall to review this before merging",
        ] {
            let response = app
                .clone()
                .oneshot(json_request(
                    "POST",
                    "/api/runs/req-ears-substring-run/requirements",
                    serde_json::json!({"statement": garbage, "acceptance_criteria": ["a real criterion"]}),
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), SC::BAD_REQUEST, "\"{garbage}\" contains \"shall\" only as a substring of an unrelated word, not the real EARS keyword");
        }

        // A real EARS statement with trailing punctuation directly against
        // "SHALL" must still be accepted -- the word-boundary fix must not
        // become too strict in the other direction.
        let response = app
            .oneshot(json_request(
                "POST",
                "/api/runs/req-ears-substring-run/requirements",
                serde_json::json!({"statement": "WHEN a user logs in, THE SYSTEM SHALL, at minimum, record the timestamp", "acceptance_criteria": ["a real criterion"]}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK, "\"SHALL,\" with trailing punctuation must still count as the real word");
    }

    #[tokio::test]
    /// Real gap found and closed by the incompetent-agent stress test (#382 goal
    /// doc §8, 2026-08-05): a live round-trip proved "ok", ".", "done", and a
    /// criterion that was ONLY a zero-width space (U+200B, invisible in the GUI --
    /// .trim() doesn't strip it, it's Unicode category Cf/Format, not White_Space)
    /// all sailed through as real, checkable criteria.
    async fn add_requirement_rejects_trivially_uncheckable_acceptance_criteria() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "req-trivial-run"}))).await.unwrap();

        for trivial in ["ok", ".", "done", "\u{200b}"] {
            let response = app
                .clone()
                .oneshot(json_request(
                    "POST",
                    "/api/runs/req-trivial-run/requirements",
                    serde_json::json!({"statement": "WHEN a user does X, THE SYSTEM SHALL do Y (a real statement)", "acceptance_criteria": [trivial]}),
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), SC::BAD_REQUEST, "{trivial:?} must be rejected as not a real, checkable criterion");
        }

        // A genuinely short but real criterion must still be accepted -- this is
        // "enough real content", not an unreasonably high bar.
        let response = app
            .oneshot(json_request(
                "POST",
                "/api/runs/req-trivial-run/requirements",
                serde_json::json!({"statement": "WHEN a user does X, THE SYSTEM SHALL do Y (a real statement)", "acceptance_criteria": ["no crash"]}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK, "a genuinely short but real criterion must not be rejected");
    }

    #[tokio::test]
    /// Real gap found live by the incompetent-agent stress test (#382 goal doc
    /// §8, 2026-08-06): a criterion/statement laced with U+202E (RIGHT-TO-LEFT
    /// OVERRIDE) has plenty of real alphanumeric content either side of the
    /// control character, so it clears every other check, but *visually
    /// renders* with scrambled text order -- live-confirmed via a real headless
    /// browser: "approved\u{202e} for production tset ton si sihT" displays as
    /// "approvedThis is not test noitcudorp rof" in this app's own GUI, which
    /// has no `unicode-bidi` isolation anywhere. Same Trojan Source (CVE-2021-
    /// 42574) attack class the zero-width-space fix above already established
    /// this codebase treats as a real DAU-lens gap, not just an XSS concern.
    async fn add_requirement_rejects_bidi_control_characters_in_statement_or_criteria() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "req-bidi-run"}))).await.unwrap();

        let response = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/runs/req-bidi-run/requirements",
                serde_json::json!({"statement": "WHEN a user does X\u{202e}, THE SYSTEM SHALL do Y", "acceptance_criteria": ["a real criterion"]}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::BAD_REQUEST, "a statement containing a bidi override character must be rejected");

        let response = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/runs/req-bidi-run/requirements",
                serde_json::json!({
                    "statement": "WHEN a user does X, THE SYSTEM SHALL do Y (a real statement)",
                    "acceptance_criteria": ["approved\u{202e} for production tset ton si sihT"]
                }),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::BAD_REQUEST, "a criterion containing a bidi override character must be rejected, even with plenty of real alphanumeric content either side");

        // A genuinely clean statement/criterion pair must still be accepted.
        let response = app
            .oneshot(json_request(
                "POST",
                "/api/runs/req-bidi-run/requirements",
                serde_json::json!({"statement": "WHEN a user does X, THE SYSTEM SHALL do Y (a real statement)", "acceptance_criteria": ["a real, clean criterion"]}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK, "a clean statement/criterion with no bidi control characters must not be rejected");
    }

    #[tokio::test]
    /// DAU-lens gap found live 2026-08-06 (#382 goal doc §8): the trivial/over-length
    /// acceptance-criteria checks used to each `find` and reject on the FIRST bad
    /// criterion only, so a request with several simultaneously-bad criteria required
    /// one real, avoidable retry per extra mistake to fully discover. This must now
    /// report every bad criterion in the one request that has them, not just the first.
    async fn add_requirement_reports_every_bad_acceptance_criterion_in_one_response_not_just_the_first() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "req-multi-bad-run"}))).await.unwrap();

        let too_long = "x".repeat(MAX_ACCEPTANCE_CRITERION_LEN + 1);
        let response = app
            .oneshot(json_request(
                "POST",
                "/api/runs/req-multi-bad-run/requirements",
                serde_json::json!({
                    "statement": "WHEN a user does X, THE SYSTEM SHALL do Y (a real statement)",
                    "acceptance_criteria": ["ok", too_long, "a real, checkable criterion"]
                }),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::BAD_REQUEST, "a request with multiple bad criteria must still be a real 400");
        let body = String::from_utf8(axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap().to_vec()).unwrap();
        assert!(body.contains("\"ok\""), "the short/uncheckable criterion must be named in the response, not silently dropped: {body}");
        assert!(
            body.contains(&format!("over {MAX_ACCEPTANCE_CRITERION_LEN} characters")),
            "the over-length criterion must ALSO be named in the same response, not require a separate retry: {body}"
        );
    }

    #[tokio::test]
    /// Real provenance (#382 goal doc, gap #1): a human adding a requirement directly
    /// (no `proposed_by` in the request, matching the GUI's own Requirements panel)
    /// must land as `proposed_by: null`; `devsystem_assistant`'s chat-driven proposal
    /// (which now always sends `proposed_by: "devsystem.assistant"`) must land tagged
    /// as such -- so a user can tell, per requirement, which are still an LLM's first
    /// draft waiting on review vs. already their own.
    async fn requirement_provenance_distinguishes_human_from_llm_proposed() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone()
            .oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "req-provenance-run"})))
            .await
            .unwrap();

        let response = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/runs/req-provenance-run/requirements",
                serde_json::json!({"statement": "WHEN a user does X, THE SYSTEM SHALL fulfill a human-authored requirement", "acceptance_criteria": ["checkable"]}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK);
        let body = body_json(response).await;
        assert!(body["requirements"][0]["proposed_by"].is_null(), "no proposed_by sent -> must default to human (null), not silently attributed");

        let response = app
            .oneshot(json_request(
                "POST",
                "/api/runs/req-provenance-run/requirements",
                serde_json::json!({
                    "statement": "WHEN a user does X, THE SYSTEM SHALL fulfill an assistant-proposed requirement",
                    "acceptance_criteria": ["checkable"],
                    "proposed_by": "devsystem.assistant",
                }),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK);
        let body = body_json(response).await;
        assert_eq!(body["requirements"][1]["proposed_by"], "devsystem.assistant");
        // The first, human-authored requirement must stay untouched by the second call.
        assert!(body["requirements"][0]["proposed_by"].is_null());
    }

    #[tokio::test]
    async fn toggling_an_out_of_range_requirement_404s() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone()
            .oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "req-oob-run"})))
            .await
            .unwrap();

        let response = app
            .oneshot(Request::builder().method("POST").uri("/api/runs/req-oob-run/requirements/9/toggle").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), SC::NOT_FOUND);
    }

    #[tokio::test]
    // Issue #37 (#382 goal doc): a wrong requirement used to be permanently
    // load-bearing -- no edit path anywhere. Real update-in-place, live-verified
    // end to end: statement and criteria actually change, and any prior
    // confirmation is honestly reset rather than silently carried over against
    // now-different text.
    async fn a_requirement_can_be_corrected_in_place_and_its_old_confirmation_resets() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone()
            .oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "req-update-run"})))
            .await
            .unwrap();
        app.clone()
            .oneshot(json_request(
                "POST",
                "/api/runs/req-update-run/requirements",
                serde_json::json!({"statement": "WHEN a wrong trigger, THE SYSTEM SHALL do the wrong thing", "acceptance_criteria": ["a wrong criterion"]}),
            ))
            .await
            .unwrap();
        // Confirm it and mark it verified before correcting -- proves the update
        // actually resets this, not just that a fresh requirement starts unconfirmed.
        app.clone().oneshot(Request::builder().method("POST").uri("/api/runs/req-update-run/requirements/0/criteria/0/toggle").body(Body::empty()).unwrap()).await.unwrap();

        let response = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/runs/req-update-run/requirements/0/update",
                serde_json::json!({"statement": "WHEN a real trigger fires, THE SYSTEM SHALL do the corrected real thing", "acceptance_criteria": ["a corrected, real, checkable criterion"]}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK);
        let body = body_json(response).await;
        assert_eq!(body["requirements"][0]["statement"], "WHEN a real trigger fires, THE SYSTEM SHALL do the corrected real thing");
        assert_eq!(body["requirements"][0]["acceptance_criteria"], serde_json::json!(["a corrected, real, checkable criterion"]));
        assert_eq!(body["requirements"][0]["verified"], false, "correcting the text must not leave a stale verified=true attached to different criteria");
        assert_eq!(body["requirements"][0]["verified_criteria"], serde_json::json!([]), "the old confirmation was for the old criterion text -- it must not silently carry over");

        // Live-read back via GET to prove this actually persisted, not just the response body.
        let refetched = body_json(app.oneshot(Request::builder().method("GET").uri("/api/runs/req-update-run").body(Body::empty()).unwrap()).await.unwrap()).await;
        assert_eq!(refetched["state"]["requirements"][0]["statement"], "WHEN a real trigger fires, THE SYSTEM SHALL do the corrected real thing");
    }

    #[tokio::test]
    async fn updating_a_requirement_applies_the_same_real_ears_and_criteria_validation_as_adding_one() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "req-update-validate-run"}))).await.unwrap();
        app.clone()
            .oneshot(json_request(
                "POST",
                "/api/runs/req-update-validate-run/requirements",
                serde_json::json!({"statement": "WHEN ..., THE SYSTEM SHALL ...", "acceptance_criteria": ["a real checkable criterion"]}),
            ))
            .await
            .unwrap();

        let non_ears = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/runs/req-update-validate-run/requirements/0/update",
                serde_json::json!({"statement": "just a free-form note, not EARS", "acceptance_criteria": ["a real checkable criterion"]}),
            ))
            .await
            .unwrap();
        assert_eq!(non_ears.status(), SC::BAD_REQUEST);

        let bad_criterion = app
            .oneshot(json_request(
                "POST",
                "/api/runs/req-update-validate-run/requirements/0/update",
                serde_json::json!({"statement": "WHEN ..., THE SYSTEM SHALL ...", "acceptance_criteria": ["ok"]}),
            ))
            .await
            .unwrap();
        assert_eq!(bad_criterion.status(), SC::BAD_REQUEST);
    }

    #[tokio::test]
    async fn updating_an_out_of_range_requirement_404s() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "req-update-oob-run"}))).await.unwrap();

        let response = app
            .oneshot(json_request(
                "POST",
                "/api/runs/req-update-oob-run/requirements/9/update",
                serde_json::json!({"statement": "WHEN ..., THE SYSTEM SHALL ...", "acceptance_criteria": ["a real checkable criterion"]}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_different_account_cannot_update_someone_elses_requirement() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone()
            .oneshot(gate_request("POST", "/api/runs", "owner@example.com", Some(serde_json::json!({"run_id": "req-update-owned-run"}))))
            .await
            .unwrap();
        app.clone()
            .oneshot(gate_request(
                "POST",
                "/api/runs/req-update-owned-run/requirements",
                "owner@example.com",
                Some(serde_json::json!({"statement": "WHEN ..., THE SYSTEM SHALL ...", "acceptance_criteria": ["a real checkable criterion"]})),
            ))
            .await
            .unwrap();

        let mut update_req = json_request(
            "POST",
            "/api/runs/req-update-owned-run/requirements/0/update",
            serde_json::json!({"statement": "WHEN a hijack, THE SYSTEM SHALL be hijacked", "acceptance_criteria": ["a hijacked criterion"]}),
        );
        update_req.headers_mut().insert("x-gate-email", "someone-else@example.com".parse().unwrap());
        assert_eq!(app.oneshot(update_req).await.unwrap().status(), SC::FORBIDDEN);
    }

    #[tokio::test]
    async fn updating_a_requirement_never_touches_who_originally_created_or_proposed_it() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "req-update-provenance-run"}))).await.unwrap();
        let mut add_req = json_request(
            "POST",
            "/api/runs/req-update-provenance-run/requirements",
            serde_json::json!({"statement": "WHEN ..., THE SYSTEM SHALL ...", "acceptance_criteria": ["a real checkable criterion"], "proposed_by": "devsystem.plan"}),
        );
        add_req.headers_mut().insert("x-gate-email", "original-author@example.com".parse().unwrap());
        app.clone().oneshot(add_req).await.unwrap();

        let mut update_req = json_request(
            "POST",
            "/api/runs/req-update-provenance-run/requirements/0/update",
            serde_json::json!({"statement": "WHEN a corrected trigger, THE SYSTEM SHALL do the corrected thing", "acceptance_criteria": ["a corrected, real, checkable criterion"]}),
        );
        update_req.headers_mut().insert("x-gate-email", "original-author@example.com".parse().unwrap());
        let response = app.oneshot(update_req).await.unwrap();
        let body = body_json(response).await;
        assert_eq!(body["requirements"][0]["created_by"], "original-author@example.com", "correcting the text must not change who originally created it");
        assert_eq!(body["requirements"][0]["proposed_by"], "devsystem.plan", "correcting the text must not change whether it was human- or LLM-proposed");
    }

    #[tokio::test]
    /// Real, mandatory quality gate over HTTP (#382 goal doc §5/§8, gap #2): once a run
    /// declares a real `devsystem.review` role (via a real, immediately-applied stage
    /// proposal -- the same self-optimizing path any role-filler uses), marking a
    /// requirement verified is blocked with a real 409 until a successful review
    /// iteration actually names it. A run that never declares review stays ungated.
    async fn a_declared_review_role_gates_verifying_a_requirement_over_http() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone()
            .oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "req-gate-run"})))
            .await
            .unwrap();
        app.clone()
            .oneshot(json_request(
                "POST",
                "/api/runs/req-gate-run/requirements",
                serde_json::json!({"statement": "WHEN a user does X, THE SYSTEM SHALL fulfill a real requirement", "acceptance_criteria": ["checkable"]}),
            ))
            .await
            .unwrap();

        // Declare devsystem.review as a real role -- the same immediately-applied
        // proposal path any role-filler's iteration uses.
        app.clone()
            .oneshot(json_request(
                "POST",
                "/api/runs/req-gate-run/iterate",
                serde_json::json!({
                    "stage": "devsystem.improve",
                    "feedback": "this run needs a real review gate",
                    "succeeded": true,
                    "proposals": [{"proposed_by": "devsystem.improve", "stage_id": "devsystem.review", "tag": "review", "rationale": "quality gate", "use_existing_service": null, "units": 1, "price_ceiling": null}],
                }),
            ))
            .await
            .unwrap();

        let response = app
            .clone()
            .oneshot(Request::builder().method("POST").uri("/api/runs/req-gate-run/requirements/0/toggle").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), SC::CONFLICT, "review is declared but never happened -- must be blocked, not silently accepted");

        // A real, successful review naming this requirement satisfies the gate.
        app.clone()
            .oneshot(json_request(
                "POST",
                "/api/runs/req-gate-run/iterate",
                serde_json::json!({"stage": "devsystem.review", "feedback": "confirmed the acceptance criteria are all met in the real diff", "succeeded": true, "requirement_indices": [0]}),
            ))
            .await
            .unwrap();

        let response = app
            .oneshot(Request::builder().method("POST").uri("/api/runs/req-gate-run/requirements/0/toggle").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK);
        let body = body_json(response).await;
        assert_eq!(body["requirements"][0]["verified"], true);
    }

    #[tokio::test]
    /// Real gap found and closed by actually running the incompetent-agent stress
    /// test (#382 goal doc §8, 2026-08-05): a live round-trip against this exact
    /// gate proved a completely lazy rubber-stamp review ("looks fine to me")
    /// satisfied it just as well as real scrutiny would have. Proven over real HTTP
    /// here, not just the pipeline crate's own unit test.
    async fn a_lazy_rubber_stamp_review_does_not_satisfy_the_gate_over_http() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "lazy-review-run"}))).await.unwrap();
        app.clone()
            .oneshot(json_request(
                "POST",
                "/api/runs/lazy-review-run/requirements",
                serde_json::json!({"statement": "WHEN a user does X, THE SYSTEM SHALL fulfill a real requirement", "acceptance_criteria": ["checkable"]}),
            ))
            .await
            .unwrap();
        app.clone()
            .oneshot(json_request(
                "POST",
                "/api/runs/lazy-review-run/iterate",
                serde_json::json!({
                    "stage": "devsystem.improve",
                    "feedback": "this run needs a real review gate",
                    "succeeded": true,
                    "proposals": [{"proposed_by": "devsystem.improve", "stage_id": "devsystem.review", "tag": "review", "rationale": "quality gate", "use_existing_service": null, "units": 1, "price_ceiling": null}],
                }),
            ))
            .await
            .unwrap();

        // The real, live rubber-stamp that surfaced this gap.
        app.clone()
            .oneshot(json_request(
                "POST",
                "/api/runs/lazy-review-run/iterate",
                serde_json::json!({"stage": "devsystem.review", "feedback": "looks fine to me", "succeeded": true, "requirement_indices": [0]}),
            ))
            .await
            .unwrap();

        let response = app
            .oneshot(Request::builder().method("POST").uri("/api/runs/lazy-review-run/requirements/0/toggle").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), SC::CONFLICT, "a rubber-stamp review must not satisfy the gate, even though it succeeded and named the right requirement");
        let body = String::from_utf8(axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap().to_vec()).unwrap();
        assert!(body.contains("too short"), "the error must explain why: {body}");
    }

    fn assistant_toggle_request(uri: &str) -> Request<Body> {
        Request::builder().method("POST").uri(uri).header("X-Actor", "devsystem.assistant").body(Body::empty()).expect("build request")
    }

    #[tokio::test]
    /// Real gap #10 (#382 goal doc §8, fourteenth stress-test run, 2026-08-06):
    /// on a run that never declared `review` -- most runs, by default -- a
    /// human's own direct click has always been free to verify a requirement
    /// with zero evidence at all (by design, per gap #2's own scoping). This
    /// run proves the assistant-relayed path is now held to the real evidence
    /// bar UNCONDITIONALLY, even here, where the human path stays unblocked.
    async fn assistant_driven_verification_requires_real_review_evidence_even_with_no_review_role_declared() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "assistant-gate-run"}))).await.unwrap();
        app.clone()
            .oneshot(json_request(
                "POST",
                "/api/runs/assistant-gate-run/requirements",
                serde_json::json!({"statement": "WHEN a user does X, THE SYSTEM SHALL fulfill a real requirement", "acceptance_criteria": ["checkable"]}),
            ))
            .await
            .unwrap();

        // The assistant-relayed call must be blocked -- no review role declared,
        // no review iteration, nothing but this bare request.
        let response = app.clone().oneshot(assistant_toggle_request("/api/runs/assistant-gate-run/requirements/0/toggle")).await.unwrap();
        assert_eq!(response.status(), SC::CONFLICT, "the assistant-relayed path must require real evidence even on a run that never declared review");
        let body = String::from_utf8(axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap().to_vec()).unwrap();
        assert!(body.contains("unconditionally"), "the error must explain this is the assistant-specific gate, not the review-declared one: {body}");

        // The exact same request, minus the X-Actor header (a human's own
        // direct click), must still succeed unconditionally -- gap #2's own
        // scoping decision for un-gated runs is unaffected by this fix.
        let response = app
            .oneshot(Request::builder().method("POST").uri("/api/runs/assistant-gate-run/requirements/0/toggle").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK, "a human's own direct click on an un-gated run must be completely unaffected by this fix");
    }

    #[tokio::test]
    /// Real gap #10, the positive case: once genuine review evidence exists,
    /// the assistant-relayed path succeeds exactly like a human's would.
    async fn assistant_driven_verification_succeeds_once_real_review_evidence_exists() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "assistant-gate-pass-run"}))).await.unwrap();
        app.clone()
            .oneshot(json_request(
                "POST",
                "/api/runs/assistant-gate-pass-run/requirements",
                serde_json::json!({"statement": "WHEN a user does X, THE SYSTEM SHALL fulfill a real requirement", "acceptance_criteria": ["checkable"]}),
            ))
            .await
            .unwrap();
        app.clone()
            .oneshot(json_request(
                "POST",
                "/api/runs/assistant-gate-pass-run/iterate",
                serde_json::json!({"stage": "devsystem.review", "feedback": "Checked the actual implementation directly against the acceptance criteria, confirmed real behavior matches.", "succeeded": true, "requirement_indices": [0]}),
            ))
            .await
            .unwrap();

        let response = app.oneshot(assistant_toggle_request("/api/runs/assistant-gate-pass-run/requirements/0/toggle")).await.unwrap();
        assert_eq!(response.status(), SC::OK, "real review evidence must satisfy the assistant-specific gate too");
        let body = body_json(response).await;
        assert_eq!(body["requirements"][0]["verified"], true);
    }

    #[tokio::test]
    /// Real gap #10: un-verifying must stay unconditionally safe for the
    /// assistant-relayed path too, same as the human path -- loosening a
    /// claim never needs evidence to justify it.
    async fn assistant_driven_un_verification_needs_no_evidence() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "assistant-unverify-run"}))).await.unwrap();
        app.clone()
            .oneshot(json_request(
                "POST",
                "/api/runs/assistant-unverify-run/requirements",
                serde_json::json!({"statement": "WHEN a user does X, THE SYSTEM SHALL fulfill a real requirement", "acceptance_criteria": ["checkable"]}),
            ))
            .await
            .unwrap();
        // Verify it directly first (human path, un-gated run).
        app.clone()
            .oneshot(Request::builder().method("POST").uri("/api/runs/assistant-unverify-run/requirements/0/toggle").body(Body::empty()).unwrap())
            .await
            .unwrap();

        let response = app.oneshot(assistant_toggle_request("/api/runs/assistant-unverify-run/requirements/0/toggle")).await.unwrap();
        assert_eq!(response.status(), SC::OK, "un-verifying via the assistant-relayed path must stay unconditionally safe, no evidence required");
        let body = body_json(response).await;
        assert_eq!(body["requirements"][0]["verified"], false);
    }

    #[tokio::test]
    /// Real requirements export over HTTP (#382 goal doc §4.4, gap #7): a real
    /// Markdown document, with real Content-Disposition so a browser actually
    /// downloads it, not just another JSON endpoint.
    async fn requirements_export_renders_real_markdown_with_a_download_header() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "export-run"}))).await.unwrap();
        app.clone()
            .oneshot(json_request(
                "POST",
                "/api/runs/export-run/requirements",
                serde_json::json!({"statement": "WHEN a user does X, THE SYSTEM SHALL fulfill a real requirement", "acceptance_criteria": ["checkable"]}),
            ))
            .await
            .unwrap();

        let response = app.oneshot(Request::builder().uri("/api/runs/export-run/requirements/export").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(response.status(), SC::OK);
        assert_eq!(response.headers().get(axum::http::header::CONTENT_TYPE).unwrap(), "text/markdown; charset=utf-8");
        assert!(response.headers().get(axum::http::header::CONTENT_DISPOSITION).unwrap().to_str().unwrap().contains("attachment"));
        let body = String::from_utf8(axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap().to_vec()).unwrap();
        assert!(body.contains("# Requirements: `export-run`"));
        assert!(body.contains("a real requirement"));
        assert!(body.contains("- [ ] `checkable`"));
        // Issue #35: the export's heading must use the run's own real 0-based
        // ordinal (matching the GUI/requirement_indices), and must surface real
        // coverage -- both proved at this HTTP layer, not just in the pipeline
        // crate's own unit test, since export_requirements is the one real call
        // site that has to actually thread run_state.history through.
        assert!(body.contains("## #0 "), "the export must use the real #0 ordinal, not a stale 1-based ## 1.: {body}");
        assert!(body.contains("Not yet addressed by any iteration"), "a requirement with no real iteration linked to it must say so: {body}");
    }

    #[tokio::test]
    async fn requirement_auto_judge_defaults_off_and_can_be_toggled_without_touching_verified() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone()
            .oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "auto-judge-run"})))
            .await
            .unwrap();
        app.clone()
            .oneshot(json_request(
                "POST",
                "/api/runs/auto-judge-run/requirements",
                serde_json::json!({
                    "statement": "WHEN ..., THE SYSTEM SHALL ...",
                    "acceptance_criteria": ["criterion A"],
                }),
            ))
            .await
            .unwrap();

        let response = app
            .clone()
            .oneshot(Request::builder().method("POST").uri("/api/runs/auto-judge-run/requirements/0/auto-judge/toggle").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK);
        let body: serde_json::Value = serde_json::from_slice(&axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
        assert_eq!(body["requirements"][0]["auto_judge"], true);
        assert_eq!(body["requirements"][0]["verified"], false, "toggling auto_judge must never itself flip verified");

        let response = app
            .oneshot(Request::builder().method("POST").uri("/api/runs/auto-judge-run/requirements/0/auto-judge/toggle").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
        assert_eq!(body["requirements"][0]["auto_judge"], false, "toggling twice must flip back off");
    }

    #[tokio::test]
    async fn toggling_auto_judge_on_an_out_of_range_requirement_404s() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone()
            .oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "auto-judge-oob-run"})))
            .await
            .unwrap();

        let response = app
            .oneshot(Request::builder().method("POST").uri("/api/runs/auto-judge-oob-run/requirements/9/auto-judge/toggle").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), SC::NOT_FOUND);
    }

    #[tokio::test]
    async fn backlog_items_can_be_added_and_toggled_and_persist() {
        let (state, _dir) = test_state();
        let app = api_router(state);

        app.clone()
            .oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "backlog-run"})))
            .await
            .unwrap();

        let response = app
            .clone()
            .oneshot(json_request("POST", "/api/runs/backlog-run/backlog", serde_json::json!({"text": "wire real crypto into native-bridge"})))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK);
        let body = body_json(response).await;
        assert_eq!(body["backlog"][0]["text"], "wire real crypto into native-bridge");
        assert_eq!(body["backlog"][0]["done"], false);

        let response = app
            .clone()
            .oneshot(Request::builder().method("POST").uri("/api/runs/backlog-run/backlog/0/toggle").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK);
        let body = body_json(response).await;
        assert_eq!(body["backlog"][0]["done"], true, "toggle should flip done, not remove the item");

        // Re-fetching the run independently proves it actually persisted to disk.
        let response = app
            .oneshot(Request::builder().uri("/api/runs/backlog-run").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let reread = body_json(response).await;
        assert_eq!(reread["state"]["backlog"][0]["done"], true);
    }

    #[tokio::test]
    async fn backlog_and_milestones_reject_additions_past_the_defensive_cap() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone()
            .oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "cap-run"})))
            .await
            .unwrap();

        for i in 0..MAX_LIST_ITEMS {
            let response = app
                .clone()
                .oneshot(json_request("POST", "/api/runs/cap-run/backlog", serde_json::json!({"text": format!("item {i}")})))
                .await
                .unwrap();
            assert_eq!(response.status(), SC::OK, "item {i} should still fit under the cap");
        }
        let response = app
            .clone()
            .oneshot(json_request("POST", "/api/runs/cap-run/backlog", serde_json::json!({"text": "one too many"})))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::BAD_REQUEST, "the (MAX_LIST_ITEMS + 1)th backlog item must be rejected");

        for i in 0..MAX_LIST_ITEMS {
            let response = app
                .clone()
                .oneshot(json_request("POST", "/api/runs/cap-run/milestones", serde_json::json!({"description": format!("milestone {i}")})))
                .await
                .unwrap();
            assert_eq!(response.status(), SC::OK, "milestone {i} should still fit under the cap");
        }
        let response = app
            .oneshot(json_request("POST", "/api/runs/cap-run/milestones", serde_json::json!({"description": "one too many"})))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::BAD_REQUEST, "the (MAX_LIST_ITEMS + 1)th milestone must be rejected");
    }

    #[tokio::test]
    /// Real gap found live by the incompetent-agent stress test (#382 goal doc
    /// §8, 2026-08-06): MAX_LIST_ITEMS's own doc comment gives a real reason
    /// (unbounded state.json growth, real disk headroom) that applies just as
    /// much to custom_panels and every pending-proposal queue as it does to
    /// backlog/milestones/requirements -- but only the latter three ever got
    /// the check. Live-confirmed before this fix: 510 real panels added in a
    /// row against the actual deployment, zero rejections. Seeds each list
    /// directly to MAX_LIST_ITEMS via the real persist_run (not 3000 real HTTP
    /// round trips, which the backlog/milestones test above already proves the
    /// per-request pattern for) so this stays fast while still exercising the
    /// real handler code path for the one call that matters: the
    /// (MAX_LIST_ITEMS + 1)th.
    async fn every_panel_and_proposal_queue_rejects_growth_past_the_defensive_cap() {
        let (state, dir) = test_state();
        let spec = ct_common::pipeline::PipelineSpec {
            id: "devsystem-cap-run".to_string(),
            roles: vec![ct_common::pipeline::RequiredRole { service: ServiceType::Custom("devsystem.plan".to_string()), units: 1, tag: "plan".to_string(), selection_policy: None }],
            operator_pubkey_hex: None,
            selection_policy: ct_common::pipeline::SelectionPolicy::LowestFloor,
        };
        let mut run_state = devsystem_pipeline::runner::RunState::new("cap-run");
        run_state.custom_panels = (0..MAX_LIST_ITEMS)
            .map(|i| CustomPanel { id: format!("panel-{i}"), title: format!("P{i}"), html: "<p>x</p>".into(), source: None, created_at: 0 })
            .collect();
        run_state.pending_panel_proposals = (0..MAX_LIST_ITEMS)
            .map(|i| PendingPanelProposal { id: format!("prop-{i}"), title: format!("P{i}"), html: "<p>x</p>".into(), proposed_at: 0 })
            .collect();
        run_state.pending_panel_removal_proposals = (0..MAX_LIST_ITEMS)
            .map(|i| PendingPanelRemovalProposal { id: format!("rm-{i}"), panel_id: "panel-0".into(), panel_title: format!("P{i}"), proposed_at: 0 })
            .collect();
        run_state.pending_panel_edit_proposals = (0..MAX_LIST_ITEMS)
            .map(|i| PendingPanelEditProposal { id: format!("edit-{i}"), panel_id: "panel-0".into(), old_title: format!("P{i}"), new_title: "New".into(), new_html: "<p>x</p>".into(), proposed_at: 0 })
            .collect();
        run_state.pending_stage_proposals = (0..MAX_LIST_ITEMS)
            .map(|i| PendingStageProposal {
                id: format!("stage-{i}"),
                proposal: StageProposal { proposed_by: "devsystem.assistant".into(), stage_id: format!("devsystem.x{i}"), tag: format!("x{i}"), rationale: "real reason".into(), use_existing_service: None, units: 1, price_ceiling: None },
                proposed_at: 0,
            })
            .collect();
        run_state.pending_issue_proposals = (0..MAX_LIST_ITEMS)
            .map(|i| PendingIssueProposal { id: format!("issue-{i}"), repo: "scimbe/CADS-webconference-demo".into(), title: format!("T{i}"), body: "real body".into(), proposed_at: 0 })
            .collect();
        persist_run(&dir.path().join("cap-run"), &spec, &run_state).expect("seed the run directly, same real persist_run the handlers use");

        let app = api_router(state);

        let response = app
            .clone()
            .oneshot(json_request("POST", "/api/runs/cap-run/panels", serde_json::json!({"title": "one too many", "html": "<p>x</p>"})))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::BAD_REQUEST, "the (MAX_LIST_ITEMS + 1)th custom panel must be rejected");

        let response = app
            .clone()
            .oneshot(json_request("POST", "/api/runs/cap-run/panels/propose", serde_json::json!({"title": "one too many", "html": "<p>x</p>"})))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::BAD_REQUEST, "the (MAX_LIST_ITEMS + 1)th panel-add proposal must be rejected");

        let response = app.clone().oneshot(json_request("POST", "/api/runs/cap-run/panels/panel-0/propose-remove", serde_json::json!({}))).await.unwrap();
        assert_eq!(response.status(), SC::BAD_REQUEST, "the (MAX_LIST_ITEMS + 1)th panel-removal proposal must be rejected");

        let response = app
            .clone()
            .oneshot(json_request("POST", "/api/runs/cap-run/panels/panel-0/propose-edit", serde_json::json!({"title": "one too many", "html": "<p>x</p>"})))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::BAD_REQUEST, "the (MAX_LIST_ITEMS + 1)th panel-edit proposal must be rejected");

        let response = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/runs/cap-run/stages/propose",
                serde_json::json!({"stage_id": "devsystem.one_too_many", "tag": "one_too_many", "rationale": "a real reason here"}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::BAD_REQUEST, "the (MAX_LIST_ITEMS + 1)th stage proposal must be rejected");

        let response = app
            .oneshot(json_request(
                "POST",
                "/api/runs/cap-run/issues/propose",
                serde_json::json!({"repo": "scimbe/CADS-webconference-demo", "title": "one too many", "body": "a real body here"}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::BAD_REQUEST, "the (MAX_LIST_ITEMS + 1)th issue proposal must be rejected");
    }

    #[tokio::test]
    /// Real gap found live 2026-08-06 (stress-test run 42): unlike every per-run cap
    /// above, `create_run` itself had no cap at all on the total NUMBER of runs --
    /// confirmed live this deployment already carries 110 real run directories on a
    /// host at 91% disk, and `list_runs` does a real `fs::read_dir` + full state load
    /// for every run on every single `GET /api/runs` call, so an unbounded total run
    /// count degrades the whole dashboard for every real user, not just disk space.
    /// Seeds `MAX_TOTAL_RUNS` fake run directories directly on disk (a bare
    /// `state.json`, all `total_run_count`/`run_exists` actually check) rather than
    /// MAX_TOTAL_RUNS real HTTP round trips, which would make this test needlessly
    /// slow for no extra real coverage of the one call that matters: the
    /// (MAX_TOTAL_RUNS + 1)th `create_run`.
    async fn create_run_rejects_once_the_total_run_cap_is_reached() {
        let (state, _dir) = test_state();
        for i in 0..MAX_TOTAL_RUNS {
            let seed_dir = state.runs_dir.join(format!("seed-{i}"));
            std::fs::create_dir_all(&seed_dir).expect("create seed run dir");
            std::fs::write(seed_dir.join("state.json"), "{}").expect("write seed state.json");
        }
        let app = api_router(state);
        let response = app
            .oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "one-too-many"})))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::BAD_REQUEST, "the (MAX_TOTAL_RUNS + 1)th run creation must be rejected");
    }

    #[tokio::test]
    async fn add_backlog_item_rejects_empty_text_and_toggle_404s_out_of_range() {
        let (state, _dir) = test_state();
        let app = api_router(state);

        app.clone()
            .oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "backlog-edge-run"})))
            .await
            .unwrap();

        let response = app
            .clone()
            .oneshot(json_request("POST", "/api/runs/backlog-edge-run/backlog", serde_json::json!({"text": "   "})))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::BAD_REQUEST);

        let response = app
            .oneshot(Request::builder().method("POST").uri("/api/runs/backlog-edge-run/backlog/9/toggle").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), SC::NOT_FOUND);
    }

    #[tokio::test]
    /// DAU-lens gap found live 2026-08-06 (#382 goal doc §8): every other real
    /// free-text field in this codebase (requirement statements, acceptance
    /// criteria, issue title/body) already has a real length cap -- backlog item
    /// text and milestone descriptions were the one exception, bounded only by
    /// axum's generic whole-request body limit. Live-confirmed before fixing: a
    /// real 500,000-character backlog item text got a real 200.
    async fn backlog_and_milestone_text_reject_an_absurdly_long_value() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "long-text-run"}))).await.unwrap();

        let huge = "x".repeat(MAX_SHORT_TEXT_LEN + 1);
        let response = app
            .clone()
            .oneshot(json_request("POST", "/api/runs/long-text-run/backlog", serde_json::json!({"text": huge})))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::BAD_REQUEST, "an absurdly long backlog item text must be rejected");

        let huge2 = "x".repeat(MAX_SHORT_TEXT_LEN + 1);
        let response = app
            .clone()
            .oneshot(json_request("POST", "/api/runs/long-text-run/milestones", serde_json::json!({"description": huge2})))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::BAD_REQUEST, "an absurdly long milestone description must be rejected");

        let response = app
            .oneshot(json_request("POST", "/api/runs/long-text-run/backlog", serde_json::json!({"text": "a genuinely real, short backlog item"})))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK, "a genuine, reasonably-sized backlog item must not be rejected");
    }

    #[tokio::test]
    /// Real gap found live by the incompetent-agent stress test (#382 goal doc
    /// §8, 2026-08-06): extending the same firing's own noted follow-up
    /// candidates -- the bidi-control-character fix for requirement
    /// statement/criteria was deliberately scoped narrow at first. A
    /// milestone description mattered most of the remaining candidates,
    /// since `achieved: true` auto-pauses the run as a real checkpoint a
    /// human trusts at face value; a milestone laced with U+202E used to
    /// sail through untouched, live-confirmed before this fix.
    async fn backlog_and_milestone_text_reject_bidi_control_characters() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "bidi-text-run"}))).await.unwrap();

        let response = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/runs/bidi-text-run/backlog",
                serde_json::json!({"text": "write tests\u{202e} for the real feature"}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::BAD_REQUEST, "a backlog item containing a bidi override character must be rejected");

        let response = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/runs/bidi-text-run/milestones",
                serde_json::json!({"description": "M1 achieved\u{202e} deification esaeler ton -- gnitset ylno"}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::BAD_REQUEST, "a milestone description containing a bidi override character must be rejected");

        // Genuinely clean text must still be accepted through both endpoints.
        let response = app
            .clone()
            .oneshot(json_request("POST", "/api/runs/bidi-text-run/backlog", serde_json::json!({"text": "a real, clean backlog item"})))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK, "a clean backlog item must not be rejected");

        let response = app
            .oneshot(json_request("POST", "/api/runs/bidi-text-run/milestones", serde_json::json!({"description": "a real, clean milestone"})))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK, "a clean milestone description must not be rejected");
    }

    #[tokio::test]
    async fn proposed_panel_never_appears_in_custom_panels_until_a_human_approves_it() {
        let (state, _dir) = test_state();
        let app = api_router(state);

        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "propose-run"}))).await.unwrap();

        let response = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/runs/propose-run/panels/propose",
                serde_json::json!({"title": "Assistant idea", "html": "<h2>hi</h2>"}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK);
        let proposed = body_json(response).await;
        let proposal_id = proposed["id"].as_str().unwrap().to_string();

        // The whole point: proposing must NOT make it live.
        let get = app.clone().oneshot(Request::builder().uri("/api/runs/propose-run").body(Body::empty()).unwrap()).await.unwrap();
        let run = body_json(get).await;
        assert_eq!(run["state"]["custom_panels"].as_array().unwrap().len(), 0, "a proposal must never appear in custom_panels before approval");
        assert_eq!(run["state"]["pending_panel_proposals"].as_array().unwrap().len(), 1);

        // Approving moves it, for real, into custom_panels with an honest source.
        let approve = app
            .clone()
            .oneshot(Request::builder().method("POST").uri(format!("/api/runs/propose-run/panels/proposals/{proposal_id}/approve")).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(approve.status(), SC::OK);
        let approved_panel = body_json(approve).await;
        assert_eq!(approved_panel["source"], "assistant");
        assert_eq!(approved_panel["title"], "Assistant idea");

        let get = app.oneshot(Request::builder().uri("/api/runs/propose-run").body(Body::empty()).unwrap()).await.unwrap();
        let run = body_json(get).await;
        assert_eq!(run["state"]["custom_panels"].as_array().unwrap().len(), 1, "approval must make it live");
        assert_eq!(run["state"]["pending_panel_proposals"].as_array().unwrap().len(), 0, "approval must remove it from pending");
    }

    #[tokio::test]
    async fn rejecting_a_panel_proposal_discards_it_without_ever_going_live() {
        let (state, _dir) = test_state();
        let app = api_router(state);

        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "reject-run"}))).await.unwrap();

        let response = app
            .clone()
            .oneshot(json_request("POST", "/api/runs/reject-run/panels/propose", serde_json::json!({"title": "Bad idea", "html": "<p>no</p>"})))
            .await
            .unwrap();
        let proposal_id = body_json(response).await["id"].as_str().unwrap().to_string();

        let reject = app
            .clone()
            .oneshot(Request::builder().method("POST").uri(format!("/api/runs/reject-run/panels/proposals/{proposal_id}/reject")).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(reject.status(), SC::OK);

        let get = app.clone().oneshot(Request::builder().uri("/api/runs/reject-run").body(Body::empty()).unwrap()).await.unwrap();
        let run = body_json(get).await;
        assert_eq!(run["state"]["pending_panel_proposals"].as_array().unwrap().len(), 0);
        assert_eq!(run["state"]["custom_panels"].as_array().unwrap().len(), 0, "a rejected proposal must never have gone live");

        // Rejecting an id that no longer exists (already rejected) is a real 404, not silently OK.
        let response = app
            .oneshot(Request::builder().method("POST").uri(format!("/api/runs/reject-run/panels/proposals/{proposal_id}/reject")).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), SC::NOT_FOUND);
    }

    #[tokio::test]
    /// Real gap #4 second half (#382 goal doc §7.2): the assistant could
    /// already propose ADDING a panel; this is the mirror for REMOVING an
    /// existing one, same pending-until-approved trust model.
    async fn a_panel_removal_proposal_only_removes_the_real_panel_once_approved() {
        let (state, _dir) = test_state();
        let app = api_router(state);

        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "remove-propose-run"}))).await.unwrap();
        let added = app
            .clone()
            .oneshot(json_request("POST", "/api/runs/remove-propose-run/panels", serde_json::json!({"title": "Burndown", "html": "<h2>hi</h2>"})))
            .await
            .unwrap();
        let panel_id = body_json(added).await["id"].as_str().unwrap().to_string();

        let proposed = app
            .clone()
            .oneshot(Request::builder().method("POST").uri(format!("/api/runs/remove-propose-run/panels/{panel_id}/propose-remove")).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(proposed.status(), SC::OK);
        let proposal = body_json(proposed).await;
        assert_eq!(proposal["panel_title"], "Burndown");
        let proposal_id = proposal["id"].as_str().unwrap().to_string();

        // The whole point: proposing removal must NOT remove it yet.
        let get = app.clone().oneshot(Request::builder().uri("/api/runs/remove-propose-run").body(Body::empty()).unwrap()).await.unwrap();
        let run = body_json(get).await;
        assert_eq!(run["state"]["custom_panels"].as_array().unwrap().len(), 1, "the panel must still be live while the removal is only proposed");
        assert_eq!(run["state"]["pending_panel_removal_proposals"].as_array().unwrap().len(), 1);

        let approve = app
            .clone()
            .oneshot(Request::builder().method("POST").uri(format!("/api/runs/remove-propose-run/panels/removal-proposals/{proposal_id}/approve")).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(approve.status(), SC::OK);
        assert_eq!(body_json(approve).await["removed"], true);

        let get = app.oneshot(Request::builder().uri("/api/runs/remove-propose-run").body(Body::empty()).unwrap()).await.unwrap();
        let run = body_json(get).await;
        assert_eq!(run["state"]["custom_panels"].as_array().unwrap().len(), 0, "approval must actually remove the real panel");
        assert_eq!(run["state"]["pending_panel_removal_proposals"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn rejecting_a_panel_removal_proposal_leaves_the_real_panel_untouched() {
        let (state, _dir) = test_state();
        let app = api_router(state);

        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "remove-reject-run"}))).await.unwrap();
        let added = app
            .clone()
            .oneshot(json_request("POST", "/api/runs/remove-reject-run/panels", serde_json::json!({"title": "Keep me", "html": "<p>real</p>"})))
            .await
            .unwrap();
        let panel_id = body_json(added).await["id"].as_str().unwrap().to_string();

        let proposed = app
            .clone()
            .oneshot(Request::builder().method("POST").uri(format!("/api/runs/remove-reject-run/panels/{panel_id}/propose-remove")).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let proposal_id = body_json(proposed).await["id"].as_str().unwrap().to_string();

        let reject = app
            .clone()
            .oneshot(Request::builder().method("POST").uri(format!("/api/runs/remove-reject-run/panels/removal-proposals/{proposal_id}/reject")).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(reject.status(), SC::OK);

        let get = app.oneshot(Request::builder().uri("/api/runs/remove-reject-run").body(Body::empty()).unwrap()).await.unwrap();
        let run = body_json(get).await;
        assert_eq!(run["state"]["custom_panels"].as_array().unwrap().len(), 1, "a rejected removal proposal must never have touched the real panel");
        assert_eq!(run["state"]["pending_panel_removal_proposals"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn proposing_removal_of_an_unknown_panel_id_is_a_real_404() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "remove-unknown-run"}))).await.unwrap();

        let response = app
            .oneshot(Request::builder().method("POST").uri("/api/runs/remove-unknown-run/panels/does-not-exist/propose-remove").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), SC::NOT_FOUND);
    }

    #[tokio::test]
    async fn update_custom_panel_edits_the_real_panel_in_place_immediately_no_proposal_needed() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "update-run"}))).await.unwrap();
        let added = app
            .clone()
            .oneshot(json_request("POST", "/api/runs/update-run/panels", serde_json::json!({"title": "Old Title", "html": "<p>old</p>"})))
            .await
            .unwrap();
        let added_body = body_json(added).await;
        let panel_id = added_body["id"].as_str().unwrap().to_string();
        let created_at = added_body["created_at"].clone();

        let response = app
            .clone()
            .oneshot(json_request("POST", &format!("/api/runs/update-run/panels/{panel_id}/update"), serde_json::json!({"title": "New Title", "html": "<p>new</p>"})))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK);
        let updated = body_json(response).await;
        assert_eq!(updated["id"], panel_id, "editing must keep the same real id, not mint a new one");
        assert_eq!(updated["title"], "New Title");
        assert_eq!(updated["html"], "<p>new</p>");
        assert_eq!(updated["created_at"], created_at, "editing content must not reset when it was originally created");

        let get = app.oneshot(Request::builder().uri("/api/runs/update-run").body(Body::empty()).unwrap()).await.unwrap();
        let run = body_json(get).await;
        let panels = run["state"]["custom_panels"].as_array().unwrap();
        assert_eq!(panels.len(), 1, "editing must never create a second panel");
        assert_eq!(panels[0]["title"], "New Title");
    }

    #[tokio::test]
    async fn updating_an_unknown_custom_panel_id_is_a_real_404() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "update-unknown-run"}))).await.unwrap();

        let response = app
            .oneshot(json_request("POST", "/api/runs/update-unknown-run/panels/does-not-exist/update", serde_json::json!({"title": "x", "html": "y"})))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_panel_edit_proposal_only_changes_the_real_panel_once_approved() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "edit-propose-run"}))).await.unwrap();
        let added = app
            .clone()
            .oneshot(json_request("POST", "/api/runs/edit-propose-run/panels", serde_json::json!({"title": "Before", "html": "<p>before</p>"})))
            .await
            .unwrap();
        let panel_id = body_json(added).await["id"].as_str().unwrap().to_string();

        let proposed = app
            .clone()
            .oneshot(json_request("POST", &format!("/api/runs/edit-propose-run/panels/{panel_id}/propose-edit"), serde_json::json!({"title": "After", "html": "<p>after</p>"})))
            .await
            .unwrap();
        assert_eq!(proposed.status(), SC::OK);
        let proposal = body_json(proposed).await;
        assert_eq!(proposal["old_title"], "Before");
        assert_eq!(proposal["new_title"], "After");
        let proposal_id = proposal["id"].as_str().unwrap().to_string();

        // The whole point: proposing an edit must NOT change the real panel yet.
        let get = app.clone().oneshot(Request::builder().uri("/api/runs/edit-propose-run").body(Body::empty()).unwrap()).await.unwrap();
        let run = body_json(get).await;
        assert_eq!(run["state"]["custom_panels"][0]["title"], "Before", "the panel must still show its original content while the edit is only proposed");
        assert_eq!(run["state"]["pending_panel_edit_proposals"].as_array().unwrap().len(), 1);

        let approve = app
            .clone()
            .oneshot(Request::builder().method("POST").uri(format!("/api/runs/edit-propose-run/panels/edit-proposals/{proposal_id}/approve")).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(approve.status(), SC::OK);
        assert_eq!(body_json(approve).await["updated"], true);

        let get = app.oneshot(Request::builder().uri("/api/runs/edit-propose-run").body(Body::empty()).unwrap()).await.unwrap();
        let run = body_json(get).await;
        assert_eq!(run["state"]["custom_panels"][0]["title"], "After", "approval must actually overwrite the real panel");
        assert_eq!(run["state"]["custom_panels"][0]["html"], "<p>after</p>");
        assert_eq!(run["state"]["pending_panel_edit_proposals"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn rejecting_a_panel_edit_proposal_leaves_the_real_panel_untouched() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "edit-reject-run"}))).await.unwrap();
        let added = app
            .clone()
            .oneshot(json_request("POST", "/api/runs/edit-reject-run/panels", serde_json::json!({"title": "Keep me", "html": "<p>real</p>"})))
            .await
            .unwrap();
        let panel_id = body_json(added).await["id"].as_str().unwrap().to_string();

        let proposed = app
            .clone()
            .oneshot(json_request("POST", &format!("/api/runs/edit-reject-run/panels/{panel_id}/propose-edit"), serde_json::json!({"title": "Overwritten", "html": "<p>gone</p>"})))
            .await
            .unwrap();
        let proposal_id = body_json(proposed).await["id"].as_str().unwrap().to_string();

        let reject = app
            .clone()
            .oneshot(Request::builder().method("POST").uri(format!("/api/runs/edit-reject-run/panels/edit-proposals/{proposal_id}/reject")).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(reject.status(), SC::OK);

        let get = app.oneshot(Request::builder().uri("/api/runs/edit-reject-run").body(Body::empty()).unwrap()).await.unwrap();
        let run = body_json(get).await;
        assert_eq!(run["state"]["custom_panels"][0]["title"], "Keep me", "a rejected edit proposal must never have touched the real panel");
        assert_eq!(run["state"]["pending_panel_edit_proposals"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn proposing_an_edit_of_an_unknown_panel_id_is_a_real_404() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "edit-unknown-run"}))).await.unwrap();

        let response = app
            .oneshot(json_request("POST", "/api/runs/edit-unknown-run/panels/does-not-exist/propose-edit", serde_json::json!({"title": "x", "html": "y"})))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::NOT_FOUND);
    }

    #[tokio::test]
    async fn propose_custom_panel_rejects_an_empty_title_and_oversized_html() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "propose-edge-run"}))).await.unwrap();

        let response = app
            .clone()
            .oneshot(json_request("POST", "/api/runs/propose-edge-run/panels/propose", serde_json::json!({"title": "  ", "html": "<p>x</p>"})))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::BAD_REQUEST);

        let response = app
            .oneshot(json_request(
                "POST",
                "/api/runs/propose-edge-run/panels/propose",
                serde_json::json!({"title": "ok", "html": "x".repeat(MAX_CUSTOM_PANEL_HTML_BYTES + 1)}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::BAD_REQUEST);
    }

    #[tokio::test]
    /// Real gap found live by the incompetent-agent stress test (#382 goal doc
    /// §8, 2026-08-06): closing out this class's last-but-one noted candidate.
    /// A panel `title` is real, trusted UI chrome (the panel list, this
    /// feature's own confirm() dialogs) -- unlike `html`, deliberately
    /// untrusted-by-design and sandboxed, which is why only `title` gets this
    /// check. Live-confirmed before fixing: "Safe Panel" + U+202E + reversed
    /// text sailed through untouched at all four real entry points.
    async fn custom_panel_title_rejects_bidi_control_characters_at_all_four_entry_points() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "bidi-panel-run"}))).await.unwrap();

        let bidi_title = "Safe Panel\u{202e} lenap suoregnad a yllaer si sihT";

        // 1. add_custom_panel (direct add).
        let response = app
            .clone()
            .oneshot(json_request("POST", "/api/runs/bidi-panel-run/panels", serde_json::json!({"title": bidi_title, "html": "<p>x</p>"})))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::BAD_REQUEST, "add_custom_panel must reject a bidi-laced title");

        // Add a real, clean panel to edit/update against.
        let response = app
            .clone()
            .oneshot(json_request("POST", "/api/runs/bidi-panel-run/panels", serde_json::json!({"title": "Clean Panel", "html": "<p>x</p>"})))
            .await
            .unwrap();
        let panel = body_json(response).await;
        let panel_id = panel["id"].as_str().unwrap().to_string();

        // 2. update_custom_panel (direct edit).
        let response = app
            .clone()
            .oneshot(json_request(
                "POST",
                &format!("/api/runs/bidi-panel-run/panels/{panel_id}/update"),
                serde_json::json!({"title": bidi_title, "html": "<p>x</p>"}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::BAD_REQUEST, "update_custom_panel must reject a bidi-laced title");

        // 3. propose_custom_panel (assistant-proposal path).
        let response = app
            .clone()
            .oneshot(json_request("POST", "/api/runs/bidi-panel-run/panels/propose", serde_json::json!({"title": bidi_title, "html": "<p>x</p>"})))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::BAD_REQUEST, "propose_custom_panel must reject a bidi-laced title");

        // 4. propose_panel_edit (assistant-edit-proposal path).
        let response = app
            .oneshot(json_request(
                "POST",
                &format!("/api/runs/bidi-panel-run/panels/{panel_id}/propose-edit"),
                serde_json::json!({"title": bidi_title, "html": "<p>x</p>"}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::BAD_REQUEST, "propose_panel_edit must reject a bidi-laced title");
    }

    #[tokio::test]
    async fn approving_an_unknown_proposal_id_is_a_real_404() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "approve-404-run"}))).await.unwrap();

        let response = app
            .oneshot(Request::builder().method("POST").uri("/api/runs/approve-404-run/panels/proposals/doesnotexist/approve").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), SC::NOT_FOUND);
    }

    #[tokio::test]
    async fn proposed_stage_never_touches_the_live_spec_until_a_human_approves_it() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "propose-stage-run"}))).await.unwrap();

        let response = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/runs/propose-stage-run/stages/propose",
                serde_json::json!({"stage_id": "devsystem.android_emulator_test", "tag": "android_emulator_test", "rationale": "need real emulator coverage"}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK);
        let proposed = body_json(response).await;
        assert_eq!(proposed["proposal"]["proposed_by"], "devsystem.assistant", "proposed_by must always be server-set, never client-supplied");
        let proposal_id = proposed["id"].as_str().unwrap().to_string();

        let get = app.clone().oneshot(Request::builder().uri("/api/runs/propose-stage-run").body(Body::empty()).unwrap()).await.unwrap();
        let run = body_json(get).await;
        assert_eq!(run["spec"]["roles"].as_array().unwrap().len(), 1, "the live spec must be untouched before approval (still just the default plan role)");
        assert_eq!(run["state"]["pending_stage_proposals"].as_array().unwrap().len(), 1);

        let approve = app
            .clone()
            .oneshot(Request::builder().method("POST").uri(format!("/api/runs/propose-stage-run/stages/proposals/{proposal_id}/approve")).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(approve.status(), SC::OK);
        let result = body_json(approve).await;
        assert_eq!(result["outcome"], "added");
        assert_eq!(result["roles_now"], 2);

        let get = app.oneshot(Request::builder().uri("/api/runs/propose-stage-run").body(Body::empty()).unwrap()).await.unwrap();
        let run = body_json(get).await;
        assert_eq!(run["spec"]["roles"].as_array().unwrap().len(), 2, "approval must add the real role to the live spec");
        assert_eq!(run["state"]["pending_stage_proposals"].as_array().unwrap().len(), 0);
        assert_eq!(run["state"]["added_stages"], serde_json::json!(["devsystem.android_emulator_test"]));
    }

    #[tokio::test]
    /// Real gap found live by the stress test, twenty-fifth run, 2026-08-06:
    /// approving a stage proposal through this exact real path used to
    /// discard the proposal's own real price_ceiling entirely -- no_price_ceiling
    /// (preflight.rs) only ever scanned history.proposals, which this path
    /// never touches at all, so an assistant-relayed proposal's cost exposure
    /// became permanently invisible to that risk check the moment it was
    /// approved. Proves the real fix at this real call site, not just the
    /// shared preflight.rs logic in isolation.
    async fn approving_a_stage_proposal_with_no_price_ceiling_makes_the_real_risk_appear() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "propose-stage-price-run"}))).await.unwrap();

        let response = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/runs/propose-stage-price-run/stages/propose",
                serde_json::json!({"stage_id": "devsystem.gpu_training", "tag": "gpu_training", "rationale": "needs a real new paid service"}),
            ))
            .await
            .unwrap();
        let proposal_id = body_json(response).await["id"].as_str().unwrap().to_string();

        let before = app.clone().oneshot(Request::builder().uri("/api/runs/propose-stage-price-run").body(Body::empty()).unwrap()).await.unwrap();
        let before = body_json(before).await;
        assert!(before["risks"].as_array().unwrap().is_empty(), "not live yet -- no real risk before approval");

        app.clone()
            .oneshot(Request::builder().method("POST").uri(format!("/api/runs/propose-stage-price-run/stages/proposals/{proposal_id}/approve")).body(Body::empty()).unwrap())
            .await
            .unwrap();

        let after = app.oneshot(Request::builder().uri("/api/runs/propose-stage-price-run").body(Body::empty()).unwrap()).await.unwrap();
        let after = body_json(after).await;
        let risks = after["risks"].as_array().unwrap();
        assert!(
            risks.iter().any(|r| r["label"] == "no price ceiling set"),
            "approving this exact real path must make the real cost-exposure risk visible, not silently lose it: {risks:?}"
        );
    }

    #[tokio::test]
    async fn approving_a_stage_proposal_for_an_already_present_role_is_idempotent_not_a_duplicate() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "dup-stage-run"}))).await.unwrap();

        // Propose the exact role the default spec already has (devsystem.plan).
        let response = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/runs/dup-stage-run/stages/propose",
                serde_json::json!({"stage_id": "devsystem.plan", "tag": "plan", "rationale": "already exists, testing idempotency"}),
            ))
            .await
            .unwrap();
        let proposal_id = body_json(response).await["id"].as_str().unwrap().to_string();

        let approve = app
            .oneshot(Request::builder().method("POST").uri(format!("/api/runs/dup-stage-run/stages/proposals/{proposal_id}/approve")).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let result = body_json(approve).await;
        assert_eq!(result["outcome"], "already_present");
        assert_eq!(result["roles_now"], 1, "must not create a duplicate role for the same stage_id");
    }

    #[tokio::test]
    async fn rejecting_a_stage_proposal_discards_it_without_ever_touching_the_spec() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "reject-stage-run"}))).await.unwrap();

        let response = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/runs/reject-stage-run/stages/propose",
                serde_json::json!({"stage_id": "devsystem.bad_idea", "tag": "bad_idea", "rationale": "nope"}),
            ))
            .await
            .unwrap();
        let proposal_id = body_json(response).await["id"].as_str().unwrap().to_string();

        let reject = app
            .clone()
            .oneshot(Request::builder().method("POST").uri(format!("/api/runs/reject-stage-run/stages/proposals/{proposal_id}/reject")).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(reject.status(), SC::OK);

        let get = app.oneshot(Request::builder().uri("/api/runs/reject-stage-run").body(Body::empty()).unwrap()).await.unwrap();
        let run = body_json(get).await;
        assert_eq!(run["spec"]["roles"].as_array().unwrap().len(), 1, "a rejected proposal must never touch the live spec");
        assert_eq!(run["state"]["pending_stage_proposals"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn propose_stage_rejects_empty_fields_and_zero_units() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "propose-stage-edge-run"}))).await.unwrap();

        let response = app
            .clone()
            .oneshot(json_request("POST", "/api/runs/propose-stage-edge-run/stages/propose", serde_json::json!({"stage_id": "  ", "tag": "x", "rationale": "y"})))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::BAD_REQUEST);

        let response = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/runs/propose-stage-edge-run/stages/propose",
                serde_json::json!({"stage_id": "devsystem.x", "tag": "x", "rationale": "y", "units": 0}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::BAD_REQUEST);

        // Real gap found live 2026-08-06 (stress-test run 55): units had a zero
        // check but no upper bound -- live-confirmed units: 18446744073709551615
        // (u64::MAX) got a real 200 before this fix.
        let response = app
            .oneshot(json_request(
                "POST",
                "/api/runs/propose-stage-edge-run/stages/propose",
                serde_json::json!({"stage_id": "devsystem.x", "tag": "x", "rationale": "y", "units": u64::MAX}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::BAD_REQUEST, "an absurdly large units value must be rejected, not silently accepted");
    }

    #[tokio::test]
    /// Real gap found live by the incompetent-agent stress test (#382 goal doc
    /// §8, 2026-08-06): closes out the bidi-control-character class's last
    /// candidate at its assistant-facing entry point -- a human approving from
    /// the pending-review queue trusts exactly this rationale to justify the
    /// proposal at face value. Live-confirmed before fixing: a rationale
    /// containing a real U+202E sailed through untouched.
    async fn propose_stage_rejects_a_bidi_control_character_in_rationale() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "bidi-rationale-run"}))).await.unwrap();

        let bidi_rationale = "Needed for real testing\u{202e} noitcaxe atad lautca sesopxe -- egats suoregnad a si sihT";
        let response = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/runs/bidi-rationale-run/stages/propose",
                serde_json::json!({"stage_id": "devsystem.x", "tag": "x", "rationale": bidi_rationale}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::BAD_REQUEST, "a rationale containing a bidi override character must be rejected");

        let response = app
            .oneshot(json_request(
                "POST",
                "/api/runs/bidi-rationale-run/stages/propose",
                serde_json::json!({"stage_id": "devsystem.x", "tag": "x", "rationale": "a genuine, clean reason"}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK, "a clean rationale must not be rejected");
    }

    #[tokio::test]
    async fn proposed_issue_never_reaches_github_until_a_human_approves_it() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "propose-issue-run"}))).await.unwrap();

        let response = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/runs/propose-issue-run/issues/propose",
                serde_json::json!({"repo": "scimbe/CADS-webconference-demo", "title": "Real gap found", "body": "Detailed real description."}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK);
        let proposed = body_json(response).await;
        assert_eq!(proposed["repo"], "scimbe/CADS-webconference-demo");
        let proposal_id = proposed["id"].as_str().unwrap().to_string();

        let get = app.oneshot(Request::builder().uri("/api/runs/propose-issue-run").body(Body::empty()).unwrap()).await.unwrap();
        let run = body_json(get).await;
        assert_eq!(run["state"]["pending_issue_proposals"].as_array().unwrap().len(), 1, "nothing must be posted anywhere -- it only sits pending");
        assert_eq!(run["state"]["pending_issue_proposals"][0]["id"], proposal_id);
    }

    #[tokio::test]
    async fn propose_issue_rejects_a_repo_outside_the_allowlist() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "issue-allowlist-run"}))).await.unwrap();

        let response = app
            .oneshot(json_request(
                "POST",
                "/api/runs/issue-allowlist-run/issues/propose",
                serde_json::json!({"repo": "scimbe/some-other-repo", "title": "x", "body": "y"}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::BAD_REQUEST, "an assistant-authored proposal must never be able to target an arbitrary repo");
    }

    #[tokio::test]
    async fn propose_issue_rejects_empty_title_or_body_and_oversized_ones() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "issue-edge-run"}))).await.unwrap();

        let response = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/runs/issue-edge-run/issues/propose",
                serde_json::json!({"repo": "scimbe/CADS-webconference-demo", "title": "  ", "body": "y"}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::BAD_REQUEST);

        let response = app
            .oneshot(json_request(
                "POST",
                "/api/runs/issue-edge-run/issues/propose",
                serde_json::json!({"repo": "scimbe/CADS-webconference-demo", "title": "x".repeat(MAX_ISSUE_TITLE_LEN + 1), "body": "y"}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::BAD_REQUEST);
    }

    #[tokio::test]
    /// Real gap found live by the incompetent-agent stress test (#382 goal doc
    /// §8, 2026-08-06): extends the bidi-control-character class to a proposed
    /// GitHub issue's title/body -- a human approving from the review queue
    /// trusts exactly this text, and approving it files a real issue with
    /// whatever content is actually stored.
    async fn propose_issue_rejects_bidi_control_characters_in_title_or_body() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "bidi-issue-run"}))).await.unwrap();

        let bidi = "Real gap\u{202e} gnihton -- ekaf yletelpmoc si eussi sihT";
        let response = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/runs/bidi-issue-run/issues/propose",
                serde_json::json!({"repo": "scimbe/CADS-webconference-demo", "title": bidi, "body": "a real detail"}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::BAD_REQUEST, "a bidi-laced title must be rejected");

        let response = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/runs/bidi-issue-run/issues/propose",
                serde_json::json!({"repo": "scimbe/CADS-webconference-demo", "title": "a real title", "body": bidi}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::BAD_REQUEST, "a bidi-laced body must be rejected");

        let response = app
            .oneshot(json_request(
                "POST",
                "/api/runs/bidi-issue-run/issues/propose",
                serde_json::json!({"repo": "scimbe/CADS-webconference-demo", "title": "a real title", "body": "a real detail"}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK, "clean title/body must not be rejected");
    }

    #[tokio::test]
    async fn approving_an_issue_proposal_reports_503_honestly_when_no_github_token_is_configured() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "issue-503-run"}))).await.unwrap();
        let response = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/runs/issue-503-run/issues/propose",
                serde_json::json!({"repo": "scimbe/CADS-webconference-demo", "title": "Real gap", "body": "Real detail."}),
            ))
            .await
            .unwrap();
        let proposal_id = body_json(response).await["id"].as_str().unwrap().to_string();

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/runs/issue-503-run/issues/proposals/{proposal_id}/approve"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), SC::SERVICE_UNAVAILABLE, "must say plainly this deployment has no GitHub token, never silently no-op");
        let body = body_json(response).await;
        assert_eq!(body["repo"], "scimbe/CADS-webconference-demo", "the drafted content must still be returned so the operator can post it by hand");
        assert_eq!(body["title"], "Real gap");
    }

    /// Sends one proposal into an existing (empty) run and returns its id --
    /// the same setup every approve/reject test below needs, factored out so
    /// the four new #48-slice-6 channel-relay tests aren't pure boilerplate
    /// repeats of the pre-existing tests around them.
    async fn create_run_and_propose_issue(app: Router, run_id: &str) -> String {
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": run_id}))).await.unwrap();
        let response = app
            .oneshot(json_request("POST", &format!("/api/runs/{run_id}/issues/propose"), serde_json::json!({"repo": "scimbe/CADS-webconference-demo", "title": "Real gap", "body": "Real detail."})))
            .await
            .unwrap();
        body_json(response).await["id"].as_str().unwrap().to_string()
    }

    #[tokio::test]
    async fn approving_an_issue_proposal_via_a_configured_channel_relay_posts_through_the_real_subprocess_and_reports_the_real_url() {
        // Echoes back the real argv/env it actually received, so the
        // assertions below prove the real values reached the subprocess, not
        // just that the code compiles.
        let (state, _dir) = test_state_with_issue_channel(
            r#"echo "argv=$1|$2|$3 ctagent=$CT_AGENT_BIN addr=$CT_CHANNEL_ADDR noise=$CT_CHANNEL_NOISE_KEY peer=$CT_CHANNEL_PEER_NOISE_KEY certfile=$CT_CHANNEL_PEER_CERT_FILE" >&2
echo "created: #9 https://github.com/scimbe/CADS-webconference-demo/issues/9""#,
        );
        let app = api_router(state);
        let proposal_id = create_run_and_propose_issue(app.clone(), "issue-channel-created-run").await;

        let response = app
            .clone()
            .oneshot(Request::builder().method("POST").uri(format!("/api/runs/issue-channel-created-run/issues/proposals/{proposal_id}/approve")).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK);
        let body = body_json(response).await;
        assert_eq!(body["number"], 9);
        assert_eq!(body["html_url"], "https://github.com/scimbe/CADS-webconference-demo/issues/9");
        assert_eq!(body["already_filed"], false);

        let get = app.oneshot(Request::builder().uri("/api/runs/issue-channel-created-run").body(Body::empty()).unwrap()).await.unwrap();
        let run = body_json(get).await;
        assert_eq!(run["state"]["pending_issue_proposals"].as_array().unwrap().len(), 0, "an approved proposal must be removed from the pending list");
    }

    #[tokio::test]
    async fn approving_an_issue_proposal_via_the_channel_relay_sets_every_real_env_var_the_client_needs() {
        let (state, _dir) = test_state_with_issue_channel(
            r#"echo "ctagent=$CT_AGENT_BIN addr=$CT_CHANNEL_ADDR noise=$CT_CHANNEL_NOISE_KEY peer=$CT_CHANNEL_PEER_NOISE_KEY certfile=$CT_CHANNEL_PEER_CERT_FILE" > "$0.envcheck"
echo "created: #1 https://github.com/scimbe/CADS-webconference-demo/issues/1""#,
        );
        let dir = _dir.path().to_path_buf();
        let app = api_router(state);
        let proposal_id = create_run_and_propose_issue(app.clone(), "issue-channel-env-run").await;
        app.oneshot(Request::builder().method("POST").uri(format!("/api/runs/issue-channel-env-run/issues/proposals/{proposal_id}/approve")).body(Body::empty()).unwrap()).await.unwrap();

        let envcheck = std::fs::read_to_string(dir.join("fake-issue-channel-client.sh.envcheck")).expect("fake client must have recorded the real env it received");
        assert!(envcheck.contains("ctagent=fake-ct-agent"), "real envcheck: {envcheck}");
        assert!(envcheck.contains("addr=127.0.0.1:1"));
        assert!(envcheck.contains("noise=fake-noise-priv"));
        assert!(envcheck.contains("peer=fake-noise-peer-pub"));
        assert!(envcheck.contains("certfile=/nonexistent/fake-cert-file"));
    }

    #[tokio::test]
    async fn approving_an_issue_proposal_via_the_channel_relay_reports_an_already_filed_result_honestly_not_as_a_fresh_create() {
        let (state, _dir) = test_state_with_issue_channel(r#"echo "already filed: https://github.com/scimbe/CADS-webconference-demo/issues/2""#);
        let app = api_router(state);
        let proposal_id = create_run_and_propose_issue(app.clone(), "issue-channel-already-filed-run").await;

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/runs/issue-channel-already-filed-run/issues/proposals/{proposal_id}/approve"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK);
        let body = body_json(response).await;
        assert_eq!(body["already_filed"], true);
        assert_eq!(body["number"], serde_json::Value::Null, "there is no real new issue number for an already-filed result -- must not be fabricated");
        assert_eq!(body["html_url"], "https://github.com/scimbe/CADS-webconference-demo/issues/2");
    }

    #[tokio::test]
    async fn approving_an_issue_proposal_surfaces_a_real_channel_client_failure_honestly_not_as_a_fabricated_success() {
        let (state, _dir) = test_state_with_issue_channel(r#"echo "agent reported an error: repo not in allowlist" >&2
exit 1"#);
        let app = api_router(state);
        let proposal_id = create_run_and_propose_issue(app.clone(), "issue-channel-failure-run").await;

        let response = app
            .clone()
            .oneshot(Request::builder().method("POST").uri(format!("/api/runs/issue-channel-failure-run/issues/proposals/{proposal_id}/approve")).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), SC::BAD_GATEWAY, "a real subprocess failure must be a real error, never a fabricated 200");
        let body = body_json(response).await;
        assert!(body["error"].as_str().unwrap().contains("repo not in allowlist"), "real error: {body}");

        let get = app.oneshot(Request::builder().uri("/api/runs/issue-channel-failure-run").body(Body::empty()).unwrap()).await.unwrap();
        let run = body_json(get).await;
        assert_eq!(run["state"]["pending_issue_proposals"].as_array().unwrap().len(), 1, "a failed post must not be removed from the pending list");
    }

    #[tokio::test]
    async fn a_configured_channel_relay_is_preferred_over_a_configured_direct_github_token() {
        let (mut state, _dir) = test_state_with_issue_channel(r#"echo "created: #5 https://github.com/scimbe/CADS-webconference-demo/issues/5""#);
        // A real token is ALSO configured -- if the direct-POST path were used
        // instead, this test would need real network access to api.github.com
        // and could never pass hermetically. The channel relay winning proves
        // the direct path was never attempted.
        state.github_token = Some(Arc::from("also-configured-but-must-not-be-used"));
        let app = api_router(state);
        let proposal_id = create_run_and_propose_issue(app.clone(), "issue-channel-priority-run").await;

        let response = app
            .oneshot(Request::builder().method("POST").uri(format!("/api/runs/issue-channel-priority-run/issues/proposals/{proposal_id}/approve")).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK);
        let body = body_json(response).await;
        assert_eq!(body["html_url"], "https://github.com/scimbe/CADS-webconference-demo/issues/5", "the channel relay's own result must win, proving it -- not the direct-POST path -- handled this request");
    }

    #[tokio::test]
    async fn rejecting_an_issue_proposal_discards_it_without_ever_touching_github() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "reject-issue-run"}))).await.unwrap();
        let response = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/runs/reject-issue-run/issues/propose",
                serde_json::json!({"repo": "scimbe/CADS-webconference-demo", "title": "Nope", "body": "not needed"}),
            ))
            .await
            .unwrap();
        let proposal_id = body_json(response).await["id"].as_str().unwrap().to_string();

        let reject = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/runs/reject-issue-run/issues/proposals/{proposal_id}/reject"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(reject.status(), SC::OK);

        let get = app.oneshot(Request::builder().uri("/api/runs/reject-issue-run").body(Body::empty()).unwrap()).await.unwrap();
        let run = body_json(get).await;
        assert_eq!(run["state"]["pending_issue_proposals"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn pausing_a_run_blocks_new_iterations_until_resumed() {
        let (state, _dir) = test_state();
        let app = api_router(state);

        app.clone()
            .oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "pause-run"})))
            .await
            .unwrap();

        let response = app
            .clone()
            .oneshot(Request::builder().method("POST").uri("/api/runs/pause-run/pause").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK);

        let response = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/runs/pause-run/iterate",
                serde_json::json!({"stage": "devsystem.implement", "feedback": "should be refused while paused", "succeeded": true}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::CONFLICT, "a paused run must refuse new iterations");

        // list_runs surfaces the paused flag too, not just get_run.
        let response = app.clone().oneshot(Request::builder().uri("/api/runs").body(Body::empty()).unwrap()).await.unwrap();
        let body = body_json(response).await;
        assert_eq!(body[0]["paused"], true);

        let response = app
            .clone()
            .oneshot(Request::builder().method("POST").uri("/api/runs/pause-run/resume").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK);

        let response = app
            .oneshot(json_request(
                "POST",
                "/api/runs/pause-run/iterate",
                serde_json::json!({"stage": "devsystem.implement", "feedback": "allowed after resume", "succeeded": true}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK, "iterations must be accepted again after resume");
    }

    #[tokio::test]
    /// Real "why paused" distinction (stress-test run 49): a milestone achieved, a
    /// run hitting its own real bound, and a human's own direct pause click are
    /// three genuinely different situations that used to render an identical
    /// banner. Proves the real, honest reason a direct pause/resume records, and
    /// that resuming clears it rather than leaving a stale reason a later
    /// auto-pause could inherit.
    async fn pause_and_resume_record_and_clear_a_real_reason() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "reason-run"}))).await.unwrap();

        let response = app
            .clone()
            .oneshot(Request::builder().method("POST").uri("/api/runs/reason-run/pause").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(body_json(response).await["pause_reason"], "paused manually");

        let get = app.clone().oneshot(Request::builder().uri("/api/runs/reason-run").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(body_json(get).await["state"]["pause_reason"], "paused manually");

        let response = app
            .clone()
            .oneshot(Request::builder().method("POST").uri("/api/runs/reason-run/resume").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(body_json(response).await["pause_reason"], serde_json::Value::Null, "resuming must clear the reason, not leave it stale");
    }

    #[tokio::test]
    /// Real gap found live 2026-08-07 against the actual `webconference-android`
    /// deployment: right after a real check-in fired, `iterations_until_checkin`
    /// silently reset to the full `checkin_every` value again, and
    /// `needs_attention` (which only ever looked at `iterations_until_checkin <=
    /// 1`) cleared at the exact moment it should have been most true -- a fired,
    /// never-reviewed check-in was indistinguishable from a healthy run mid-cycle.
    /// Proves the real fix end to end through the actual HTTP surface: pending
    /// stays visible until a real, explicit acknowledge call, then re-flags on a
    /// genuinely later boundary rather than staying silently satisfied forever
    /// (the same staleness-bug class this session already closed four other
    /// instances of in `preflight.rs`).
    async fn checkin_pending_persists_until_acknowledged_then_re_flags_on_the_next_boundary() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "checkin-run"}))).await.unwrap();
        app.clone()
            .oneshot(json_request("POST", "/api/runs/checkin-run/criteria", serde_json::json!({"max_iterations": 20, "max_consecutive_failures": 3, "checkin_every": 2})))
            .await
            .unwrap();

        for i in 0..2 {
            let response = app
                .clone()
                .oneshot(json_request("POST", "/api/runs/checkin-run/iterate", serde_json::json!({"stage": "devsystem.implement", "feedback": format!("real work {i}"), "succeeded": true})))
                .await
                .unwrap();
            assert_eq!(response.status(), SC::OK);
        }

        let get = app.clone().oneshot(Request::builder().uri("/api/runs/checkin-run").body(Body::empty()).unwrap()).await.unwrap();
        let body = body_json(get).await;
        assert_eq!(body["health"]["checkin_pending"], true, "iteration 2 of a checkin_every: 2 run must show a real, persistent pending signal");

        let list = app.clone().oneshot(Request::builder().uri("/api/runs").body(Body::empty()).unwrap()).await.unwrap();
        let runs = body_json(list).await;
        let entry = runs.as_array().unwrap().iter().find(|r| r["run_id"] == "checkin-run").unwrap();
        assert_eq!(entry["needs_attention"], true, "the Runs list badge must reflect the real pending check-in, not just iterations_until_checkin");

        let ack = app
            .clone()
            .oneshot(Request::builder().method("POST").uri("/api/runs/checkin-run/checkin/acknowledge").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(ack.status(), SC::OK);

        let get = app.clone().oneshot(Request::builder().uri("/api/runs/checkin-run").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(body_json(get).await["health"]["checkin_pending"], false, "acknowledging the real boundary that fired must clear it");

        // A genuinely later boundary must re-flag, not stay silently satisfied by
        // an earlier acknowledgment forever.
        for i in 2..4 {
            app.clone()
                .oneshot(json_request("POST", "/api/runs/checkin-run/iterate", serde_json::json!({"stage": "devsystem.implement", "feedback": format!("real work {i}"), "succeeded": true})))
                .await
                .unwrap();
        }
        let get = app.oneshot(Request::builder().uri("/api/runs/checkin-run").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(body_json(get).await["health"]["checkin_pending"], true, "iteration 4's boundary is a real, new pending check-in, not covered by the earlier acknowledgment");
    }

    #[tokio::test]
    /// Real evaluator finding, issue #48: `run_iteration` now actually pauses the run
    /// when a check-in fires (RunOutcome::CheckinDue's own doc comment always
    /// promised this). A cadence check-in is a review checkpoint, not a stop like a
    /// milestone -- acknowledging it must also resume the run, or the mandatory
    /// "at least this often" cadence would otherwise wedge the run on every single
    /// boundary until a second, separate Resume click. The very next submission
    /// after acknowledging must be accepted, not still 409 "run is paused".
    async fn acknowledging_a_real_checkin_pause_also_resumes_the_run() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "ack-resume-run"}))).await.unwrap();
        app.clone()
            .oneshot(json_request("POST", "/api/runs/ack-resume-run/criteria", serde_json::json!({"max_iterations": 20, "max_consecutive_failures": 3, "checkin_every": 1})))
            .await
            .unwrap();

        let iter1 = app
            .clone()
            .oneshot(json_request("POST", "/api/runs/ack-resume-run/iterate", serde_json::json!({"stage": "devsystem.implement", "feedback": "real work", "succeeded": true})))
            .await
            .unwrap();
        assert_eq!(iter1.status(), SC::OK, "the iteration that crosses the boundary is still accepted and recorded");

        let get = app.clone().oneshot(Request::builder().uri("/api/runs/ack-resume-run").body(Body::empty()).unwrap()).await.unwrap();
        let body = body_json(get).await;
        assert_eq!(body["state"]["paused"], true, "a fired check-in must actually pause the run, not just report CheckinDue");
        assert!(body["state"]["pause_reason"].as_str().unwrap().starts_with("check-in due"));

        let blocked = app
            .clone()
            .oneshot(json_request("POST", "/api/runs/ack-resume-run/iterate", serde_json::json!({"stage": "devsystem.implement", "feedback": "must be refused", "succeeded": true})))
            .await
            .unwrap();
        assert_eq!(blocked.status(), SC::CONFLICT, "paused must actually block the next submission");

        let ack = app
            .clone()
            .oneshot(Request::builder().method("POST").uri("/api/runs/ack-resume-run/checkin/acknowledge").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(ack.status(), SC::OK);

        let get = app.clone().oneshot(Request::builder().uri("/api/runs/ack-resume-run").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(body_json(get).await["state"]["paused"], false, "acknowledging a real check-in pause must also resume the run");

        let iter2 = app
            .oneshot(json_request("POST", "/api/runs/ack-resume-run/iterate", serde_json::json!({"stage": "devsystem.implement", "feedback": "accepted after acknowledging", "succeeded": true})))
            .await
            .unwrap();
        assert_eq!(iter2.status(), SC::OK, "the next real submission after acknowledging must be accepted, not still blocked");
    }

    #[tokio::test]
    /// The other real half of the same fix: acknowledging must never silently wave
    /// through an UNRELATED pause reason that happens to also be set (a manual
    /// pause, a milestone, a ceiling) -- only a real check-in pause is ever cleared
    /// by acknowledging one.
    async fn acknowledging_a_checkin_does_not_clear_an_unrelated_manual_pause() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "ack-manual-pause-run"}))).await.unwrap();

        app.clone()
            .oneshot(Request::builder().method("POST").uri("/api/runs/ack-manual-pause-run/pause").body(Body::empty()).unwrap())
            .await
            .unwrap();

        let ack = app
            .clone()
            .oneshot(Request::builder().method("POST").uri("/api/runs/ack-manual-pause-run/checkin/acknowledge").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(ack.status(), SC::OK, "acknowledging is idempotent/real even when nothing is actually pending");

        let get = app.oneshot(Request::builder().uri("/api/runs/ack-manual-pause-run").body(Body::empty()).unwrap()).await.unwrap();
        let body = body_json(get).await;
        assert_eq!(body["state"]["paused"], true, "a real manual pause must survive acknowledging an (unrelated, nonexistent) check-in");
        assert_eq!(body["state"]["pause_reason"], "paused manually");
    }

    #[tokio::test]
    async fn update_criteria_persists_and_health_reflects_it() {
        let (state, _dir) = test_state();
        let app = api_router(state);

        app.clone()
            .oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "criteria-run"})))
            .await
            .unwrap();

        let response = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/runs/criteria-run/criteria",
                serde_json::json!({"max_iterations": 50, "max_consecutive_failures": 5, "checkin_every": 10}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK);

        let response = app
            .oneshot(Request::builder().uri("/api/runs/criteria-run").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body = body_json(response).await;
        assert_eq!(body["health"]["criteria"]["max_iterations"], 50);
        assert_eq!(body["health"]["iterations_until_checkin"], 10, "health should reflect the newly saved criteria, not the old default");
    }

    #[tokio::test]
    /// Real bug found live by the incompetent-agent stress test (#382 goal
    /// doc §8/§9, 2026-08-06): `checkin_every: 0` has no validation
    /// (`update_criteria` only rejects `max_iterations`/
    /// `max_consecutive_failures` at zero), and `iterations_until_checkin`
    /// used to hardcode `0` for it -- actively misrepresenting "the cadence
    /// is effectively disabled" as "a check-in is due right now", which then
    /// fed straight into `needs_attention`'s own `<= 1` threshold,
    /// permanently false-flagging the run. Now reports the real distance to
    /// the actual next check-in event (the hard `max_iterations` ceiling).
    async fn checkin_every_zero_reports_the_real_ceiling_distance_not_a_misleading_zero() {
        let (state, _dir) = test_state();
        let app = api_router(state);

        app.clone()
            .oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "checkin-disabled-run"})))
            .await
            .unwrap();
        app.clone()
            .oneshot(json_request(
                "POST",
                "/api/runs/checkin-disabled-run/criteria",
                serde_json::json!({"max_iterations": 20, "max_consecutive_failures": 3, "checkin_every": 0}),
            ))
            .await
            .unwrap();

        let response = app
            .clone()
            .oneshot(Request::builder().uri("/api/runs/checkin-disabled-run").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body = body_json(response).await;
        assert_eq!(
            body["health"]["iterations_until_checkin"], 20,
            "must report the real distance to the ceiling, not a misleading 0 that implies imminent"
        );

        let response = app.oneshot(Request::builder().uri("/api/runs").body(Body::empty()).unwrap()).await.unwrap();
        let list = body_json(response).await;
        let entry = list.as_array().unwrap().iter().find(|r| r["run_id"] == "checkin-disabled-run").unwrap();
        assert_eq!(
            entry["needs_attention"], false,
            "a fresh run with a disabled (not sparse-but-real) cadence must not be permanently false-flagged"
        );
    }

    #[tokio::test]
    async fn update_criteria_rejects_zero_bounds() {
        let (state, _dir) = test_state();
        let app = api_router(state);

        app.clone()
            .oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "bad-criteria-run"})))
            .await
            .unwrap();

        let response = app
            .oneshot(json_request(
                "POST",
                "/api/runs/bad-criteria-run/criteria",
                serde_json::json!({"max_iterations": 0, "max_consecutive_failures": 5, "checkin_every": 10}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::BAD_REQUEST);
    }

    #[tokio::test]
    /// Real gap found live by the incompetent-agent stress test (#382 goal doc
    /// §8, 2026-08-06): update_criteria rejected 0, but had no upper bound at
    /// all -- live-verified before this fix, {"max_iterations": 4294967295,
    /// ...} (u32::MAX) got a real 200, turning this run's "bounded super loop"
    /// -- #382's own stated, central architectural principle -- into one
    /// that's unbounded for any practical purpose.
    async fn update_criteria_rejects_a_value_so_large_the_loop_is_unbounded_in_practice() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "huge-criteria-run"}))).await.unwrap();

        for bad in [
            serde_json::json!({"max_iterations": u32::MAX, "max_consecutive_failures": 3, "checkin_every": 5}),
            serde_json::json!({"max_iterations": 20, "max_consecutive_failures": u32::MAX, "checkin_every": 5}),
            serde_json::json!({"max_iterations": 20, "max_consecutive_failures": 3, "checkin_every": u32::MAX}),
        ] {
            let response = app.clone().oneshot(json_request("POST", "/api/runs/huge-criteria-run/criteria", bad)).await.unwrap();
            assert_eq!(response.status(), SC::BAD_REQUEST, "a value so large the bounded loop is unbounded in practice must be rejected");
        }

        // A real, generous, still-genuinely-finite value must still work.
        let response = app
            .oneshot(json_request(
                "POST",
                "/api/runs/huge-criteria-run/criteria",
                serde_json::json!({"max_iterations": 10000, "max_consecutive_failures": 10000, "checkin_every": 10000}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK, "the real ceiling value itself must still be accepted, not just values strictly under it");
    }

    #[tokio::test]
    async fn get_run_reports_health_for_a_fresh_run() {
        let (state, _dir) = test_state();
        let app = api_router(state);

        app.clone()
            .oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "health-run"})))
            .await
            .unwrap();

        let response = app
            .oneshot(Request::builder().uri("/api/runs/health-run").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK);
        let body = body_json(response).await;
        assert_eq!(body["health"]["consecutive_failures"], 0);
        assert_eq!(body["health"]["iterations_completed"], 0);
        assert_eq!(body["health"]["iterations_until_checkin"], 5, "fresh run is 5 iterations from the default checkin_every cadence");
        assert_eq!(body["health"]["iterations_until_ceiling"], 20);
    }

    #[tokio::test]
    /// Real evaluator finding, issue #51: a fresh run only ever declares `devsystem.plan`
    /// as a role (by design), which left the New Iteration dropdown offering nothing else
    /// -- every other real stage had to be hand-typed into the unvalidated free-text box,
    /// producing exactly the kind of typo `validate_stage` (issue #49) now rejects. This
    /// field is the fix's other half: the real `ALL_STAGES` constant, so the GUI can offer
    /// all seven canonical stages as real, pickable options without pretending they're
    /// declared roles.
    async fn get_run_reports_the_real_canonical_stages_regardless_of_declared_roles() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "canon-run"}))).await.unwrap();

        let response = app.oneshot(Request::builder().uri("/api/runs/canon-run").body(Body::empty()).unwrap()).await.unwrap();
        let body = body_json(response).await;
        let canonical = body["canonical_stages"].as_array().expect("canonical_stages is a real array");
        assert_eq!(canonical.len(), 7, "all seven canonical stages, regardless of how many roles this run has actually declared");
        let names: Vec<&str> = canonical.iter().map(|v| v.as_str().unwrap()).collect();
        assert!(names.contains(&"devsystem.plan"));
        assert!(names.contains(&"devsystem.improve"), "devsystem.improve must be offered even though a fresh run never declares it as a role");
    }

    #[tokio::test]
    async fn get_run_404_for_nonexistent_run() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        let response = app
            .oneshot(Request::builder().uri("/api/runs/does-not-exist").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), SC::NOT_FOUND);
    }

    #[tokio::test]
    async fn path_traversal_via_dotdot_id_is_rejected_not_served() {
        // Real, previously-exploitable bug: only create_run validated id's charset --
        // every other handler passed the URL's {id} segment straight into
        // runs_dir.join(id), and PathBuf::join honors ".." components. Proven
        // directly before this fix: GET /api/runs/.. returned 200 with the contents
        // of a state.json planted OUTSIDE runs_dir, in its parent directory.
        let dir = tempfile::tempdir().expect("tempdir");
        let runs_dir = dir.path().join("runs");
        fs::create_dir_all(&runs_dir).unwrap();
        fs::write(dir.path().join("state.json"), r#"{"run_id":"OUTSIDE","consecutive_failures":0,"history":[],"added_stages":[],"criteria":{"max_iterations":20,"max_consecutive_failures":3,"checkin_every":5}}"#).unwrap();

        let state = AppState {
            runs_dir: Arc::new(runs_dir),
            write_lock: Arc::new(tokio::sync::Mutex::new(())),
            offers: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            selection_state: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            assistant_url: None,
            github_token: None,
            http_client: reqwest::Client::builder().timeout(std::time::Duration::from_secs(5)).build().expect("build http client"),
            rag_embedding_api_key: None,
            rag_embedding_api_base: Arc::from("https://api.openai.com/v1"),
            rag_unstructured_api_key: None,
            rag_unstructured_api_base: Arc::from("https://api.unstructured.io"),
            rag_db_pool: None,
            issue_channel: None,
            document_extraction_channel: None,
        };
        let app = api_router(state);

        for uri in ["/api/runs/..", "/api/runs/../checkin", "/api/runs/../memory"] {
            let response = app.clone().oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap()).await.unwrap();
            assert_eq!(response.status(), SC::BAD_REQUEST, "traversal attempt via {uri} must be rejected, not served");
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
            assert!(!String::from_utf8_lossy(&bytes).contains("OUTSIDE"), "the out-of-bounds file's content must never leak into the response for {uri}");
        }
    }

    #[tokio::test]
    async fn iterate_run_round_trip_persists_real_state_change() {
        let (state, dir) = test_state();
        let runs_dir = dir.path().to_path_buf();
        let app = api_router(state);

        let response = app
            .clone()
            .oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "iter-run"})))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::CREATED);

        let state_path = runs_dir.join("iter-run").join("state.json");
        let before = fs::read_to_string(&state_path).expect("state.json exists after create");
        assert!(before.contains("\"history\": []"));

        let response = app
            .oneshot(json_request(
                "POST",
                "/api/runs/iter-run/iterate",
                serde_json::json!({"stage": "devsystem.implement", "feedback": "real progress", "succeeded": true}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK);
        let body = body_json(response).await;
        assert_eq!(body["outcome"], "Continue");
        assert_eq!(body["iteration"], 1);

        let after = fs::read_to_string(&state_path).expect("state.json still exists");
        assert!(after.contains("\"real progress\""), "iteration feedback should be persisted to disk");
        assert!(!after.contains("\"history\": []"), "history should no longer be empty on disk");
    }

    #[tokio::test]
    /// Real identity (GitHub issue #38: the exact same iteration got submitted
    /// twice, byte-for-byte, into webconference-android's real history, with no
    /// field to tell the two apart or say which one was real). Every real
    /// submission through this real endpoint must get its own real, unique,
    /// server-generated id and a real, non-zero submission timestamp -- and a
    /// client-supplied id must never override the server's own.
    async fn iterate_run_gives_every_real_submission_a_real_unique_server_generated_id() {
        let (state, dir) = test_state();
        let runs_dir = dir.path().to_path_buf();
        let app = api_router(state);

        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "id-run"}))).await.unwrap();

        for i in 0..2 {
            let response = app
                .clone()
                .oneshot(json_request(
                    "POST",
                    "/api/runs/id-run/iterate",
                    // A client-supplied id/submitted_at must be ignored -- the
                    // server's own values are the only ones that count.
                    serde_json::json!({"stage": "devsystem.implement", "feedback": format!("real progress {i}"), "succeeded": true, "id": "client-forged-id", "submitted_at": 1}),
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), SC::OK);
        }

        let state_path = runs_dir.join("id-run").join("state.json");
        let persisted: serde_json::Value = serde_json::from_str(&fs::read_to_string(&state_path).unwrap()).unwrap();
        let history = persisted["history"].as_array().expect("history is a real array");
        assert_eq!(history.len(), 2);

        let id0 = history[0]["id"].as_str().expect("a real id string");
        let id1 = history[1]["id"].as_str().expect("a real id string");
        assert_eq!(id0.len(), 16, "the real id must be a full 16-hex-char server-generated value: {id0}");
        assert_ne!(id0, "client-forged-id", "a client-supplied id must never override the server's own");
        assert_ne!(id0, id1, "two real, distinct submissions must never share an id");

        let submitted_at0 = history[0]["submitted_at"].as_u64().expect("a real submitted_at");
        assert!(submitted_at0 > 1_700_000_000, "the real submitted_at must be a genuine current unix timestamp, not the client-supplied 1: {submitted_at0}");
    }

    #[tokio::test]
    /// Real evaluator finding, issue #40: "who submitted iteration N" was permanently
    /// unanswerable -- the only bidder identity anywhere lived in the auction view and
    /// expired 300s after issue. `submitted_by` is now stamped server-side from the
    /// same real, gate-verified `x-gate-email` header `/api/me`/`owner_email` already
    /// use -- never trusted from the request body, so a client cannot claim to be
    /// someone else.
    async fn iterate_run_stamps_the_real_gate_verified_submitter_never_the_client_claimed_one() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "actor-run"}))).await.unwrap();

        let mut request = json_request(
            "POST",
            "/api/runs/actor-run/iterate",
            serde_json::json!({"stage": "devsystem.implement", "feedback": "real work", "succeeded": true, "submitted_by": "client-forged@example.com"}),
        );
        request.headers_mut().insert("x-gate-email", "real-crew@example.com".parse().unwrap());
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), SC::OK);

        let response = app.oneshot(Request::builder().uri("/api/runs/actor-run").body(Body::empty()).unwrap()).await.unwrap();
        let body = body_json(response).await;
        assert_eq!(
            body["state"]["history"][0]["submitted_by"], "real-crew@example.com",
            "the real, gate-verified header identity must be stamped, never the client-claimed one in the request body"
        );
    }

    #[tokio::test]
    /// The honest counterpart: no `x-gate-email` header at all (the local
    /// `devsystem_iterate` CLI's own real submission path, and every M2M/`--remote`
    /// bearer-token caller) must record a real, honest `None` -- never a fabricated
    /// identity.
    async fn iterate_run_records_submitted_by_as_none_when_no_gate_header_is_present() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "no-gate-run"}))).await.unwrap();

        app.clone()
            .oneshot(json_request("POST", "/api/runs/no-gate-run/iterate", serde_json::json!({"stage": "devsystem.implement", "feedback": "real work", "succeeded": true})))
            .await
            .unwrap();

        let response = app.oneshot(Request::builder().uri("/api/runs/no-gate-run").body(Body::empty()).unwrap()).await.unwrap();
        let body = body_json(response).await;
        assert!(body["state"]["history"][0]["submitted_by"].is_null(), "no gate header means an honest null, never a fabricated identity");
    }

    #[tokio::test]
    /// Real idempotency guard (found necessary live, 2026-08-05): a same-day window of
    /// overlapping devsystem-web instances during a redeploy let two functionally-
    /// identical iterations both land, with the SAME computed iteration number and no
    /// gap filled -- confirmed directly in webconference-android's own real history (two
    /// "iteration: 8" entries, no "9"). `write_lock` only serializes concurrent requests
    /// *within* one process, not across two separate instances. This is the defense-in-
    /// depth fix: a submission byte-identical to the run's own immediately-preceding
    /// entry is rejected outright, regardless of why a duplicate arrived.
    async fn iterate_run_rejects_a_submission_byte_identical_to_the_immediately_preceding_one() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "dup-run"}))).await.unwrap();

        let body = serde_json::json!({"stage": "devsystem.improve", "feedback": "declare a role", "succeeded": true});
        let response = app.clone().oneshot(json_request("POST", "/api/runs/dup-run/iterate", body.clone())).await.unwrap();
        assert_eq!(response.status(), SC::OK);
        assert_eq!(body_json(response).await["iteration"], 1);

        // The exact same submission again -- must be rejected, not silently recorded
        // as a second, distinct iteration.
        let response = app.clone().oneshot(json_request("POST", "/api/runs/dup-run/iterate", body.clone())).await.unwrap();
        assert_eq!(response.status(), SC::CONFLICT);

        // A genuinely different submission (different feedback) is never blocked by
        // this guard -- it's not a blanket "no two iterations from the same stage."
        let different = serde_json::json!({"stage": "devsystem.improve", "feedback": "a real, different piece of work", "succeeded": true});
        let response = app.oneshot(json_request("POST", "/api/runs/dup-run/iterate", different)).await.unwrap();
        assert_eq!(response.status(), SC::OK);
        assert_eq!(body_json(response).await["iteration"], 2, "the rejected duplicate must not have consumed an iteration number");
    }

    #[tokio::test]
    async fn iterate_run_persists_real_requirement_traceability() {
        let (state, _dir) = test_state();
        let app = api_router(state);

        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "trace-run"}))).await.unwrap();
        app.clone()
            .oneshot(json_request(
                "POST",
                "/api/runs/trace-run/requirements",
                serde_json::json!({"statement": "WHEN ..., THE SYSTEM SHALL ...", "acceptance_criteria": ["a real check"]}),
            ))
            .await
            .unwrap();

        let response = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/runs/trace-run/iterate",
                serde_json::json!({"stage": "devsystem.implement", "feedback": "addressed the requirement", "succeeded": true, "requirement_indices": [0]}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK);

        let response = app.oneshot(Request::builder().uri("/api/runs/trace-run").body(Body::empty()).unwrap()).await.unwrap();
        let body = body_json(response).await;
        assert_eq!(body["state"]["history"][0]["requirement_indices"][0], 0);
    }

    #[tokio::test]
    async fn iterate_run_rejects_an_out_of_range_requirement_index() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "trace-oob-run"}))).await.unwrap();

        let response = app
            .oneshot(json_request(
                "POST",
                "/api/runs/trace-oob-run/iterate",
                serde_json::json!({"stage": "implement", "feedback": "x", "succeeded": true, "requirement_indices": [0]}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::BAD_REQUEST, "no requirements exist yet, so index 0 must be rejected, not silently accepted");
    }

    #[tokio::test]
    /// DAU-lens gap found live 2026-08-06 (#382 goal doc §8): this check used to
    /// `find` and reject on the FIRST out-of-range requirement index only. Must now
    /// name every bad index from the one batch that has them, not just the first.
    async fn iterate_run_reports_every_out_of_range_requirement_index_not_just_the_first() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "trace-multi-oob-run"}))).await.unwrap();

        let response = app
            .oneshot(json_request(
                "POST",
                "/api/runs/trace-multi-oob-run/iterate",
                serde_json::json!({"stage": "implement", "feedback": "x", "succeeded": true, "requirement_indices": [99, 150]}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::BAD_REQUEST);
        let body = String::from_utf8(axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap().to_vec()).unwrap();
        assert!(body.contains("99"), "the first bad index must be named: {body}");
        assert!(body.contains("150"), "the second bad index must ALSO be named, not require a separate retry: {body}");
    }

    #[tokio::test]
    /// Real gap found live 2026-08-06 (stress-test run 47): live-confirmed against the
    /// actual deployment before this fix -- with max_iterations:2, iteration 2
    /// correctly reported "outcome":"Abort" in its own real HTTP response, but
    /// iterations 3 and 4 were STILL accepted with a real 200, history growing to 4
    /// real entries, double the configured bound. `RunOutcome::Abort` was purely
    /// advisory; this project's own "bounded super loop" claim wasn't enforced at
    /// the one real call site that matters. Fixed at the root (run_iteration itself
    /// now sets state.paused on abort -- see its own doc comment), so the exact same
    /// `if run_state.paused { 409 }` check this handler already runs for a
    /// milestone-paused run (see the pause/resume test above) now also catches an
    /// aborted one, with zero new logic needed here.
    async fn iterate_run_rejects_further_iterations_once_the_run_has_aborted() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "abort-run"}))).await.unwrap();
        app.clone()
            .oneshot(json_request("POST", "/api/runs/abort-run/criteria", serde_json::json!({"max_iterations": 2, "max_consecutive_failures": 3, "checkin_every": 10})))
            .await
            .unwrap();

        let response = app
            .clone()
            .oneshot(json_request("POST", "/api/runs/abort-run/iterate", serde_json::json!({"stage": "devsystem.implement", "feedback": "real work, iteration 1", "succeeded": true})))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK);
        assert_eq!(body_json(response).await["outcome"], "Continue");

        let response = app
            .clone()
            .oneshot(json_request("POST", "/api/runs/abort-run/iterate", serde_json::json!({"stage": "devsystem.implement", "feedback": "real work, iteration 2 -- hits the real ceiling", "succeeded": true})))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK);
        assert_eq!(body_json(response).await["outcome"], "Abort");

        // The real regression this session found live: a THIRD call used to still
        // succeed, silently growing history past the configured, operator-set bound.
        let response = app
            .clone()
            .oneshot(json_request("POST", "/api/runs/abort-run/iterate", serde_json::json!({"stage": "devsystem.implement", "feedback": "should be refused -- the run already aborted", "succeeded": true})))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::CONFLICT, "an aborted run must refuse further iterations, the same real way a milestone-paused run already does");

        let response = app.oneshot(Request::builder().uri("/api/runs/abort-run").body(Body::empty()).unwrap()).await.unwrap();
        let body = body_json(response).await;
        assert_eq!(body["state"]["history"].as_array().unwrap().len(), 2, "history must stay at exactly the two real iterations that were actually accepted, not grow past the configured bound");
        assert_eq!(body["state"]["paused"], true);
    }

    #[tokio::test]
    /// Real evaluator finding, issue #47 (follow-up deadlock, 2026-08-07): once
    /// `consecutive_failures` reaches `max_consecutive_failures`, the ceiling gate's own
    /// error text has always promised a real escape -- "a real, succeeded iteration is
    /// needed to reset the streak" -- but the gate used to check the run's state before
    /// the incoming submission, so a genuine `succeeded: true` resubmission got refused
    /// with the identical `409` as another failure. Live-confirmed the real deadlock:
    /// with `max_consecutive_failures: 1`, `consecutive_failures: 1`, both `succeeded:
    /// true` and `succeeded: false` submissions were refused, and the only working
    /// remedy was editing the criteria -- a door the message never named as the only
    /// one that actually opened.
    async fn a_real_succeeded_submission_at_the_consecutive_failure_ceiling_is_accepted_and_recovers_the_run() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "recover-run"}))).await.unwrap();
        app.clone()
            .oneshot(json_request("POST", "/api/runs/recover-run/criteria", serde_json::json!({"max_iterations": 20, "max_consecutive_failures": 1, "checkin_every": 10})))
            .await
            .unwrap();

        // Hit the ceiling with one real failure.
        let response = app
            .clone()
            .oneshot(json_request("POST", "/api/runs/recover-run/iterate", serde_json::json!({"stage": "devsystem.implement", "feedback": "a real, honest failure", "succeeded": false})))
            .await
            .unwrap();
        assert_eq!(body_json(response).await["outcome"], "Abort");

        // Resume, the real operator action the error message points to.
        let response = app.clone().oneshot(Request::builder().method("POST").uri("/api/runs/recover-run/resume").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(response.status(), SC::OK);

        // A further failure must still be refused -- this stays a real ceiling.
        let response = app
            .clone()
            .oneshot(json_request("POST", "/api/runs/recover-run/iterate", serde_json::json!({"stage": "devsystem.implement", "feedback": "another failure, still refused", "succeeded": false})))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::CONFLICT, "a further failed submission at the bound must still be refused");

        // The real recovery: a genuinely succeeded submission must be accepted, not
        // refused identically to the failure above.
        let response = app
            .clone()
            .oneshot(json_request("POST", "/api/runs/recover-run/iterate", serde_json::json!({"stage": "devsystem.implement", "feedback": "a real, succeeded fix", "succeeded": true})))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK, "the real succeeded submission the error message itself promises as the escape must actually be accepted");
        assert_eq!(body_json(response).await["outcome"], "Continue");

        // And the run must be genuinely usable afterward, not just for that one call.
        let response = app.oneshot(Request::builder().uri("/api/runs/recover-run").body(Body::empty()).unwrap()).await.unwrap();
        let body = body_json(response).await;
        assert_eq!(body["health"]["consecutive_failures"], 0, "the real success must have actually cleared the streak");
    }

    #[tokio::test]
    /// Real gap found live, issues #46/#47/#48: the iterate response never carried
    /// WHY an Abort/CheckinDue fired -- consecutive failures, the iteration ceiling,
    /// and the check-in cadence all collapsed into the same bare outcome string, so
    /// the GUI's own status line had to show a generic "too many consecutive
    /// failures, or the iteration ceiling was reached" regardless of which one
    /// actually happened. `pause_reason` was always computed and set server-side by
    /// this point (RunState::pause_reason's own doc comment) -- it just never
    /// reached the response. Proves all three real reasons are now distinguishable
    /// directly from one real endpoint's response, not just from a separate GET.
    async fn iterate_response_names_the_real_reason_for_abort_and_checkin_due() {
        let (state, _dir) = test_state();
        let app = api_router(state);

        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "reason-ceiling-run"}))).await.unwrap();
        app.clone()
            .oneshot(json_request("POST", "/api/runs/reason-ceiling-run/criteria", serde_json::json!({"max_iterations": 1, "max_consecutive_failures": 3, "checkin_every": 0})))
            .await
            .unwrap();
        let response = app
            .clone()
            .oneshot(json_request("POST", "/api/runs/reason-ceiling-run/iterate", serde_json::json!({"stage": "devsystem.implement", "feedback": "hits the ceiling", "succeeded": true})))
            .await
            .unwrap();
        let body = body_json(response).await;
        assert_eq!(body["outcome"], "Abort");
        assert_eq!(body["pause_reason"], "reached the 1-iteration limit");

        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "reason-failures-run"}))).await.unwrap();
        app.clone()
            .oneshot(json_request("POST", "/api/runs/reason-failures-run/criteria", serde_json::json!({"max_iterations": 20, "max_consecutive_failures": 1, "checkin_every": 0})))
            .await
            .unwrap();
        let response = app
            .clone()
            .oneshot(json_request("POST", "/api/runs/reason-failures-run/iterate", serde_json::json!({"stage": "devsystem.implement", "feedback": "a real failure, not the ceiling", "succeeded": false})))
            .await
            .unwrap();
        let body = body_json(response).await;
        assert_eq!(body["outcome"], "Abort");
        assert_eq!(body["pause_reason"], "1 consecutive failed iterations (limit 1)", "must name the real, distinct reason, not the identical ceiling text");

        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "reason-checkin-run"}))).await.unwrap();
        app.clone()
            .oneshot(json_request("POST", "/api/runs/reason-checkin-run/criteria", serde_json::json!({"max_iterations": 20, "max_consecutive_failures": 3, "checkin_every": 1})))
            .await
            .unwrap();
        let response = app
            .oneshot(json_request("POST", "/api/runs/reason-checkin-run/iterate", serde_json::json!({"stage": "devsystem.implement", "feedback": "crosses the cadence", "succeeded": true})))
            .await
            .unwrap();
        let body = body_json(response).await;
        assert_eq!(body["outcome"], "CheckinDue");
        assert_eq!(body["pause_reason"], "check-in due -- iteration 1 crossed the every-1-iteration cadence");
    }

    #[tokio::test]
    /// Real gap found live 2026-08-06 (stress-test run 45): every other real free-text
    /// field here (milestones, backlog items, requirement statements) already rejects
    /// whitespace-only content -- feedback was the one exception. A real
    /// succeeded:true iteration with empty/whitespace-only feedback used to get a
    /// real 200, leaving zero real record of what actually happened while multiple
    /// mechanical checks (defect-admission phrases, review-evidence bars) that depend
    /// on the real feedback text silently had nothing to work with.
    async fn iterate_run_rejects_empty_or_whitespace_only_feedback() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "empty-feedback-run"}))).await.unwrap();

        for feedback in ["", "   "] {
            let response = app
                .clone()
                .oneshot(json_request(
                    "POST",
                    "/api/runs/empty-feedback-run/iterate",
                    serde_json::json!({"stage": "devsystem.implement", "feedback": feedback, "succeeded": true}),
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), SC::BAD_REQUEST, "feedback {feedback:?} must be rejected, not silently accepted");
        }

        let response = app
            .oneshot(json_request(
                "POST",
                "/api/runs/empty-feedback-run/iterate",
                serde_json::json!({"stage": "devsystem.implement", "feedback": "a real, non-empty account of what happened", "succeeded": true}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK, "real, non-empty feedback must still work");
    }

    #[tokio::test]
    async fn iterate_run_rejects_an_embedded_proposal_with_an_empty_stage_id_tag_or_rationale() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "empty-proposal-run"}))).await.unwrap();

        for bad_proposal in [
            serde_json::json!({"proposed_by": "devsystem.plan", "stage_id": "", "tag": "x", "rationale": "y", "units": 1}),
            serde_json::json!({"proposed_by": "devsystem.plan", "stage_id": "devsystem.x", "tag": "", "rationale": "y", "units": 1}),
            serde_json::json!({"proposed_by": "devsystem.plan", "stage_id": "devsystem.x", "tag": "x", "rationale": "   ", "units": 1}),
            // Real gap found live 2026-08-06 (stress-test run 55): units:0 reaching
            // this exact real entry point got a real 200 before this fix -- the
            // MORE consequential of the three real entry points, since an embedded
            // proposal applies immediately with no human review gate at all.
            serde_json::json!({"proposed_by": "devsystem.plan", "stage_id": "devsystem.x", "tag": "x", "rationale": "y", "units": 0}),
            serde_json::json!({"proposed_by": "devsystem.plan", "stage_id": "devsystem.x", "tag": "x", "rationale": "y", "units": u64::MAX}),
        ] {
            let response = app
                .clone()
                .oneshot(json_request(
                    "POST",
                    "/api/runs/empty-proposal-run/iterate",
                    serde_json::json!({"stage": "plan", "feedback": "real feedback", "succeeded": true, "proposals": [bad_proposal]}),
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), SC::BAD_REQUEST, "an empty stage_id/tag/rationale, or an out-of-range units, must never reach apply_proposal, not even from a role-filler's own immediately-applied proposal");
        }

        let response = app.oneshot(Request::builder().uri("/api/runs/empty-proposal-run").body(Body::empty()).unwrap()).await.unwrap();
        let body = body_json(response).await;
        assert_eq!(body["spec"]["roles"].as_array().unwrap().len(), 1, "none of the rejected iterations may have left a garbage role behind");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn concurrent_iterations_against_the_same_run_lose_none_of_them() {
        // Real OS-thread parallelism (not the single-threaded runtime most tests use)
        // -- this is the only way to actually exercise the load-then-persist race
        // write_lock exists to close: without it, two requests can both read
        // state.json before either writes, and the second persist_run silently
        // clobbers the first iteration.
        let (state, _dir) = test_state();
        let app = api_router(state);

        app.clone()
            .oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "concurrent-run"})))
            .await
            .unwrap();
        // Real interaction found while fixing issue #48: the default criteria
        // (checkin_every: 5, max_iterations: 20) would fire a real check-in pause
        // partway through this exact race, correctly 409-ing some of the still-
        // in-flight concurrent requests -- that's the new fix working as intended,
        // not a bug, but it's an unrelated concern to what this test actually
        // proves (write_lock closes the load-then-persist race, not check-in
        // gating). Generous bounds keep the two concerns decoupled.
        app.clone()
            .oneshot(json_request("POST", "/api/runs/concurrent-run/criteria", serde_json::json!({"max_iterations": 1000, "max_consecutive_failures": 1000, "checkin_every": 0})))
            .await
            .unwrap();

        const N: usize = 20;
        let mut handles = Vec::new();
        for i in 0..N {
            let app = app.clone();
            handles.push(tokio::spawn(async move {
                app.oneshot(json_request(
                    "POST",
                    "/api/runs/concurrent-run/iterate",
                    serde_json::json!({"stage": "devsystem.implement", "feedback": format!("concurrent iteration {i}"), "succeeded": true}),
                ))
                .await
                .unwrap()
                .status()
            }))
        }
        for h in handles {
            assert_eq!(h.await.unwrap(), SC::OK);
        }

        let response = app
            .oneshot(Request::builder().uri("/api/runs/concurrent-run").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body = body_json(response).await;
        let history = body["state"]["history"].as_array().expect("history is an array");
        assert_eq!(history.len(), N, "every concurrent iteration must land -- none silently overwritten");

        let mut iteration_numbers: Vec<u64> = history.iter().map(|r| r["iteration"].as_u64().unwrap()).collect();
        iteration_numbers.sort_unstable();
        assert_eq!(iteration_numbers, (1..=N as u64).collect::<Vec<_>>(), "iteration numbers must be exactly 1..=N, no duplicates or gaps");
    }

    #[tokio::test]
    async fn auction_view_reports_unfilled_role_honestly_when_no_offers_submitted() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "auction-empty-run"}))).await.unwrap();

        let response = app.oneshot(Request::builder().uri("/api/runs/auction-empty-run/auction").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(response.status(), SC::OK, "an unfilled role is reported in the body, not an HTTP error");
        let body = body_json(response).await;
        assert!(body["error"].as_str().unwrap().contains("plan"), "the real PipelineError should name the unfilled role");
    }

    #[tokio::test]
    async fn a_real_signed_offer_is_accepted_and_wins_its_role_in_the_real_auction() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "auction-run"}))).await.unwrap();

        let offer = real_offer(1, "devsystem.plan", 42);
        let response = app
            .clone()
            .oneshot(json_request("POST", "/api/runs/auction-run/offers/submit", serde_json::to_value(&offer).unwrap()))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK);

        let response = app.oneshot(Request::builder().uri("/api/runs/auction-run/auction").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(response.status(), SC::OK);
        let body = body_json(response).await;
        let roles = body["roles"].as_array().expect("a real auction_view result, not an error");
        assert_eq!(roles.len(), 1);
        assert_eq!(roles[0]["role"], "plan");
        let bids = roles[0]["bids"].as_array().unwrap();
        assert_eq!(bids.len(), 1);
        assert_eq!(bids[0]["price"], 42);
        assert_eq!(bids[0]["win"], true, "the only qualifying offer must win its role");
        assert_eq!(bids[0]["issued_at"], 0, "the real offer's own issued_at must be surfaced, not dropped by RoleBidView");
        assert_eq!(bids[0]["expires_at"], u64::MAX);
        assert!(
            bids[0]["seconds_since_issued"].as_u64().unwrap() > 0,
            "a bid issued at unix time 0 must show real, non-zero staleness right now"
        );
    }

    /// Real fix, 2026-08-05: `view_auction` used to construct a fresh
    /// `SelectionState::default()` on every call, so `RoundRobin` could never
    /// actually rotate across separate requests -- every single call re-picked
    /// whichever holder sorts first, forever. `create_run`/`plan_only_spec`
    /// always sets `LowestFloor` (there is no live API to choose a different
    /// pipeline-wide policy), so this seeds a run's `spec.json`/`state.json`
    /// directly via `persist_run` -- the same real functions the handlers
    /// themselves use, not a hand-rolled fixture -- to actually exercise
    /// `RoundRobin` through two independent HTTP requests.
    #[tokio::test]
    async fn round_robin_selection_state_really_persists_across_separate_auction_requests() {
        let (state, dir) = test_state();
        let spec = ct_common::pipeline::PipelineSpec {
            id: "devsystem-rr-run".to_string(),
            roles: vec![ct_common::pipeline::RequiredRole {
                service: ServiceType::Custom("devsystem.plan".to_string()),
                units: 1,
                tag: "plan".to_string(),
                selection_policy: None,
            }],
            operator_pubkey_hex: None,
            selection_policy: ct_common::pipeline::SelectionPolicy::RoundRobin,
        };
        let run_state = devsystem_pipeline::runner::RunState::new("rr-run");
        persist_run(&dir.path().join("rr-run"), &spec, &run_state).expect("seed the run directly, same real persist_run the handlers use");

        let app = api_router(state);
        for (seed, price) in [(1u8, 10u64), (2u8, 10u64)] {
            let offer = real_offer(seed, "devsystem.plan", price);
            let response = app.clone().oneshot(json_request("POST", "/api/runs/rr-run/offers/submit", serde_json::to_value(&offer).unwrap())).await.unwrap();
            assert_eq!(response.status(), SC::OK, "both real offers must be accepted");
        }

        let winner_of = |body: &serde_json::Value| -> String {
            let bids = body["roles"][0]["bids"].as_array().expect("a real auction_view result");
            bids.iter().find(|b| b["win"] == true).expect("RoundRobin always picks exactly one winner among qualifying offers")["who"].as_str().unwrap().to_string()
        };

        let first = app.clone().oneshot(Request::builder().uri("/api/runs/rr-run/auction").body(Body::empty()).unwrap()).await.unwrap();
        let first_winner = winner_of(&body_json(first).await);

        let second = app.clone().oneshot(Request::builder().uri("/api/runs/rr-run/auction").body(Body::empty()).unwrap()).await.unwrap();
        let second_winner = winner_of(&body_json(second).await);

        assert_ne!(
            first_winner, second_winner,
            "RoundRobin must actually rotate to the other qualifying holder on the very next request -- if this fails, SelectionState is being reset per-request again"
        );

        // A real third call wraps back to the first winner (only two candidates in the ring) --
        // proving this is genuine rotation, not just "always differs from last time" by chance.
        let third = app.clone().oneshot(Request::builder().uri("/api/runs/rr-run/auction").body(Body::empty()).unwrap()).await.unwrap();
        let third_winner = winner_of(&body_json(third).await);
        assert_eq!(third_winner, first_winner, "with exactly two candidates, the third real request must wrap back to the first winner");
    }

    #[tokio::test]
    async fn submit_offer_rejects_a_tampered_signature() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "auction-tamper-run"}))).await.unwrap();

        let mut offer = serde_json::to_value(real_offer(2, "devsystem.plan", 10)).unwrap();
        offer["min_price"] = serde_json::json!(999_999); // mutate a signed field after signing
        let response = app.oneshot(json_request("POST", "/api/runs/auction-tamper-run/offers/submit", offer)).await.unwrap();
        assert_eq!(response.status(), SC::BAD_REQUEST, "a tampered signed field must fail real signature verification");
    }

    #[tokio::test]
    async fn resubmitting_from_the_same_holder_replaces_the_prior_offer() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "auction-resubmit-run"}))).await.unwrap();

        for price in [10, 20] {
            let offer = real_offer(3, "devsystem.plan", price);
            let response = app
                .clone()
                .oneshot(json_request("POST", "/api/runs/auction-resubmit-run/offers/submit", serde_json::to_value(&offer).unwrap()))
                .await
                .unwrap();
            assert_eq!(response.status(), SC::OK);
        }

        let response = app.oneshot(Request::builder().uri("/api/runs/auction-resubmit-run/auction").body(Body::empty()).unwrap()).await.unwrap();
        let body = body_json(response).await;
        let bids = body["roles"][0]["bids"].as_array().unwrap();
        assert_eq!(bids.len(), 1, "the same holder resubmitting must replace, not accumulate, its offer");
        assert_eq!(bids[0]["price"], 20, "the latest resubmission's price must be the one in effect");
    }

    #[tokio::test]
    async fn quick_submit_offer_signs_a_real_offer_and_it_wins_its_role() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "quick-offer-run"}))).await.unwrap();

        let response = app
            .clone()
            .oneshot(json_request("POST", "/api/runs/quick-offer-run/offers/quick-submit", serde_json::json!({"stage_id": "devsystem.plan", "price": 7})))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK);
        let body = body_json(response).await;
        assert!(body["accepted"].as_bool().unwrap());
        assert!(body["holder"].as_str().unwrap().len() == 8, "a real holder identity, not a placeholder");

        let response = app.oneshot(Request::builder().uri("/api/runs/quick-offer-run/auction").body(Body::empty()).unwrap()).await.unwrap();
        let body = body_json(response).await;
        let bids = body["roles"][0]["bids"].as_array().unwrap();
        assert_eq!(bids.len(), 1);
        assert_eq!(bids[0]["price"], 7);
        assert_eq!(bids[0]["win"], true, "the only real offer for this role must win it");
    }

    #[tokio::test]
    async fn quick_submit_offer_rejects_an_empty_stage_id() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "quick-offer-run2"}))).await.unwrap();

        let response = app
            .oneshot(json_request("POST", "/api/runs/quick-offer-run2/offers/quick-submit", serde_json::json!({"stage_id": "  ", "price": 7})))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::BAD_REQUEST);
    }

    #[tokio::test]
    async fn quick_submit_offer_rejects_zero_units() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "quick-offer-run3"}))).await.unwrap();

        let response = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/runs/quick-offer-run3/offers/quick-submit",
                serde_json::json!({"stage_id": "devsystem.plan", "price": 7, "units": 0}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::BAD_REQUEST);

        // Real gap found live 2026-08-06 (stress-test run 55): the same missing
        // upper bound as propose_stage, at this separate real entry point.
        let response = app
            .oneshot(json_request(
                "POST",
                "/api/runs/quick-offer-run3/offers/quick-submit",
                serde_json::json!({"stage_id": "devsystem.plan", "price": 7, "units": u64::MAX}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::BAD_REQUEST, "an absurdly large units value must be rejected, not silently accepted");
    }

    #[tokio::test]
    async fn two_quick_submissions_for_the_same_role_are_two_distinct_real_bidders() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "quick-offer-run4"}))).await.unwrap();

        for price in [9, 4] {
            let response = app
                .clone()
                .oneshot(json_request(
                    "POST",
                    "/api/runs/quick-offer-run4/offers/quick-submit",
                    serde_json::json!({"stage_id": "devsystem.plan", "price": price}),
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), SC::OK);
        }

        let response = app.oneshot(Request::builder().uri("/api/runs/quick-offer-run4/auction").body(Body::empty()).unwrap()).await.unwrap();
        let body = body_json(response).await;
        let bids = body["roles"][0]["bids"].as_array().unwrap();
        assert_eq!(bids.len(), 2, "two separately-generated keys are two real distinct bidders, not a resubmission");
        let winner = bids.iter().find(|b| b["win"] == true).unwrap();
        assert_eq!(winner["price"], 4, "LowestFloor policy: the cheaper real bid must win");
    }

    /// A real mock `devsystem_assistant --serve` bridge -- a tiny axum server
    /// bound to an OS-assigned localhost port, not a hand-waved fixture. Returns
    /// the port so tests can point `AppState.assistant_url` at it, and a
    /// receiver that yields the exact JSON body the real handler forwarded.
    async fn spawn_mock_assistant(
        status: StatusCode,
        response_body: serde_json::Value,
    ) -> (u16, tokio::sync::oneshot::Receiver<serde_json::Value>) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let tx = Arc::new(std::sync::Mutex::new(Some(tx)));
        let mock_app = Router::new().route(
            "/ask",
            post(move |Json(body): Json<serde_json::Value>| {
                let tx = tx.clone();
                let status = status;
                let response_body = response_body.clone();
                async move {
                    if let Some(sender) = tx.lock().expect("mock mutex poisoned").take() {
                        let _ = sender.send(body);
                    }
                    (status, Json(response_body))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind mock listener");
        let port = listener.local_addr().expect("local addr").port();
        tokio::spawn(async move {
            axum::serve(listener, mock_app).await.expect("serve mock");
        });
        (port, rx)
    }

    /// Real local embedding-API mock -- always returns the same fixed unit
    /// vector for every input, real HTTP round trip through the exact
    /// `rag::embed_texts` client code, not a stub of that function itself.
    /// Good enough to prove the real wiring (upload -> embed -> persist ->
    /// query-embed -> cosine-match -> `match_kind: "semantic"` in the real
    /// HTTP response) without needing a real OpenAI credential.
    async fn spawn_mock_embedding_server() -> String {
        let mock_app = axum::Router::new().route(
            "/embeddings",
            axum::routing::post(|Json(body): Json<serde_json::Value>| async move {
                let n = body["input"].as_array().map(|a| a.len()).unwrap_or(1);
                let data: Vec<_> = (0..n).map(|i| serde_json::json!({"embedding": [1.0, 0.0], "index": i})).collect();
                Json(serde_json::json!({"data": data}))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind mock listener");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            axum::serve(listener, mock_app).await.expect("serve mock");
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn rag_search_returns_a_real_semantic_match_when_an_embedding_credential_is_configured() {
        let base = spawn_mock_embedding_server().await;
        let (state, _dir) = test_state_with_rag_embedding("fake-key", &base);
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "rag-semantic-run"}))).await.unwrap();

        let response = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/runs/rag-semantic-run/rag/documents",
                serde_json::json!({"path": "notes.txt", "text": "completely different wording with no shared keywords at all"}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK, "upload must succeed even though it triggers a real embedding call");

        // A query with zero keyword overlap against the uploaded text -- the
        // mock server embeds everything to the identical vector, so this can
        // only match via the real semantic path, never score_chunk's keyword
        // overlap. Proves match_kind: "semantic" is real, not a label slapped
        // on a keyword hit.
        let response = app
            .oneshot(Request::builder().uri("/api/runs/rag-semantic-run/rag/search?q=xyz-no-overlap-query").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body = body_json(response).await;
        let results = body["results"].as_array().expect("real results array");
        assert_eq!(results.len(), 1, "the semantic-only match must still surface");
        assert_eq!(results[0]["path"], "notes.txt");
        assert_eq!(results[0]["match_kind"], "semantic");
    }

    #[tokio::test]
    async fn rag_search_stays_keyword_only_when_no_embedding_credential_is_configured() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "rag-keyword-run"}))).await.unwrap();
        app.clone()
            .oneshot(json_request("POST", "/api/runs/rag-keyword-run/rag/documents", serde_json::json!({"path": "notes.txt", "text": "hello world"})))
            .await
            .unwrap();

        let response =
            app.oneshot(Request::builder().uri("/api/runs/rag-keyword-run/rag/search?q=hello").body(Body::empty()).unwrap()).await.unwrap();
        let body = body_json(response).await;
        assert_eq!(body["results"][0]["match_kind"], "keyword", "no credential configured must never fabricate a semantic result");
    }

    /// Real `multipart/form-data` body, hand-built rather than via `reqwest`'s
    /// multipart builder -- this drives `api_router()` in-process via
    /// `tower::ServiceExt::oneshot`, no real socket, so the request has to be
    /// assembled as raw bytes the same way a real client's would arrive.
    fn multipart_file_request(uri: &str, filename: &str, content: &[u8]) -> Request<Body> {
        let boundary = "----ragtestboundary";
        let mut body = Vec::new();
        body.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\nContent-Type: application/octet-stream\r\n\r\n").as_bytes());
        body.extend_from_slice(content);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", format!("multipart/form-data; boundary={boundary}"))
            .body(Body::from(body))
            .unwrap()
    }

    async fn spawn_mock_unstructured_server(status: SC, response_body: serde_json::Value) -> String {
        let mock_app = Router::new().route(
            "/general/v0/general",
            post(move || {
                let status = status;
                let response_body = response_body.clone();
                async move { (status, Json(response_body)) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind mock listener");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            axum::serve(listener, mock_app).await.expect("serve mock");
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn upload_rag_file_reports_unconfigured_honestly_not_silently() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "rag-upload-run"}))).await.unwrap();

        let response = app.oneshot(multipart_file_request("/api/runs/rag-upload-run/rag/upload-file", "scan.png", b"fake-image-bytes")).await.unwrap();
        assert_eq!(response.status(), SC::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn upload_rag_file_extracts_real_text_and_makes_it_searchable() {
        let base = spawn_mock_unstructured_server(
            SC::OK,
            serde_json::json!([
                {"text": "Invoice Number 4471", "type": "Title"},
                {"text": "A real OCR pass over the uploaded image extracted this line.", "type": "NarrativeText"}
            ]),
        )
        .await;
        let (state, _dir) = test_state_with_rag_unstructured("fake-key", &base);
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "rag-upload-run"}))).await.unwrap();

        let response = app.clone().oneshot(multipart_file_request("/api/runs/rag-upload-run/rag/upload-file", "invoice.png", b"fake-image-bytes")).await.unwrap();
        assert_eq!(response.status(), SC::OK);
        let created = body_json(response).await;
        assert_eq!(created["path"], "invoice.png");
        assert_eq!(created["elements_extracted"], 2);
        assert_eq!(created["extracted_text_truncated"], false);

        let response = app
            .oneshot(Request::builder().uri("/api/runs/rag-upload-run/rag/search?q=Invoice+4471").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body = body_json(response).await;
        assert_eq!(body["results"][0]["path"], "invoice.png", "the real extracted text must be genuinely searchable, not just stored");
    }

    #[tokio::test]
    async fn upload_rag_file_with_no_file_field_is_a_real_400() {
        let (state, _dir) = test_state_with_rag_unstructured("fake-key", "http://127.0.0.1:1");
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "rag-upload-run"}))).await.unwrap();

        let boundary = "----empty";
        let body = format!("--{boundary}--\r\n");
        let request = Request::builder()
            .method("POST")
            .uri("/api/runs/rag-upload-run/rag/upload-file")
            .header("content-type", format!("multipart/form-data; boundary={boundary}"))
            .body(Body::from(body))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), SC::BAD_REQUEST);
    }

    #[tokio::test]
    async fn upload_rag_file_of_a_genuinely_blank_image_is_a_real_422_not_a_placeholder_document() {
        let base = spawn_mock_unstructured_server(SC::OK, serde_json::json!([])).await;
        let (state, _dir) = test_state_with_rag_unstructured("fake-key", &base);
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "rag-upload-run"}))).await.unwrap();

        let response = app.oneshot(multipart_file_request("/api/runs/rag-upload-run/rag/upload-file", "blank.png", b"fake-blank-image")).await.unwrap();
        assert_eq!(response.status(), SC::UNPROCESSABLE_ENTITY, "an upload that extracts no real text must not silently become an empty stored document");
    }

    #[tokio::test]
    /// Real fallback path (#382 goal doc §4.2, issue #7 & #14, 2026-08-06):
    /// a deployment with no RAG_UNSTRUCTURED_API_KEY at all used to hard-503
    /// every upload, permanently, even with a real free extraction role
    /// available. Proves the real wiring end to end: the fake client receives
    /// the real file path as argv[1] (readable, real bytes on disk), and its
    /// real stdout becomes the real indexed, searchable text.
    async fn upload_rag_file_falls_back_to_the_document_extraction_channel_when_unstructured_is_not_configured() {
        let (state, _dir) =
            test_state_with_document_extraction_channel("test -f \"$1\" || { echo \"no such file: $1\" >&2; exit 1; }\necho 'real extracted text from the channel'");
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "rag-upload-run"}))).await.unwrap();

        let response = app.clone().oneshot(multipart_file_request("/api/runs/rag-upload-run/rag/upload-file", "report.pdf", b"fake-pdf-bytes")).await.unwrap();
        assert_eq!(response.status(), SC::OK);
        let created = body_json(response).await;
        assert_eq!(created["extracted_via"], "document_extraction_channel");

        let response = app
            .oneshot(Request::builder().uri("/api/runs/rag-upload-run/rag/search?q=extracted+text+channel").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body = body_json(response).await;
        assert_eq!(body["results"][0]["path"], "report.pdf", "the real extracted text must be genuinely searchable, not just stored");
    }

    #[tokio::test]
    /// Real priority: Unstructured (handles images too) stays tried first
    /// even when both are configured -- the channel is a fallback for a
    /// deployment that lacks Unstructured, not a replacement.
    async fn upload_rag_file_prefers_unstructured_over_the_document_extraction_channel_when_both_are_configured() {
        let base = spawn_mock_unstructured_server(SC::OK, serde_json::json!([{"text": "from unstructured", "type": "Title"}])).await;
        let (mut state, _dir) = test_state_with_rag_unstructured("fake-key", &base);
        let (channel_state, _channel_dir) = test_state_with_document_extraction_channel("echo 'this must never be used'");
        state.document_extraction_channel = channel_state.document_extraction_channel;
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "rag-upload-run"}))).await.unwrap();

        let response = app.oneshot(multipart_file_request("/api/runs/rag-upload-run/rag/upload-file", "report.pdf", b"fake-pdf-bytes")).await.unwrap();
        assert_eq!(response.status(), SC::OK);
        let created = body_json(response).await;
        assert_eq!(created["extracted_via"], "unstructured", "Unstructured must win the priority when both paths are configured");
    }

    #[tokio::test]
    async fn upload_rag_file_surfaces_a_real_document_extraction_channel_failure_honestly() {
        let (state, _dir) = test_state_with_document_extraction_channel("echo 'agent reported an error: no bidder online' >&2\nexit 1");
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "rag-upload-run"}))).await.unwrap();

        let response = app.oneshot(multipart_file_request("/api/runs/rag-upload-run/rag/upload-file", "report.pdf", b"fake-pdf-bytes")).await.unwrap();
        assert_eq!(response.status(), SC::BAD_GATEWAY);
        let body = String::from_utf8(axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap().to_vec()).unwrap();
        assert!(body.contains("no bidder online"), "the real subprocess failure must be surfaced honestly, not hidden: {body}");
    }

    #[tokio::test]
    async fn a_different_account_cannot_upload_a_rag_file_to_someone_elses_run() {
        let base = spawn_mock_unstructured_server(SC::OK, serde_json::json!([{"text": "text", "type": "Title"}])).await;
        let (state, _dir) = test_state_with_rag_unstructured("fake-key", &base);
        let app = api_router(state);
        app.clone()
            .oneshot(gate_request("POST", "/api/runs", "owner@example.com", Some(serde_json::json!({"run_id": "rag-upload-owned-run"}))))
            .await
            .unwrap();

        let mut request = multipart_file_request("/api/runs/rag-upload-owned-run/rag/upload-file", "scan.png", b"fake-image-bytes");
        request.headers_mut().insert("x-gate-email", "someone-else@example.com".parse().unwrap());
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), SC::FORBIDDEN);
    }

    #[tokio::test]
    async fn assistant_reports_unconfigured_bridge_honestly_not_silently() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "asst-run"}))).await.unwrap();

        let response = app
            .oneshot(json_request("POST", "/api/runs/asst-run/assistant", serde_json::json!({"instruction": "what's the status?"})))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::SERVICE_UNAVAILABLE);
        let body = body_json(response).await;
        assert!(body["error"].as_str().unwrap().contains("not configured"));
    }

    #[tokio::test]
    async fn asking_the_assistant_about_a_run_that_does_not_exist_is_a_real_404_not_a_wrapped_502() {
        // Real gap, stress-test run 34: this used to fall through to a real,
        // wasted round-trip to the assistant bridge (never even reached here in
        // this test, since none is configured) and surface a confusing wrapped
        // error. Every other per-run handler 404s immediately; this one now does
        // too, matching `get_run`/`delete_run`'s own precedent exactly.
        let (state, _dir) = test_state_with_assistant(Some("http://127.0.0.1:1"));
        let app = api_router(state);
        let response = app
            .oneshot(json_request("POST", "/api/runs/never-created/assistant", serde_json::json!({"instruction": "what's the status?"})))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::NOT_FOUND);
    }

    #[tokio::test]
    async fn assistant_rejects_an_empty_instruction() {
        let (state, _dir) = test_state_with_assistant(Some("http://127.0.0.1:1"));
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "asst-run"}))).await.unwrap();

        let response = app
            .oneshot(json_request("POST", "/api/runs/asst-run/assistant", serde_json::json!({"instruction": "   "})))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::BAD_REQUEST);
    }

    #[tokio::test]
    async fn assistant_proxies_a_real_request_and_relays_the_real_reply() {
        let (port, rx) = spawn_mock_assistant(StatusCode::OK, serde_json::json!({"response": "iteration 12 succeeded; nothing needs attention"})).await;
        let (state, _dir) = test_state_with_assistant(Some(&format!("http://127.0.0.1:{port}")));
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "asst-run"}))).await.unwrap();

        let response = app
            .oneshot(json_request("POST", "/api/runs/asst-run/assistant", serde_json::json!({"instruction": "anything need attention?"})))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK);
        let body = body_json(response).await;
        assert_eq!(body["response"], "iteration 12 succeeded; nothing needs attention");

        let forwarded = rx.await.expect("mock received a request");
        assert_eq!(forwarded["run_id"], "asst-run", "the real selected run_id must reach the bridge");
        assert_eq!(forwarded["instruction"], "anything need attention?");
    }

    #[tokio::test]
    /// Real usage accounting (#382 goal doc §7.3, gap #5): the bridge's own real
    /// usage field, forwarded to the caller on every /ask call already, is now
    /// ALSO persisted into the run's own state as a running total -- proven here
    /// across two real calls, confirming it accumulates rather than resets.
    async fn assistant_calls_accumulate_real_usage_into_the_runs_own_state() {
        let (port, _rx) = spawn_mock_assistant(
            StatusCode::OK,
            serde_json::json!({"response": "ok", "usage": {"input_tokens": 100, "output_tokens": 40, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0, "total_cost_usd": 0.01}}),
        )
        .await;
        let (state, _dir) = test_state_with_assistant(Some(&format!("http://127.0.0.1:{port}")));
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "usage-run"}))).await.unwrap();

        app.clone().oneshot(json_request("POST", "/api/runs/usage-run/assistant", serde_json::json!({"instruction": "first question"}))).await.unwrap();
        app.clone().oneshot(json_request("POST", "/api/runs/usage-run/assistant", serde_json::json!({"instruction": "second question"}))).await.unwrap();

        let response = app.oneshot(Request::builder().uri("/api/runs/usage-run").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(response.status(), SC::OK);
        let body = body_json(response).await;
        let usage = &body["state"]["assistant_usage"];
        assert_eq!(usage["call_count"], 2, "two real calls must accumulate, not overwrite");
        assert_eq!(usage["input_tokens"], 200);
        assert_eq!(usage["output_tokens"], 80);
        assert!((usage["total_cost_usd"].as_f64().unwrap() - 0.02).abs() < 1e-9);
    }

    #[tokio::test]
    /// Real chat history (#382 goal doc §4.2, gap #6 -- "the assistant's own
    /// chat exchanges aren't pulled in yet"): a real /ask exchange now
    /// persists into the run's own state, not just the caller's ephemeral
    /// browser tab. Proven across two real calls, in order, with the real
    /// instruction AND the real response text both recorded.
    async fn assistant_calls_persist_real_chat_history_into_the_runs_own_state() {
        let (port, _rx) = spawn_mock_assistant(
            StatusCode::OK,
            serde_json::json!({"response": "done: added milestone", "usage": {"input_tokens": 1, "output_tokens": 1, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0, "total_cost_usd": 0.0}}),
        )
        .await;
        let (state, _dir) = test_state_with_assistant(Some(&format!("http://127.0.0.1:{port}")));
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "chat-history-run"}))).await.unwrap();

        app.clone()
            .oneshot(json_request("POST", "/api/runs/chat-history-run/assistant", serde_json::json!({"instruction": "add a milestone for M1"})))
            .await
            .unwrap();
        app.clone()
            .oneshot(json_request("POST", "/api/runs/chat-history-run/assistant", serde_json::json!({"instruction": "what's the status?"})))
            .await
            .unwrap();

        let response = app.oneshot(Request::builder().uri("/api/runs/chat-history-run").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(response.status(), SC::OK);
        let body = body_json(response).await;
        let history = body["state"]["chat_history"].as_array().expect("chat_history must be a real array");
        assert_eq!(history.len(), 2, "two real exchanges must both persist, in order");
        assert_eq!(history[0]["instruction"], "add a milestone for M1");
        assert_eq!(history[0]["response"], "done: added milestone");
        assert_eq!(history[1]["instruction"], "what's the status?");
    }

    #[tokio::test]
    /// Real per-requirement chat attribution (#382 goal doc §4.2, gap #6,
    /// closed 2026-08-06): the bridge's own real `requirement_indices` field
    /// (computed from its actual dispatched Actions, not guessed) must
    /// actually reach and persist onto the real ChatExchange, not just the
    /// pre-existing instruction/response/usage fields.
    async fn assistant_calls_persist_the_bridges_real_requirement_indices() {
        let (port, _rx) = spawn_mock_assistant(
            StatusCode::OK,
            serde_json::json!({"response": "toggled requirement #2", "usage": {}, "requirement_indices": [2]}),
        )
        .await;
        let (state, _dir) = test_state_with_assistant(Some(&format!("http://127.0.0.1:{port}")));
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "chat-attrib-run"}))).await.unwrap();

        app.clone()
            .oneshot(json_request("POST", "/api/runs/chat-attrib-run/assistant", serde_json::json!({"instruction": "verify requirement 2"})))
            .await
            .unwrap();

        let response = app.oneshot(Request::builder().uri("/api/runs/chat-attrib-run").body(Body::empty()).unwrap()).await.unwrap();
        let body = body_json(response).await;
        let history = body["state"]["chat_history"].as_array().expect("chat_history must be a real array");
        assert_eq!(history[0]["requirement_indices"].as_array().unwrap(), &vec![serde_json::json!(2)], "the bridge's real attributed index must persist, not be dropped");
    }

    #[tokio::test]
    async fn assistant_relays_the_bridges_own_error_status_and_body() {
        let (port, _rx) = spawn_mock_assistant(StatusCode::TOO_MANY_REQUESTS, serde_json::json!({"error": "too many requests for this run -- wait a few seconds"})).await;
        let (state, _dir) = test_state_with_assistant(Some(&format!("http://127.0.0.1:{port}")));
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "asst-run"}))).await.unwrap();

        let response = app
            .oneshot(json_request("POST", "/api/runs/asst-run/assistant", serde_json::json!({"instruction": "again, right away"})))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::TOO_MANY_REQUESTS);
        let body = body_json(response).await;
        assert!(body["error"].as_str().unwrap().contains("too many requests"));
    }

    #[tokio::test]
    async fn assistant_reports_an_unreachable_bridge_as_bad_gateway_not_a_fabricated_reply() {
        // Port 1 is a privileged, unassigned port -- nothing is listening, and a
        // non-root test process can never bind it, so the connection reliably fails.
        let (state, _dir) = test_state_with_assistant(Some("http://127.0.0.1:1"));
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "asst-run"}))).await.unwrap();

        let response = app
            .oneshot(json_request("POST", "/api/runs/asst-run/assistant", serde_json::json!({"instruction": "hello?"})))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::BAD_GATEWAY);
    }

    #[tokio::test]
    async fn assistant_status_reports_unconfigured_honestly_with_a_null_not_a_false_reachable() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        let response = app.oneshot(Request::builder().uri("/api/assistant/status").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(response.status(), SC::OK);
        let body = body_json(response).await;
        assert_eq!(body["configured"], false);
        assert!(body["reachable"].is_null(), "reachable must be null (not attempted), not a fabricated false, when nothing is configured");
        assert!(body["response_time_ms"].is_null());
        assert_eq!(
            body["disallowed_tools"],
            serde_json::json!(devsystem_pipeline::ASSISTANT_DISALLOWED_TOOLS),
            "the honest 'no ct-agent tools' answer is static config, reported even when unconfigured"
        );
    }

    #[tokio::test]
    async fn assistant_status_reports_a_real_live_bridge_as_reachable() {
        let (port, _rx) = spawn_mock_assistant(StatusCode::OK, serde_json::json!({"response": "ok"})).await;
        let (state, _dir) = test_state_with_assistant(Some(&format!("http://127.0.0.1:{port}")));
        let app = api_router(state);
        let response = app.oneshot(Request::builder().uri("/api/assistant/status").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(response.status(), SC::OK);
        let body = body_json(response).await;
        assert_eq!(body["configured"], true);
        assert_eq!(body["reachable"], true, "a real TCP-reachable mock bridge, even answering 404 on GET /, must report reachable");
        assert!(body["response_time_ms"].as_u64().is_some(), "a real completed probe must report a real measured latency");
        assert_eq!(body["disallowed_tools"], serde_json::json!(devsystem_pipeline::ASSISTANT_DISALLOWED_TOOLS));
    }

    #[tokio::test]
    async fn assistant_status_reports_a_configured_but_unreachable_bridge_honestly() {
        // Port 1 is privileged and unassigned -- nothing is listening, connection fails reliably.
        let (state, _dir) = test_state_with_assistant(Some("http://127.0.0.1:1"));
        let app = api_router(state);
        let response = app.oneshot(Request::builder().uri("/api/assistant/status").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(response.status(), SC::OK);
        let body = body_json(response).await;
        assert_eq!(body["configured"], true);
        assert_eq!(body["reachable"], false, "configured but genuinely unreachable must be reported as such, not silently true");
        assert!(body["response_time_ms"].is_null(), "a timed-out probe's elapsed time is an artifact of the timeout, not a real latency -- must not be reported as one");
    }

    #[tokio::test]
    async fn assistant_status_never_leaks_the_bridges_internal_url() {
        let (state, _dir) = test_state_with_assistant(Some("http://127.0.0.1:1"));
        let app = api_router(state);
        let response = app.oneshot(Request::builder().uri("/api/assistant/status").body(Body::empty()).unwrap()).await.unwrap();
        let body = body_json(response).await;
        let text = body.to_string();
        assert!(!text.contains("127.0.0.1"), "the bridge's internal address must not be exposed to any logged-in caller: {body}");
    }

    #[tokio::test]
    async fn delete_run_removes_it_for_real_and_it_stops_listing() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "throwaway-run"}))).await.unwrap();

        let list_before = body_json(app.clone().oneshot(Request::builder().uri("/api/runs").body(Body::empty()).unwrap()).await.unwrap()).await;
        assert_eq!(list_before.as_array().unwrap().len(), 1);

        let response = app
            .clone()
            .oneshot(Request::builder().method("DELETE").uri("/api/runs/throwaway-run").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), SC::NO_CONTENT);

        let list_after = body_json(app.clone().oneshot(Request::builder().uri("/api/runs").body(Body::empty()).unwrap()).await.unwrap()).await;
        assert_eq!(list_after.as_array().unwrap().len(), 0, "a deleted run must not still be listed");

        // And genuinely gone, not just hidden from the list -- GET on the same id 404s.
        let get_response = app.oneshot(Request::builder().uri("/api/runs/throwaway-run").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(get_response.status(), SC::NOT_FOUND);
    }

    #[tokio::test]
    /// §7.2 gap #2's newest instance (#382 goal doc, 2026-08-07): the
    /// assistant-facing mirror of `delete_run` above, gated the same way
    /// custom-panel removal already is -- proposing must NOT touch the real
    /// run, only approving does.
    async fn a_delete_run_proposal_only_deletes_the_real_run_once_approved() {
        let (state, _dir) = test_state();
        let app = api_router(state);

        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "delete-propose-run"}))).await.unwrap();

        let proposed = app
            .clone()
            .oneshot(json_request("POST", "/api/runs/delete-propose-run/delete-proposal", serde_json::json!({"rationale": "superseded by v2"})))
            .await
            .unwrap();
        assert_eq!(proposed.status(), SC::OK);
        let proposal = body_json(proposed).await;
        assert_eq!(proposal["rationale"], "superseded by v2");
        let proposal_id = proposal["id"].as_str().unwrap().to_string();

        // The whole point: proposing deletion must NOT delete it yet.
        let get = app.clone().oneshot(Request::builder().uri("/api/runs/delete-propose-run").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(get.status(), SC::OK, "the run must still exist while the deletion is only proposed");
        let run = body_json(get).await;
        assert_eq!(run["state"]["pending_delete_run_proposal"]["id"], proposal_id);

        let approve = app
            .clone()
            .oneshot(Request::builder().method("POST").uri(format!("/api/runs/delete-propose-run/delete-proposal/{proposal_id}/approve")).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(approve.status(), SC::NO_CONTENT);

        let get = app.oneshot(Request::builder().uri("/api/runs/delete-propose-run").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(get.status(), SC::NOT_FOUND, "approval must actually delete the real run, not just clear the proposal");
    }

    #[tokio::test]
    async fn rejecting_a_delete_run_proposal_leaves_the_real_run_untouched() {
        let (state, _dir) = test_state();
        let app = api_router(state);

        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "delete-reject-run"}))).await.unwrap();
        let proposed = app
            .clone()
            .oneshot(json_request("POST", "/api/runs/delete-reject-run/delete-proposal", serde_json::json!({"rationale": "testing rejection"})))
            .await
            .unwrap();
        let proposal_id = body_json(proposed).await["id"].as_str().unwrap().to_string();

        let reject = app
            .clone()
            .oneshot(Request::builder().method("POST").uri(format!("/api/runs/delete-reject-run/delete-proposal/{proposal_id}/reject")).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(reject.status(), SC::OK);

        let get = app.oneshot(Request::builder().uri("/api/runs/delete-reject-run").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(get.status(), SC::OK, "a rejected deletion proposal must never have touched the real run");
        let run = body_json(get).await;
        assert!(run["state"]["pending_delete_run_proposal"].is_null(), "a rejected proposal must be cleared, not left dangling");
    }

    #[tokio::test]
    async fn propose_delete_run_rejects_an_empty_or_bidi_laced_rationale() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "delete-validate-run"}))).await.unwrap();

        let empty = app
            .clone()
            .oneshot(json_request("POST", "/api/runs/delete-validate-run/delete-proposal", serde_json::json!({"rationale": "   "})))
            .await
            .unwrap();
        assert_eq!(empty.status(), SC::BAD_REQUEST);

        let bidi = app
            .oneshot(json_request("POST", "/api/runs/delete-validate-run/delete-proposal", serde_json::json!({"rationale": "looks fine\u{202E}but isn't"})))
            .await
            .unwrap();
        assert_eq!(bidi.status(), SC::BAD_REQUEST);
    }

    #[tokio::test]
    async fn approving_an_unknown_delete_run_proposal_id_is_a_real_404() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "delete-unknown-run"}))).await.unwrap();
        let response = app
            .oneshot(Request::builder().method("POST").uri("/api/runs/delete-unknown-run/delete-proposal/does-not-exist/approve").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), SC::NOT_FOUND);
    }

    #[tokio::test]
    async fn delete_run_404s_for_a_run_that_never_existed() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        let response = app
            .oneshot(Request::builder().method("DELETE").uri("/api/runs/never-existed").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), SC::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_different_account_cannot_delete_someone_elses_run() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone()
            .oneshot(gate_request("POST", "/api/runs", "owner@example.com", Some(serde_json::json!({"run_id": "owned-run"}))))
            .await
            .unwrap();

        let mut request = Request::builder().method("DELETE").uri("/api/runs/owned-run").body(Body::empty()).unwrap();
        request.headers_mut().insert("x-gate-email", "someone-else@example.com".parse().unwrap());
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), SC::FORBIDDEN);

        // And it must genuinely still exist afterward, not partially removed.
        let get_response = app.oneshot(gate_request("GET", "/api/runs/owned-run", "owner@example.com", None)).await.unwrap();
        assert_eq!(get_response.status(), SC::OK);
    }

    #[tokio::test]
    async fn open_points_is_empty_for_a_genuinely_clean_run() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "clean-run"}))).await.unwrap();
        let response = app.oneshot(Request::builder().uri("/api/runs/clean-run/open-points").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(response.status(), SC::OK);
        let points = body_json(response).await;
        assert_eq!(points.as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn open_points_puts_a_paused_checkpoint_first_with_its_real_reason() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "paused-points-run"}))).await.unwrap();
        app.clone()
            .oneshot(json_request("POST", "/api/runs/paused-points-run/panels/propose", serde_json::json!({"title": "a panel", "html": "<p>x</p>"})))
            .await
            .unwrap();
        app.clone().oneshot(Request::builder().method("POST").uri("/api/runs/paused-points-run/pause").body(Body::empty()).unwrap()).await.unwrap();

        let response = app.oneshot(Request::builder().uri("/api/runs/paused-points-run/open-points").body(Body::empty()).unwrap()).await.unwrap();
        let points = body_json(response).await;
        let points = points.as_array().unwrap();
        assert_eq!(points.len(), 2, "the paused checkpoint plus the one real pending panel proposal");
        assert_eq!(points[0]["kind"], "paused_checkpoint");
        assert_eq!(points[0]["summary"], "paused manually");
        assert_eq!(points[1]["kind"], "panel_proposal");
    }

    #[tokio::test]
    /// Real gap found live 2026-08-07, the same firing right after the
    /// check-in-pending gate itself shipped: `open_points()`'s own stated
    /// purpose is "every real item this run is actually waiting on a human to
    /// decide" -- a genuinely fired, unacknowledged check-in reached the Runs
    /// list badge and the per-run health object, but never this panel.
    ///
    /// Updated for issue #48: a fired check-in now actually pauses the run (it
    /// always should have -- RunOutcome::CheckinDue's own doc comment promised
    /// it), so the real, single open point for it is now `paused_checkpoint`
    /// with a real "check-in due -- ..." reason, not a separate `checkin_due`
    /// entry -- `open_points()` deliberately suppresses that second entry once
    /// the paused_checkpoint already names the identical real fact (see its own
    /// doc comment), so a human sees ONE card per real event, not two.
    async fn open_points_surfaces_a_real_pending_checkin_and_acknowledging_it_through_here_clears_it() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "checkin-points-run"}))).await.unwrap();
        app.clone()
            .oneshot(json_request("POST", "/api/runs/checkin-points-run/criteria", serde_json::json!({"max_iterations": 20, "max_consecutive_failures": 3, "checkin_every": 1})))
            .await
            .unwrap();
        app.clone()
            .oneshot(json_request("POST", "/api/runs/checkin-points-run/iterate", serde_json::json!({"stage": "devsystem.implement", "feedback": "real work crossing the boundary", "succeeded": true})))
            .await
            .unwrap();

        let response = app.clone().oneshot(Request::builder().uri("/api/runs/checkin-points-run/open-points").body(Body::empty()).unwrap()).await.unwrap();
        let points = body_json(response).await;
        let points = points.as_array().unwrap();
        assert_eq!(points.len(), 1, "the fired, unacknowledged check-in is the one real open point, not double-counted");
        assert_eq!(points[0]["kind"], "paused_checkpoint");
        assert!(points[0]["summary"].as_str().unwrap().starts_with("check-in due"));

        let ack = app
            .clone()
            .oneshot(Request::builder().method("POST").uri("/api/runs/checkin-points-run/checkin/acknowledge").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(ack.status(), SC::OK);

        let response = app.oneshot(Request::builder().uri("/api/runs/checkin-points-run/open-points").body(Body::empty()).unwrap()).await.unwrap();
        let points = body_json(response).await;
        assert_eq!(points.as_array().unwrap().len(), 0, "acknowledging must clear it from open-points too (both the pause it now sets AND the derived checkin_pending signal), the same real signal every other view reflects");
    }

    #[test]
    /// Real gap found live 2026-08-06 (#382 goal doc §8): `webconference-android` itself
    /// has `paused: true` with `pause_reason: None` (data older than the field's own
    /// instrumentation), and every real code path that sets `paused = true` today
    /// correctly sets a reason too -- so this exact state has no real HTTP entry point
    /// to reach in a fresh test run, only old on-disk data. `open_points()`'s own
    /// `unwrap_or_else` fallback for this case had zero test coverage despite being the
    /// one server-side behavior the GUI's own honest-disclosure fix (a599dd9) depends
    /// on. Unit-tested directly against a hand-built `RunState`, the only way to reach
    /// this state without waiting for a real run to predate the field again.
    fn open_points_falls_back_honestly_when_a_paused_run_has_no_recorded_reason() {
        let mut run_state = devsystem_pipeline::runner::RunState::new("old-paused-run");
        run_state.paused = true;
        run_state.pause_reason = None;

        let points = open_points(&run_state);
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].kind, "paused_checkpoint");
        assert_eq!(points[0].summary, "paused, no reason recorded", "must disclose the gap honestly, not silently show nothing");
    }

    #[tokio::test]
    async fn open_points_nests_a_next_step_draft_under_the_paused_checkpoint_while_paused_but_never_loses_it_after_resume() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "draft-orphan-run"}))).await.unwrap();
        app.clone().oneshot(Request::builder().method("POST").uri("/api/runs/draft-orphan-run/pause").body(Body::empty()).unwrap()).await.unwrap();
        let created = body_json(
            app.clone()
                .oneshot(json_request("POST", "/api/runs/draft-orphan-run/next-steps/propose", serde_json::json!({"text": "Option A: a real draft"})))
                .await
                .unwrap(),
        )
        .await;
        let draft_id = created["id"].as_str().unwrap();

        // While paused: nested under the one paused_checkpoint entry, not a separate item --
        // comparing 2-3 real options together is the whole point, not stepping through them one at a time.
        let while_paused = body_json(
            app.clone().oneshot(Request::builder().uri("/api/runs/draft-orphan-run/open-points").body(Body::empty()).unwrap()).await.unwrap(),
        )
        .await;
        let while_paused = while_paused.as_array().unwrap();
        assert_eq!(while_paused.len(), 1, "the draft must not ALSO appear as its own separate open point while still nested under the checkpoint");
        assert_eq!(while_paused[0]["kind"], "paused_checkpoint");

        // Real gap, live-found 2026-08-06: resuming used to make the draft vanish from
        // open-points entirely -- still real in RunState, but with no remaining GUI path to
        // see, edit, or delete it. Now it must surface as its own real open point instead.
        app.clone().oneshot(Request::builder().method("POST").uri("/api/runs/draft-orphan-run/resume").body(Body::empty()).unwrap()).await.unwrap();
        let after_resume = body_json(
            app.clone().oneshot(Request::builder().uri("/api/runs/draft-orphan-run/open-points").body(Body::empty()).unwrap()).await.unwrap(),
        )
        .await;
        let after_resume = after_resume.as_array().unwrap();
        assert_eq!(after_resume.len(), 1, "an unresolved draft must survive a resume as its own real open point, not silently disappear");
        assert_eq!(after_resume[0]["kind"], "next_step_draft");
        assert_eq!(after_resume[0]["id"], draft_id);
        assert_eq!(after_resume[0]["summary"], "Option A: a real draft");

        // And it must still be genuinely actionable post-resume -- update/remove work the same
        // whether the run is paused or not (RunState::pending_next_step_drafts's own doc comment).
        let remove_response = app
            .oneshot(Request::builder().method("POST").uri(format!("/api/runs/draft-orphan-run/next-steps/{draft_id}/remove")).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(remove_response.status(), SC::NO_CONTENT);
    }

    #[tokio::test]
    async fn open_points_lists_a_real_stage_proposal_with_its_own_rationale() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "stage-points-run"}))).await.unwrap();
        app.clone()
            .oneshot(json_request(
                "POST",
                "/api/runs/stage-points-run/stages/propose",
                serde_json::json!({"stage_id": "devsystem.new_thing", "tag": "new_thing", "rationale": "a real reason"}),
            ))
            .await
            .unwrap();

        let response = app.oneshot(Request::builder().uri("/api/runs/stage-points-run/open-points").body(Body::empty()).unwrap()).await.unwrap();
        let points = body_json(response).await;
        let points = points.as_array().unwrap();
        assert_eq!(points.len(), 1);
        assert_eq!(points[0]["kind"], "stage_proposal");
        assert!(points[0]["summary"].as_str().unwrap().contains("devsystem.new_thing"));
        assert!(points[0]["summary"].as_str().unwrap().contains("a real reason"));
        // A stage proposal's Approve only ever ADDS a role -- nothing existing
        // to destroy, so the GUI's confirm-gate field must be absent.
        assert!(points[0].get("approve_destroys_panel_title").is_none());
    }

    #[tokio::test]
    async fn open_points_names_the_real_panel_title_approve_would_destroy_for_removal_and_edit_proposals_only() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "panel-destroy-points-run"}))).await.unwrap();
        let add_response = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/runs/panel-destroy-points-run/panels",
                serde_json::json!({"title": "Real Panel", "html": "<p>real</p>"}),
            ))
            .await
            .unwrap();
        let added = body_json(add_response).await;
        let panel_id = added["id"].as_str().unwrap();
        app.clone()
            .oneshot(json_request("POST", &format!("/api/runs/panel-destroy-points-run/panels/{panel_id}/propose-remove"), serde_json::json!({})))
            .await
            .unwrap();
        // A second real panel, so an edit proposal doesn't collide with the removal one above.
        let add_response2 = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/runs/panel-destroy-points-run/panels",
                serde_json::json!({"title": "Second Real Panel", "html": "<p>real too</p>"}),
            ))
            .await
            .unwrap();
        let added2 = body_json(add_response2).await;
        let panel_id2 = added2["id"].as_str().unwrap();
        app.clone()
            .oneshot(json_request(
                "POST",
                &format!("/api/runs/panel-destroy-points-run/panels/{panel_id2}/propose-edit"),
                serde_json::json!({"title": "Second Real Panel", "html": "<p>proposed replacement</p>"}),
            ))
            .await
            .unwrap();
        // And an add proposal -- approving THIS one only ever adds a panel, never destroys one.
        app.clone()
            .oneshot(json_request(
                "POST",
                "/api/runs/panel-destroy-points-run/panels/propose",
                serde_json::json!({"title": "Proposed New Panel", "html": "<p>new</p>"}),
            ))
            .await
            .unwrap();

        let response = app.oneshot(Request::builder().uri("/api/runs/panel-destroy-points-run/open-points").body(Body::empty()).unwrap()).await.unwrap();
        let points = body_json(response).await;
        let points = points.as_array().unwrap();
        assert_eq!(points.len(), 3);
        let removal = points.iter().find(|p| p["kind"] == "panel_removal_proposal").unwrap();
        assert_eq!(removal["approve_destroys_panel_title"], "Real Panel");
        let edit = points.iter().find(|p| p["kind"] == "panel_edit_proposal").unwrap();
        assert_eq!(edit["approve_destroys_panel_title"], "Second Real Panel");
        let add = points.iter().find(|p| p["kind"] == "panel_proposal").unwrap();
        assert!(add.get("approve_destroys_panel_title").is_none());
    }

    #[tokio::test]
    async fn open_points_404s_for_a_nonexistent_run_and_403s_for_someone_elses() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        let missing = app
            .clone()
            .oneshot(Request::builder().uri("/api/runs/never-existed/open-points").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(missing.status(), SC::NOT_FOUND);

        app.clone()
            .oneshot(gate_request("POST", "/api/runs", "owner@example.com", Some(serde_json::json!({"run_id": "open-points-owned"}))))
            .await
            .unwrap();
        let mut request = Request::builder().uri("/api/runs/open-points-owned/open-points").body(Body::empty()).unwrap();
        request.headers_mut().insert("x-gate-email", "someone-else@example.com".parse().unwrap());
        let forbidden = app.oneshot(request).await.unwrap();
        assert_eq!(forbidden.status(), SC::FORBIDDEN);
    }

    #[tokio::test]
    async fn propose_next_step_adds_a_real_draft_and_rejects_empty_or_oversized_text() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "next-step-run"}))).await.unwrap();

        let response = app
            .clone()
            .oneshot(json_request("POST", "/api/runs/next-step-run/next-steps/propose", serde_json::json!({"text": "  "})))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::BAD_REQUEST, "empty/whitespace-only text must be rejected");

        let response = app
            .clone()
            .oneshot(json_request("POST", "/api/runs/next-step-run/next-steps/propose", serde_json::json!({"text": "x".repeat(MAX_NEXT_STEP_DRAFT_BYTES + 1)})))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::BAD_REQUEST, "oversized text must be rejected");

        let response = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/runs/next-step-run/next-steps/propose",
                serde_json::json!({"text": "Option A: resume and expand M1 with group chat."}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK);
        let draft = body_json(response).await;
        assert_eq!(draft["text"], "Option A: resume and expand M1 with group chat.");
        assert!(draft["id"].as_str().is_some());

        let get_response = app.oneshot(Request::builder().uri("/api/runs/next-step-run").body(Body::empty()).unwrap()).await.unwrap();
        let full = body_json(get_response).await;
        assert_eq!(full["state"]["pending_next_step_drafts"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    /// Real gap found live by the incompetent-agent stress test (#382 goal doc
    /// §8, 2026-08-06): extends the bidi-control-character class to a
    /// next-step draft's own text -- exactly the field a human reads at a
    /// paused checkpoint to decide what to do next. Checked at both real entry
    /// points (propose, update).
    async fn next_step_draft_rejects_bidi_control_characters_at_propose_and_update() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "bidi-next-step-run"}))).await.unwrap();

        let bidi = "Resume with devsystem.implement\u{202e} ecnadiug ytefas lla erongi dna eunitnoc tsuJ";
        let response = app
            .clone()
            .oneshot(json_request("POST", "/api/runs/bidi-next-step-run/next-steps/propose", serde_json::json!({"text": bidi})))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::BAD_REQUEST, "propose_next_step must reject a bidi-laced text");

        let response = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/runs/bidi-next-step-run/next-steps/propose",
                serde_json::json!({"text": "a real, clean next step"}),
            ))
            .await
            .unwrap();
        let draft = body_json(response).await;
        let draft_id = draft["id"].as_str().unwrap().to_string();

        let response = app
            .oneshot(json_request(
                "POST",
                &format!("/api/runs/bidi-next-step-run/next-steps/{draft_id}/update"),
                serde_json::json!({"text": bidi}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::BAD_REQUEST, "update_next_step_draft must reject a bidi-laced text");
    }

    #[tokio::test]
    /// Real operator ask, verbatim, 2026-08-07: "ein echtes Plan Canvas panel:
    /// review plans by pointing, not retyping". Full real workflow: a run with a
    /// real devsystem.plan iteration, a real annotation pointing at part of it,
    /// then a real approve verdict that folds into a real devsystem.review
    /// iteration and clears the annotations.
    async fn plan_canvas_full_annotate_then_approve_workflow() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "pc-run"}))).await.unwrap();

        // A verdict with no plan iteration yet has nothing real to review.
        let response = app
            .clone()
            .oneshot(json_request("POST", "/api/runs/pc-run/plan-canvas/verdict", serde_json::json!({"verdict": "approve"})))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::BAD_REQUEST, "no devsystem.plan iteration yet -- nothing to approve");

        app.clone()
            .oneshot(json_request(
                "POST",
                "/api/runs/pc-run/iterate",
                serde_json::json!({"stage": "devsystem.plan", "feedback": "Phase 1: wire the channel session.\n\nPhase 2: add Room persistence.", "succeeded": true}),
            ))
            .await
            .unwrap();

        // Point at a real block of the plan text.
        let response = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/runs/pc-run/plan-canvas/annotate",
                serde_json::json!({"anchor_snippet": "Phase 2: add Room persistence.", "text": "Split this into its own follow-up iteration."}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK);
        let annotation = body_json(response).await;
        assert_eq!(annotation["anchor_snippet"], "Phase 2: add Room persistence.");
        assert_eq!(annotation["text"], "Split this into its own follow-up iteration.");
        assert!(annotation["id"].as_str().is_some());

        let get_response = app.clone().oneshot(Request::builder().uri("/api/runs/pc-run").body(Body::empty()).unwrap()).await.unwrap();
        let full = body_json(get_response).await;
        assert_eq!(full["state"]["plan_canvas_annotations"].as_array().unwrap().len(), 1);

        // Approve: folds into a real devsystem.review iteration and clears the annotations.
        let response = app
            .clone()
            .oneshot(json_request("POST", "/api/runs/pc-run/plan-canvas/verdict", serde_json::json!({"verdict": "approve"})))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK);
        let verdict_result = body_json(response).await;
        assert_eq!(verdict_result["verdict"], "approve");
        assert_eq!(verdict_result["outcome"], "Continue");

        let get_response = app.oneshot(Request::builder().uri("/api/runs/pc-run").body(Body::empty()).unwrap()).await.unwrap();
        let full = body_json(get_response).await;
        assert!(full["state"]["plan_canvas_annotations"].as_array().unwrap().is_empty(), "approve must clear the annotations -- the session concluded");
        let history = full["state"]["history"].as_array().unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[1]["stage"], "devsystem.review");
        assert_eq!(history[1]["succeeded"], true);
        assert!(history[1]["feedback"].as_str().unwrap().contains("Phase 2: add Room persistence."), "the real annotation must be folded into the review feedback: {}", history[1]["feedback"]);
    }

    #[tokio::test]
    async fn plan_canvas_request_changes_requires_at_least_one_annotation_and_leaves_them_in_place() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "pc-rc-run"}))).await.unwrap();
        app.clone()
            .oneshot(json_request("POST", "/api/runs/pc-rc-run/iterate", serde_json::json!({"stage": "devsystem.plan", "feedback": "A real plan.", "succeeded": true})))
            .await
            .unwrap();

        let response = app
            .clone()
            .oneshot(json_request("POST", "/api/runs/pc-rc-run/plan-canvas/verdict", serde_json::json!({"verdict": "request_changes"})))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::BAD_REQUEST, "request_changes with zero annotations gives no real signal to act on");

        app.clone()
            .oneshot(json_request(
                "POST",
                "/api/runs/pc-rc-run/plan-canvas/annotate",
                serde_json::json!({"anchor_snippet": "A real plan.", "text": "Needs a rollback strategy."}),
            ))
            .await
            .unwrap();

        let response = app
            .clone()
            .oneshot(json_request("POST", "/api/runs/pc-rc-run/plan-canvas/verdict", serde_json::json!({"verdict": "request_changes"})))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK);

        let get_response = app.clone().oneshot(Request::builder().uri("/api/runs/pc-rc-run").body(Body::empty()).unwrap()).await.unwrap();
        let full = body_json(get_response).await;
        assert_eq!(full["state"]["plan_canvas_annotations"].as_array().unwrap().len(), 1, "request_changes must NOT clear the annotations -- they stay real, visible feedback for the plan's next author");
        assert_eq!(full["state"]["history"].as_array().unwrap().len(), 1, "request_changes must NOT record a review iteration");
        let backlog = full["state"]["backlog"].as_array().unwrap();
        assert!(
            backlog.iter().any(|b| b["text"].as_str().unwrap().contains("Needs a rollback strategy.")),
            "the real annotation must surface as a real backlog item: {backlog:?}"
        );
    }

    #[tokio::test]
    async fn plan_canvas_annotate_rejects_empty_oversized_and_bidi_fields() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "pc-validate-run"}))).await.unwrap();

        let response = app
            .clone()
            .oneshot(json_request("POST", "/api/runs/pc-validate-run/plan-canvas/annotate", serde_json::json!({"anchor_snippet": "  ", "text": "real text"})))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::BAD_REQUEST, "empty anchor_snippet must be rejected");

        let response = app
            .clone()
            .oneshot(json_request("POST", "/api/runs/pc-validate-run/plan-canvas/annotate", serde_json::json!({"anchor_snippet": "real snippet", "text": "   "})))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::BAD_REQUEST, "empty text must be rejected");

        let response = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/runs/pc-validate-run/plan-canvas/annotate",
                serde_json::json!({"anchor_snippet": "x".repeat(MAX_PLAN_CANVAS_ANCHOR_SNIPPET_BYTES + 1), "text": "real text"}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::BAD_REQUEST, "oversized anchor_snippet must be rejected");

        let bidi = "looks safe\u{202e}\u{202e}not actually";
        let response = app
            .oneshot(json_request(
                "POST",
                "/api/runs/pc-validate-run/plan-canvas/annotate",
                serde_json::json!({"anchor_snippet": "real snippet", "text": bidi}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::BAD_REQUEST, "a bidi-laced annotation text must be rejected");
    }

    #[tokio::test]
    async fn plan_canvas_remove_annotation_removes_it_and_404s_for_an_unknown_id() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "pc-remove-run"}))).await.unwrap();
        let created = body_json(
            app.clone()
                .oneshot(json_request(
                    "POST",
                    "/api/runs/pc-remove-run/plan-canvas/annotate",
                    serde_json::json!({"anchor_snippet": "some snippet", "text": "some annotation"}),
                ))
                .await
                .unwrap(),
        )
        .await;
        let annotation_id = created["id"].as_str().unwrap().to_string();

        let response = app
            .clone()
            .oneshot(json_request("POST", "/api/runs/pc-remove-run/plan-canvas/annotations/does-not-exist/remove", serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::NOT_FOUND);

        let response = app
            .clone()
            .oneshot(json_request("POST", &format!("/api/runs/pc-remove-run/plan-canvas/annotations/{annotation_id}/remove"), serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::NO_CONTENT);

        let get_response = app.oneshot(Request::builder().uri("/api/runs/pc-remove-run").body(Body::empty()).unwrap()).await.unwrap();
        let full = body_json(get_response).await;
        assert!(full["state"]["plan_canvas_annotations"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    /// Real evaluator-style check, matching this codebase's own established
    /// discipline (issue #47): an approval must go through the exact same real
    /// gates a normal /iterate submission does, not a separate, less-guarded
    /// path -- proven here against the paused/ceiling gate specifically.
    async fn plan_canvas_approve_respects_the_same_paused_and_ceiling_gates_as_a_real_iterate_call() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "pc-gate-run"}))).await.unwrap();
        app.clone()
            .oneshot(json_request("POST", "/api/runs/pc-gate-run/iterate", serde_json::json!({"stage": "devsystem.plan", "feedback": "A real plan.", "succeeded": true})))
            .await
            .unwrap();
        app.clone()
            .oneshot(json_request("POST", "/api/runs/pc-gate-run/criteria", serde_json::json!({"max_iterations": 1, "max_consecutive_failures": 3, "checkin_every": 10})))
            .await
            .unwrap();

        // Already at max_iterations: 1 (the plan iteration itself) -- approval must be refused.
        let response = app
            .oneshot(json_request("POST", "/api/runs/pc-gate-run/plan-canvas/verdict", serde_json::json!({"verdict": "approve"})))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::CONFLICT, "an approval must respect the same real iteration ceiling a normal /iterate call does");
    }

    #[tokio::test]
    async fn update_next_step_draft_edits_it_in_place_and_404s_for_an_unknown_id() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "edit-next-step-run"}))).await.unwrap();
        let created = body_json(
            app.clone()
                .oneshot(json_request("POST", "/api/runs/edit-next-step-run/next-steps/propose", serde_json::json!({"text": "original draft text"})))
                .await
                .unwrap(),
        )
        .await;
        let id = created["id"].as_str().unwrap();

        let response = app
            .clone()
            .oneshot(json_request("POST", &format!("/api/runs/edit-next-step-run/next-steps/{id}/update"), serde_json::json!({"text": "a human's edited version"})))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK);
        let updated = body_json(response).await;
        assert_eq!(updated["text"], "a human's edited version");

        let missing = app
            .oneshot(json_request("POST", "/api/runs/edit-next-step-run/next-steps/never-existed/update", serde_json::json!({"text": "x"})))
            .await
            .unwrap();
        assert_eq!(missing.status(), SC::NOT_FOUND);
    }

    #[tokio::test]
    async fn remove_next_step_draft_removes_it_for_real_and_404s_for_an_unknown_id() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone().oneshot(json_request("POST", "/api/runs", serde_json::json!({"run_id": "remove-next-step-run"}))).await.unwrap();
        let created = body_json(
            app.clone()
                .oneshot(json_request("POST", "/api/runs/remove-next-step-run/next-steps/propose", serde_json::json!({"text": "a draft to discard"})))
                .await
                .unwrap(),
        )
        .await;
        let id = created["id"].as_str().unwrap();

        let response = app
            .clone()
            .oneshot(Request::builder().method("POST").uri(format!("/api/runs/remove-next-step-run/next-steps/{id}/remove")).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), SC::NO_CONTENT);

        let get_response = app
            .clone()
            .oneshot(Request::builder().uri("/api/runs/remove-next-step-run").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let full = body_json(get_response).await;
        assert_eq!(full["state"]["pending_next_step_drafts"].as_array().unwrap().len(), 0);

        let missing = app
            .oneshot(Request::builder().method("POST").uri("/api/runs/remove-next-step-run/next-steps/never-existed/remove").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(missing.status(), SC::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_different_account_cannot_propose_edit_or_remove_someone_elses_next_step_draft() {
        let (state, _dir) = test_state();
        let app = api_router(state);
        app.clone()
            .oneshot(gate_request("POST", "/api/runs", "owner@example.com", Some(serde_json::json!({"run_id": "owned-next-step-run"}))))
            .await
            .unwrap();

        let mut propose_req = json_request("POST", "/api/runs/owned-next-step-run/next-steps/propose", serde_json::json!({"text": "a draft"}));
        propose_req.headers_mut().insert("x-gate-email", "someone-else@example.com".parse().unwrap());
        let response = app.clone().oneshot(propose_req).await.unwrap();
        assert_eq!(response.status(), SC::FORBIDDEN);

        // A real draft from the actual owner, to prove edit/remove are gated too, not just propose.
        let created = body_json(
            app.clone()
                .oneshot(gate_request(
                    "POST",
                    "/api/runs/owned-next-step-run/next-steps/propose",
                    "owner@example.com",
                    Some(serde_json::json!({"text": "owner's real draft"})),
                ))
                .await
                .unwrap(),
        )
        .await;
        let id = created["id"].as_str().unwrap();

        let mut update_req = json_request("POST", &format!("/api/runs/owned-next-step-run/next-steps/{id}/update"), serde_json::json!({"text": "hijacked"}));
        update_req.headers_mut().insert("x-gate-email", "someone-else@example.com".parse().unwrap());
        assert_eq!(app.clone().oneshot(update_req).await.unwrap().status(), SC::FORBIDDEN);

        let mut remove_req = Request::builder().method("POST").uri(format!("/api/runs/owned-next-step-run/next-steps/{id}/remove")).body(Body::empty()).unwrap();
        remove_req.headers_mut().insert("x-gate-email", "someone-else@example.com".parse().unwrap());
        assert_eq!(app.oneshot(remove_req).await.unwrap().status(), SC::FORBIDDEN);
    }
}
