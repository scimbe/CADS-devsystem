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
        Ok(resp) if resp.status().is_success() => resp
            .text()
            .map(|body| condense_history(&body))
            .map_err(|e| format!("could not read response body from {url}: {e}")),
        Ok(resp) => Err(format!("could not fetch run context from {url}: HTTP {}", resp.status())),
        Err(e) => Err(format!("could not reach {url}: {e}")),
    }
}

/// A real run's `history` grows one full-prose feedback entry per iteration
/// forever (13+ already, each several hundred words) -- fed to the LLM
/// unbounded, this made real calls take 90+ seconds and made the GUI's fetch
/// time out ("Failed to fetch", reported live). Keeps the most recent
/// `KEEP_FULL` iterations verbatim (the actionable ones) and collapses older
/// ones to a one-line index (stage/iteration/succeeded, no prose) so prompt
/// size stays roughly constant regardless of how long the run has been going.
/// Falls back to the original text untouched if the shape isn't what's
/// expected -- never invents data, never silently drops the whole context.
fn condense_history(body: &str) -> String {
    const KEEP_FULL: usize = 6;
    let Ok(mut root) = serde_json::from_str::<serde_json::Value>(body) else {
        return body.to_string();
    };
    let Some(history) = root.pointer_mut("/state/history").and_then(|v| v.as_array_mut()) else {
        return body.to_string();
    };
    let total = history.len();
    if total <= KEEP_FULL {
        return body.to_string();
    }
    let omitted = total - KEEP_FULL;
    let mut condensed: Vec<serde_json::Value> = history
        .drain(..omitted)
        .map(|entry| {
            serde_json::json!({
                "iteration": entry.get("iteration"),
                "stage": entry.get("stage"),
                "succeeded": entry.get("succeeded"),
            })
        })
        .collect();
    condensed.push(serde_json::json!({"note": format!("{omitted} earlier iteration(s) condensed to iteration/stage/succeeded only (feedback text dropped) to keep this prompt a reasonable size; {KEEP_FULL} most recent kept in full below")}));
    condensed.extend(history.drain(..));
    *history = condensed;
    serde_json::to_string(&root).unwrap_or_else(|_| body.to_string())
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
         concise and reference real field values from the state. When presenting \
         structured data with more than two real fields (a status summary, a \
         comparison, a per-iteration/per-role breakdown), use a real Markdown pipe \
         table (`| Field | Value |` with a `|---|---|` separator row) instead of an \
         inline arrow-chain or a loose list -- the GUI renders real tables properly, \
         not ad-hoc formatting.\n\n\
         Current real run state (JSON):\n{context}"
    )
}

/// A real reply plus real token/cost accounting (operator: "am besten auch
/// verbrauchte Token bei der Anfrage und bei der Antwort") -- both come from
/// the exact same `--output-format json` call, not a second/estimated pass.
#[derive(Debug)]
struct LlmReply {
    text: String,
    usage: serde_json::Value,
}

/// Parses `claude --output-format json`'s real stdout shape (verified
/// directly against this host: `{"result": "...", "is_error": bool,
/// "usage": {"input_tokens", "output_tokens", "cache_creation_input_tokens",
/// "cache_read_input_tokens", ...}, "total_cost_usd": f64, ...}`). Pulled out
/// of `ask_llm` so the parsing itself -- the part that can actually be
/// wrong -- is unit-testable without spawning a real subprocess.
fn parse_llm_json_output(stdout: &str) -> Result<LlmReply, String> {
    let parsed: serde_json::Value =
        serde_json::from_str(stdout.trim()).map_err(|e| format!("could not parse LLM CLI's JSON output: {e} (raw: {stdout})"))?;
    if parsed.get("is_error").and_then(|v| v.as_bool()) == Some(true) {
        let msg = parsed.get("result").and_then(|v| v.as_str()).unwrap_or("LLM CLI reported an error with no message");
        return Err(format!("LLM CLI reported an error: {msg}"));
    }
    let text = parsed
        .get("result")
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("LLM CLI's JSON output has no string \"result\" field (raw: {stdout})"))?
        .to_string();
    let tok = |field: &str| parsed.pointer(&format!("/usage/{field}")).and_then(|v| v.as_u64()).unwrap_or(0);
    let usage = serde_json::json!({
        "input_tokens": tok("input_tokens"),
        "output_tokens": tok("output_tokens"),
        "cache_creation_input_tokens": tok("cache_creation_input_tokens"),
        "cache_read_input_tokens": tok("cache_read_input_tokens"),
        "total_cost_usd": parsed.get("total_cost_usd").and_then(|v| v.as_f64()),
    });
    Ok(LlmReply { text, usage })
}

