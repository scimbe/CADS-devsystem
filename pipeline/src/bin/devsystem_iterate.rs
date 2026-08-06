//! Real CLI that drives one iteration of a run's super loop end to end: load the run's
//! persisted spec + state (or start fresh), fold in a real [`IterationRecord`] read from
//! a JSON file, apply any proposals, persist the result, and print the outcome. This is
//! not a simulation harness -- every invocation is a real run of `runner::run_iteration`
//! against real files under `runs/<run_id>/` in this repo (#382).
//!
//! Usage:
//!   devsystem_iterate <run_id> <record.json>                          (local, on this host's runs/)
//!   devsystem_iterate --remote <api-base-url> <run_id> <record.json>  (HTTP, for an external agent)
//!
//! The `--remote` mode exists because an auction winner has never had a real way to
//! submit real work back -- `devsystem_offer` already lets an external agent bid
//! remotely, but the only way to submit an actual iteration result was always this
//! binary's local mode, which requires filesystem access to `runs/<run_id>/` on
//! *this* host. `POST /api/runs/{id}/iterate` (devsystem-web) already exists and
//! already runs the exact same `run_iteration` core logic -- what was actually
//! missing was just this CLI companion, not a new server endpoint.
//!
//! **Gate auth (issue #7 / CADS-Tunnel#382-follow):** a public deployment fronted by
//! CADS-Tunnel's login gate (`require_login` on) 302s every `/api/*` call, including
//! this one -- a browser-oriented Keycloak SSO redirect a headless caller can never
//! complete. CADS-Tunnel's gate now also accepts a real Keycloak `client_credentials`
//! bearer token (task #42's M2M service accounts) as an alternative to the cookie
//! session, checked against the same tunnel-owner-controlled allow-list. Set
//! `DEVSYSTEM_OIDC_TOKEN_URL` + `DEVSYSTEM_OIDC_CLIENT_ID` + `DEVSYSTEM_OIDC_CLIENT_SECRET`
//! to have `--remote` fetch a fresh token and send it as `Authorization: Bearer`; all
//! three unset means "no auth header," matching this CLI's original behavior against
//! an ungated deployment.

use devsystem_pipeline::envelope::{append_to_memory_log, envelope_from_iteration};
use devsystem_pipeline::runner::{load_or_init_run, persist_run, run_iteration, valid_run_id, RunOutcome};
use devsystem_pipeline::{validate_proposals, IterationRecord};
use std::env;
use std::fs;
use std::path::PathBuf;

fn run_local(run_id: &str, record_path: &str) -> std::process::ExitCode {
    // Real gap found live by the incompetent-agent stress test (#382 goal doc
    // §8, 2026-08-06): this binary builds runs/<run_id>/ straight from a raw
    // CLI argument, with no HTTP layer -- and no equivalent of devsystem-web's
    // own path-traversal guard -- anywhere in between. Confirmed live before
    // this fix: `devsystem_iterate ../traversal-poc-marker record.json` wrote
    // a real spec.json/state.json pair directly into this repo's own root,
    // completely outside runs/. Checked before any filesystem access at all.
    if !valid_run_id(run_id) {
        eprintln!("rejected: run_id {run_id:?} must be non-empty alphanumeric/-/_ only");
        return std::process::ExitCode::FAILURE;
    }
    let run_dir = PathBuf::from("runs").join(run_id);
    let (mut spec, mut state) = load_or_init_run(&run_dir, run_id).expect("load or initialize run");

    let record: IterationRecord =
        serde_json::from_str(&fs::read_to_string(record_path).expect("read record.json")).expect("valid record.json");

    // Real gap found live by the incompetent-agent stress test's twelfth run (#382
    // goal doc §8, 2026-08-06): this local path calls run_iteration directly, with
    // no HTTP layer in between at all -- devsystem-web's own equivalent check
    // (POST /api/runs/{id}/iterate) never protected this entry point, since it's a
    // separate binary reading runs/<run_id>/ straight off disk. Confirmed live: the
    // exact same garbage proposal devsystem-web now rejects with a real 400 still
    // sailed straight through here and permanently added a real, empty-tag
    // ServiceType::Custom("") role to this run's on-disk spec.json. Checked before
    // any write happens (memory log append, persist_run) -- an invalid record.json
    // must leave the run's files completely untouched, not partially applied.
    if let Err(e) = validate_proposals(&record.proposals) {
        eprintln!("rejected: {e}");
        return std::process::ExitCode::FAILURE;
    }

    // devsystem.remember, made real: every iteration's zylos envelope is appended to
    // the run's durable memory log before anything else happens to `record`.
    let memory_path = run_dir.join("memory.jsonl");
    let envelope = envelope_from_iteration(&record, &state.requirements);
    append_to_memory_log(&memory_path, &envelope).expect("append to memory.jsonl");

    let criteria = state.criteria;
    let outcome = run_iteration(&mut spec, &mut state, record, &criteria);

    persist_run(&run_dir, &spec, &state).expect("persist run");

    println!("run_id={run_id} iteration_outcome={outcome:?} roles_now={} added_stages={:?}", spec.roles.len(), state.added_stages);
    match outcome {
        RunOutcome::Abort => std::process::ExitCode::FAILURE,
        RunOutcome::CheckinDue => {
            println!("CHECK-IN REQUIRED before the next iteration -- do not proceed unsupervised.");
            std::process::ExitCode::SUCCESS
        }
        RunOutcome::Continue => std::process::ExitCode::SUCCESS,
    }
}

