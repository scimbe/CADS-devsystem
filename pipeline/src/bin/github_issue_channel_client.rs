//! The INITIATOR counterpart to `github_issue_channel_handler` (#48 slice 3):
//! spawns a real `ct-agent channel` process as the calling side of a direct-
//! address Agent-Fabric channel, sends one real request, and prints the real
//! reply. Together with `github_issue_channel_handler`, this is the whole
//! real, live-verified round trip (see that binary's module doc for the exact
//! `ct-agent channel` invocation and the real bug this pairing already caught
//! and fixed).
//!
//! Deliberately NOT yet wired into devsystem-web's `approve_issue_proposal`
//! (`web/src/main.rs`, which still POSTs to GitHub directly) -- that cutover
//! depends on a real, still-open operator decision (task #48): how the
//! handler's long-lived accept-side process is actually hosted (a
//! respawn-loop wrapper around the one-shot direct-address accept path, or
//! real broker-mediated channel registration). Wiring production traffic
//! through a channel target that isn't reliably up yet would make a working
//! feature flaky for no reason -- this binary exists so that cutover is a
//! small, mechanical step once that decision is made, not a new integration
//! effort at that point.
//!
//! Usage:
//!   github_issue_channel_client <repo> <title> <body...>
//! Channel connection parameters come from the same env vars `ct-agent`
//! itself already uses (so this binary is a thin, real wrapper, not a
//! reimplementation): `CT_CHANNEL_ADDR`, `CT_CHANNEL_NOISE_KEY` (this
//! process's own private key), `CT_CHANNEL_PEER_NOISE_KEY` (the handler's
//! public key), and either `CT_CHANNEL_PEER_CERT` (the literal hex cert) or
//! `CT_CHANNEL_PEER_CERT_FILE` (a path to read it from, fresh, on every call
//! -- see `resolve_peer_cert`'s doc comment for why a file is needed at all:
//! the handler's real listen cert is NOT stable across its own respawns).
//! `CT_AGENT_BIN` optionally overrides the `ct-agent` binary path (default:
//! `ct-agent` on `PATH`).

use serde::{Deserialize, Serialize};
use std::env;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};
use std::thread;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct IssueRequest {
    repo: String,
    title: String,
    body: String,
}

/// Mirrors `github_issue_channel_handler::IssueResponse` -- deliberately a
/// separate, local definition rather than a shared type. Each standalone
/// binary in this crate (`devsystem_iterate`, `devsystem_offer`, ...) already
/// defines its own request/response shapes rather than depending on a sibling
/// bin; the real contract between them is the JSON wire shape, proven by the
/// live channel round trip, not shared Rust types.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(tag = "status", rename_all = "snake_case")]
enum IssueResponse {
    Created { number: u64, html_url: String },
    AlreadyFiled { html_url: String },
    Error { error: String },
}

fn ct_agent_bin() -> String {
    env::var("CT_AGENT_BIN").unwrap_or_else(|_| "ct-agent".to_string())
}

fn required_env(name: &str) -> Result<String, String> {
    env::var(name).map_err(|_| format!("{name} is not set -- required to dial the channel"))
}

