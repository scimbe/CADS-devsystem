//! The INITIATOR counterpart to a `devsystem.document_extraction` role-filler
//! (issue #7): spawns a real `ct-agent channel` process, sends a real document
//! (base64, since the MCP `service/<slug>` schema fixes the request shape to a
//! single string -- `ct_common::mcp::register_service_tools`'s own doc comment),
//! and prints whatever extracted text comes back. Same proven pattern as
//! `github_issue_channel_client.rs` (#48) -- this file exists so wiring devsystem-web
//! to a real extraction agent, once one exists and wins the role's auction, is the
//! same "small, mechanical step" that comment already called out, not a new
//! integration effort at that point.
//!
//! Deliberately NOT wired into `web/src/rag.rs` yet: `devsystem.document_extraction`
//! is a declared role with no real filler behind it today (no agent has bid on or
//! won the auction). This binary is the caller-side half, provable on its own via a
//! stand-in `ct-agent` script (same hermetic-test discipline as #48's pair), ready
//! for real use the moment a real handler exists on the other end.
//!
//! Usage:
//!   devsystem_document_extraction_client <file-path>
//! Channel connection parameters come from the same env vars `ct-agent` itself
//! already uses, and the same ones `github_issue_channel_client` reads (so this
//! binary is a thin, real wrapper, not a reimplementation): `CT_CHANNEL_ADDR`,
//! `CT_CHANNEL_NOISE_KEY`, `CT_CHANNEL_PEER_NOISE_KEY`, and either
//! `CT_CHANNEL_PEER_CERT` or `CT_CHANNEL_PEER_CERT_FILE`. `CT_AGENT_BIN` optionally
//! overrides the `ct-agent` binary path (default: `ct-agent` on `PATH`).

use base64::Engine;
use serde::{Deserialize, Serialize};
use std::env;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};
use std::thread;

/// The fixed `service/<slug>` this role's MCP tool is exposed as --
/// `ct_common::mcp::service_slug` applied to `ServiceType::Custom("devsystem.document_extraction")`:
/// lowercased, non-alphanumeric -> `_`. Kept as a literal here (not computed) since
/// this binary has no `ct-common` dependency of its own -- see the module doc's
/// "thin wrapper" framing.
const CALL_SERVICE: &str = "devsystem_document_extraction";

/// #183: `ct_common::mcp::MAX_SERVICE_INPUT_BYTES` bounds the whole MCP `input`
/// string server-side at 4 MiB -- checked client-side too so an oversized document
/// fails fast and locally with a clear message, instead of dialing a real channel
/// only to have the handler reject it after the fact.
const MAX_INPUT_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ExtractionRequest {
    /// Original filename, so a real handler can use the extension as a hint
    /// alongside/instead of `mime_type` -- neither field alone is fully reliable
    /// (a mislabeled upload happens), so both travel.
    filename: String,
    mime_type: String,
    /// Real document bytes, base64-encoded -- see the module doc for why this has
    /// to be a string at all (the MCP schema's fixed `{input: string}` shape).
    content_base64: String,
}

/// Mirrors a real handler's response shape -- deliberately a separate, local
/// definition rather than a shared type, matching `github_issue_channel_client`'s
/// own established precedent (the JSON wire shape is the real contract, not a
/// shared Rust type between sibling binaries).
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(tag = "status", rename_all = "snake_case")]
enum ExtractionResponse {
    Extracted { text: String },
    Error { error: String },
}

fn ct_agent_bin() -> String {
    env::var("CT_AGENT_BIN").unwrap_or_else(|_| "ct-agent".to_string())
}

fn required_env(name: &str) -> Result<String, String> {
    env::var(name).map_err(|_| format!("{name} is not set -- required to dial the channel"))
}

/// Same shape as `github_issue_channel_client::resolve_peer_cert` -- see that
/// file's doc comment for why a respawn-loop's cert needs to be read fresh from a
/// file rather than configured once.
fn resolve_peer_cert() -> Result<String, String> {
    if let Ok(literal) = env::var("CT_CHANNEL_PEER_CERT") {
        return Ok(literal);
    }
    let path = required_env("CT_CHANNEL_PEER_CERT_FILE")
        .map_err(|_| "neither CT_CHANNEL_PEER_CERT nor CT_CHANNEL_PEER_CERT_FILE is set -- one is required to dial the channel".to_string())?;
    std::fs::read_to_string(&path).map(|s| s.trim().to_string()).map_err(|e| format!("could not read CT_CHANNEL_PEER_CERT_FILE ({path}): {e}"))
}

