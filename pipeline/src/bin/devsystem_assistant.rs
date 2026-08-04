//! Real devsystem.assistant role-filler -- the smallest honest slice of the
//! operator's "Assistent" request: "ein LLM Agent... wie bei flappy editor, der
//! auch ausgetauscht werden kann, es ist nur eine spezialisierte Rolle." Uses the
//! exact proven, isolated pattern CADS-flappy-demo's own handlers use
//! (`${CT_LLM_CMD:-claude} -p ... --disallowedTools ... --append-system-prompt
//! ...`, verified directly against this host, not assumed), grounded in a run's
//! real current state fetched from devsystem-web -- never invented data.
//!
//! v1 was deliberately ADVICE ONLY, matching the operator's original framing --
//! "Die Task sollen eigentlich nur im absoluten Notfall vom Menschen angepasst
//! werden... so dass ich nicht etwas in den grundsätzlich formalisierten
//! Requirement- und Organisationsprozess negativ eingreife." The operator later
//! reversed that explicitly: told the assistant "Eintragen musst du M1-M3 selbst
//! im Milestones-Panel" and pushed back -- "der Sinn soll sein, das der
//! Devsystem Assistent alles fuer mich eintragen und alles fuer mich ueberpruefen
//! kann." This v2 slice gives it real, narrow write access to exactly two kinds
//! of run state (milestones, backlog items) it can act on directly, still via
//! pure text generation -- the LLM itself keeps zero tool access
//! (`ASSISTANT_DISALLOWED_TOOLS` disallows Edit/Write/Bash/WebFetch/WebSearch/
//! Agent, same as art-handler.sh's isolated role). It signals intent to act by
//! emitting a structured `devsystem-actions` JSON block in its own reply text;
//! this trusted Rust bridge (never the LLM) is what actually calls back into
//! devsystem-web's real API and reports honestly what happened. Anything beyond
//! these two data kinds (e.g. filing a GitHub feature request) is deliberately
//! out of scope for this slice -- the operator wants that kind of
//! externally-visible action discussed first, not auto-executed.
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

const ACTIONS_FENCE_OPEN: &str = "```devsystem-actions";
const ACTIONS_FENCE_CLOSE: &str = "```";

fn build_system_prompt(context: &str) -> String {
    format!(
        "You are devsystem.assistant, a specialized role in The Development System -- \
         a real, self-optimizing, agent-driven pipeline (CADS-Tunnel#382). Your job is \
         to help the human operator understand, control, and optimize a real pipeline \
         run without them having to hand-edit raw state directly. Give concrete, \
         grounded advice based ONLY on the real current run state given below -- never \
         invent data that isn't there, and say plainly if the state doesn't contain \
         enough information to answer. Be concise and reference real field values from \
         the state. When presenting structured data with more than two real fields (a \
         status summary, a comparison, a per-iteration/per-role breakdown), use a real \
         Markdown pipe table (`| Field | Value |` with a `|---|---|` separator row) \
         instead of an inline arrow-chain or a loose list -- the GUI renders real \
         tables properly, not ad-hoc formatting.\n\n\
         You CAN take real action on exactly two kinds of run state: milestones and \
         backlog items. When the operator asks you to add a milestone, check one off, \
         add a backlog item, or mark one done -- and their intent is clear and \
         unambiguous -- do it yourself instead of telling them to enter it by hand. To \
         act, end your reply with a fenced block exactly like this (include it ONLY \
         when you are actually taking action; omit it entirely otherwise -- never emit \
         an empty or placeholder block):\n\
         {ACTIONS_FENCE_OPEN}\n\
         [{{\"type\":\"add_milestone\",\"description\":\"...\"}},{{\"type\":\"toggle_milestone\",\"index\":0}},{{\"type\":\"add_backlog_item\",\"text\":\"...\"}},{{\"type\":\"toggle_backlog_item\",\"index\":0}}]\n\
         {ACTIONS_FENCE_CLOSE}\n\
         Indices refer to the real state.milestones/state.backlog arrays already shown \
         to you below -- never guess an index you can't see there. Never invent or add \
         a milestone/backlog item the operator didn't actually ask for, and never mark \
         one achieved/done unless the operator told you it's done or clearly confirmed \
         it. If a request is ambiguous, or you're not confident it's safe to act on, \
         say so in prose and ask instead of emitting an action. You have NO other tool \
         or system access in this version -- only these four action types against \
         these two kinds of data; for anything else (e.g. a code change, a GitHub \
         issue, a different panel's data) tell the operator what you'd want to do and \
         let them decide.\n\n\
         Current real run state (JSON):\n{context}"
    )
}

