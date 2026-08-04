//! Real interactive control surface for The Development System's pipeline runs
//! (#382). Every request here goes through the *exact same* `devsystem-pipeline`
//! library functions the CLI tools (`devsystem_iterate`/`devsystem_checkin`) use --
//! no separate/parallel logic path, no fixture data. The pipeline mechanism itself
//! stays project-agnostic: this server lists whatever `runs/<id>/` directories
//! actually exist on disk and lets a human create new ones -- `webconference-android`
//! is just the first one, not a hardcoded case anywhere in this file.

use axum::extract::{Path as AxPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json};
use axum::routing::{get, post};
use axum::Router;
use devsystem_pipeline::checkin::render_plan_markdown;
use devsystem_pipeline::envelope::{append_to_memory_log, envelope_from_iteration, govern_memory_entry, read_memory_log};
use devsystem_pipeline::improve::stalled_stages;
use devsystem_pipeline::preflight::preflight_annotations;
use devsystem_pipeline::runner::{load_or_init_run, persist_run, run_iteration, toggle_milestone, BacklogItem, Milestone, RunOutcome};
use devsystem_pipeline::{AbortCriteria, IterationRecord, StageProposal};
use ct_common::channel::CapacityOffer;
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
        .route("/api/runs/{id}/offers/submit", post(submit_offer))
        .route("/api/runs/{id}/auction", get(view_auction))
        .with_state(state)
}

#[tokio::main]
async fn main() {
    let runs_dir = PathBuf::from(std::env::var("DEVSYSTEM_RUNS_DIR").unwrap_or_else(|_| "runs".to_string()));
    fs::create_dir_all(&runs_dir).expect("create runs dir");
    let state = AppState { runs_dir: Arc::new(runs_dir), write_lock: Arc::new(tokio::sync::Mutex::new(())), offers: Arc::new(tokio::sync::Mutex::new(HashMap::new())) };

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
}

/// True when a run is close enough to its own bound that a human should notice it
/// before opening the run, not just after -- the same danger/warn thresholds the
/// GUI's health panel already uses, just evaluated once here so the run list can
/// surface it too (matches the stalled-stage badge precedent: proactive, not
/// only-on-click).
fn needs_attention(health: &RunHealth) -> bool {
    health.consecutive_failures + 1 >= health.criteria.max_consecutive_failures || health.iterations_until_checkin <= 1
}

async fn list_runs(State(state): State<AppState>) -> impl IntoResponse {
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
            let stalled = stalled_stages(&run_state);
            let risk_count = preflight_annotations(&run_state).len();
            let health = run_health(&run_state);
            let alert = needs_attention(&health);
            let paused = run_state.paused;
            runs.push(RunSummary {
                run_id: id,
                iterations: run_state.history.len(),
                roles: spec.roles.len(),
                added_stages: run_state.added_stages,
                stalled_stages: stalled,
                risk_count,
                needs_attention: alert,
                paused,
            });
        }
    }
    runs.sort_by(|a, b| a.run_id.cmp(&b.run_id));
    Json(runs).into_response()
}

async fn create_run(State(state): State<AppState>, Json(body): Json<CreateRunRequest>) -> impl IntoResponse {
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
        Ok((spec, run_state)) => match persist_run(&dir, &spec, &run_state) {
            Ok(()) => (StatusCode::CREATED, Json(serde_json::json!({"run_id": id, "roles": spec.roles.len()}))).into_response(),
            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("persist failed: {e}")).into_response(),
        },
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("init failed: {e}")).into_response(),
    }
}

#[derive(Deserialize)]
struct CreateRunRequest {
    run_id: String,
}

