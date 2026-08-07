//! The INITIATOR counterpart to a `devsystem.embedding` role-filler (issue #7,
//! operator-directed architecture correction, 2026-08-05): "RAG's embedding/
//! document-extraction capability should come from an LLM-capable ct-agent,
//! discovered/assigned through this pipeline's own auction mechanism -- not a
//! static `RAG_EMBEDDING_API_KEY`/`RAG_UNSTRUCTURED_API_KEY` credential."
//! Document extraction already got this treatment
//! (`devsystem_document_extraction_client.rs`, #7); this is the same real
//! pattern applied to embeddings, the other half of that same correction that
//! was never finished. Mirrors that binary's own real, live-verified shape
//! deliberately -- same broker-mediated, relay-only connection model (this
//! deployment's own caller identity has no dialable public address either, the
//! same real constraint that made relay-only the only correct mode there).
//!
//! Usage: reads a JSON array of strings (the texts to embed) from this
//! binary's own stdin, writes a JSON array of embedding vectors to stdout on
//! success. Unlike document extraction (one file, one CLI arg), embedding is
//! naturally a batch operation -- `rag::embed_texts` already batches, and the
//! real cost/latency of dialing a channel per text would be wasteful.
//!
//! Channel connection parameters (same broker-mediated shape
//! `devsystem_document_extraction_client` uses, and the same real, already-
//! working `alice` identity convention this host's own config follows):
//! `CT_CHANNEL_BROKER`, `CT_CHANNEL_RELAY`, `CT_CHANNEL_GRANT` (this caller
//! identity's real `SignedChannelGrant`), `CT_CHANNEL_HOLDER_KEY` (this caller
//! identity's own real private key), `CT_CHANNEL_NOISE_KEY`. `CT_AGENT_BIN`
//! optionally overrides the `ct-agent` binary path (default: `ct-agent` on
//! `PATH`).

use serde::{Deserialize, Serialize};
use std::env;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};
use std::thread;

/// The fixed `service/<slug>` this role's MCP tool is exposed as --
/// `ct_common::mcp::service_slug` applied to `ServiceType::Custom("devsystem.embedding")`:
/// lowercased, non-alphanumeric -> `_`. Kept as a literal here (not computed) since
/// this binary has no `ct-common` dependency of its own -- same convention
/// `devsystem_document_extraction_client`'s own `CALL_SERVICE` constant follows.
const CALL_SERVICE: &str = "devsystem_embedding";

/// Same real defensive cap `devsystem_document_extraction_client` enforces
/// client-side (`ct_common::mcp::MAX_SERVICE_INPUT_BYTES` bounds the whole MCP
/// `input` string server-side at 4 MiB) -- checked here too so an oversized
/// batch fails fast and locally instead of dialing a real channel only to be
/// rejected after the fact.
const MAX_INPUT_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EmbeddingRequest {
    texts: Vec<String>,
}

/// Mirrors a real handler's response shape -- deliberately a separate, local
/// definition rather than a shared type, matching `devsystem_document_extraction_client`'s
/// own established precedent (the JSON wire shape is the real contract, not a
/// shared Rust type between sibling binaries).
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(tag = "status", rename_all = "snake_case")]
enum EmbeddingResponse {
    Embedded { embeddings: Vec<Vec<f32>> },
    Error { error: String },
}

fn ct_agent_bin() -> String {
    env::var("CT_AGENT_BIN").unwrap_or_else(|_| "ct-agent".to_string())
}

fn required_env(name: &str) -> Result<String, String> {
    env::var(name).map_err(|_| format!("{name} is not set -- required to dial the channel"))
}

