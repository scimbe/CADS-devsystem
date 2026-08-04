//! Real interactive control surface for The Development System's pipeline runs
//! (#382). Every request here goes through the *exact same* `devsystem-pipeline`
//! library functions the CLI tools (`devsystem_iterate`/`devsystem_checkin`) use --
//! no separate/parallel logic path, no fixture data. The pipeline mechanism itself
//! stays project-agnostic: this server lists whatever `runs/<id>/` directories
//! actually exist on disk and lets a human create new ones -- `webconference-android`
//! is just the first one, not a hardcoded case anywhere in this file.

mod rag;

use axum::extract::{Path as AxPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json};
use axum::routing::{get, post};
use axum::Router;
use devsystem_pipeline::checkin::render_plan_markdown;
use devsystem_pipeline::envelope::{append_to_memory_log, envelope_from_iteration, govern_memory_entry, read_memory_log};
use devsystem_pipeline::improve::stalled_stages;
use devsystem_pipeline::preflight::preflight_annotations;
use devsystem_pipeline::runner::{load_or_init_run, persist_run, run_iteration, toggle_milestone, BacklogItem, CustomPanel, Milestone, RoleFillMode, RunOutcome};
use devsystem_pipeline::{AbortCriteria, IterationRecord, StageProposal};
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
    /// Base URL of a running `devsystem_assistant --serve` bridge (e.g.
    /// `http://host.docker.internal:8791` when the assistant runs as a host
    /// process alongside this container's Docker host -- the real LLM CLI lives
    /// on the host, not in this container). `None` when unconfigured: the
    /// assistant panel then reports a clear "not configured" error rather than
    /// silently doing nothing or fabricating a response.
    assistant_url: Option<Arc<str>>,
    http_client: reqwest::Client,
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).expect("system clock before 1970").as_secs()
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
        .route("/api/runs/{id}/repo", post(set_repo_url))
        .route("/api/runs/{id}/roles/{tag}/fill-mode", post(set_role_fill_mode))
        .route("/api/runs/{id}/rag/sync", post(sync_rag))
        .route("/api/runs/{id}/rag/search", get(search_rag))
        .route("/api/runs/{id}/rag/documents", post(add_rag_document))
        .route("/api/runs/{id}/rag/documents/{doc_id}/remove", post(remove_rag_document))
        .route("/api/runs/{id}/panels", post(add_custom_panel))
        .route("/api/runs/{id}/panels/{panel_id}/remove", post(remove_custom_panel))
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
    let state = AppState {
        runs_dir: Arc::new(runs_dir),
        write_lock: Arc::new(tokio::sync::Mutex::new(())),
        offers: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        assistant_url,
        http_client: reqwest::Client::builder().timeout(std::time::Duration::from_secs(90)).build().expect("build http client"),
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
    paused: bool,
    owner_email: Option<String>,
}

/// True when a run is close enough to its own bound that a human should notice it
/// before opening the run, not just after -- the same danger/warn thresholds the
/// GUI's health panel already uses, just evaluated once here so the run list can
/// surface it too (matches the stalled-stage badge precedent: proactive, not
/// only-on-click).
fn needs_attention(health: &RunHealth) -> bool {
    health.consecutive_failures + 1 >= health.criteria.max_consecutive_failures || health.iterations_until_checkin <= 1
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
            let alert = needs_attention(&health);
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

    let iteration = run_state.history.len() as u32 + 1;
    let record = IterationRecord {
        run_id: id.clone(),
        stage: body.stage,
        iteration,
        feedback: body.feedback,
        succeeded: body.succeeded,
        proposals: body.proposals,
    };

    let memory_path = dir.join("memory.jsonl");
    let envelope = envelope_from_iteration(&record);
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
struct SetRepoUrlRequest {
    repo_url: String,
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
#[serde(tag = "mode", rename_all = "snake_case")]
enum SetRoleFillModeRequest {
    Auction,
    Dedicated { label: String },
}

/// `POST /api/runs/{id}/roles/{tag}/fill-mode` (#382 Roles panel ask 1/4): switch one
/// role between `Auction` (today's default, unchanged) and `Dedicated` (a plain,
/// human-chosen label -- not yet a real reachability-checked identity; see
/// [`RoleFillMode`]'s own doc comment for why this stops short of a fuller registry).
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
    if let SetRoleFillModeRequest::Dedicated { label } = &body {
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
        SetRoleFillModeRequest::Dedicated { label } => RoleFillMode::Dedicated { label: label.trim().to_string() },
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
    let results = rag::search(&index, q.q.trim(), 10);
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
    let doc = rag::RagDocument { id: format!("{:016x}", rand::random::<u64>()), path, text: body.text, added_at: unix_now() };
    index.manual_documents.push(doc.clone());
    match persist_rag_index(&state, &id, &index) {
        Ok(()) => Json(serde_json::json!({"id": doc.id, "path": doc.path, "added_at": doc.added_at})).into_response(),
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
    let before = index.manual_documents.len();
    index.manual_documents.retain(|d| d.id != doc_id);
    if index.manual_documents.len() == before {
        return (StatusCode::NOT_FOUND, format!("no manual document with id {doc_id:?}")).into_response();
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
    let mut selection_state = SelectionState::default();
    // No real identity-resolution registry is wired up yet (a real, honest gap,
    // not fabricated) -- label by a short hex prefix of the holder pubkey so bids
    // from different holders are at least distinguishable.
    let label = |holder: &[u8; 32]| holder.iter().take(4).map(|b| format!("{b:02x}")).collect::<String>();
    match spec.auction_view(&run_offers, unix_now(), spec.selection_policy, &mut selection_state, label) {
        Ok(views) => Json(serde_json::json!({"roles": views})).into_response(),
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
            (status, [(axum::http::header::CONTENT_TYPE, "application/json")], text).into_response()
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({"error": format!("could not reach devsystem.assistant bridge at {url}: {e}")})),
        )
            .into_response(),
    }
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
            assistant_url: assistant_url.map(Arc::from),
            http_client: reqwest::Client::builder().timeout(std::time::Duration::from_secs(5)).build().expect("build http client"),
        };
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

        // Push it to 2 consecutive failures against the default max of 3 --
        // one away from the abort bound, so the list should flag it now.
        for _ in 0..2 {
            app.clone()
                .oneshot(json_request(
                    "POST",
                    "/api/runs/list-health-run/iterate",
                    serde_json::json!({
                        "stage": "devsystem.implement",
                        "feedback": "wired the real session handshake and key material",
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
            assistant_url: None,
            http_client: reqwest::Client::builder().timeout(std::time::Duration::from_secs(5)).build().expect("build http client"),
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