/// One real, narrow action the assistant can take on the operator's behalf --
/// deliberately just these two kinds of run state (see module doc). Anything
/// the LLM asks for outside this shape simply fails to deserialize and is
/// reported as a parse error, never silently ignored.
#[derive(Debug, Clone, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Action {
    AddMilestone { description: String },
    ToggleMilestone { index: usize },
    AddBacklogItem { text: String },
    ToggleBacklogItem { index: usize },
}

/// Pulls a trailing ` ```devsystem-actions ... ``` ` block out of the LLM's raw
/// reply text. Returns the text with that block removed (what the human should
/// actually see) plus the parsed actions. If no block is present, the text and
/// an empty action list come back untouched -- the common case, a purely
/// advisory reply. If a block is present but malformed (unclosed or not valid
/// JSON), the ORIGINAL text is returned untouched (nothing silently hidden)
/// together with an explicit parse-error message the caller must surface, not
/// swallow.
fn extract_actions(reply_text: &str) -> (String, Vec<Action>, Option<String>) {
    let Some(start) = reply_text.find(ACTIONS_FENCE_OPEN) else {
        return (reply_text.to_string(), Vec::new(), None);
    };
    let after_open = &reply_text[start + ACTIONS_FENCE_OPEN.len()..];
    let Some(close_rel) = after_open.find(ACTIONS_FENCE_CLOSE) else {
        return (reply_text.to_string(), Vec::new(), Some("a devsystem-actions block was opened but never closed -- no actions were taken".to_string()));
    };
    let json_block = after_open[..close_rel].trim();
    match serde_json::from_str::<Vec<Action>>(json_block) {
        Ok(actions) => {
            let display = format!("{}{}", &reply_text[..start], &after_open[close_rel + ACTIONS_FENCE_CLOSE.len()..]);
            (display.trim().to_string(), actions, None)
        }
        Err(e) => (reply_text.to_string(), Vec::new(), Some(format!("the devsystem-actions block did not parse as valid JSON ({e}) -- no actions were taken"))),
    }
}

/// Actually performs one action against devsystem-web's real, already-existing
/// milestone/backlog API (the exact endpoints the human-driven panels use) --
/// this is the one place the LLM's stated intent turns into a real write.
/// Always returns a human-readable line describing what really happened,
/// success or failure, so the operator never has to guess.
fn apply_action(client: &reqwest::blocking::Client, api_base: &str, run_id: &str, action: &Action) -> String {
    let base = api_base.trim_end_matches('/');
    let (method_desc, url, body): (String, String, serde_json::Value) = match action {
        Action::AddMilestone { description } => {
            (format!("add milestone \"{description}\""), format!("{base}/api/runs/{run_id}/milestones"), serde_json::json!({"description": description}))
        }
        Action::ToggleMilestone { index } => (format!("toggle milestone #{index}"), format!("{base}/api/runs/{run_id}/milestones/{index}/toggle"), serde_json::json!({})),
        Action::AddBacklogItem { text } => (format!("add backlog item \"{text}\""), format!("{base}/api/runs/{run_id}/backlog"), serde_json::json!({"text": text})),
        Action::ToggleBacklogItem { index } => (format!("toggle backlog item #{index}"), format!("{base}/api/runs/{run_id}/backlog/{index}/toggle"), serde_json::json!({})),
    };
    match client.post(&url).json(&body).send() {
        Ok(resp) if resp.status().is_success() => format!("done: {method_desc}"),
        Ok(resp) => {
            let status = resp.status();
            let text = resp.text().unwrap_or_default();
            format!("FAILED to {method_desc}: HTTP {status}: {text}")
        }
        Err(e) => format!("FAILED to {method_desc}: could not reach {url}: {e}"),
    }
}

fn apply_actions(api_base: &str, run_id: &str, actions: &[Action]) -> Vec<String> {
    let client = reqwest::blocking::Client::builder().timeout(Duration::from_secs(10)).build().expect("build blocking http client");
    actions.iter().map(|a| apply_action(&client, api_base, run_id, a)).collect()
}