/// Same real, live-verified shape as `devsystem_document_extraction_client::call_with`
/// -- reads stdout line by line rather than trusting the child to exit (a real
/// upstream `ct-agent channel` behavior that binary's own doc comment already
/// defends against), returns as soon as one line parses as a real
/// `EmbeddingResponse`, then kills the child outright.
///
/// Real broker-mediated dial, relay-only hardcoded (not read from the
/// environment) for the identical reason `devsystem_document_extraction_client`
/// hardcodes it: this binary's own real deployment constraint (devsystem-web
/// has no dialable public address either) makes relay-only the only mode
/// that's ever actually correct here.
fn call_with(ct_agent_bin: &str, broker: &str, relay: &str, grant: &str, holder_key: &str, noise_key: &str, req: &EmbeddingRequest) -> Result<EmbeddingResponse, String> {
    let body = serde_json::to_string(req).expect("EmbeddingRequest always serializes");
    if body.len() > MAX_INPUT_BYTES {
        return Err(format!("request ({} bytes) exceeds MAX_INPUT_BYTES ({MAX_INPUT_BYTES}) -- the server-side MCP dispatch would reject this anyway", body.len()));
    }

    let mut child = Command::new(ct_agent_bin)
        .arg("channel")
        .env("CT_CHANNEL_ROLE", "initiate")
        .env("CT_CHANNEL_BROKER", broker)
        .env("CT_CHANNEL_RELAY", relay)
        .env("CT_CHANNEL_GRANT", grant)
        .env("CT_CHANNEL_HOLDER_KEY", holder_key)
        .env("CT_CHANNEL_NOISE_KEY", noise_key)
        .env("CT_CHANNEL_RELAY_ONLY", "1")
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
        .find_map(|line| serde_json::from_str::<EmbeddingResponse>(line.trim()).ok());

    let _ = child.kill();
    let _ = child.wait();
    let stderr_text = stderr_thread.join().unwrap_or_default();

    response.ok_or_else(|| format!("no line of {ct_agent_bin}'s output parsed as a real EmbeddingResponse -- stderr: {stderr_text:?}"))
}

fn call(req: &EmbeddingRequest) -> Result<EmbeddingResponse, String> {
    let broker = required_env("CT_CHANNEL_BROKER")?;
    let relay = required_env("CT_CHANNEL_RELAY")?;
    let grant = required_env("CT_CHANNEL_GRANT")?;
    let holder_key = required_env("CT_CHANNEL_HOLDER_KEY")?;
    let noise_key = required_env("CT_CHANNEL_NOISE_KEY")?;
    call_with(&ct_agent_bin(), &broker, &relay, &grant, &holder_key, &noise_key, req)
}