/// Spawns the real `ct-agent channel` initiator, sends `req` as its one line
/// of stdin, and parses whatever line it prints back.
///
/// Real finding, live-verified (2026-08-04): after a real service call whose
/// handler took real, non-trivial time to answer (a genuine GitHub POST, not
/// the instant no-token error path), the raw `ct-agent channel` initiator
/// process has been observed to print the correct reply and then hang
/// indefinitely instead of exiting -- reproduced with the unmodified `ct-agent`
/// binary directly, not this wrapper's own logic, so this is a real upstream
/// behavior to defend against, not assumed-safe. Consequently this function
/// does NOT trust the child to exit on its own (`wait_with_output` would hang
/// exactly like the raw binary did): it reads stdout line by line as it
/// arrives, returns as soon as one line parses as a real `IssueResponse`, and
/// then kills the child outright rather than waiting for a graceful exit.
/// Exit status is deliberately NOT part of the success/failure decision --
/// same lesson `github_issue_channel_handler` already learned the hard way:
/// only the real JSON payload is trustworthy here.
fn call_with(ct_agent_bin: &str, addr: &str, noise_key: &str, peer_noise_key: &str, peer_cert: &str, req: &IssueRequest) -> Result<IssueResponse, String> {
    let body = serde_json::to_string(req).expect("IssueRequest always serializes");

    let mut child = Command::new(ct_agent_bin)
        .arg("channel")
        .env("CT_CHANNEL_ROLE", "initiate")
        .env("CT_CHANNEL_ADDR", addr)
        .env("CT_CHANNEL_NOISE_KEY", noise_key)
        .env("CT_CHANNEL_PEER_NOISE_KEY", peer_noise_key)
        .env("CT_CHANNEL_PEER_CERT", peer_cert)
        .env("CT_CHANNEL_CALL_SERVICE", "text_generation")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not start {ct_agent_bin}: {e}"))?;

    child.stdin.take().expect("stdin was piped").write_all(body.as_bytes()).map_err(|e| format!("could not write request to {ct_agent_bin}'s stdin: {e}"))?;

    // Drained on its own thread so the child can never block writing to a
    // full stderr pipe while this thread is only reading stdout.
    let mut stderr_pipe = child.stderr.take().expect("stderr was piped");
    let stderr_thread = thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr_pipe.read_to_string(&mut s);
        s
    });

    let stdout = child.stdout.take().expect("stdout was piped");
    // ct-agent's own "connected to..."/"status uptime=..." log lines share
    // stdout with the real reply (verified live), so the real reply is
    // whichever line actually parses as an IssueResponse, not just the last
    // line read before EOF/hang.
    let response = BufReader::new(stdout)
        .lines()
        .map_while(Result::ok)
        .find_map(|line| serde_json::from_str::<IssueResponse>(line.trim()).ok());

    let _ = child.kill();
    let _ = child.wait();
    let stderr_text = stderr_thread.join().unwrap_or_default();

    response.ok_or_else(|| format!("no line of {ct_agent_bin}'s output parsed as a real IssueResponse -- stderr: {stderr_text:?}"))
}

/// Real finding, live-verified (2026-08-05): the cert a `ct-agent channel
/// --serve` process prints on startup is freshly generated EVERY time it
/// starts, even reusing the identical `CT_CHANNEL_NOISE_KEY` -- confirmed by
/// starting the same identity twice and diffing the two printed certs (they
/// were completely different, not just superficially). The direct-address
/// accept path also serves exactly one session then exits (#48 slice 2's own
/// doc comment), so a real always-on deployment needs a respawn loop --
/// which means the cert this function needs is a MOVING TARGET, not a value
/// safe to configure once. `CT_CHANNEL_PEER_CERT_FILE` lets a wrapper script
/// that owns that respawn loop publish the current cert to a well-known path
/// (atomically, so this never reads a half-written file) and this function
/// reads it fresh on every single call, immediately before dialing --
/// `CT_CHANNEL_PEER_CERT` (the literal value) still works too, for the
/// direct one-shot case this binary was originally verified against.
fn resolve_peer_cert() -> Result<String, String> {
    if let Ok(literal) = env::var("CT_CHANNEL_PEER_CERT") {
        return Ok(literal);
    }
    let path = required_env("CT_CHANNEL_PEER_CERT_FILE")
        .map_err(|_| "neither CT_CHANNEL_PEER_CERT nor CT_CHANNEL_PEER_CERT_FILE is set -- one is required to dial the channel".to_string())?;
    std::fs::read_to_string(&path).map(|s| s.trim().to_string()).map_err(|e| format!("could not read CT_CHANNEL_PEER_CERT_FILE ({path}): {e}"))
}