/// The subset of an [`IterationRecord`] that `POST /api/runs/{id}/iterate` actually
/// accepts -- `run_id` and `iteration` are deliberately NOT sent: the server derives
/// `run_id` from the URL path and `iteration` from the real persisted history length,
/// exactly like the local path derives nothing and trusts the file. Pulled out as its
/// own function so the request shape is unit-testable without a real HTTP round trip.
fn remote_request_body(record: &IterationRecord) -> serde_json::Value {
    serde_json::json!({
        "stage": record.stage,
        "feedback": record.feedback,
        "succeeded": record.succeeded,
        "proposals": record.proposals,
        "requirement_indices": record.requirement_indices,
    })
}

/// A real, structured summary line from devsystem-web's own iterate-response shape
/// (`{"outcome": "...", "iteration": N, "roles_now": N, "added_stages": [...]}`).
/// Kept separate from the actual HTTP call so a malformed/unexpected response shape
/// is a real, testable, honest error -- never a silent panic or a fabricated summary.
#[derive(Debug)]
struct RemoteOutcome {
    summary: String,
    checkin_required: bool,
    should_fail: bool,
}

fn parse_remote_response(run_id: &str, body: &str) -> Result<RemoteOutcome, String> {
    let parsed: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("could not parse devsystem-web's iterate response as JSON: {e} (raw: {body})"))?;
    let outcome = parsed.get("outcome").and_then(|v| v.as_str()).ok_or_else(|| format!("response has no string \"outcome\" field (raw: {body})"))?;
    let iteration = parsed.get("iteration").and_then(|v| v.as_u64()).unwrap_or(0);
    let roles_now = parsed.get("roles_now").and_then(|v| v.as_u64()).unwrap_or(0);
    let added_stages: Vec<String> = parsed
        .get("added_stages")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    Ok(RemoteOutcome {
        summary: format!("run_id={run_id} iteration={iteration} iteration_outcome={outcome} roles_now={roles_now} added_stages={added_stages:?}"),
        checkin_required: outcome == "CheckinDue",
        should_fail: outcome == "Abort",
    })
}