/// Same real, live-verified shape as `github_issue_channel_client::call_with` --
/// reads stdout line by line rather than trusting the child to exit
/// (`wait_with_output` would hang, per that file's own doc comment on the real
/// upstream `ct-agent channel` behavior this defends against), returns as soon as
/// one line parses as a real `ExtractionResponse`, then kills the child outright.
fn call_with(ct_agent_bin: &str, addr: &str, noise_key: &str, peer_noise_key: &str, peer_cert: &str, req: &ExtractionRequest) -> Result<ExtractionResponse, String> {
    let body = serde_json::to_string(req).expect("ExtractionRequest always serializes");
    if body.len() > MAX_INPUT_BYTES {
        return Err(format!("request ({} bytes) exceeds MAX_INPUT_BYTES ({MAX_INPUT_BYTES}) -- the server-side MCP dispatch would reject this anyway", body.len()));
    }

    let mut child = Command::new(ct_agent_bin)
        .arg("channel")
        .env("CT_CHANNEL_ROLE", "initiate")
        .env("CT_CHANNEL_ADDR", addr)
        .env("CT_CHANNEL_NOISE_KEY", noise_key)
        .env("CT_CHANNEL_PEER_NOISE_KEY", peer_noise_key)
        .env("CT_CHANNEL_PEER_CERT", peer_cert)
        .env("CT_CHANNEL_CALL_SERVICE", CALL_SERVICE)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not start {ct_agent_bin}: {e}"))?;

    child.stdin.take().expect("stdin was piped").write_all(body.as_bytes()).map_err(|e| format!("could not write request to {ct_agent_bin}'s stdin: {e}"))?;

    // Drained on its own thread so the child can never block writing to a full
    // stderr pipe while this thread is only reading stdout.
    let mut stderr_pipe = child.stderr.take().expect("stderr was piped");
    let stderr_thread = thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr_pipe.read_to_string(&mut s);
        s
    });

    let stdout = child.stdout.take().expect("stdout was piped");
    let response = BufReader::new(stdout)
        .lines()
        .map_while(Result::ok)
        .find_map(|line| serde_json::from_str::<ExtractionResponse>(line.trim()).ok());

    let _ = child.kill();
    let _ = child.wait();
    let stderr_text = stderr_thread.join().unwrap_or_default();

    response.ok_or_else(|| format!("no line of {ct_agent_bin}'s output parsed as a real ExtractionResponse -- stderr: {stderr_text:?}"))
}

fn mime_type_for(path: &std::path::Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).map(str::to_lowercase).as_deref() {
        Some("pdf") => "application/pdf",
        Some("docx") => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("txt") | Some("md") => "text/plain",
        _ => "application/octet-stream",
    }
}

fn call(req: &ExtractionRequest) -> Result<ExtractionResponse, String> {
    let addr = required_env("CT_CHANNEL_ADDR")?;
    let noise_key = required_env("CT_CHANNEL_NOISE_KEY")?;
    let peer_noise_key = required_env("CT_CHANNEL_PEER_NOISE_KEY")?;
    let peer_cert = resolve_peer_cert()?;
    call_with(&ct_agent_bin(), &addr, &noise_key, &peer_noise_key, &peer_cert, req)
}