fn ask_llm(instruction: &str, system_prompt: &str) -> Result<LlmReply, String> {
    let llm = env::var("CT_LLM_CMD").unwrap_or_else(|_| "claude".to_string());
    let output = Command::new(&llm)
        .arg("-p")
        .arg(instruction)
        .arg("--output-format")
        .arg("json")
        .arg("--disallowedTools")
        .arg(devsystem_pipeline::ASSISTANT_DISALLOWED_TOOLS.join(","))
        .arg("--append-system-prompt")
        .arg(system_prompt)
        .stdin(Stdio::null())
        .output();

    match output {
        Ok(out) if out.status.success() => parse_llm_json_output(&String::from_utf8_lossy(&out.stdout)),
        Ok(out) => Err(format!("{llm} exited with {}: {}", out.status, String::from_utf8_lossy(&out.stderr))),
        Err(e) => Err(format!("could not run {llm}: {e} (set CT_LLM_CMD to point at a non-interactive LLM CLI)")),
    }
}

fn ask(api_base: &str, run_id: &str, instruction: &str) -> Result<LlmReply, String> {
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
        Ok(reply) => {
            print!("{}", reply.text);
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
/// `{"response": "...", "usage": {...}}`. Meant to sit behind the same reverse-proxy gate as
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
            Ok(reply) => {
                let _ = request.respond(json_response(200, &serde_json::json!({"response": reply.text, "usage": reply.usage}).to_string()));
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

    // Real captured shape from `claude -p "say hi in exactly one word"
    // --output-format json ...`, run directly against this host -- not a
    // hand-invented fixture (trimmed of fields this parser doesn't read).
    const REAL_CLAUDE_JSON_OUTPUT: &str = r#"{"is_error":false,"duration_api_ms":1749,"num_turns":1,"stop_reason":"end_turn","session_id":"2d85529b","total_cost_usd":0.16173249999999997,"usage":{"input_tokens":2,"cache_creation_input_tokens":15451,"cache_read_input_tokens":14175,"output_tokens":5,"service_tier":"standard"},"result":"Hi","type":"result"}"#;

    #[test]
    fn parses_the_real_claude_cli_json_output_shape() {
        let reply = parse_llm_json_output(REAL_CLAUDE_JSON_OUTPUT).expect("real captured output must parse");
        assert_eq!(reply.text, "Hi");
        assert_eq!(reply.usage["input_tokens"], 2);
        assert_eq!(reply.usage["output_tokens"], 5);
        assert_eq!(reply.usage["cache_creation_input_tokens"], 15451);
        assert_eq!(reply.usage["cache_read_input_tokens"], 14175);
        assert!((reply.usage["total_cost_usd"].as_f64().unwrap() - 0.16173249999999997).abs() < 1e-12);
    }

    #[test]
    fn surfaces_a_real_is_error_result_as_an_error_not_a_fabricated_success() {
        let output = r#"{"is_error":true,"result":"the model refused","usage":{"input_tokens":1,"output_tokens":1}}"#;
        let err = parse_llm_json_output(output).expect_err("is_error:true must surface as Err");
        assert!(err.contains("the model refused"), "the real error text must be preserved: {err}");
    }

    #[test]
    fn missing_usage_fields_default_to_zero_not_a_parse_failure() {
        // A future CLI version or a different provider might omit some usage
        // sub-fields -- this must degrade to 0, not fail the whole response.
        let output = r#"{"is_error":false,"result":"ok","usage":{"input_tokens":3}}"#;
        let reply = parse_llm_json_output(output).expect("partial usage must still parse");
        assert_eq!(reply.usage["input_tokens"], 3);
        assert_eq!(reply.usage["output_tokens"], 0);
        assert_eq!(reply.usage["cache_creation_input_tokens"], 0);
    }

    #[test]
    fn malformed_json_output_is_a_real_error_not_a_panic() {
        let err = parse_llm_json_output("not json").expect_err("garbage stdout must error, not panic");
        assert!(err.contains("could not parse"));
    }

    #[test]
    fn missing_result_field_is_a_real_error() {
        let output = r#"{"is_error":false,"usage":{}}"#;
        let err = parse_llm_json_output(output).expect_err("no result field must error");
        assert!(err.contains("no string"));
    }

    #[test]
    fn system_prompt_embeds_the_real_context_and_states_advice_only_scope() {
        let context = r#"{"state":{"run_id":"test-run","paused":false}}"#;
        let prompt = build_system_prompt(context);
        assert!(prompt.contains(context), "the real fetched context must appear verbatim in the prompt");
        assert!(prompt.contains("do NOT execute any action"), "the advice-only boundary must be explicit");
        assert!(prompt.contains("never invent data"), "the no-fabrication instruction must be explicit");
        assert!(prompt.contains("Markdown pipe table"), "structured-data replies should be steered toward real tables the GUI can actually render");
    }

    fn history_entry(iteration: u32, feedback: &str) -> serde_json::Value {
        serde_json::json!({"iteration": iteration, "stage": "devsystem.test", "succeeded": true, "feedback": feedback, "proposals": []})
    }

    #[test]
    fn a_short_history_is_left_completely_untouched() {
        let entries: Vec<_> = (1..=3).map(|i| history_entry(i, "short real feedback")).collect();
        let body = serde_json::json!({"state": {"history": entries}}).to_string();
        assert_eq!(condense_history(&body), body, "nothing to condense below the keep-full threshold");
    }

    #[test]
    fn a_long_history_keeps_the_most_recent_entries_full_and_condenses_the_rest() {
        // Real iterations' feedback runs several hundred words (see any real
        // entry in runs/*/state.json) -- a short fixture wouldn't exercise the
        // actual size problem this fix addresses.
        let paragraph = "a real, long, verbose feedback paragraph describing exactly what was built, how it was verified hermetically, and what commit it landed as, repeated to resemble a genuine multi-sentence iteration report. ".repeat(15);
        let entries: Vec<_> = (1..=13).map(|i| history_entry(i, &paragraph)).collect();
        let body = serde_json::json!({"state": {"history": entries}}).to_string();
        let condensed = condense_history(&body);

        assert!(condensed.len() < body.len() / 2, "condensing a long history must substantially shrink the prompt, not just trim it");
        for i in 8..=13 {
            assert!(condensed.contains(&format!("\"iteration\":{i}")), "recent iteration {i} must stay in full, in order");
        }
        assert_eq!(condensed.matches(&paragraph).count(), 6, "exactly the KEEP_FULL most recent iterations' prose must survive, the rest dropped");
        assert!(condensed.contains("7 earlier iteration"), "how many were condensed must be stated honestly, not silently dropped");

        let parsed: serde_json::Value = serde_json::from_str(&condensed).expect("condensed output is still valid JSON");
        assert!(parsed.pointer("/state/history").unwrap().is_array());
    }

    #[test]
    fn malformed_or_unexpected_json_falls_back_to_the_original_text_untouched() {
        let not_json = "not json at all";
        assert_eq!(condense_history(not_json), not_json);
        let no_history = r#"{"state":{"run_id":"x"}}"#;
        assert_eq!(condense_history(no_history), no_history);
    }
}
