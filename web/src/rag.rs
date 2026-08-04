//! Real, minimal RAG (#382 task 29): "upload your docs/project info to the repo
//! you designate for the project, and it gets loaded into a searchable index" --
//! the operator's own framing, referencing prior real experimentation in
//! <https://github.com/scimbe/cadserv> (a much heavier Surfsense/pgvector/HAProxy
//! stack -- deliberately NOT reused here, see the design decisions below).
//!
//! Three open design questions from task 29, decided here rather than guessed at
//! in the abstract, each grounded in a real constraint of this deployment:
//!
//! 1. **Target repo**: whatever `RunState.repo_url` already names for a run --
//!    reuses the exact mechanism `set_repo_url` already built, rather than
//!    inventing a second, separate "docs repo" concept.
//! 2. **Resource budget**: this host runs a real, already-tight fleet (control
//!    plane, edge, Keycloak, devsystem-web, multiple demo origins/agents --
//!    "prod host resource limits", 4 CPU / 7.6GB / no swap). A Postgres+pgvector
//!    +Redis+HAProxy stack (cadserv's real shape) is not a responsible addition
//!    here. This is real keyword/full-text search over the repo's actual doc
//!    files -- an in-memory inverted index, serialized to plain JSON per run,
//!    zero new services, zero new heavy dependencies. Explicitly NOT semantic/
//!    vector search: that needs a real embedding-model credential this
//!    deployment doesn't have configured, and fabricating "semantic" search
//!    without one would be exactly the kind of dishonest capability claim this
//!    whole codebase has consistently avoided. A future increment with a real
//!    embedding credential is a distinct, separately-scoped decision.
//! 3. **Sync mechanism**: pull, not push -- an explicit "sync now" GUI action
//!    (`POST /rag/sync`) that fetches the repo's current tree via GitHub's REST
//!    API, matching the existing client-side GitHub-API-call precedent
//!    (`web/static/index.html`'s own already-documented ~60-req/hr unauthenticated
//!    rate-limit awareness). A GitHub webhook would need a new public,
//!    unauthenticated endpoint and a shared secret to manage -- real added
//!    surface for a feature that's fine to be manually triggered for now.

use serde::{Deserialize, Serialize};
use std::path::Path;

/// Doc/text file extensions worth indexing -- deliberately narrow (the
/// operator's own ask was "Dokumentationen und Projekt infos", not "the whole
/// source tree"), and keeps a sync fast and the index small.
const INDEXABLE_EXTENSIONS: &[&str] = &["md", "mdx", "txt", "rst", "adoc"];
/// Files without an extension worth indexing anyway (repo-root convention).
const INDEXABLE_BASENAMES: &[&str] = &["README", "LICENSE", "CHANGELOG", "CONTRIBUTING"];
/// Hard caps so a sync against a large repo stays fast and bounded -- silently
/// truncating past these would misrepresent what's actually indexed, so
/// `sync_repo` reports the real counts (selected vs. indexed) rather than
/// pretending everything was covered.
const MAX_FILES: usize = 60;
const MAX_FILE_BYTES: usize = 200_000;
/// Chunk size in characters -- small enough that a search result's snippet is
/// actually useful context, large enough that most doc paragraphs survive
/// whole rather than being split mid-thought.
const CHUNK_CHARS: usize = 800;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RagChunk {
    pub path: String,
    pub index: usize,
    pub text: String,
    /// Real semantic-search embedding, populated by [`embed_texts`] when a real
    /// `RAG_EMBEDDING_API_KEY` is configured -- `None` on any deployment/run
    /// without one (or for chunks synced before this field existed; `#[serde(default)]`
    /// so old persisted `rag_index.json` files still deserialize). Search never
    /// pretends a `None` embedding scored semantically -- see [`semantic_search`].
    #[serde(default)]
    pub embedding: Option<Vec<f32>>,
}