async fn get_run(State(state): State<AppState>, AxPath(id): AxPath<String>) -> impl IntoResponse {
    if !valid_run_id(&id) {
        return (StatusCode::BAD_REQUEST, "run_id must be non-empty alphanumeric/-/_ only").into_response();
    }
    if !run_exists(&state, &id) {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    }
    match load_or_init_run(&run_dir(&state, &id), &id) {
        Ok((spec, run_state)) => {
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

async fn iterate_run(State(state): State<AppState>, AxPath(id): AxPath<String>, Json(body): Json<IterateRequest>) -> impl IntoResponse {
    if !valid_run_id(&id) {
        return (StatusCode::BAD_REQUEST, "run_id must be non-empty alphanumeric/-/_ only").into_response();
    }
    let _guard = state.write_lock.lock().await;
    let dir = run_dir(&state, &id);
    let (mut spec, mut run_state) = match load_or_init_run(&dir, &id) {
        Ok(v) => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("load failed: {e}")).into_response(),
    };
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

async fn checkin_run(State(state): State<AppState>, AxPath(id): AxPath<String>) -> impl IntoResponse {
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
    match render_plan_markdown(&run_state) {
        Some(markdown) => Json(serde_json::json!({"markdown": markdown})).into_response(),
        None => (StatusCode::NOT_FOUND, "no iteration history yet").into_response(),
    }
}

/// `devsystem.remember`'s durable log (`memory.jsonl`) was write-only until now --
/// `iterate_run` has appended a real envelope every iteration since the mechanism
/// was built, but nothing could ever read it back. Real data, not a stub: whatever
/// this run's actual history produced.
async fn memory_run(State(state): State<AppState>, AxPath(id): AxPath<String>) -> impl IntoResponse {
    if !valid_run_id(&id) {
        return (StatusCode::BAD_REQUEST, "run_id must be non-empty alphanumeric/-/_ only").into_response();
    }
    if !run_exists(&state, &id) {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    }
    let memory_path = run_dir(&state, &id).join("memory.jsonl");
    match read_memory_log(&memory_path) {
        Ok(entries) => Json(entries).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response(),
    }
}

/// The only place `Trust::Governed` should ever get set: a human, through the GUI,
/// explicitly marking one memory entry as reviewed. Never automatic.
async fn govern_memory(State(state): State<AppState>, AxPath((id, index)): AxPath<(String, usize)>) -> impl IntoResponse {
    if !valid_run_id(&id) {
        return (StatusCode::BAD_REQUEST, "run_id must be non-empty alphanumeric/-/_ only").into_response();
    }
    if !run_exists(&state, &id) {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
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
async fn pause_run(State(state): State<AppState>, AxPath(id): AxPath<String>) -> impl IntoResponse {
    set_paused(state, id, true).await
}

async fn resume_run(State(state): State<AppState>, AxPath(id): AxPath<String>) -> impl IntoResponse {
    set_paused(state, id, false).await
}

async fn set_paused(state: AppState, id: String, paused: bool) -> axum::response::Response {
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
    if run_state.backlog.len() >= MAX_LIST_ITEMS {
        return (StatusCode::BAD_REQUEST, format!("backlog is at its defensive cap of {MAX_LIST_ITEMS} items")).into_response();
    }
    run_state.backlog.push(BacklogItem { text, done: false });
    match persist_run(&dir, &spec, &run_state) {
        Ok(()) => Json(serde_json::json!({"backlog": run_state.backlog})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("persist failed: {e}")).into_response(),
    }
}

async fn toggle_backlog_item(State(state): State<AppState>, AxPath((id, index)): AxPath<(String, usize)>) -> impl IntoResponse {
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
    if run_state.milestones.len() >= MAX_LIST_ITEMS {
        return (StatusCode::BAD_REQUEST, format!("milestones is at its defensive cap of {MAX_LIST_ITEMS} items")).into_response();
    }
    run_state.milestones.push(Milestone { description, achieved: false });
    match persist_run(&dir, &spec, &run_state) {
        Ok(()) => Json(serde_json::json!({"milestones": run_state.milestones})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("persist failed: {e}")).into_response(),
    }
}

async fn toggle_milestone_handler(State(state): State<AppState>, AxPath((id, index)): AxPath<(String, usize)>) -> impl IntoResponse {
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
async fn set_repo_url(State(state): State<AppState>, AxPath(id): AxPath<String>, Json(body): Json<SetRepoUrlRequest>) -> impl IntoResponse {
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
    run_state.repo_url = if trimmed.is_empty() { None } else { Some(trimmed) };
    match persist_run(&dir, &spec, &run_state) {
        Ok(()) => Json(serde_json::json!({"repo_url": run_state.repo_url})).into_response(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode as SC};
    use tower::ServiceExt;

    fn test_state() -> (AppState, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = AppState { runs_dir: Arc::new(dir.path().to_path_buf()), write_lock: Arc::new(tokio::sync::Mutex::new(())), offers: Arc::new(tokio::sync::Mutex::new(HashMap::new())) };
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

        let state = AppState { runs_dir: Arc::new(runs_dir), write_lock: Arc::new(tokio::sync::Mutex::new(())), offers: Arc::new(tokio::sync::Mutex::new(HashMap::new())) };
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
}