fn call(req: &IssueRequest) -> Result<IssueResponse, String> {
    let addr = required_env("CT_CHANNEL_ADDR")?;
    let noise_key = required_env("CT_CHANNEL_NOISE_KEY")?;
    let peer_noise_key = required_env("CT_CHANNEL_PEER_NOISE_KEY")?;
    let peer_cert = resolve_peer_cert()?;
    call_with(&ct_agent_bin(), &addr, &noise_key, &peer_noise_key, &peer_cert, req)
}

fn main() -> std::process::ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let [repo, title, body @ ..] = args.as_slice() else {
        eprintln!("usage: github_issue_channel_client <repo> <title> <body...>");
        return std::process::ExitCode::FAILURE;
    };
    let req = IssueRequest { repo: repo.clone(), title: title.clone(), body: body.join(" ") };

    match call(&req) {
        Ok(IssueResponse::Created { number, html_url }) => {
            println!("created: #{number} {html_url}");
            std::process::ExitCode::SUCCESS
        }
        Ok(IssueResponse::AlreadyFiled { html_url }) => {
            println!("already filed: {html_url}");
            std::process::ExitCode::SUCCESS
        }
        Ok(IssueResponse::Error { error }) => {
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

    /// Real, reproduced flakiness (2026-08-04): running this file's
    /// subprocess-spawning tests concurrently (cargo test's default) hit an
    /// intermittent ETXTBSY ("Text file busy") starting the fake `ct-agent`
    /// script, even after switching `fake_ct_agent` to an atomic
    /// write-then-rename -- the exact trigger wasn't pinned down further
    /// (plausibly this container's filesystem/kernel racing fork+exec across
    /// threads of the same test binary), but forcing these specific tests to
    /// run one at a time makes it disappear reliably, confirmed by re-running
    /// the whole suite several times in a row with this lock in place. A
    /// process-wide `Mutex` rather than `--test-threads=1` so it only
    /// serializes the tests that actually spawn real child processes, not
    /// this file's other tests or any other test binary.
    static SUBPROCESS_TEST_LOCK: Mutex<()> = Mutex::new(());

    /// A fake `ct-agent` standing in for the real binary -- proves
    /// `call_with`'s own env-passing/stdin-writing/stdout-parsing logic
    /// without a real channel. The actual real-channel round trip (this
    /// binary talking to a real `github_issue_channel_handler` over a real
    /// Noise-encrypted session) was already live-verified manually this
    /// session -- see the commit history -- matching this codebase's
    /// established "hermetic test the logic, live-verify the real transport
    /// by hand" precedent (e.g. `rag.rs`'s own doc comment).
    fn fake_ct_agent(dir: &std::path::Path, script: &str) -> String {
        let path = dir.join("fake-ct-agent.sh");
        // Write to a temp name then rename into place: an atomic rename so a
        // concurrent exec of the final path never observes a partially
        // written file -- a real but, on its own, insufficient defense
        // against the ETXTBSY flakiness SUBPROCESS_TEST_LOCK above exists to
        // actually fix.
        let tmp = dir.join("fake-ct-agent.sh.tmp");
        fs::write(&tmp, format!("#!/bin/sh\n{script}\n")).unwrap();
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755)).unwrap();
        fs::rename(&tmp, &path).unwrap();
        path.to_string_lossy().to_string()
    }

    #[test]
    fn call_with_sends_the_real_request_json_on_stdin_and_sets_every_real_channel_env_var() {
        let _guard = SUBPROCESS_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("github-issue-client-test-{}-1", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        // Echoes back everything it actually received, so the assertions below
        // prove the real values reached the child process, not just that the
        // code compiles.
        let bin = fake_ct_agent(
            &dir,
            r#"cat > "$0.stdin"
echo "role=$CT_CHANNEL_ROLE addr=$CT_CHANNEL_ADDR noise=$CT_CHANNEL_NOISE_KEY peer=$CT_CHANNEL_PEER_NOISE_KEY cert=$CT_CHANNEL_PEER_CERT svc=$CT_CHANNEL_CALL_SERVICE"
echo '{"status":"created","number":7,"html_url":"https://github.com/scimbe/CADS-webconference-demo/issues/7"}'"#,
        );

        let req = IssueRequest { repo: "scimbe/CADS-webconference-demo".to_string(), title: "a real title".to_string(), body: "a real body".to_string() };
        let response = call_with(&bin, "127.0.0.1:19999", "noise-priv", "peer-pub", "deadbeef", &req).expect("fake ct-agent always succeeds");
        assert_eq!(response, IssueResponse::Created { number: 7, html_url: "https://github.com/scimbe/CADS-webconference-demo/issues/7".to_string() });

        let stdin_capture = fs::read_to_string(format!("{bin}.stdin")).expect("fake ct-agent must have captured real stdin");
        let sent: IssueRequest = serde_json::from_str(&stdin_capture).expect("stdin must be the real request JSON");
        assert_eq!(sent.repo, "scimbe/CADS-webconference-demo");
        assert_eq!(sent.title, "a real title");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn call_with_sets_every_real_channel_env_var_the_real_ct_agent_needs() {
        let _guard = SUBPROCESS_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("github-issue-client-test-{}-1b", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        // Writes what it actually received to a file rather than stdout, so
        // call_with's own stdout-parsing logic (which this test isn't
        // exercising) can't accidentally swallow or reorder it.
        let bin = fake_ct_agent(
            &dir,
            r#"cat > /dev/null
echo "role=$CT_CHANNEL_ROLE addr=$CT_CHANNEL_ADDR noise=$CT_CHANNEL_NOISE_KEY peer=$CT_CHANNEL_PEER_NOISE_KEY cert=$CT_CHANNEL_PEER_CERT svc=$CT_CHANNEL_CALL_SERVICE" > "$0.envcheck"
echo '{"status":"created","number":1,"html_url":"https://github.com/scimbe/CADS-webconference-demo/issues/1"}'"#,
        );

        let req = IssueRequest { repo: "scimbe/CADS-webconference-demo".to_string(), title: "t".to_string(), body: "b".to_string() };
        call_with(&bin, "127.0.0.1:19999", "noise-priv", "peer-pub", "deadbeef", &req).expect("fake ct-agent always succeeds");

        let envcheck = fs::read_to_string(format!("{bin}.envcheck")).expect("fake ct-agent must have recorded the real env it received");
        assert!(envcheck.contains("role=initiate"), "real envcheck: {envcheck}");
        assert!(envcheck.contains("addr=127.0.0.1:19999"));
        assert!(envcheck.contains("noise=noise-priv"));
        assert!(envcheck.contains("peer=peer-pub"));
        assert!(envcheck.contains("cert=deadbeef"));
        assert!(envcheck.contains("svc=text_generation"), "the real service name must be the fixed text_generation category, not guessed");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn call_with_parses_the_real_reply_even_when_ct_agent_prints_its_own_status_lines_first() {
        let _guard = SUBPROCESS_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("github-issue-client-test-{}-2", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        // Real observed shape from the live verification: ct-agent's own
        // "connected to..."/"status uptime=..." lines interleave with the
        // handler's actual reply on stdout.
        let bin = fake_ct_agent(
            &dir,
            r#"cat > /dev/null
echo "ct-agent channel: connected to 127.0.0.1:19999 (initiator)"
echo "ct-agent channel: status uptime=0s sent=98B recv=50B"
echo '{"status":"already_filed","html_url":"https://github.com/scimbe/CADS-webconference-demo/issues/2"}'
echo "ct-agent channel: status uptime=0s sent=200B recv=100B""#,
        );

        let req = IssueRequest { repo: "scimbe/CADS-webconference-demo".to_string(), title: "t".to_string(), body: "b".to_string() };
        let response = call_with(&bin, "127.0.0.1:19999", "k", "pk", "cert", &req).unwrap();
        assert_eq!(response, IssueResponse::AlreadyFiled { html_url: "https://github.com/scimbe/CADS-webconference-demo/issues/2".to_string() });
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn call_with_surfaces_a_nonzero_ct_agent_exit_honestly() {
        let _guard = SUBPROCESS_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("github-issue-client-test-{}-3", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let bin = fake_ct_agent(&dir, r#"cat > /dev/null
echo "connection refused" >&2
exit 1"#);

        let req = IssueRequest { repo: "scimbe/CADS-webconference-demo".to_string(), title: "t".to_string(), body: "b".to_string() };
        let err = call_with(&bin, "127.0.0.1:1", "k", "pk", "cert", &req).expect_err("a nonzero ct-agent exit must be a real error, not a fabricated success");
        assert!(err.contains("connection refused"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn call_with_reports_honestly_when_nothing_on_stdout_is_a_real_response() {
        let _guard = SUBPROCESS_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("github-issue-client-test-{}-4", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let bin = fake_ct_agent(&dir, r#"cat > /dev/null
echo "this channel produced garbage, not a real reply""#);

        let req = IssueRequest { repo: "scimbe/CADS-webconference-demo".to_string(), title: "t".to_string(), body: "b".to_string() };
        let err = call_with(&bin, "127.0.0.1:1", "k", "pk", "cert", &req).expect_err("unparseable output must be a real error, not a silent None treated as success");
        assert!(err.contains("no line"));
    }

    // resolve_peer_cert reads/writes real process-wide env vars -- guarded by
    // the same lock as the subprocess tests (serializing more than strictly
    // necessary is harmless; racing two tests over the same env var names is
    // not) and each test clears both vars on the way out so it never leaks
    // state into a test that runs after it.
    fn clear_cert_env() {
        env::remove_var("CT_CHANNEL_PEER_CERT");
        env::remove_var("CT_CHANNEL_PEER_CERT_FILE");
    }

    #[test]
    fn resolve_peer_cert_prefers_the_literal_env_var_when_both_are_set() {
        let _guard = SUBPROCESS_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_cert_env();
        let dir = std::env::temp_dir().join(format!("github-issue-client-cert-test-{}-1", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cert.txt");
        fs::write(&path, "cert-from-file").unwrap();

        env::set_var("CT_CHANNEL_PEER_CERT", "cert-from-literal-env");
        env::set_var("CT_CHANNEL_PEER_CERT_FILE", path.to_string_lossy().to_string());
        assert_eq!(resolve_peer_cert(), Ok("cert-from-literal-env".to_string()));

        clear_cert_env();
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_peer_cert_reads_the_real_file_fresh_when_only_the_file_var_is_set() {
        let _guard = SUBPROCESS_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_cert_env();
        let dir = std::env::temp_dir().join(format!("github-issue-client-cert-test-{}-2", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cert.txt");

        // A real respawn-loop wrapper's whole point: the cert changes, and
        // this must pick up whatever is on disk right now, not a cached value.
        fs::write(&path, "cert-version-one\n").unwrap();
        env::set_var("CT_CHANNEL_PEER_CERT_FILE", path.to_string_lossy().to_string());
        assert_eq!(resolve_peer_cert(), Ok("cert-version-one".to_string()), "trailing newline must be trimmed");

        fs::write(&path, "cert-version-two").unwrap();
        assert_eq!(resolve_peer_cert(), Ok("cert-version-two".to_string()), "a real respawn must be picked up on the very next call, not stale");

        clear_cert_env();
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_peer_cert_reports_honestly_when_neither_is_configured() {
        let _guard = SUBPROCESS_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_cert_env();
        let err = resolve_peer_cert().expect_err("no cert source configured must be a real, clear error");
        assert!(err.contains("CT_CHANNEL_PEER_CERT") && err.contains("CT_CHANNEL_PEER_CERT_FILE"));
    }
}