fn main() -> std::process::ExitCode {
    let mut input = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut input) {
        eprintln!("could not read texts (a JSON array of strings) from stdin: {e}");
        return std::process::ExitCode::FAILURE;
    }
    let texts: Vec<String> = match serde_json::from_str(&input) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("stdin did not parse as a JSON array of strings: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let req = EmbeddingRequest { texts };

    match call(&req) {
        Ok(EmbeddingResponse::Embedded { embeddings }) => match serde_json::to_string(&embeddings) {
            Ok(json) => {
                println!("{json}");
                std::process::ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("could not serialize the real embeddings response: {e}");
                std::process::ExitCode::FAILURE
            }
        },
        Ok(EmbeddingResponse::Error { error }) => {
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
    /// `devsystem_document_extraction_client`'s own tests guard against -- see
    /// that file's doc comment. A process-wide lock, not `--test-threads=1`, so
    /// only the tests that actually spawn real child processes serialize.
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
    fn call_with_sends_the_real_request_json_on_stdin_and_sets_the_embedding_service() {
        let _guard = SUBPROCESS_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("embedding-client-test-{}-1", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let bin = fake_ct_agent(
            &dir,
            r#"cat > "$0.stdin"
echo "role=$CT_CHANNEL_ROLE svc=$CT_CHANNEL_CALL_SERVICE"
echo '{"status":"embedded","embeddings":[[0.1,0.2,0.3]]}'"#,
        );

        let req = EmbeddingRequest { texts: vec!["a real chunk of text".to_string()] };
        let response = call_with(&bin, "127.0.0.1:4435", "127.0.0.1:4436", "fake-grant", "fake-holder-key", "noise-priv", &req).expect("fake ct-agent always succeeds");
        assert_eq!(response, EmbeddingResponse::Embedded { embeddings: vec![vec![0.1, 0.2, 0.3]] });

        let stdin_capture = fs::read_to_string(format!("{bin}.stdin")).expect("fake ct-agent must have captured real stdin");
        let sent: EmbeddingRequest = serde_json::from_str(&stdin_capture).expect("stdin must be the real request JSON");
        assert_eq!(sent.texts, vec!["a real chunk of text".to_string()]);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn call_with_sets_the_real_embedding_service_slug_not_document_extraction() {
        let _guard = SUBPROCESS_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("embedding-client-test-{}-2", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let bin = fake_ct_agent(
            &dir,
            r#"cat > /dev/null
echo "svc=$CT_CHANNEL_CALL_SERVICE" > "$0.envcheck"
echo '{"status":"embedded","embeddings":[]}'"#,
        );

        let req = EmbeddingRequest { texts: vec![] };
        call_with(&bin, "127.0.0.1:4435", "127.0.0.1:4436", "fake-grant", "fake-holder-key", "k", &req).expect("fake ct-agent always succeeds");

        let envcheck = fs::read_to_string(format!("{bin}.envcheck")).unwrap();
        assert!(envcheck.contains("svc=devsystem_embedding"), "must use the real declared role's own service slug, not another role's: {envcheck}");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn call_with_surfaces_an_embedding_error_honestly_not_as_a_fabricated_success() {
        let _guard = SUBPROCESS_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("embedding-client-test-{}-3", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let bin = fake_ct_agent(&dir, r#"cat > /dev/null
echo '{"status":"error","error":"embedding model unavailable"}'"#);

        let req = EmbeddingRequest { texts: vec!["x".to_string()] };
        let response = call_with(&bin, "127.0.0.1:4435", "127.0.0.1:4436", "fake-grant", "fake-holder-key", "k", &req).expect("a well-formed error response still parses as Ok");
        assert_eq!(response, EmbeddingResponse::Error { error: "embedding model unavailable".to_string() });
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn call_with_rejects_an_oversized_request_locally_before_ever_dialing() {
        let huge_texts: Vec<String> = vec!["A".repeat(MAX_INPUT_BYTES)];
        let req = EmbeddingRequest { texts: huge_texts };
        // "does-not-exist" as the binary proves this never even tries to spawn a
        // process -- if it did, this would fail with "could not start", not the
        // real MAX_INPUT_BYTES message asserted below.
        let err = call_with("this-binary-does-not-exist", "127.0.0.1:4435", "127.0.0.1:4436", "fake-grant", "fake-holder-key", "k", &req).expect_err("an oversized request must be rejected locally");
        assert!(err.contains("MAX_INPUT_BYTES"), "got: {err}");
    }

    #[test]
    /// Proves the real connection shape this binary speaks -- broker/relay/
    /// grant/holder-key, relay-only hardcoded -- reaches the real spawned
    /// `ct-agent channel` subprocess, mirroring
    /// `devsystem_document_extraction_client`'s own equivalent test.
    fn call_with_sends_the_real_broker_mediated_relay_only_env_to_ct_agent() {
        let _guard = SUBPROCESS_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("embedding-client-test-{}-4", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let bin = fake_ct_agent(
            &dir,
            r#"cat > /dev/null
echo "role=$CT_CHANNEL_ROLE broker=$CT_CHANNEL_BROKER relay=$CT_CHANNEL_RELAY grant=$CT_CHANNEL_GRANT holder=$CT_CHANNEL_HOLDER_KEY noise=$CT_CHANNEL_NOISE_KEY relay_only=$CT_CHANNEL_RELAY_ONLY" > "$0.envcheck"
echo '{"status":"embedded","embeddings":[]}'"#,
        );

        let req = EmbeddingRequest { texts: vec!["x".to_string()] };
        call_with(&bin, "real-broker:4435", "real-relay:4436", "real-grant-hex", "real-holder-key-hex", "real-noise-key", &req).expect("fake ct-agent always succeeds");

        let envcheck = fs::read_to_string(format!("{bin}.envcheck")).unwrap();
        assert!(envcheck.contains("role=initiate"), "got: {envcheck}");
        assert!(envcheck.contains("broker=real-broker:4435"), "got: {envcheck}");
        assert!(envcheck.contains("relay=real-relay:4436"), "got: {envcheck}");
        assert!(envcheck.contains("grant=real-grant-hex"), "got: {envcheck}");
        assert!(envcheck.contains("holder=real-holder-key-hex"), "got: {envcheck}");
        assert!(envcheck.contains("noise=real-noise-key"), "got: {envcheck}");
        assert!(envcheck.contains("relay_only=1"), "relay-only must always be set -- this binary's own real deployment has no dialable address either: {envcheck}");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn call_requires_every_real_channel_env_var_named_honestly() {
        // No env vars set at all -- must fail on the first missing one, not panic.
        std::env::remove_var("CT_CHANNEL_BROKER");
        let req = EmbeddingRequest { texts: vec![] };
        let err = call(&req).expect_err("with no channel env configured, this must fail, not silently succeed");
        assert!(err.contains("CT_CHANNEL_BROKER"), "the error must name which real env var is missing: {err}");
    }
}
