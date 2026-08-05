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
use devsystem_pipeline::preflight::preflight_annotations;
use devsystem_pipeline::runner::{
    load_or_init_run, persist_run, render_requirements_markdown, run_iteration, toggle_acceptance_criterion, toggle_milestone, toggle_requirement,
    toggle_requirement_auto_judge, BacklogItem, CustomPanel,
    Milestone, PendingIssueProposal, PendingPanelProposal, PendingStageProposal, Requirement, RoleFillMode, RunOutcome,
};
use devsystem_pipeline::{apply_proposal, AbortCriteria, IterationRecord, ProposalOutcome, StageProposal};
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
        .route("/api/runs/{id}", get(get_run))
        .route("/api/runs/{id}/iterate", post(iterate_run))
        .route("/api/runs/{id}/checkin", get(checkin_run))
        .route("/api/runs/{id}/criteria", post(update_criteria))
        .route("/api/runs/{id}/pause", post(pause_run))
        .route("/api/runs/{id}/resume", post(resume_run))
        .route("/api/runs/{id}/memory", get(memory_run))
        .route("/api/runs/{id}/memory/{index}/govern", post(govern_memory))
        .route("/api/runs/{id}/backlog", post(add_backlog_item))
        .route("/api/runs/{id}/backlog/{index}/toggle", post(toggle_backlog_item))
        .route("/api/runs/{id}/milestones", post(add_milestone))
        .route("/api/runs/{id}/milestones/{index}/toggle", post(toggle_milestone_handler))
        .route("/api/runs/{id}/requirements", post(add_requirement))
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
        .route("/api/runs/{id}/panels/propose", post(propose_custom_panel))
        .route("/api/runs/{id}/panels/proposals/{proposal_id}/approve", post(approve_panel_proposal))
        .route("/api/runs/{id}/panels/proposals/{proposal_id}/reject", post(reject_panel_proposal))
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
        .with_state(state)
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

/// Real path-traversal guard: `create_run` was the only handler that ever validated
/// `id`'s charset. Every other handler (`get_run`, `iterate_run`, `checkin_run`,
/// `memory_run`, `govern_memory`, `update_criteria`) took `id` straight from the URL
/// path and fed it into `runs_dir.join(id)` unvalidated -- `PathBuf::join` honors
/// `..` components, and axum's `{id}` path segment happily captures a literal `..`
/// (proven directly: `GET /api/runs/..` returned 200 with a `state.json` planted
/// outside `runs_dir`, in its parent directory, before this fix). Every handler that
/// touches the filesystem now calls this first, and `run_dir`/`run_exists` assert on
/// it too as a defense-in-depth backstop against a future call site that forgets.
fn valid_run_id(id: &str) -> bool {
    !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
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
    owner_email: Option<String>,
}

/// True when a run is close enough to its own bound that a human should notice it
/// before opening the run, not just after -- the same danger/warn thresholds the
/// GUI's health panel already uses -- OR a real proposal is genuinely waiting on a
/// human decision (`pending_reviews`), evaluated once here so the run list can
/// surface either case (matches the stalled-stage badge precedent: proactive, not
/// only-on-click).
fn needs_attention(health: &RunHealth, pending_reviews: usize) -> bool {
    pending_reviews > 0 || health.consecutive_failures + 1 >= health.criteria.max_consecutive_failures || health.iterations_until_checkin <= 1
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
            let risk_count = preflight_annotations(&run_state).len();
            let health = run_health(&run_state);
            let pending_reviews = run_state.pending_panel_proposals.len() + run_state.pending_stage_proposals.len() + run_state.pending_issue_proposals.len();
            let alert = needs_attention(&health, pending_reviews);
            let paused = run_state.paused;
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
                owner_email,
            });
        }
    }
    runs.sort_by(|a, b| a.run_id.cmp(&b.run_id));
    Json(runs).into_response()
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
            let risks = preflight_annotations(&run_state);
            Json(serde_json::json!({
                "spec": spec,
                "state": run_state,
                "stalled_stages": stalled,
                "health": health,
                "risks": risks,
            }))
            .into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response(),
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
}