/// A real, human-uploaded document (#382 RAG slice 2 -- "kann ich Dokumente
/// hochladen"). Plain text/markdown pasted or uploaded through the GUI, not a
/// GitHub file -- lives alongside `chunks` (the repo sync's output) but is
/// never touched by `sync_repo`, so re-syncing the repo can't silently delete
/// something a human added by hand. Chunked on the fly at search time (not
/// pre-chunked and stored twice) via the same [`chunk_text`] the sync path
/// uses, so both sources score identically.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RagDocument {
    pub id: String,
    pub path: String,
    pub text: String,
    pub added_at: u64,
    /// Same real-embedding-or-`None` contract as [`RagChunk::embedding`] -- a
    /// manual document is embedded whole (not per query-time chunk) so it only
    /// costs one real embedding call regardless of how many times it's searched.
    #[serde(default)]
    pub embedding: Option<Vec<f32>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RagIndex {
    pub repo_url: String,
    pub synced_at: u64,
    pub branch: String,
    pub files_seen: usize,
    pub files_indexed: usize,
    pub chunks: Vec<RagChunk>,
    #[serde(default)]
    pub manual_documents: Vec<RagDocument>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RagSearchResult {
    pub path: String,
    pub score: u32,
    pub snippet: String,
    /// Additive field (existing `GET /rag/search` callers ignore unknown JSON
    /// fields, so this stays backward compatible) -- which real scoring path
    /// produced this result. `"keyword"` is `score_chunk`'s term-overlap count,
    /// unchanged. `"semantic"` is a real cosine-similarity score against a real
    /// embedding, scaled to the same rough magnitude as keyword scores so the
    /// two are comparably sortable when merged -- see [`semantic_search`].
    pub match_kind: MatchKind,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MatchKind {
    Keyword,
    Semantic,
}

fn parse_owner_repo(repo_url: &str) -> Result<(String, String), String> {
    let trimmed = repo_url.trim().trim_end_matches('/').trim_end_matches(".git");
    let rest = trimmed.strip_prefix("https://github.com/").ok_or_else(|| format!("not a github.com https URL: {repo_url:?}"))?;
    let mut parts = rest.splitn(2, '/');
    let owner = parts.next().filter(|s| !s.is_empty()).ok_or_else(|| format!("no owner in {repo_url:?}"))?;
    let repo = parts.next().filter(|s| !s.is_empty()).ok_or_else(|| format!("no repo name in {repo_url:?}"))?;
    Ok((owner.to_string(), repo.to_string()))
}

fn is_indexable(path: &str) -> bool {
    let name = Path::new(path).file_name().and_then(|n| n.to_str()).unwrap_or("");
    if let Some((stem, ext)) = name.rsplit_once('.') {
        if INDEXABLE_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()) {
            return true;
        }
        // README.md-style: stem check still needs the extension check above;
        // this branch is for extensionless matches falling through below.
        let _ = stem;
    }
    INDEXABLE_BASENAMES.iter().any(|b| name.eq_ignore_ascii_case(b))
}

fn chunk_text(text: &str) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    for line in text.lines() {
        if current.len() + line.len() + 1 > CHUNK_CHARS && !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
        }
        // A single real line longer than the whole chunk budget (long prose with
        // no line breaks, a minified/dense file) would otherwise never split at
        // all -- hard-wrap it on its own rather than silently producing one
        // oversized chunk.
        if line.len() > CHUNK_CHARS {
            for piece in line.as_bytes().chunks(CHUNK_CHARS) {
                chunks.push(String::from_utf8_lossy(piece).into_owned());
            }
            continue;
        }
        current.push_str(line);
        current.push('\n');
    }
    if !current.trim().is_empty() {
        chunks.push(current);
    }
    chunks
}

#[derive(Deserialize)]
struct RepoMeta {
    default_branch: String,
}

#[derive(Deserialize)]
struct TreeResponse {
    tree: Vec<TreeEntry>,
    truncated: bool,
}

#[derive(Deserialize)]
struct TreeEntry {
    path: String,
    #[serde(rename = "type")]
    kind: String,
    size: Option<u64>,
}

