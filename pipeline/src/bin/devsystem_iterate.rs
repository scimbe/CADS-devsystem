//! Real CLI that drives one iteration of a run's super loop end to end: load the run's
//! persisted spec + state (or start fresh), fold in a real [`IterationRecord`] read from
//! a JSON file, apply any proposals, persist the result, and print the outcome. This is
//! not a simulation harness -- every invocation is a real run of `runner::run_iteration`
//! against real files under `runs/<run_id>/` in this repo (#382).
//!
//! Usage: `devsystem_iterate <run_id> <record.json>`

use devsystem_pipeline::envelope::{append_to_memory_log, envelope_from_iteration};
use devsystem_pipeline::runner::{load_or_init_run, persist_run, run_iteration, RunOutcome};
use devsystem_pipeline::{AbortCriteria, IterationRecord};
use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    let mut args = env::args().skip(1);
    let run_id = args.next().expect("usage: devsystem_iterate <run_id> <record.json>");
    let record_path = args.next().expect("usage: devsystem_iterate <run_id> <record.json>");

    let run_dir = PathBuf::from("runs").join(&run_id);
    let (mut spec, mut state) = load_or_init_run(&run_dir, &run_id).expect("load or initialize run");

    let record: IterationRecord =
        serde_json::from_str(&fs::read_to_string(&record_path).expect("read record.json")).expect("valid record.json");

    // devsystem.remember, made real: every iteration's zylos envelope is appended to
    // the run's durable memory log before anything else happens to `record`.
    let memory_path = run_dir.join("memory.jsonl");
    let envelope = envelope_from_iteration(&record);
    append_to_memory_log(&memory_path, &envelope).expect("append to memory.jsonl");

    let criteria = AbortCriteria::default();
    let outcome = run_iteration(&mut spec, &mut state, record, &criteria);

    persist_run(&run_dir, &spec, &state).expect("persist run");

    println!("run_id={run_id} iteration_outcome={outcome:?} roles_now={} added_stages={:?}", spec.roles.len(), state.added_stages);
    match outcome {
        RunOutcome::Abort => std::process::exit(1),
        RunOutcome::CheckinDue => println!("CHECK-IN REQUIRED before the next iteration -- do not proceed unsupervised."),
        RunOutcome::Continue => {}
    }
}
