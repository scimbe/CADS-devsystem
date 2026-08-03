//! Real CLI that drives one iteration of a run's super loop end to end: load the run's
//! persisted spec + state (or start fresh), fold in a real [`IterationRecord`] read from
//! a JSON file, apply any proposals, persist the result, and print the outcome. This is
//! not a simulation harness -- every invocation is a real run of `runner::run_iteration`
//! against real files under `runs/<run_id>/` in this repo (#382).
//!
//! Usage: `devsystem_iterate <run_id> <record.json>`

use devsystem_pipeline::runner::{run_iteration, RunOutcome, RunState};
use devsystem_pipeline::{plan_only_spec, AbortCriteria, IterationRecord};
use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    let mut args = env::args().skip(1);
    let run_id = args.next().expect("usage: devsystem_iterate <run_id> <record.json>");
    let record_path = args.next().expect("usage: devsystem_iterate <run_id> <record.json>");

    let run_dir = PathBuf::from("runs").join(&run_id);
    fs::create_dir_all(&run_dir).expect("create runs/<run_id>/");

    let spec_path = run_dir.join("spec.json");
    let state_path = run_dir.join("state.json");

    let mut spec = if spec_path.exists() {
        serde_json::from_str(&fs::read_to_string(&spec_path).unwrap()).expect("valid spec.json")
    } else {
        plan_only_spec(&run_id, None)
    };
    let mut state = if state_path.exists() {
        serde_json::from_str(&fs::read_to_string(&state_path).unwrap()).expect("valid state.json")
    } else {
        RunState::new(run_id.clone())
    };

    let record: IterationRecord =
        serde_json::from_str(&fs::read_to_string(&record_path).expect("read record.json")).expect("valid record.json");

    let criteria = AbortCriteria::default();
    let outcome = run_iteration(&mut spec, &mut state, record, &criteria);

    fs::write(&spec_path, serde_json::to_string_pretty(&spec).unwrap()).expect("write spec.json");
    fs::write(&state_path, serde_json::to_string_pretty(&state).unwrap()).expect("write state.json");

    println!("run_id={run_id} iteration_outcome={outcome:?} roles_now={} added_stages={:?}", spec.roles.len(), state.added_stages);
    match outcome {
        RunOutcome::Abort => std::process::exit(1),
        RunOutcome::CheckinDue => println!("CHECK-IN REQUIRED before the next iteration -- do not proceed unsupervised."),
        RunOutcome::Continue => {}
    }
}
