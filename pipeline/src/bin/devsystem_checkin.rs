//! Delivers a run's pending check-in through the real `ecc-plan-canvas` channel
//! (`docs/plan-stage.md`), not just a GitHub comment. Loads `runs/<run_id>/state.json`,
//! renders the latest iteration as a `.plan.md` artifact at
//! `.claude/plans/<run_id>.plan.md`, and runs `ecc-plan-canvas open <file>` -- a
//! non-blocking start, deliberately never `await` (this runs from an autonomous loop
//! firing; blocking on a human verdict here would hang the whole loop).
//!
//! Usage: `devsystem_checkin <run_id>`

use devsystem_pipeline::checkin::render_plan_markdown;
use devsystem_pipeline::runner::RunState;
use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let run_id = env::args().nth(1).expect("usage: devsystem_checkin <run_id>");

    let state_path = PathBuf::from("runs").join(&run_id).join("state.json");
    let state: RunState = serde_json::from_str(&fs::read_to_string(&state_path).unwrap_or_else(|e| {
        panic!("read {}: {e}", state_path.display());
    }))
    .expect("valid state.json");

    let markdown = render_plan_markdown(&state).unwrap_or_else(|| {
        panic!("run {run_id} has no iteration history yet -- nothing to check in");
    });

    let plans_dir = PathBuf::from(".claude/plans");
    fs::create_dir_all(&plans_dir).expect("create .claude/plans/");
    let plan_path = plans_dir.join(format!("{run_id}.plan.md"));
    fs::write(&plan_path, &markdown).expect("write plan artifact");
    println!("wrote {}", plan_path.display());

    let status = Command::new("ecc-plan-canvas").arg("open").arg(&plan_path).status();
    match status {
        Ok(s) if s.success() => println!("ecc-plan-canvas opened {} -- awaiting human review (not blocking this process).", plan_path.display()),
        Ok(s) => println!("ecc-plan-canvas exited with {s} -- artifact is still written at {}.", plan_path.display()),
        Err(e) => println!("could not run ecc-plan-canvas ({e}) -- artifact is still written at {}.", plan_path.display()),
    }
}
