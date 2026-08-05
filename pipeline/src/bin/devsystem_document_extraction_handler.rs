//! The AGENT side of `devsystem.document_extraction` (issue #7/#14, labor-setup.com):
//! meant to be pointed at by `CT_AGENT_SERVICE_HANDLER_CMD` on a `ct-agent channel
//! --serve` process, same real, live-verified contract as
//! `github_issue_channel_handler.rs` (#48) -- reads exactly one line of JSON from
//! stdin, writes exactly one line of JSON to stdout, ALWAYS exits 0 once a response
//! line has been printed (a non-zero exit is treated by `ct-agent` as a hard
//! transport-level error and drops stdout entirely -- that file's own doc comment,
//! not re-guessed here).
//!
//! Request/response shape matches `devsystem_document_extraction_client.rs` exactly
//! (the real, already-tested caller side, not re-derived): `{filename, mime_type,
//! content_base64}` in, `{"status":"extracted","text":"..."}` or
//! `{"status":"error","error":"..."}` out.
//!
//! **Real correction to the original requirements doc, not silently followed**:
//! that doc frames "embeddings/vector-storage architecture" as still open for this
//! agent to decide. Checked directly against `web/src/vector_store.rs` and
//! `web/src/rag.rs` before writing a line of this file: both are real and already
//! merged -- Postgres+pgvector storage (`vector_store.rs`'s own module doc) and
//! OpenAI `text-embedding-3-small` via `RAG_EMBEDDING_API_KEY`
//! (`rag.rs::EMBEDDING_MODEL`). That decision is not open; this agent's job is
//! exactly what its role name says -- extraction -- and nothing else.
//!
//! **First real increment: PDF only**, via the real `pdftotext` binary (poppler-utils,
//! confirmed installed on this operator's own infrastructure, not assumed). DOCX and
//! image/OCR support are separate, later increments (same "small real commits, not
//! one giant one" discipline the RAG PR #8 work already established) -- a request for
//! an unsupported `mime_type` gets a real, honest `Error`, never a fabricated
//! `Extracted`.

use base64::Engine;
use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};
use std::process::Command;

#[derive(Debug, Clone, Deserialize)]
struct ExtractionRequest {
    filename: String,
    mime_type: String,
    content_base64: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "status", rename_all = "snake_case")]
enum ExtractionResponse {
    Extracted { text: String },
    Error { error: String },
}

/// #183's own `MAX_SERVICE_INPUT_BYTES` bound (4 MiB), mirrored client-side already
/// by `devsystem_document_extraction_client`'s `MAX_INPUT_BYTES` -- re-enforced here
/// too since this handler must not trust an upstream caller already checked, same
/// reasoning `github_issue_channel_handler.rs::validate` states for its own bounds.
const MAX_CONTENT_BYTES: usize = 4 * 1024 * 1024;

/// Real extraction dispatch, pure with respect to the filesystem/subprocess except
/// for the injected `run_pdftotext` effect -- hermetically testable without a real
/// `pdftotext` binary, matching `github_issue_channel_handler::handle`'s own
/// dependency-injection shape.
fn extract(req: &ExtractionRequest, run_pdftotext: impl FnOnce(&[u8]) -> Result<String, String>) -> ExtractionResponse {
    let bytes = match base64::engine::general_purpose::STANDARD.decode(&req.content_base64) {
        Ok(b) => b,
        Err(e) => return ExtractionResponse::Error { error: format!("content_base64 did not decode: {e}") },
    };
    if bytes.len() > MAX_CONTENT_BYTES {
        return ExtractionResponse::Error { error: format!("document ({} bytes) exceeds MAX_CONTENT_BYTES ({MAX_CONTENT_BYTES})", bytes.len()) };
    }
    if bytes.is_empty() {
        return ExtractionResponse::Error { error: "document is empty after base64 decoding".to_string() };
    }

    match req.mime_type.as_str() {
        "application/pdf" => match run_pdftotext(&bytes) {
            Ok(text) if !text.trim().is_empty() => ExtractionResponse::Extracted { text },
            Ok(_) => ExtractionResponse::Error {
                error: format!("{}: pdftotext produced no extractable text -- likely a scanned/image-only PDF with no real text layer (OCR is a separate, not-yet-built increment)", req.filename),
            },
            Err(e) => ExtractionResponse::Error { error: format!("{}: pdftotext failed: {e}", req.filename) },
        },
        other => ExtractionResponse::Error {
            error: format!("{}: unsupported mime_type {other:?} -- only application/pdf is implemented so far (DOCX/image are separate, later increments)", req.filename),
        },
    }
}