/// Real `client_credentials` token fetch against a Keycloak (or any OIDC-compliant)
/// token endpoint -- the same grant type task #42's M2M service accounts issue.
/// Returns the raw `access_token`; the caller decides what to do with it, so this
/// stays testable against a plain HTTP mock, not a real Keycloak.
fn fetch_client_credentials_token(token_url: &str, client_id: &str, client_secret: &str) -> Result<String, String> {
    let client = reqwest::blocking::Client::new();
    let resp = client
        .post(token_url)
        .form(&[("grant_type", "client_credentials"), ("client_id", client_id), ("client_secret", client_secret)])
        .send()
        .map_err(|e| format!("could not reach token endpoint {token_url}: {e}"))?;
    let status = resp.status();
    let body = resp.text().unwrap_or_default();
    if !status.is_success() {
        return Err(format!("token endpoint {token_url} returned HTTP {status}: {body}"));
    }
    let parsed: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("token endpoint response wasn't valid JSON: {e} (raw: {body})"))?;
    parsed
        .get("access_token")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| format!("token endpoint response has no string \"access_token\" field (raw: {body})"))
}

/// The three env vars that, together, opt `--remote` into sending a real M2M bearer
/// token (issue #7 / CADS-Tunnel#382-follow). All three unset -> `None`, meaning "no
/// auth header" -- this CLI's original behavior against an ungated deployment. Any
/// subset set but not all three is a real misconfiguration, not silently ignored.
/// Pulled apart from env-reading so the tri-state logic is testable without mutating
/// process-global env vars (which would race across parallel `cargo test` threads).
fn bearer_token_from_parts(
    token_url: Option<String>,
    client_id: Option<String>,
    client_secret: Option<String>,
) -> Result<Option<String>, String> {
    match (token_url, client_id, client_secret) {
        (None, None, None) => Ok(None),
        (Some(url), Some(id), Some(secret)) => fetch_client_credentials_token(&url, &id, &secret).map(Some),
        _ => Err(
            "DEVSYSTEM_OIDC_TOKEN_URL/DEVSYSTEM_OIDC_CLIENT_ID/DEVSYSTEM_OIDC_CLIENT_SECRET must be set together or not at all"
                .to_string(),
        ),
    }
}

fn bearer_token_from_env() -> Result<Option<String>, String> {
    bearer_token_from_parts(
        std::env::var("DEVSYSTEM_OIDC_TOKEN_URL").ok(),
        std::env::var("DEVSYSTEM_OIDC_CLIENT_ID").ok(),
        std::env::var("DEVSYSTEM_OIDC_CLIENT_SECRET").ok(),
    )
}

