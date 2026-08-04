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
//! Usage:
//!   devsystem_assistant <api-base-url> <run-id> <instruction...>   (one-shot CLI)
//!   devsystem_assistant --serve <listen-addr> <api-base-url>       (HTTP bridge for the GUI)
//!
//! `CT_LLM_CMD` selects the non-interactive LLM CLI (default: `claude`) -- the
//! same env var flappy-demo's handlers read, so this role is genuinely swappable
//! for a different backend without a code change.

use std::collections::HashMap;
use std::env;
use std::process::{Command, ExitCode, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

fn fetch_context(api_base: &str, run_id: &str) -> Result<String, String> {
    let url = format!("{}/api/runs/{}", api_base.trim_end_matches('/'), run_id);
    match reqwest::blocking::get(&url) {
        Ok(resp) if resp.status().is_success() => resp.text().map_err(|e| format!("could not read response body from {url}: {e}")),
        Ok(resp) => Err(format!("could not fetch run context from {url}: HTTP {}", resp.status())),
        Err(e) => Err(format!("could not reach {url}: {e}")),
    }
}

fn build_system_prompt(context: &str) -> String {
    format!(
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
    )
}

fn ask_llm(instruction: &str, system_prompt: &str) -> Result<String, String> {
    let llm = env::var("CT_LLM_CMD").unwrap_or_else(|_| "claude".to_string());
    let output = Command::new(&llm)
        .arg("-p")
        .arg(instruction)
        .arg("--output-format")
        .arg("text")
        .arg("--disallowedTools")
        .arg("Edit,Write,Bash,WebFetch,WebSearch,Agent")
        .arg("--append-system-prompt")
        .arg(system_prompt)
        .stdin(Stdio::null())
        .output();

    match output {
        Ok(out) if out.status.success() => Ok(String::from_utf8_lossy(&out.stdout).to_string()),
        Ok(out) => Err(format!("{llm} exited with {}: {}", out.status, String::from_utf8_lossy(&out.stderr))),
        Err(e) => Err(format!("could not run {llm}: {e} (set CT_LLM_CMD to point at a non-interactive LLM CLI)")),
    }
}

fn ask(api_base: &str, run_id: &str, instruction: &str) -> Result<String, String> {
    let context = fetch_context(api_base, run_id)?;
    ask_llm(instruction, &build_system_prompt(&context))
}

fn main() -> ExitCode {
    let mut args = env::args().skip(1);
    let first = args.next();

    if first.as_deref() == Some("--serve") {
        let (Some(listen_addr), Some(api_base)) = (args.next(), args.next()) else {
            eprintln!("usage: devsystem_assistant --serve <listen-addr> <api-base-url>");
            return ExitCode::FAILURE;
        };
        return serve(&listen_addr, &api_base);
    }

    let Some(api_base) = first else {
        eprintln!("usage: devsystem_assistant <api-base-url> <run-id> <instruction...>");
        eprintln!("   or: devsystem_assistant --serve <listen-addr> <api-base-url>");
        return ExitCode::FAILURE;
    };
    let Some(run_id) = args.next() else {
        eprintln!("usage: devsystem_assistant <api-base-url> <run-id> <instruction...>");
        return ExitCode::FAILURE;
    };
    let instruction: String = args.collect::<Vec<_>>().join(" ");
    if instruction.trim().is_empty() {
        eprintln!("an instruction is required");
        return ExitCode::FAILURE;
    }

    match ask(&api_base, &run_id, &instruction) {
        Ok(response) => {
            print!("{response}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

fn json_response(status: u16, body: &str) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
    let header = tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).expect("static header is valid");
    tiny_http::Response::from_string(body).with_status_code(status).with_header(header)
}

/// HTTP bridge for the GUI: `POST /ask {"run_id": "...", "instruction": "..."}` ->
/// `{"response": "..."}`. Meant to sit behind the same reverse-proxy gate as
/// devsystem-web itself (same-origin from the browser's perspective -- no CORS
/// needed), on whatever host actually has a real LLM CLI available. Per-run rate
/// limit (10s) is a deliberate safety backstop against a double-click or a stuck
/// retry loop burning real LLM spend -- not a security control, just a sane floor.
fn serve(listen_addr: &str, api_base: &str) -> ExitCode {
    let server = match tiny_http::Server::http(listen_addr) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("could not bind {listen_addr}: {e}");
            return ExitCode::FAILURE;
        }
    };
    println!("devsystem_assistant serving on {listen_addr}, run context via {api_base}");

    let last_request: Mutex<HashMap<String, Instant>> = Mutex::new(HashMap::new());
    const MIN_INTERVAL: Duration = Duration::from_secs(10);

    for mut request in server.incoming_requests() {
        if request.url() != "/ask" || *request.method() != tiny_http::Method::Post {
            let _ = request.respond(json_response(404, r#"{"error":"not found -- POST /ask"}"#));
            continue;
        }

        let mut body = String::new();
        if let Err(e) = request.as_reader().read_to_string(&mut body) {
            let _ = request.respond(json_response(400, &serde_json::json!({"error": format!("could not read body: {e}")}).to_string()));
            continue;
        }

        let parsed: serde_json::Value = match serde_json::from_str(&body) {
            Ok(v) => v,
            Err(e) => {
                let _ = request.respond(json_response(400, &serde_json::json!({"error": format!("invalid JSON body: {e}")}).to_string()));
                continue;
            }
        };
        let run_id = parsed.get("run_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let instruction = parsed.get("instruction").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if run_id.is_empty() || instruction.trim().is_empty() {
            let _ = request.respond(json_response(400, r#"{"error":"run_id and instruction are required"}"#));
            continue;
        }

        {
            let mut guard = last_request.lock().expect("rate-limit mutex poisoned");
            let now = Instant::now();
            if let Some(prev) = guard.get(&run_id) {
                if now.duration_since(*prev) < MIN_INTERVAL {
                    let _ = request.respond(json_response(429, r#"{"error":"too many requests for this run -- wait a few seconds"}"#));
                    continue;
                }
            }
            guard.insert(run_id.clone(), now);
        }

        match ask(api_base, &run_id, &instruction) {
            Ok(response) => {
                let _ = request.respond(json_response(200, &serde_json::json!({"response": response}).to_string()));
            }
            Err(e) => {
                let _ = request.respond(json_response(502, &serde_json::json!({"error": e}).to_string()));
            }
        }
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_prompt_embeds_the_real_context_and_states_advice_only_scope() {
        let context = r#"{"state":{"run_id":"test-run","paused":false}}"#;
        let prompt = build_system_prompt(context);
        assert!(prompt.contains(context), "the real fetched context must appear verbatim in the prompt");
        assert!(prompt.contains("do NOT execute any action"), "the advice-only boundary must be explicit");
        assert!(prompt.contains("never invent data"), "the no-fabrication instruction must be explicit");
    }
}