/// Renders the human-visible reply: the LLM's own (action-block-stripped)
/// prose, plus an honest "Actions taken" section listing exactly what was
/// attempted and whether it really succeeded -- present only when there was
/// something to report, never fabricated.
fn render_reply_with_action_results(display_text: &str, results: &[String], parse_error: Option<&str>) -> String {
    let mut out = display_text.to_string();
    if let Some(err) = parse_error {
        out.push_str(&format!("\n\n---\n_(tried to take an action but it failed: {err})_"));
    } else if !results.is_empty() {
        out.push_str("\n\n---\n**Actions taken:**\n");
        for r in results {
            out.push_str(&format!("- {r}\n"));
        }
    }
    out
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
    let mut reply = ask_llm(instruction, &build_system_prompt(&context))?;
    let (display_text, actions, parse_error) = extract_actions(&reply.text);
    let results = apply_actions(api_base, run_id, &actions);
    reply.text = render_reply_with_action_results(&display_text, &results, parse_error.as_deref());
    Ok(reply)
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
    fn system_prompt_embeds_the_real_context_and_states_the_narrow_action_boundary() {
        let context = r#"{"state":{"run_id":"test-run","paused":false}}"#;
        let prompt = build_system_prompt(context);
        assert!(prompt.contains(context), "the real fetched context must appear verbatim in the prompt");
        assert!(prompt.contains("never invent data"), "the no-fabrication instruction must be explicit");
        assert!(prompt.contains("Markdown pipe table"), "structured-data replies should be steered toward real tables the GUI can actually render");
        assert!(prompt.contains(ACTIONS_FENCE_OPEN), "the prompt must teach the LLM the exact action-block contract");
        assert!(prompt.contains("add_milestone") && prompt.contains("toggle_backlog_item"), "all four real action types must be documented");
        assert!(prompt.contains("NO other tool or system access"), "the action capability must be explicitly bounded to just these two data kinds");
    }

    #[test]
    fn extract_actions_leaves_a_purely_advisory_reply_completely_untouched() {
        let text = "You should iterate on the plan stage next.";
        let (display, actions, err) = extract_actions(text);
        assert_eq!(display, text);
        assert!(actions.is_empty());
        assert!(err.is_none());
    }

    #[test]
    fn extract_actions_parses_a_real_action_block_and_strips_it_from_the_display_text() {
        let text = "Done -- I've added the milestone.\n\n```devsystem-actions\n[{\"type\":\"add_milestone\",\"description\":\"M1: ship the APK\"}]\n```";
        let (display, actions, err) = extract_actions(text);
        assert_eq!(display, "Done -- I've added the milestone.");
        assert_eq!(actions, vec![Action::AddMilestone { description: "M1: ship the APK".to_string() }]);
        assert!(err.is_none());
    }

    #[test]
    fn extract_actions_parses_all_four_real_action_types() {
        let text = "```devsystem-actions\n[{\"type\":\"add_milestone\",\"description\":\"M1\"},{\"type\":\"toggle_milestone\",\"index\":2},{\"type\":\"add_backlog_item\",\"text\":\"write tests\"},{\"type\":\"toggle_backlog_item\",\"index\":0}]\n```";
        let (_, actions, err) = extract_actions(text);
        assert!(err.is_none());
        assert_eq!(
            actions,
            vec![
                Action::AddMilestone { description: "M1".to_string() },
                Action::ToggleMilestone { index: 2 },
                Action::AddBacklogItem { text: "write tests".to_string() },
                Action::ToggleBacklogItem { index: 0 },
            ]
        );
    }

    #[test]
    fn extract_actions_on_malformed_json_reports_the_error_and_takes_no_action() {
        let text = "```devsystem-actions\nnot valid json at all\n```";
        let (display, actions, err) = extract_actions(text);
        assert_eq!(display, text, "malformed block must leave the original text untouched, nothing silently hidden");
        assert!(actions.is_empty());
        assert!(err.unwrap().contains("did not parse"));
    }

    #[test]
    fn extract_actions_on_an_unclosed_block_reports_the_error_and_takes_no_action() {
        let text = "```devsystem-actions\n[{\"type\":\"add_milestone\",\"description\":\"x\"}]";
        let (display, actions, err) = extract_actions(text);
        assert_eq!(display, text);
        assert!(actions.is_empty());
        assert!(err.unwrap().contains("never closed"));
    }

    #[test]
    fn action_serde_rejects_an_unknown_action_type_instead_of_silently_dropping_it() {
        let block = r#"[{"type":"delete_everything","index":0}]"#;
        let err = serde_json::from_str::<Vec<Action>>(block).expect_err("an unknown action type must fail to deserialize");
        assert!(!err.to_string().is_empty());
    }

    /// A tiny real HTTP server standing in for devsystem-web -- proves the
    /// exact method/path/body apply_action sends, not just that it compiles.
    fn spawn_capturing_server() -> (String, std::sync::mpsc::Receiver<(String, String, String)>) {
        let server = tiny_http::Server::http("127.0.0.1:0").expect("bind ephemeral port");
        let addr = format!("http://{}", server.server_addr());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for mut req in server.incoming_requests() {
                let method = req.method().to_string();
                let url = req.url().to_string();
                let mut body = String::new();
                let _ = req.as_reader().read_to_string(&mut body);
                let _ = tx.send((method, url, body));
                let _ = req.respond(tiny_http::Response::from_string("{}").with_status_code(200));
            }
        });
        (addr, rx)
    }

    #[test]
    fn apply_action_posts_the_real_add_milestone_request_devsystem_web_actually_expects() {
        let (addr, rx) = spawn_capturing_server();
        let client = reqwest::blocking::Client::new();
        let result = apply_action(&client, &addr, "my-run", &Action::AddMilestone { description: "M1: ship it".to_string() });
        assert!(result.starts_with("done:"), "a 200 response must be reported as success: {result}");
        let (method, url, body) = rx.recv_timeout(Duration::from_secs(2)).expect("server must have received a request");
        assert_eq!(method, "POST");
        assert_eq!(url, "/api/runs/my-run/milestones");
        let parsed: serde_json::Value = serde_json::from_str(&body).expect("body must be valid JSON");
        assert_eq!(parsed["description"], "M1: ship it");
    }

    #[test]
    fn apply_action_posts_the_real_toggle_backlog_item_request() {
        let (addr, rx) = spawn_capturing_server();
        let client = reqwest::blocking::Client::new();
        let result = apply_action(&client, &addr, "my-run", &Action::ToggleBacklogItem { index: 3 });
        assert!(result.starts_with("done:"));
        let (method, url, _) = rx.recv_timeout(Duration::from_secs(2)).expect("server must have received a request");
        assert_eq!(method, "POST");
        assert_eq!(url, "/api/runs/my-run/backlog/3/toggle");
    }

    #[test]
    fn apply_action_surfaces_a_real_backend_failure_honestly_not_as_a_fabricated_success() {
        // Nothing listening on this port -- a real, reproducible connection failure.
        let client = reqwest::blocking::Client::builder().timeout(Duration::from_millis(500)).build().unwrap();
        let result = apply_action(&client, "http://127.0.0.1:1", "my-run", &Action::AddMilestone { description: "x".to_string() });
        assert!(result.starts_with("FAILED"), "an unreachable backend must be reported as a failure: {result}");
    }

    #[test]
    fn render_reply_with_action_results_lists_every_result_and_a_purely_advisory_reply_gets_no_actions_section() {
        let advisory = render_reply_with_action_results("just advice, no action taken", &[], None);
        assert_eq!(advisory, "just advice, no action taken");

        let with_actions = render_reply_with_action_results("Added it.", &["done: add milestone \"M1\"".to_string(), "FAILED to toggle milestone #9: HTTP 404 Not Found: no such milestone".to_string()], None);
        assert!(with_actions.contains("Actions taken"));
        assert!(with_actions.contains("done: add milestone \"M1\""));
        assert!(with_actions.contains("FAILED to toggle milestone #9"), "a real failure must be visible to the operator, never hidden");
    }

    #[test]
    fn render_reply_with_action_results_surfaces_a_parse_error_instead_of_silently_dropping_it() {
        let rendered = render_reply_with_action_results("some reply", &[], Some("the devsystem-actions block did not parse as valid JSON"));
        assert!(rendered.contains("tried to take an action but it failed"));
        assert!(rendered.contains("did not parse as valid JSON"));
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