fn run_health(run_state: &devsystem_pipeline::runner::RunState) -> RunHealth {
    let criteria = run_state.criteria;
    let completed = run_state.history.len() as u32;
    let until_checkin = if criteria.checkin_every == 0 {
        0
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
    if let Some(&bad) = body.requirement_indices.iter().find(|&&i| i >= run_state.requirements.len()) {
        return (StatusCode::BAD_REQUEST, format!("requirement_indices references index {bad}, but state.requirements only has {} entries", run_state.requirements.len()))
            .into_response();
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
    if let Some(last) = run_state.history.last() {
        if last.stage == body.stage
            && last.feedback == body.feedback
            && last.succeeded == body.succeeded
            && last.proposals == body.proposals
            && last.requirement_indices == body.requirement_indices
        {
            return (
                StatusCode::CONFLICT,
                format!("this submission is byte-identical to iteration {}, the run's own immediately-preceding entry -- refusing to record it as a distinct, new iteration", last.iteration),
            )
                .into_response();
        }
    }

    let iteration = run_state.history.len() as u32 + 1;
    let record = IterationRecord {
        run_id: id.clone(),
        stage: body.stage,
        iteration,
        feedback: body.feedback,
        succeeded: body.succeeded,
        proposals: body.proposals,
        requirement_indices: body.requirement_indices,
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
    Json(serde_json::json!({
        "outcome": outcome_str,
        "iteration": iteration,
        "roles_now": spec.roles.len(),
        "added_stages": run_state.added_stages,
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
    match persist_run(&dir, &spec, &run_state) {
        Ok(()) => Json(serde_json::json!({"paused": run_state.paused})).into_response(),
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
    let statement = body.statement.trim().to_string();
    if statement.is_empty() {
        return (StatusCode::BAD_REQUEST, "statement must not be empty").into_response();
    }
    if statement.len() > MAX_REQUIREMENT_STATEMENT_LEN {
        return (StatusCode::BAD_REQUEST, format!("statement must be under {MAX_REQUIREMENT_STATEMENT_LEN} characters")).into_response();
    }
    let acceptance_criteria: Vec<String> = body.acceptance_criteria.iter().map(|c| c.trim().to_string()).filter(|c| !c.is_empty()).collect();
    if acceptance_criteria.is_empty() {
        return (StatusCode::BAD_REQUEST, "at least one non-empty acceptance criterion is required").into_response();
    }
    if acceptance_criteria.len() > MAX_ACCEPTANCE_CRITERIA {
        return (StatusCode::BAD_REQUEST, format!("acceptance_criteria is at its defensive cap of {MAX_ACCEPTANCE_CRITERIA} items")).into_response();
    }
    if let Some(c) = acceptance_criteria.iter().find(|c| c.len() > MAX_ACCEPTANCE_CRITERION_LEN) {
        return (StatusCode::BAD_REQUEST, format!("acceptance criterion \"{c}\" is over {MAX_ACCEPTANCE_CRITERION_LEN} characters")).into_response();
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
    if let Some(c) = acceptance_criteria.iter().find(|c| c.chars().filter(|ch| ch.is_alphanumeric()).count() < MIN_ACCEPTANCE_CRITERION_ALNUM_CHARS) {
        return (
            StatusCode::BAD_REQUEST,
            format!(
                "acceptance criterion \"{c}\" doesn't have enough real content to be checkable \
                 (minimum {MIN_ACCEPTANCE_CRITERION_ALNUM_CHARS} letters/digits) -- \"ok\", \".\", or an \
                 invisible character aren't real acceptance criteria."
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
    if run_state.requirements.len() >= MAX_LIST_ITEMS {
        return (StatusCode::BAD_REQUEST, format!("requirements is at its defensive cap of {MAX_LIST_ITEMS} items")).into_response();
    }
    let proposed_by = body.proposed_by.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    run_state.requirements.push(Requirement {
        statement,
        acceptance_criteria,
        verified: false,
        verified_criteria: Vec::new(),
        auto_judge: false,
        proposed_by,
    });
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
            let md = render_requirements_markdown(&id, &run_state.requirements);
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
    if let Err(e) = toggle_acceptance_criterion(&mut run_state, req_index, criterion_index) {
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
    if let SetRoleFillModeRequest::Dedicated { label, .. } = &body {
        if label.trim().is_empty() {
            return (StatusCode::BAD_REQUEST, "label must be non-empty for a dedicated role").into_response();
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
                results.sort_by(|a, b| b.score.cmp(&a.score));
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

/// `POST /api/runs/{id}/rag/upload-file` -- real image/PDF/DOCX upload via the
/// real Unstructured API (`rag::parse_with_unstructured`), the operator's
/// explicit image-OCR ask from CADS-devsystem#7. `multipart/form-data`, one
/// field named `file`. Owner-restricted like every other GUI mutation; a real
/// `503` (not a silent no-op) when `RAG_UNSTRUCTURED_API_KEY` isn't
/// configured, matching `ask_assistant`'s own "not configured" precedent
/// rather than pretending to accept a file it can't actually process.
async fn upload_rag_file(State(state): State<AppState>, AxPath(id): AxPath<String>, headers: axum::http::HeaderMap, mut multipart: axum::extract::Multipart) -> impl IntoResponse {
    if !valid_run_id(&id) {
        return (StatusCode::BAD_REQUEST, "run_id must be non-empty alphanumeric/-/_ only").into_response();
    }
    if !run_exists(&state, &id) {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    }
    let Some(api_key) = state.rag_unstructured_api_key.clone() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "RAG_UNSTRUCTURED_API_KEY is not configured on this deployment").into_response();
    };
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
    let elements = match rag::parse_with_unstructured(&state.http_client, &state.rag_unstructured_api_base, &api_key, &filename, bytes).await {
        Ok(e) => e,
        Err(e) => return (StatusCode::BAD_GATEWAY, format!("Unstructured extraction failed: {e}")).into_response(),
    };
    let (text, truncated) = rag::elements_to_text(&elements);
    if text.trim().is_empty() {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({"error": "Unstructured extracted no text from this file", "elements": elements.len()})),
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
            "elements_extracted": elements.len(),
            "extracted_text_truncated": truncated,
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
    let proposal = PendingPanelProposal { id: format!("{:016x}", rand::random::<u64>()), title, html: body.html, proposed_at: unix_now() };
    run_state.pending_panel_proposals.push(proposal.clone());
    match persist_run(&dir, &spec, &run_state) {
        Ok(()) => Json(proposal).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("persist failed: {e}")).into_response(),
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
    if body.units == 0 {
        return (StatusCode::BAD_REQUEST, "units must be at least 1").into_response();
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
    let _guard = state.write_lock.lock().await;
    let dir = run_dir(&state, &id);
    let (spec, mut run_state) = match load_or_init_run(&dir, &id) {
        Ok(v) => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("load failed: {e}")).into_response(),
    };
    if !owner_authorized(&headers, &run_state) {
        return (StatusCode::FORBIDDEN, "this run belongs to a different account").into_response();
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
    if run_exists(&state, &id) {
        match load_or_init_run(&run_dir(&state, &id), &id) {
            Ok((_spec, run_state)) if !owner_authorized(&headers, &run_state) => {
                return (StatusCode::FORBIDDEN, "this run belongs to a different account").into_response();
            }
            Ok(_) => {}
            Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response(),
        }
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
            // Real usage accounting (#382 goal doc §7.3, gap #5): this bridge call
            // already computes real token/cost usage (devsystem_assistant.rs's
            // parse_llm_json_output) and has always returned it in this exact
            // response -- forwarded straight to the caller below and, until now,
            // never persisted anywhere, so a run's real cumulative assistant spend
            // was unrecoverable once the browser tab closed. Best-effort: a usage
            // field that's missing or fails to parse never blocks the real reply
            // below from reaching the caller.
            if status.is_success() {
                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&text) {
                    if let Some(usage) = parsed.get("usage") {
                        persist_assistant_usage(&state, &id, usage).await;
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
/// load/persist round-trip fails for any reason -- usage accounting is real, but
/// it must never be the reason a real assistant reply fails to reach the caller.
async fn persist_assistant_usage(state: &AppState, id: &str, usage: &serde_json::Value) {
    if !run_exists(state, id) {
        return;
    }
    let _guard = state.write_lock.lock().await;
    let dir = run_dir(state, id);
    let Ok((spec, mut run_state)) = load_or_init_run(&dir, id) else { return };
    run_state.assistant_usage.add_call(usage);
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
            .oneshot(json_request("POST", "/api/runs/panels-bad-run/panels", serde_json::json!({"title": "T", "html": huge})))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::BAD_REQUEST);
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

        let response = app
            .clone()
            .oneshot(Request::builder().method("POST").uri("/api/runs/criteria-run/requirements/0/criteria/1/toggle").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK);
        let body = body_json(response).await;
        assert_eq!(body["requirements"][0]["verified_criteria"], serde_json::json!([false, true]), "must grow with real false padding, not just record index 1 alone");
        assert_eq!(body["requirements"][0]["verified"], false, "toggling one criterion must never silently flip the independent whole-requirement flag");

        // Independently confirms it actually persisted, not just the response.
        let response = app.oneshot(Request::builder().uri("/api/runs/criteria-run").body(Body::empty()).unwrap()).await.unwrap();
        let body = body_json(response).await;
        assert_eq!(body["state"]["requirements"][0]["verified_criteria"], serde_json::json!([false, true]));
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
                serde_json::json!({"statement": "a real statement", "acceptance_criteria": []}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::BAD_REQUEST, "a requirement with no checkable acceptance criteria must be rejected");
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
                    serde_json::json!({"statement": "a real statement", "acceptance_criteria": [trivial]}),
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
                serde_json::json!({"statement": "a real statement", "acceptance_criteria": ["no crash"]}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::OK, "a genuinely short but real criterion must not be rejected");
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
                serde_json::json!({"statement": "a human-authored requirement", "acceptance_criteria": ["checkable"]}),
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
                    "statement": "an assistant-proposed requirement",
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
                serde_json::json!({"statement": "a real requirement", "acceptance_criteria": ["checkable"]}),
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
                serde_json::json!({"statement": "a real requirement", "acceptance_criteria": ["checkable"]}),
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
                serde_json::json!({"statement": "a real requirement", "acceptance_criteria": ["checkable"]}),
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
        assert!(body.contains("- [ ] checkable"));
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
            .oneshot(json_request(
                "POST",
                "/api/runs/propose-stage-edge-run/stages/propose",
                serde_json::json!({"stage_id": "devsystem.x", "tag": "x", "rationale": "y", "units": 0}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::BAD_REQUEST);
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
                serde_json::json!({"stage": "implement", "feedback": "real progress", "succeeded": true}),
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
                serde_json::json!({"stage": "implement", "feedback": "addressed the requirement", "succeeded": true, "requirement_indices": [0]}),
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
            .oneshot(json_request(
                "POST",
                "/api/runs/quick-offer-run3/offers/quick-submit",
                serde_json::json!({"stage_id": "devsystem.plan", "price": 7, "units": 0}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), SC::BAD_REQUEST);
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
}