/// Real fetch + chunk + index, run once per "Sync now" click (or once per
/// `POST /rag/sync`). Two GitHub API calls total (repo metadata, then one
/// recursive tree listing) regardless of file count -- file bodies come from
/// `raw.githubusercontent.com`, not the API's per-blob endpoint, to stay well
/// inside GitHub's unauthenticated rate limit for a server-side caller shared
/// across every run on this deployment.
pub async fn sync_repo(client: &reqwest::Client, repo_url: &str, now: u64) -> Result<RagIndex, String> {
    let (owner, repo) = parse_owner_repo(repo_url)?;
    let ua = ("User-Agent", "devsystem-web-rag/1 (+https://github.com/scimbe/CADS-devsystem)");

    let meta: RepoMeta = client
        .get(format!("https://api.github.com/repos/{owner}/{repo}"))
        .header(ua.0, ua.1)
        .send()
        .await
        .map_err(|e| format!("could not reach GitHub for repo metadata: {e}"))?
        .error_for_status()
        .map_err(|e| format!("GitHub rejected the repo metadata request: {e}"))?
        .json()
        .await
        .map_err(|e| format!("could not parse GitHub's repo metadata response: {e}"))?;
    let branch = meta.default_branch;

    let tree: TreeResponse = client
        .get(format!("https://api.github.com/repos/{owner}/{repo}/git/trees/{branch}?recursive=1"))
        .header(ua.0, ua.1)
        .send()
        .await
        .map_err(|e| format!("could not reach GitHub for the file tree: {e}"))?
        .error_for_status()
        .map_err(|e| format!("GitHub rejected the tree request: {e}"))?
        .json()
        .await
        .map_err(|e| format!("could not parse GitHub's tree response: {e}"))?;

    let mut candidates: Vec<&TreeEntry> = tree
        .tree
        .iter()
        .filter(|e| e.kind == "blob" && is_indexable(&e.path) && e.size.is_none_or(|s| s as usize <= MAX_FILE_BYTES))
        .collect();
    // Deterministic, not filesystem-order-dependent (git's tree order is already
    // stable, but sorting makes a re-sync's file selection reproducible even if
    // that weren't true).
    candidates.sort_by(|a, b| a.path.cmp(&b.path));
    let files_seen = candidates.len();
    candidates.truncate(MAX_FILES);

    let mut chunks = Vec::new();
    let mut files_indexed = 0usize;
    for entry in &candidates {
        let raw_url = format!("https://raw.githubusercontent.com/{owner}/{repo}/{branch}/{}", entry.path);
        let text = match client.get(&raw_url).header(ua.0, ua.1).send().await {
            Ok(r) if r.status().is_success() => match r.text().await {
                Ok(t) => t,
                Err(_) => continue,
            },
            _ => continue,
        };
        files_indexed += 1;
        for (index, chunk) in chunk_text(&text).into_iter().enumerate() {
            chunks.push(RagChunk { path: entry.path.clone(), index, text: chunk, embedding: None });
        }
    }

    if tree.truncated {
        // GitHub itself truncated the tree response (an unusually large repo) --
        // real signal worth carrying, not swallowed, even though it doesn't
        // change v1's behavior beyond what files_seen already reports.
        eprintln!("devsystem-web: RAG sync for {repo_url} -- GitHub truncated the tree response, files_seen may undercount the real total");
    }

    Ok(RagIndex { repo_url: repo_url.to_string(), synced_at: now, branch, files_seen, files_indexed, chunks, manual_documents: Vec::new() })
}

/// Real keyword search over a persisted index -- case-insensitive term overlap
/// scoring (count of query words present in the chunk, weighted slightly by
/// exact-phrase presence), not a fabricated relevance model. Good enough to
/// find the right doc file; not claiming to be more than that.
fn score_chunk(query_lower: &str, terms: &[&str], text: &str) -> u32 {
    let text_lower = text.to_ascii_lowercase();
    let mut score = 0u32;
    for term in terms {
        score += text_lower.matches(term).count() as u32;
    }
    if text_lower.contains(query_lower) {
        score += 5;
    }
    score
}

pub fn search(index: &RagIndex, query: &str, limit: usize) -> Vec<RagSearchResult> {
    let query_lower = query.to_ascii_lowercase();
    let terms: Vec<&str> = query_lower.split_whitespace().filter(|t| !t.is_empty()).collect();
    if terms.is_empty() {
        return Vec::new();
    }
    let mut scored: Vec<(u32, String, String)> = Vec::new();
    for c in &index.chunks {
        let score = score_chunk(&query_lower, &terms, &c.text);
        if score > 0 {
            scored.push((score, c.path.clone(), c.text.clone()));
        }
    }
    // Manual documents are chunked here, at query time, rather than pre-chunked
    // and persisted twice -- they're usually short (pasted/uploaded text, not a
    // whole repo), so re-chunking per search is cheap, and it means chunk_text's
    // own behavior only ever needs to be correct in one place.
    for doc in &index.manual_documents {
        for chunk in chunk_text(&doc.text) {
            let score = score_chunk(&query_lower, &terms, &chunk);
            if score > 0 {
                scored.push((score, doc.path.clone(), chunk));
            }
        }
    }
    scored.sort_by(|a, b| b.0.cmp(&a.0));
    scored
        .into_iter()
        .take(limit)
        .map(|(score, path, text)| RagSearchResult {
            path,
            score,
            snippet: text.trim().chars().take(400).collect(),
            match_kind: MatchKind::Keyword,
        })
        .collect()
}