fn run_remote(api_base: &str, run_id: &str, record_path: &str, bearer: Option<String>) -> std::process::ExitCode {
    let record: IterationRecord =
        serde_json::from_str(&fs::read_to_string(record_path).expect("read record.json")).expect("valid record.json");
    if record.run_id != run_id {
        eprintln!(
            "warning: record.json's run_id (\"{}\") does not match the run_id argument (\"{run_id}\") -- \
             submitting against \"{run_id}\" regardless, since the server derives run identity from the URL, not the file",
            record.run_id
        );
    }

    let url = format!("{}/api/runs/{}/iterate", api_base.trim_end_matches('/'), run_id);
    // Same false-positive class devsystem_offer's #388 fix already closed: reqwest's
    // default redirect policy silently follows a still-gated endpoint's 302 to the
    // gate's own login page (itself a real 200), so a naive status check here would
    // either misreport a fabricated success or, at best, drown a real "you're not
    // authenticated" in a wall of login-page HTML instead of saying so plainly.
    // Found live against this deployment (2026-08-05): /api/runs/{id}/iterate
    // currently *is* gated, unlike an earlier report that it wasn't -- this fix
    // makes that fact loud and unambiguous instead of a confusing parse failure.
    let client = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap_or_else(|_| reqwest::blocking::Client::new());
    let mut req = client.post(&url).json(&remote_request_body(&record));
    if let Some(token) = &bearer {
        req = req.bearer_auth(token);
    }
    let resp = match req.send() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("could not reach {url}: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let status = resp.status();
    if status.is_redirection() {
        let location = resp.headers().get("location").and_then(|v| v.to_str().ok()).unwrap_or("(no Location header)").to_string();
        let hint = if bearer.is_some() {
            "an M2M bearer token was sent but the gate still redirected -- check the token's subject is on this hostname's login-allowlist"
        } else {
            "this deployment currently requires gate login for this endpoint -- set DEVSYSTEM_OIDC_TOKEN_URL/CLIENT_ID/CLIENT_SECRET for M2M bearer auth"
        };
        eprintln!("remote iterate failed: HTTP {status} redirect to {location} -- {hint}, no offer/iteration was submitted");
        return std::process::ExitCode::FAILURE;
    }
    let body = resp.text().unwrap_or_default();
    if !status.is_success() {
        eprintln!("remote iterate failed: HTTP {status}: {body}");
        return std::process::ExitCode::FAILURE;
    }

    match parse_remote_response(run_id, &body) {
        Ok(outcome) => {
            println!("{}", outcome.summary);
            if outcome.checkin_required {
                println!("CHECK-IN REQUIRED before the next iteration -- do not proceed unsupervised.");
            }
            if outcome.should_fail {
                std::process::ExitCode::FAILURE
            } else {
                std::process::ExitCode::SUCCESS
            }
        }
        Err(e) => {
            eprintln!("{e}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn main() -> std::process::ExitCode {
    let mut args = env::args().skip(1);
    let first = args.next().expect(
        "usage: devsystem_iterate <run_id> <record.json>\n   or: devsystem_iterate --remote <api-base-url> <run_id> <record.json>",
    );

    if first == "--remote" {
        let api_base = args.next().expect("usage: devsystem_iterate --remote <api-base-url> <run_id> <record.json>");
        let run_id = args.next().expect("usage: devsystem_iterate --remote <api-base-url> <run_id> <record.json>");
        let record_path = args.next().expect("usage: devsystem_iterate --remote <api-base-url> <run_id> <record.json>");
        let bearer = match bearer_token_from_env() {
            Ok(t) => t,
            Err(e) => {
                eprintln!("could not obtain M2M bearer token: {e}");
                return std::process::ExitCode::FAILURE;
            }
        };
        return run_remote(&api_base, &run_id, &record_path, bearer);
    }

    let run_id = first;
    let record_path = args.next().expect("usage: devsystem_iterate <run_id> <record.json>");
    run_local(&run_id, &record_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use devsystem_pipeline::StageProposal;

    fn record(run_id: &str, stage: &str, succeeded: bool) -> IterationRecord {
        IterationRecord {
            run_id: run_id.to_string(),
            stage: stage.to_string(),
            iteration: 99, // deliberately not 1 -- proves remote_request_body drops this
            feedback: "real feedback text".to_string(),
            proposals: vec![StageProposal {
                proposed_by: stage.to_string(),
                stage_id: "devsystem.new_stage".to_string(),
                tag: "new_tag".to_string(),
                rationale: "a real rationale".to_string(),
                use_existing_service: None,
                units: 1,
                price_ceiling: None,
            }],
            succeeded,
            requirement_indices: Vec::new(),
        }
    }

    #[test]
    fn remote_request_body_sends_exactly_what_the_server_accepts_not_run_id_or_iteration() {
        let r = record("some-run", "devsystem.plan", true);
        let body = remote_request_body(&r);
        assert_eq!(body["stage"], "devsystem.plan");
        assert_eq!(body["feedback"], "real feedback text");
        assert_eq!(body["succeeded"], true);
        assert_eq!(body["proposals"].as_array().unwrap().len(), 1);
        assert_eq!(body["proposals"][0]["stage_id"], "devsystem.new_stage");
        assert_eq!(body["requirement_indices"].as_array().unwrap().len(), 0, "record() claims no requirements, so none should be sent");
        // The server derives these from the URL/its own persisted history --
        // sending them would silently claim authority this CLI doesn't have.
        assert!(body.get("run_id").is_none(), "run_id must not be sent -- the URL path is authoritative");
        assert!(body.get("iteration").is_none(), "iteration must not be sent -- the server's history length is authoritative");
    }

    #[test]
    fn parse_remote_response_reports_continue_as_a_real_success_with_no_checkin_note() {
        let body = r#"{"outcome":"Continue","iteration":3,"roles_now":2,"added_stages":["devsystem.new_stage"]}"#;
        let outcome = parse_remote_response("my-run", body).expect("valid response must parse");
        assert!(outcome.summary.contains("run_id=my-run"));
        assert!(outcome.summary.contains("iteration_outcome=Continue"));
        assert!(outcome.summary.contains("roles_now=2"));
        assert!(outcome.summary.contains("devsystem.new_stage"));
        assert!(!outcome.checkin_required);
        assert!(!outcome.should_fail);
    }

    #[test]
    fn parse_remote_response_flags_checkin_due_without_treating_it_as_a_failure() {
        let body = r#"{"outcome":"CheckinDue","iteration":5,"roles_now":1,"added_stages":[]}"#;
        let outcome = parse_remote_response("my-run", body).expect("valid response must parse");
        assert!(outcome.checkin_required, "CheckinDue must be surfaced, not silently swallowed");
        assert!(!outcome.should_fail, "CheckinDue is not itself a failure -- it's a real, separate signal");
    }

    #[test]
    fn parse_remote_response_treats_abort_as_a_real_failure() {
        let body = r#"{"outcome":"Abort","iteration":20,"roles_now":1,"added_stages":[]}"#;
        let outcome = parse_remote_response("my-run", body).expect("valid response must parse");
        assert!(outcome.should_fail, "Abort must translate to a non-zero exit, matching the local path's std::process::exit(1)");
    }

    #[test]
    fn parse_remote_response_on_malformed_json_is_a_real_error_not_a_panic() {
        let err = parse_remote_response("my-run", "not json").expect_err("garbage body must error, not panic");
        assert!(err.contains("could not parse"));
    }

    #[test]
    fn parse_remote_response_on_missing_outcome_field_is_a_real_error() {
        let err = parse_remote_response("my-run", r#"{"iteration":1}"#).expect_err("no outcome field must error");
        assert!(err.contains("no string"));
    }

    /// (method, url, body, real Authorization header if one was sent) -- one real
    /// captured HTTP request, named so `spawn_capturing_server`'s own signature
    /// doesn't need to spell out the full tuple type at every use site.
    type CapturedRequest = (String, String, String, Option<String>);

    /// A tiny real HTTP server standing in for devsystem-web -- proves the exact
    /// method/path/body/Authorization-header devsystem_iterate --remote actually
    /// sends, not just that remote_request_body compiles. Same pattern as
    /// devsystem_assistant's own apply_action tests.
    fn spawn_capturing_server(response_body: &'static str) -> (String, std::sync::mpsc::Receiver<CapturedRequest>) {
        let server = tiny_http::Server::http("127.0.0.1:0").expect("bind ephemeral port");
        let addr = format!("http://{}", server.server_addr());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            if let Ok(mut req) = server.recv() {
                let method = req.method().to_string();
                let url = req.url().to_string();
                let auth = req
                    .headers()
                    .iter()
                    .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case("authorization"))
                    .map(|h| h.value.as_str().to_string());
                let mut body = String::new();
                let _ = std::io::Read::read_to_string(req.as_reader(), &mut body);
                let _ = tx.send((method, url, body, auth));
                let _ = req.respond(tiny_http::Response::from_string(response_body).with_status_code(200));
            }
        });
        (addr, rx)
    }

    #[test]
    fn run_remote_posts_to_the_real_iterate_path_with_the_real_body_and_succeeds_on_continue() {
        let (addr, rx) = spawn_capturing_server(r#"{"outcome":"Continue","iteration":1,"roles_now":1,"added_stages":[]}"#);
        let dir = std::env::temp_dir().join(format!("devsystem-iterate-remote-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let record_path = dir.join("record.json");
        std::fs::write(&record_path, serde_json::to_string(&record("remote-run", "devsystem.plan", true)).unwrap()).unwrap();

        let code = run_remote(&addr, "remote-run", record_path.to_str().unwrap(), None);
        assert_eq!(code, std::process::ExitCode::SUCCESS);

        let (method, url, body, auth) = rx.recv_timeout(std::time::Duration::from_secs(2)).expect("server must have received a request");
        assert_eq!(method, "POST");
        assert_eq!(url, "/api/runs/remote-run/iterate");
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["stage"], "devsystem.plan");
        assert!(parsed.get("run_id").is_none());
        assert!(auth.is_none(), "no bearer was passed -- no Authorization header should be sent");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn run_remote_surfaces_an_unreachable_server_as_a_real_failure_not_a_panic() {
        let dir = std::env::temp_dir().join(format!("devsystem-iterate-remote-unreachable-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let record_path = dir.join("record.json");
        std::fs::write(&record_path, serde_json::to_string(&record("remote-run", "devsystem.plan", true)).unwrap()).unwrap();

        // Nothing listening on this port -- a real, reproducible connection failure.
        let code = run_remote("http://127.0.0.1:1", "remote-run", record_path.to_str().unwrap(), None);
        assert_eq!(code, std::process::ExitCode::FAILURE);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// #7/#382-follow: when a bearer token IS supplied, it must actually reach the
    /// server as a real `Authorization: Bearer <token>` header -- not just be
    /// accepted as a parameter and silently dropped.
    #[test]
    fn run_remote_sends_a_supplied_bearer_token_as_a_real_authorization_header() {
        let (addr, rx) = spawn_capturing_server(r#"{"outcome":"Continue","iteration":1,"roles_now":1,"added_stages":[]}"#);
        let dir = std::env::temp_dir().join(format!("devsystem-iterate-remote-bearer-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let record_path = dir.join("record.json");
        std::fs::write(&record_path, serde_json::to_string(&record("remote-run", "devsystem.plan", true)).unwrap()).unwrap();

        let code = run_remote(&addr, "remote-run", record_path.to_str().unwrap(), Some("real-m2m-token".to_string()));
        assert_eq!(code, std::process::ExitCode::SUCCESS);

        let (_, _, _, auth) = rx.recv_timeout(std::time::Duration::from_secs(2)).expect("server must have received a request");
        assert_eq!(auth.as_deref(), Some("Bearer real-m2m-token"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn bearer_token_from_parts_with_nothing_set_means_no_auth_header() {
        assert_eq!(bearer_token_from_parts(None, None, None), Ok(None));
    }

    #[test]
    fn bearer_token_from_parts_with_a_partial_set_is_a_real_misconfiguration_error() {
        let err = bearer_token_from_parts(Some("https://kc/token".to_string()), None, None).unwrap_err();
        assert!(err.contains("must be set together"));
        let err = bearer_token_from_parts(None, Some("client-id".to_string()), Some("secret".to_string())).unwrap_err();
        assert!(err.contains("must be set together"));
    }

    /// A tiny real HTTP server standing in for Keycloak's token endpoint -- proves
    /// the real `grant_type=client_credentials` form POST and that the returned
    /// `access_token` is what gets surfaced, not a fabricated value.
    fn spawn_token_server(access_token: &'static str) -> String {
        let server = tiny_http::Server::http("127.0.0.1:0").expect("bind ephemeral port");
        let addr = format!("http://{}", server.server_addr());
        std::thread::spawn(move || {
            if let Ok(mut req) = server.recv() {
                let mut body = String::new();
                let _ = std::io::Read::read_to_string(req.as_reader(), &mut body);
                assert!(body.contains("grant_type=client_credentials"), "must send the real client_credentials grant: {body}");
                let resp = format!(r#"{{"access_token":"{access_token}","expires_in":300}}"#);
                let _ = req.respond(tiny_http::Response::from_string(resp).with_status_code(200));
            }
        });
        addr
    }

    #[test]
    fn bearer_token_from_parts_with_all_three_fetches_a_real_token_from_the_endpoint() {
        let addr = spawn_token_server("a-real-fetched-token");
        let token = bearer_token_from_parts(Some(addr), Some("client-id".to_string()), Some("client-secret".to_string()))
            .expect("token fetch must succeed")
            .expect("all three set -> Some(token)");
        assert_eq!(token, "a-real-fetched-token");
    }
}
