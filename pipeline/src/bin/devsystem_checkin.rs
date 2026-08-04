//! Delivers a run's pending check-in through the real `ecc-plan-canvas` channel
//! (`docs/plan-stage.md`), not just a GitHub comment. Loads `runs/<run_id>/state.json`,
//! renders the latest iteration as a `.plan.md` artifact at
//! `.claude/plans/<run_id>.plan.md`, and runs `ecc-plan-canvas open <file>` -- a
//! non-blocking start, deliberately never `await` (this runs from an autonomous loop
//! firing; blocking on a human verdict here would hang the whole loop).
//!
//! Also seeds any real `preflight::preflight_annotations` findings into the
//! session's chat *before* a human opens it (proposal §5's own documented "next
//! slice", `docs/plan-stage.md`) -- via the canvas server's `/api/session/<key>/reply`
//! endpoint directly, the same one `ecc-plan-canvas await --reply` posts to, but
//! without ever calling the blocking `await`.
//!
//! Usage: `devsystem_checkin <run_id>`

use devsystem_pipeline::checkin::{parse_session_key_and_origin, render_plan_markdown};
use devsystem_pipeline::preflight::preflight_annotations;
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

    let output = Command::new("ecc-plan-canvas").arg("open").arg(&plan_path).output();
    let output = match output {
        Ok(o) if o.status.success() => o,
        Ok(o) => {
            println!("ecc-plan-canvas exited with {} -- artifact is still written at {}.", o.status, plan_path.display());
            return;
        }
        Err(e) => {
            println!("could not run ecc-plan-canvas ({e}) -- artifact is still written at {}.", plan_path.display());
            return;
        }
    };
    println!("ecc-plan-canvas opened {} -- awaiting human review (not blocking this process).", plan_path.display());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let session: Result<serde_json::Value, _> = serde_json::from_str(stdout.trim());
    let Some(url) = session.as_ref().ok().and_then(|v| v.get("url")).and_then(|v| v.as_str()) else {
        println!("could not parse a session url from `open`'s output -- skipping pre-flight annotations.");
        return;
    };
    let Some((key, origin)) = parse_session_key_and_origin(url) else {
        println!("could not derive a session key/origin from {url:?} -- skipping pre-flight annotations.");
        return;
    };

    for annotation in preflight_annotations(&state) {
        let text = format!("Pre-flight: {} — {}", annotation.label, annotation.evidence);
        let body = serde_json::json!({ "text": text }).to_string();
        let result = Command::new("curl")
            .args(["-sS", "-o", "/dev/null", "-w", "%{http_code}", "-X", "POST", "-H", "content-type: application/json", "-d", &body])
            .arg(format!("{origin}/api/session/{key}/reply"))
            .output();
        match result {
            Ok(o) if o.status.success() => println!("seeded pre-flight annotation: {text} (HTTP {})", String::from_utf8_lossy(&o.stdout)),
            Ok(o) => println!("pre-flight annotation POST failed: {}", String::from_utf8_lossy(&o.stderr)),
            Err(e) => println!("could not run curl to seed pre-flight annotation: {e}"),
        }
    }
}