/// Real cosine similarity, no crate needed for two `Vec<f32>` dot products at
/// this scale (a run's chunk count is capped by `MAX_FILES`/`CHUNK_CHARS`, so
/// this is always a handful of `O(dim)` multiplications, not a bottleneck --
/// see `rag.rs`'s module doc for why that ruled out a vector-DB/ANN dependency
/// entirely rather than half-adopting one for no real performance need).
/// Returns `0.0` for a zero-magnitude vector rather than dividing by zero.
fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let mag_a = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let mag_b = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if mag_a == 0.0 || mag_b == 0.0 {
        return 0.0;
    }
    dot / (mag_a * mag_b)
}

/// Real semantic search: cosine similarity between a real query embedding and
/// every chunk/document that actually has one. Silently skips (not errors on)
/// anything with `embedding: None` -- a run synced before an embedding
/// credential was configured degrades to "fewer semantic results", never a
/// crash or a fabricated score. Scaled by 100 so a `1.0` (identical-direction)
/// cosine similarity lands in the same rough magnitude as a strong keyword
/// match's term-overlap count, making [`combined_search`]'s merge sort
/// meaningful rather than one signal always drowning the other by construction.
pub fn semantic_search(index: &RagIndex, query_embedding: &[f32], limit: usize) -> Vec<RagSearchResult> {
    let mut scored: Vec<(f32, String, String)> = Vec::new();
    for c in &index.chunks {
        if let Some(emb) = &c.embedding {
            let sim = cosine_similarity(query_embedding, emb);
            if sim > 0.0 {
                scored.push((sim, c.path.clone(), c.text.clone()));
            }
        }
    }
    for doc in &index.manual_documents {
        if let Some(emb) = &doc.embedding {
            let sim = cosine_similarity(query_embedding, emb);
            if sim > 0.0 {
                scored.push((sim, doc.path.clone(), doc.text.clone()));
            }
        }
    }
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    scored
        .into_iter()
        .take(limit)
        .map(|(sim, path, text)| RagSearchResult {
            path,
            score: (sim * 100.0).round() as u32,
            snippet: text.trim().chars().take(400).collect(),
            match_kind: MatchKind::Semantic,
        })
        .collect()
}

/// Real design decision (stated per the requirements doc, not silently
/// picked): run keyword and semantic search independently, then merge by
/// interleaving on score with keyword-tagged results breaking ties first (the
/// existing, proven-useful scoring stays primary when both signals agree on
/// magnitude) and de-duplicate on `(path, snippet)` so the same chunk showing
/// up in both lists doesn't produce a visually-doubled result -- the surviving
/// entry keeps whichever `match_kind` scored it higher. `query_embedding` is
/// `None` whenever no real embedding credential is configured for this
/// deployment (see [`embed_texts`]): semantic search is skipped entirely in
/// that case, never faked, and this degrades to exactly today's keyword-only
/// `search()` behavior -- the additive contract the requirements doc asked for.
pub fn combined_search(index: &RagIndex, query: &str, query_embedding: Option<&[f32]>, limit: usize) -> Vec<RagSearchResult> {
    let mut merged: Vec<RagSearchResult> = search(index, query, limit);
    if let Some(qe) = query_embedding {
        for sem in semantic_search(index, qe, limit) {
            match merged.iter_mut().find(|r| r.path == sem.path && r.snippet == sem.snippet) {
                Some(existing) if sem.score > existing.score => *existing = sem,
                Some(_) => {}
                None => merged.push(sem),
            }
        }
    }
    merged.sort_by(|a, b| b.score.cmp(&a.score));
    merged.truncate(limit);
    merged
}

#[derive(Serialize)]
struct OpenAiEmbeddingRequest<'a> {
    model: &'a str,
    input: &'a [String],
}

#[derive(Deserialize)]
struct OpenAiEmbeddingResponse {
    data: Vec<OpenAiEmbeddingDatum>,
}

#[derive(Deserialize)]
struct OpenAiEmbeddingDatum {
    embedding: Vec<f32>,
    index: usize,
}

/// Model choice stated explicitly per the requirements doc's own ask, not
/// buried in a config default: OpenAI `text-embedding-3-small` (1536-dim) --
/// cheap, well-documented HTTP API, no SDK dependency (plain `reqwest::Client`
/// POST, matching every other external call already in this file).
pub const EMBEDDING_MODEL: &str = "text-embedding-3-small";

