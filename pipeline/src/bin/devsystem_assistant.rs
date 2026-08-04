//! Real devsystem.assistant role-filler -- the smallest honest slice of the
//! operator's "Assistent" request: "ein LLM Agent... wie bei flappy editor, der
//! auch ausgetauscht werden kann, es ist nur eine spezialisierte Rolle." Uses the
//! exact proven, isolated pattern CADS-flappy-demo's own handlers use
//! (`${CT_LLM_CMD:-claude} -p ... --disallowedTools ... --append-system-prompt
//! ...`, verified directly against this host, not assumed), grounded in a run's
//! real current state fetched from devsystem-web -- never invented data.
//!
//! v1 scope is deliberately ADVICE ONLY: it never executes an action itself. This
//! matches the operator's own framing directly -- "Die Task sollen eigentlich nur
//! im absoluten Notfall vom Menschen angepasst werden... Ein 'Assistent' hilft mir
//! primär die Pipeline zu steuern... so dass ich nicht etwas in den grundsätzlich
//! formalisierten Requirement- und Organisationsprozess negativ eingreife." A
//! later increment can let it PROPOSE structured actions for a human to review and
//! apply through the real API; this one only talks, exactly like art-handler.sh's
//! "isolated, no tool access -- pure generation" role.
//!
//! Usage: devsystem_assistant <api-base-url> <run-id> <instruction...>
//!
//! `CT_LLM_CMD` selects the non-interactive LLM CLI (default: `claude`) -- the
//! same env var flappy-demo's handlers read, so this role is genuinely swappable
//! for a different backend without a code change.

use std::env;
use std::process::{Command, ExitCode, Stdio};

fn main() -> ExitCode {
    let mut args = env::args().skip(1);
    let (Some(api_base), Some(run_id)) = (args.next(), args.next()) else {
        eprintln!("usage: devsystem_assistant <api-base-url> <run-id> <instruction...>");
        return ExitCode::FAILURE;
    };
    let instruction: String = args.collect::<Vec<_>>().join(" ");
    if instruction.trim().is_empty() {
        eprintln!("an instruction is required");
        return ExitCode::FAILURE;
    }

    let url = format!("{}/api/runs/{}", api_base.trim_end_matches('/'), run_id);
    let context = match reqwest::blocking::get(&url) {
        Ok(resp) if resp.status().is_success() => match resp.text() {
            Ok(t) => t,
            Err(e) => {
                eprintln!("could not read response body from {url}: {e}");
                return ExitCode::FAILURE;
            }
        },
        Ok(resp) => {
            eprintln!("could not fetch run context from {url}: HTTP {}", resp.status());
            return ExitCode::FAILURE;
        }
        Err(e) => {
            eprintln!("could not reach {url}: {e}");
            return ExitCode::FAILURE;
        }
    };

    let system_prompt = format!(
        "You are devsystem.assistant, a specialized advisory role in The Development \
         System -- a real, self-optimizing, agent-driven pipeline (CADS-Tunnel#382). \
         Your job is to help the human operator understand, control, and optimize a \
         real pipeline run without them having to hand-edit raw state directly. Give \
         concrete, grounded advice based ONLY on the real current run state given \
         below -- never invent data that isn't there, and say plainly if the state \
         doesn't contain enough information to answer. You do NOT execute any action \
         yourself in this version; you only advise what the operator could do next \
         (e.g. which stage to iterate on, whether a risk finding needs attention, \
         whether a milestone looks achievable, whether the run needs a check-in). Be \
         concise and reference real field values from the state.\n\n\
         Current real run state (JSON):\n{context}"
    );

    let llm = env::var("CT_LLM_CMD").unwrap_or_else(|_| "claude".to_string());
    let output = Command::new(&llm)
        .arg("-p")
        .arg(&instruction)
        .arg("--output-format")
        .arg("text")
        .arg("--disallowedTools")
        .arg("Edit,Write,Bash,WebFetch,WebSearch,Agent")
        .arg("--append-system-prompt")
        .arg(&system_prompt)
        .stdin(Stdio::null())
        .output();

    match output {
        Ok(out) if out.status.success() => {
            print!("{}", String::from_utf8_lossy(&out.stdout));
            ExitCode::SUCCESS
        }
        Ok(out) => {
            eprintln!("{llm} exited with {}: {}", out.status, String::from_utf8_lossy(&out.stderr));
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("could not run {llm}: {e} (set CT_LLM_CMD to point at a non-interactive LLM CLI)");
            ExitCode::FAILURE
        }
    }
}