fn main() -> std::process::ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let [file_path] = args.as_slice() else {
        eprintln!("usage: devsystem_document_extraction_client <file-path>");
        return std::process::ExitCode::FAILURE;
    };
    let path = std::path::Path::new(file_path);
    let filename = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| file_path.clone());
    let mime_type = mime_type_for(path).to_string();
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("could not read {file_path}: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let content_base64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    let req = ExtractionRequest { filename, mime_type, content_base64 };

    match call(&req) {
        Ok(ExtractionResponse::Extracted { text }) => {
            println!("{text}");
            std::process::ExitCode::SUCCESS
        }
        Ok(ExtractionResponse::Error { error }) => {
            eprintln!("agent reported an error: {error}");
            std::process::ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("{e}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;

    /// Same real, reproduced subprocess-spawning flakiness
    /// `github_issue_channel_client`'s own tests guard against -- see that file's
    /// doc comment. A process-wide lock, not `--test-threads=1`, so only the tests
    /// that actually spawn real child processes serialize.
    static SUBPROCESS_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn fake_ct_agent(dir: &std::path::Path, script: &str) -> String {
        let path = dir.join("fake-ct-agent.sh");
        let tmp = dir.join("fake-ct-agent.sh.tmp");
        fs::write(&tmp, format!("#!/bin/sh\n{script}\n")).unwrap();
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755)).unwrap();
        fs::rename(&tmp, &path).unwrap();
        path.to_string_lossy().to_string()
    }

    #[test]
    fn call_with_sends_the_real_request_json_on_stdin_and_sets_the_document_extraction_service() {
        let _guard = SUBPROCESS_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("doc-extraction-client-test-{}-1", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let bin = fake_ct_agent(
            &dir,
            r#"cat > "$0.stdin"
echo "role=$CT_CHANNEL_ROLE svc=$CT_CHANNEL_CALL_SERVICE"
echo '{"status":"extracted","text":"real extracted text"}'"#,
        );

        let req = ExtractionRequest { filename: "spec.pdf".to_string(), mime_type: "application/pdf".to_string(), content_base64: "cmVhbCBieXRlcw==".to_string() };
        let response = call_with(&bin, "127.0.0.1:19999", "noise-priv", "peer-pub", "deadbeef", &req).expect("fake ct-agent always succeeds");
        assert_eq!(response, ExtractionResponse::Extracted { text: "real extracted text".to_string() });

        let stdin_capture = fs::read_to_string(format!("{bin}.stdin")).expect("fake ct-agent must have captured real stdin");
        let sent: ExtractionRequest = serde_json::from_str(&stdin_capture).expect("stdin must be the real request JSON");
        assert_eq!(sent.filename, "spec.pdf");
        assert_eq!(sent.content_base64, "cmVhbCBieXRlcw==");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn call_with_sets_the_real_document_extraction_service_slug_not_text_generation() {
        let _guard = SUBPROCESS_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("doc-extraction-client-test-{}-2", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let bin = fake_ct_agent(
            &dir,
            r#"cat > /dev/null
echo "svc=$CT_CHANNEL_CALL_SERVICE" > "$0.envcheck"
echo '{"status":"extracted","text":"x"}'"#,
        );

        let req = ExtractionRequest { filename: "f.txt".to_string(), mime_type: "text/plain".to_string(), content_base64: "eA==".to_string() };
        call_with(&bin, "127.0.0.1:19999", "k", "pk", "cert", &req).expect("fake ct-agent always succeeds");

        let envcheck = fs::read_to_string(format!("{bin}.envcheck")).unwrap();
        assert!(envcheck.contains("svc=devsystem_document_extraction"), "must use the real declared role's service slug, not a guessed one: {envcheck}");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn call_with_surfaces_an_extraction_error_honestly_not_as_a_fabricated_success() {
        let _guard = SUBPROCESS_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("doc-extraction-client-test-{}-3", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let bin = fake_ct_agent(&dir, r#"cat > /dev/null
echo '{"status":"error","error":"unsupported document format"}'"#);

        let req = ExtractionRequest { filename: "f.bin".to_string(), mime_type: "application/octet-stream".to_string(), content_base64: "AA==".to_string() };
        let response = call_with(&bin, "127.0.0.1:19999", "k", "pk", "cert", &req).expect("a well-formed error response still parses as Ok");
        assert_eq!(response, ExtractionResponse::Error { error: "unsupported document format".to_string() });
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn call_with_rejects_an_oversized_request_locally_before_ever_dialing() {
        let huge = "A".repeat(MAX_INPUT_BYTES + 1);
        let req = ExtractionRequest { filename: "huge.pdf".to_string(), mime_type: "application/pdf".to_string(), content_base64: huge };
        // "does-not-exist" as the binary proves this never even tries to spawn a
        // process -- if it did, this would fail with "could not start", not the
        // real MAX_INPUT_BYTES message asserted below.
        let err = call_with("this-binary-does-not-exist", "127.0.0.1:1", "k", "pk", "cert", &req).expect_err("an oversized request must be rejected locally");
        assert!(err.contains("MAX_INPUT_BYTES"), "got: {err}");
    }

    #[test]
    fn mime_type_for_recognizes_the_real_formats_this_service_is_for() {
        assert_eq!(mime_type_for(std::path::Path::new("spec.pdf")), "application/pdf");
        assert_eq!(mime_type_for(std::path::Path::new("report.DOCX")), "application/vnd.openxmlformats-officedocument.wordprocessingml.document");
        assert_eq!(mime_type_for(std::path::Path::new("photo.jpg")), "image/jpeg");
        assert_eq!(mime_type_for(std::path::Path::new("notes.txt")), "text/plain");
        assert_eq!(mime_type_for(std::path::Path::new("unknown.xyz")), "application/octet-stream");
    }

    #[test]
    fn resolve_peer_cert_reports_honestly_when_neither_env_var_is_configured() {
        let _guard = SUBPROCESS_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        env::remove_var("CT_CHANNEL_PEER_CERT");
        env::remove_var("CT_CHANNEL_PEER_CERT_FILE");
        let err = resolve_peer_cert().expect_err("no cert source configured must be a real, clear error");
        assert!(err.contains("CT_CHANNEL_PEER_CERT") && err.contains("CT_CHANNEL_PEER_CERT_FILE"));
    }
}