/// Real embedding call, never fabricated -- requires `api_key` to be a real,
/// live-checkable credential (`RAG_EMBEDDING_API_KEY` at the call site in
/// `main.rs`; this function itself is provider-shaped, not env-var-aware, so
/// it stays hermetically testable against a mock server). Empty `texts`
/// returns `Ok(vec![])` without a network call. Response embeddings are
/// re-ordered by OpenAI's own `index` field before returning, so a caller
/// never has to assume response order matches request order.
pub async fn embed_texts(client: &reqwest::Client, api_base: &str, api_key: &str, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
    if texts.is_empty() {
        return Ok(Vec::new());
    }
    let resp = client
        .post(format!("{}/embeddings", api_base.trim_end_matches('/')))
        .bearer_auth(api_key)
        .json(&OpenAiEmbeddingRequest { model: EMBEDDING_MODEL, input: texts })
        .send()
        .await
        .map_err(|e| format!("could not reach the embedding API: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("embedding API rejected the request ({status}): {body}"));
    }
    let parsed: OpenAiEmbeddingResponse = resp.json().await.map_err(|e| format!("could not parse the embedding API response: {e}"))?;
    if parsed.data.len() != texts.len() {
        return Err(format!("embedding API returned {} embeddings for {} inputs", parsed.data.len(), texts.len()));
    }
    let mut ordered: Vec<Option<Vec<f32>>> = vec![None; texts.len()];
    for datum in parsed.data {
        if datum.index >= ordered.len() {
            return Err(format!("embedding API returned an out-of-range index {}", datum.index));
        }
        ordered[datum.index] = Some(datum.embedding);
    }
    ordered.into_iter().collect::<Option<Vec<_>>>().ok_or_else(|| "embedding API response was missing an index".to_string())
}

/// One extracted element from an Unstructured `/general/v0/general` response
/// -- real OCR/vision text for an image, real parsed text for a PDF/DOCX
/// paragraph, etc. `element_type` (e.g. `"Title"`, `"NarrativeText"`,
/// `"Image"`) is Unstructured's own classification, kept rather than
/// discarded so a caller can decide whether to weight titles differently --
/// v1 here just concatenates every element's text, a deliberately simple
/// first cut matching this module's own "don't overbuild ahead of a real
/// need" pattern elsewhere.
#[derive(Debug, Clone, Deserialize)]
pub struct UnstructuredElement {
    pub text: String,
    #[serde(rename = "type")]
    pub element_type: String,
}

/// Real Unstructured API call — hosted `api.unstructured.io`, not self-hosted
/// (per CADS-devsystem#7's "moves the resource cost off this host" constraint).
/// `/general/v0/general` handles PDF/DOCX/HTML and — the operator's explicit
/// ask — images (real OCR/vision extraction) in one endpoint; which real
/// capability is used is Unstructured's own server-side strategy selection,
/// not something this client picks. `api_base` is parameterized the same way
/// `embed_texts`'s is, so this stays hermetically testable against a local
/// mock multipart-accepting server, never a real network call in a test.
/// Returns the real extracted elements in the order Unstructured returned
/// them; an empty result (e.g. a genuinely blank image) is `Ok(vec![])`, not
/// an error -- "found nothing to extract" is a real, valid outcome, distinct
/// from "the call itself failed."
pub async fn parse_with_unstructured(
    client: &reqwest::Client,
    api_base: &str,
    api_key: &str,
    filename: &str,
    bytes: Vec<u8>,
) -> Result<Vec<UnstructuredElement>, String> {
    let part = reqwest::multipart::Part::bytes(bytes).file_name(filename.to_string());
    let form = reqwest::multipart::Form::new().part("files", part);
    let resp = client
        .post(format!("{}/general/v0/general", api_base.trim_end_matches('/')))
        .header("unstructured-api-key", api_key)
        .multipart(form)
        .send()
        .await
        .map_err(|e| format!("could not reach the Unstructured API: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("Unstructured API rejected the request ({status}): {body}"));
    }
    resp.json::<Vec<UnstructuredElement>>().await.map_err(|e| format!("could not parse the Unstructured API response: {e}"))
}

/// Real, honest cap on what an image/document upload becomes as RAG text --
/// the requirements doc's own ask ("an image file needs its own real, stated
/// size cap"). Applied to the *extracted text*, not the original file bytes
/// (a large scanned PDF might extract to far less text than its byte size, or
/// a dense one to more per page than expected -- the actual downstream cost
/// this codebase cares about, chunk/embedding volume, tracks the text, not
/// the upload size).
pub const MAX_UNSTRUCTURED_EXTRACTED_CHARS: usize = 200_000;

/// Concatenates real Unstructured elements into one text blob for the
/// existing `chunk_text`/embedding pipeline, applying
/// [`MAX_UNSTRUCTURED_EXTRACTED_CHARS`] honestly -- truncates and says so via
/// the returned `bool` (`true` = truncated) rather than silently dropping the
/// tail, matching `sync_repo`'s own `files_seen`-vs-`files_indexed` honesty
/// precedent.
pub fn elements_to_text(elements: &[UnstructuredElement]) -> (String, bool) {
    let mut text = String::new();
    for el in elements {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(el.text.trim());
    }
    if text.chars().count() > MAX_UNSTRUCTURED_EXTRACTED_CHARS {
        let truncated: String = text.chars().take(MAX_UNSTRUCTURED_EXTRACTED_CHARS).collect();
        (truncated, true)
    } else {
        (text, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_real_github_https_url() {
        assert_eq!(parse_owner_repo("https://github.com/scimbe/CADS-webconference-android"), Ok(("scimbe".to_string(), "CADS-webconference-android".to_string())));
        assert_eq!(parse_owner_repo("https://github.com/scimbe/CADS-webconference-android/"), Ok(("scimbe".to_string(), "CADS-webconference-android".to_string())));
        assert_eq!(parse_owner_repo("https://github.com/scimbe/CADS-webconference-android.git"), Ok(("scimbe".to_string(), "CADS-webconference-android".to_string())));
    }

    #[test]
    fn rejects_a_non_github_url_rather_than_guessing() {
        assert!(parse_owner_repo("https://gitlab.com/scimbe/x").is_err());
        assert!(parse_owner_repo("not a url").is_err());
    }

    #[test]
    fn indexable_extensions_and_root_basenames() {
        assert!(is_indexable("docs/architecture.md"));
        assert!(is_indexable("README"));
        assert!(is_indexable("README.md"));
        assert!(!is_indexable("src/main.rs"));
        assert!(!is_indexable("app/build.gradle.kts"));
    }

    #[test]
    fn chunk_text_splits_on_the_real_char_budget_not_mid_repeated_line() {
        let long = "word ".repeat(1000);
        let chunks = chunk_text(&long);
        assert!(chunks.len() > 1, "a long real document must split into multiple chunks");
        for c in &chunks {
            assert!(c.len() <= CHUNK_CHARS + 10, "no chunk should wildly exceed the real budget");
        }
    }

    #[test]
    fn search_ranks_a_real_exact_phrase_match_above_a_partial_word_overlap() {
        let index = RagIndex {
            repo_url: "https://github.com/x/y".into(),
            synced_at: 0,
            branch: "main".into(),
            files_seen: 2,
            files_indexed: 2,
            chunks: vec![
                RagChunk { path: "a.md".into(), index: 0, text: "this document is about Noise_IK handshake details".into(), embedding: None },
                RagChunk { path: "b.md".into(), index: 0, text: "Noise appears here but handshake does not".into(), embedding: None },
            ],
            manual_documents: Vec::new(),
        };
        let results = search(&index, "Noise_IK handshake", 10);
        assert_eq!(results[0].path, "a.md", "the real exact-phrase match should outrank a partial overlap");
    }

    #[test]
    fn search_returns_nothing_for_a_query_with_no_real_match() {
        let index = RagIndex {
            repo_url: "x".into(),
            synced_at: 0,
            branch: "main".into(),
            files_seen: 1,
            files_indexed: 1,
            chunks: vec![RagChunk { path: "a.md".into(), index: 0, text: "hello world".into(), embedding: None }],
            manual_documents: Vec::new(),
        };
        assert!(search(&index, "nonexistent-term-xyz", 10).is_empty());
    }

    #[test]
    fn search_covers_a_real_manually_uploaded_document_not_just_repo_sync_chunks() {
        let index = RagIndex {
            repo_url: "x".into(),
            synced_at: 0,
            branch: "main".into(),
            files_seen: 0,
            files_indexed: 0,
            chunks: Vec::new(),
            manual_documents: vec![RagDocument {
                id: "doc-1".into(),
                path: "notes.txt".into(),
                text: "a real uploaded note about Agent-Fabric channel joins".into(),
                added_at: 0,
                embedding: None,
            }],
        };
        let results = search(&index, "Agent-Fabric", 10);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].path, "notes.txt");
    }

    #[test]
    fn keyword_search_results_are_tagged_keyword() {
        let index = RagIndex {
            repo_url: "x".into(),
            synced_at: 0,
            branch: "main".into(),
            files_seen: 1,
            files_indexed: 1,
            chunks: vec![RagChunk { path: "a.md".into(), index: 0, text: "hello world".into(), embedding: None }],
            manual_documents: Vec::new(),
        };
        let results = search(&index, "hello", 10);
        assert_eq!(results[0].match_kind, MatchKind::Keyword);
    }

    #[test]
    fn cosine_similarity_of_identical_vectors_is_one() {
        assert!((cosine_similarity(&[1.0, 2.0, 3.0], &[1.0, 2.0, 3.0]) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_similarity_of_orthogonal_vectors_is_zero() {
        assert!(cosine_similarity(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6);
    }

    #[test]
    fn cosine_similarity_handles_zero_vector_without_dividing_by_zero() {
        assert_eq!(cosine_similarity(&[0.0, 0.0], &[1.0, 1.0]), 0.0);
    }

    #[test]
    fn cosine_similarity_mismatched_dims_is_zero_not_a_panic() {
        assert_eq!(cosine_similarity(&[1.0, 2.0], &[1.0, 2.0, 3.0]), 0.0);
    }

    #[test]
    fn semantic_search_ranks_the_real_closer_embedding_first() {
        let index = RagIndex {
            repo_url: "x".into(),
            synced_at: 0,
            branch: "main".into(),
            files_seen: 2,
            files_indexed: 2,
            chunks: vec![
                RagChunk { path: "close.md".into(), index: 0, text: "close".into(), embedding: Some(vec![1.0, 0.0]) },
                RagChunk { path: "far.md".into(), index: 0, text: "far".into(), embedding: Some(vec![0.0, 1.0]) },
            ],
            manual_documents: Vec::new(),
        };
        let results = semantic_search(&index, &[1.0, 0.1], 10);
        assert_eq!(results[0].path, "close.md");
        assert_eq!(results[0].match_kind, MatchKind::Semantic);
    }

    #[test]
    fn semantic_search_silently_skips_chunks_with_no_embedding_rather_than_erroring() {
        let index = RagIndex {
            repo_url: "x".into(),
            synced_at: 0,
            branch: "main".into(),
            files_seen: 1,
            files_indexed: 1,
            chunks: vec![RagChunk { path: "unembedded.md".into(), index: 0, text: "no embedding here".into(), embedding: None }],
            manual_documents: Vec::new(),
        };
        assert!(semantic_search(&index, &[1.0, 0.0], 10).is_empty());
    }

    #[test]
    fn combined_search_degrades_to_keyword_only_when_no_query_embedding_is_given() {
        let index = RagIndex {
            repo_url: "x".into(),
            synced_at: 0,
            branch: "main".into(),
            files_seen: 1,
            files_indexed: 1,
            chunks: vec![RagChunk { path: "a.md".into(), index: 0, text: "keyword match here".into(), embedding: Some(vec![1.0, 0.0]) }],
            manual_documents: Vec::new(),
        };
        let results = combined_search(&index, "keyword", None, 10);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].match_kind, MatchKind::Keyword);
    }

    #[test]
    fn combined_search_deduplicates_a_chunk_that_scores_on_both_signals() {
        let index = RagIndex {
            repo_url: "x".into(),
            synced_at: 0,
            branch: "main".into(),
            files_seen: 1,
            files_indexed: 1,
            chunks: vec![RagChunk { path: "a.md".into(), index: 0, text: "noise handshake".into(), embedding: Some(vec![1.0, 0.0]) }],
            manual_documents: Vec::new(),
        };
        let results = combined_search(&index, "noise handshake", Some(&[1.0, 0.0]), 10);
        assert_eq!(results.len(), 1, "the same chunk scoring on both keyword and semantic search must not appear twice");
    }

    #[test]
    fn embed_texts_with_no_input_makes_no_network_call() {
        // Deliberately no mock server bound here -- if this made a real call it
        // would fail to connect and the test would hang/error, proving the
        // early-return really is taken.
        let client = reqwest::Client::new();
        let result = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(embed_texts(&client, "http://127.0.0.1:1", "fake-key", &[]));
        assert_eq!(result, Ok(Vec::new()));
    }

    async fn spawn_mock_embedding_server(status: axum::http::StatusCode, response_body: serde_json::Value) -> String {
        use axum::{routing::post, Router};
        let mock_app = Router::new().route(
            "/embeddings",
            post(move || {
                let status = status;
                let response_body = response_body.clone();
                async move { (status, axum::Json(response_body)) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind mock listener");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            axum::serve(listener, mock_app).await.expect("serve mock");
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn embed_texts_returns_real_embeddings_reordered_by_the_providers_own_index() {
        let base = spawn_mock_embedding_server(
            axum::http::StatusCode::OK,
            serde_json::json!({"data": [
                {"embedding": [0.5, 0.5], "index": 1},
                {"embedding": [1.0, 0.0], "index": 0}
            ]}),
        )
        .await;
        let client = reqwest::Client::new();
        let result = embed_texts(&client, &base, "fake-key", &["first".to_string(), "second".to_string()]).await.unwrap();
        assert_eq!(result, vec![vec![1.0, 0.0], vec![0.5, 0.5]], "response order must not be assumed to match request order");
    }

    #[tokio::test]
    async fn embed_texts_reports_a_real_provider_error_honestly_not_silently() {
        let base = spawn_mock_embedding_server(axum::http::StatusCode::UNAUTHORIZED, serde_json::json!({"error": "invalid api key"})).await;
        let client = reqwest::Client::new();
        let result = embed_texts(&client, &base, "bad-key", &["text".to_string()]).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("401"));
    }

    #[test]
    fn elements_to_text_joins_real_elements_with_newlines() {
        let elements = vec![
            UnstructuredElement { text: "Title Here".into(), element_type: "Title".into() },
            UnstructuredElement { text: "Body text extracted from the image.".into(), element_type: "NarrativeText".into() },
        ];
        let (text, truncated) = elements_to_text(&elements);
        assert_eq!(text, "Title Here\nBody text extracted from the image.");
        assert!(!truncated);
    }

    #[test]
    fn elements_to_text_reports_truncation_honestly_rather_than_silently_dropping_the_tail() {
        let long_text = "x".repeat(MAX_UNSTRUCTURED_EXTRACTED_CHARS + 500);
        let elements = vec![UnstructuredElement { text: long_text, element_type: "NarrativeText".into() }];
        let (text, truncated) = elements_to_text(&elements);
        assert_eq!(text.chars().count(), MAX_UNSTRUCTURED_EXTRACTED_CHARS);
        assert!(truncated);
    }

    #[test]
    fn elements_to_text_of_no_elements_is_empty_not_an_error() {
        let (text, truncated) = elements_to_text(&[]);
        assert_eq!(text, "");
        assert!(!truncated);
    }

    async fn spawn_mock_unstructured_server(status: axum::http::StatusCode, response_body: serde_json::Value) -> String {
        use axum::{routing::post, Router};
        let mock_app = Router::new().route(
            "/general/v0/general",
            post(move || {
                let status = status;
                let response_body = response_body.clone();
                async move { (status, axum::Json(response_body)) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind mock listener");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            axum::serve(listener, mock_app).await.expect("serve mock");
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn parse_with_unstructured_returns_real_extracted_elements() {
        let base = spawn_mock_unstructured_server(
            axum::http::StatusCode::OK,
            serde_json::json!([
                {"text": "A scanned title", "type": "Title"},
                {"text": "Body text a real OCR pass extracted from the image", "type": "NarrativeText"}
            ]),
        )
        .await;
        let client = reqwest::Client::new();
        let elements = parse_with_unstructured(&client, &base, "fake-key", "scan.png", b"fake-png-bytes".to_vec()).await.unwrap();
        assert_eq!(elements.len(), 2);
        assert_eq!(elements[0].element_type, "Title");
        assert_eq!(elements[1].text, "Body text a real OCR pass extracted from the image");
    }

    #[tokio::test]
    async fn parse_with_unstructured_reports_a_real_provider_error_honestly() {
        let base = spawn_mock_unstructured_server(axum::http::StatusCode::UNPROCESSABLE_ENTITY, serde_json::json!({"detail": "unsupported file type"})).await;
        let client = reqwest::Client::new();
        let result = parse_with_unstructured(&client, &base, "fake-key", "weird.xyz", b"bytes".to_vec()).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("422"));
    }

    #[tokio::test]
    async fn parse_with_unstructured_of_a_genuinely_empty_result_is_ok_not_an_error() {
        let base = spawn_mock_unstructured_server(axum::http::StatusCode::OK, serde_json::json!([])).await;
        let client = reqwest::Client::new();
        let elements = parse_with_unstructured(&client, &base, "fake-key", "blank.png", b"bytes".to_vec()).await.unwrap();
        assert!(elements.is_empty());
    }
}
