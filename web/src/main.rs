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
use devsystem_pipeline::envelope::{append_to_memory_log, envelope_from_iteration};
use devsystem_pipeline::runner::{load_or_init_run, persist_run, run_iteration, RunOutcome};
use devsystem_pipeline::{AbortCriteria, IterationRecord, StageProposal};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use tower_http::cors::CorsLayer;
use tower_http::services::ServeDir;

#[derive(Clone)]
struct AppState {
    runs_dir: Arc<PathBuf>,
}

#[tokio::main]
async fn main() {
    let runs_dir = PathBuf::from(std::env::var("DEVSYSTEM_RUNS_DIR").unwrap_or_else(|_| "runs".to_string()));
    fs::create_dir_all(&runs_dir).expect("create runs dir");
    let state = AppState { runs_dir: Arc::new(runs_dir) };

    let static_dir = std::env::var("DEVSYSTEM_STATIC_DIR").unwrap_or_else(|_| "web/static".to_string());

    let app = Router::new()
        .route("/api/runs", get(list_runs).post(create_run))
        .route("/api/runs/{id}", get(get_run))
        .route("/api/runs/{id}/iterate", post(iterate_run))
        .route("/api/runs/{id}/checkin", get(checkin_run))
        .layer(CorsLayer::permissive())
        .with_state(state)
        .fallback_service(ServeDir::new(static_dir));

    let addr = "0.0.0.0:8790";
    println!("devsystem-web listening on {addr}");
    let listener = tokio::net::TcpListener::bind(addr).await.expect("bind");
    axum::serve(listener, app).await.expect("serve");
}

fn run_dir(state: &AppState, id: &str) -> PathBuf {
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
            runs.push(RunSummary {
                run_id: id,
                iterations: run_state.history.len(),
                roles: spec.roles.len(),
                added_stages: run_state.added_stages,
            });
        }
    }
    runs.sort_by(|a, b| a.run_id.cmp(&b.run_id));
    Json(runs).into_response()
}

async fn create_run(State(state): State<AppState>, Json(body): Json<CreateRunRequest>) -> impl IntoResponse {
    let id = body.run_id.trim();
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return (StatusCode::BAD_REQUEST, "run_id must be non-empty alphanumeric/-/_ only").into_response();
    }
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
    if !run_exists(&state, &id) {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    }
    match load_or_init_run(&run_dir(&state, &id), &id) {
        Ok((spec, run_state)) => Json(serde_json::json!({"spec": spec, "state": run_state})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response(),
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
    let dir = run_dir(&state, &id);
    let (mut spec, mut run_state) = match load_or_init_run(&dir, &id) {
        Ok(v) => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("load failed: {e}")).into_response(),
    };

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

    let criteria = AbortCriteria::default();
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