/// The real `pdftotext` invocation: writes the decoded bytes to a real temp file
/// (pdftotext has no stdin-input mode for PDF -- it needs a real seekable file to
/// parse the PDF's xref table), then reads extracted text back from its stdout
/// (`-` as the output argument, avoiding a second temp file).
fn run_pdftotext_real(bytes: &[u8]) -> Result<String, String> {
    let dir = std::env::temp_dir().join(format!("devsystem-extraction-{}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| format!("could not create temp dir: {e}"))?;
    let pdf_path = dir.join("input.pdf");
    std::fs::write(&pdf_path, bytes).map_err(|e| format!("could not write temp PDF: {e}"))?;

    let output = Command::new("pdftotext")
        .arg("-layout")
        .arg(&pdf_path)
        .arg("-")
        .output()
        .map_err(|e| format!("could not run pdftotext: {e}"));

    let _ = std::fs::remove_dir_all(&dir);

    let output = output?;
    if !output.status.success() {
        return Err(format!("pdftotext exited with {}: {}", output.status, String::from_utf8_lossy(&output.stderr)));
    }
    String::from_utf8(output.stdout).map_err(|e| format!("pdftotext produced non-UTF-8 output: {e}"))
}

/// Same real finding `github_issue_channel_handler.rs::main`'s own doc comment
/// records (2026-08-04, live-verified over a real `ct-agent channel` call): a
/// non-zero handler exit is a hard TRANSPORT-level error to `ct-agent` and its
/// stdout is dropped entirely, so this always exits 0 once a response line is
/// printed; success/failure lives purely in the JSON body's `status` field.
fn main() -> std::process::ExitCode {
    let mut input = String::new();
    if let Err(e) = io::stdin().read_to_string(&mut input) {
        let resp = ExtractionResponse::Error { error: format!("could not read stdin: {e}") };
        println!("{}", serde_json::to_string(&resp).expect("ExtractionResponse always serializes"));
        return std::process::ExitCode::FAILURE;
    }

    let response = match serde_json::from_str::<ExtractionRequest>(input.trim()) {
        Ok(req) => extract(&req, run_pdftotext_real),
        Err(e) => ExtractionResponse::Error { error: format!("invalid request JSON: {e}") },
    };

    println!("{}", serde_json::to_string(&response).expect("ExtractionResponse always serializes"));
    io::stdout().flush().ok();
    std::process::ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(filename: &str, mime_type: &str, content_base64: &str) -> ExtractionRequest {
        ExtractionRequest { filename: filename.to_string(), mime_type: mime_type.to_string(), content_base64: content_base64.to_string() }
    }

    #[test]
    fn extract_returns_real_text_when_pdftotext_succeeds() {
        let response = extract(&req("spec.pdf", "application/pdf", "cmVhbCBieXRlcw=="), |bytes| {
            assert_eq!(bytes, b"real bytes", "the real decoded PDF bytes must reach pdftotext");
            Ok("real extracted PDF text".to_string())
        });
        assert_eq!(response, ExtractionResponse::Extracted { text: "real extracted PDF text".to_string() });
    }

    #[test]
    fn extract_reports_an_honest_error_when_pdftotext_finds_no_text_layer() {
        let response = extract(&req("scan.pdf", "application/pdf", "cmVhbCBieXRlcw=="), |_| Ok("   \n  \n".to_string()));
        match response {
            ExtractionResponse::Error { error } => assert!(error.contains("no extractable text"), "got: {error}"),
            other => panic!("expected a real Error for an empty text layer, got {other:?}"),
        }
    }

    #[test]
    fn extract_surfaces_a_real_pdftotext_failure_honestly() {
        let response = extract(&req("corrupt.pdf", "application/pdf", "cmVhbCBieXRlcw=="), |_| Err("not a real PDF".to_string()));
        assert_eq!(response, ExtractionResponse::Error { error: "corrupt.pdf: pdftotext failed: not a real PDF".to_string() });
    }

    #[test]
    fn extract_rejects_invalid_base64_before_ever_touching_pdftotext() {
        let mut called = false;
        let response = extract(&req("f.pdf", "application/pdf", "not valid base64!!"), |_| {
            called = true;
            Ok("should never happen".to_string())
        });
        assert!(!called, "pdftotext must never run on undecodable content");
        match response {
            ExtractionResponse::Error { error } => assert!(error.contains("did not decode"), "got: {error}"),
            other => panic!("expected a real Error, got {other:?}"),
        }
    }

    #[test]
    fn extract_reports_an_unsupported_mime_type_honestly_not_fabricated() {
        let mut called = false;
        let response = extract(&req("report.docx", "application/vnd.openxmlformats-officedocument.wordprocessingml.document", "eA=="), |_| {
            called = true;
            Ok("should never happen".to_string())
        });
        assert!(!called, "an unimplemented format must never reach pdftotext");
        match response {
            ExtractionResponse::Error { error } => assert!(error.contains("unsupported mime_type"), "got: {error}"),
            other => panic!("expected a real Error, got {other:?}"),
        }
    }

    #[test]
    fn extract_rejects_an_oversized_document_before_running_pdftotext() {
        let huge = base64::engine::general_purpose::STANDARD.encode(vec![0u8; MAX_CONTENT_BYTES + 1]);
        let mut called = false;
        let response = extract(&req("huge.pdf", "application/pdf", &huge), |_| {
            called = true;
            Ok("should never happen".to_string())
        });
        assert!(!called);
        match response {
            ExtractionResponse::Error { error } => assert!(error.contains("MAX_CONTENT_BYTES"), "got: {error}"),
            other => panic!("expected a real Error, got {other:?}"),
        }
    }

    #[test]
    fn extract_rejects_an_empty_document() {
        let response = extract(&req("empty.pdf", "application/pdf", ""), |_| Ok("should never happen".to_string()));
        match response {
            ExtractionResponse::Error { error } => assert!(error.contains("empty"), "got: {error}"),
            other => panic!("expected a real Error, got {other:?}"),
        }
    }
}
