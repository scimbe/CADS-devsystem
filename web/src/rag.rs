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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RagIndex {
    pub repo_url: String,
    pub synced_at: u64,
    pub branch: String,
    pub files_seen: usize,
    pub files_indexed: usize,
    pub chunks: Vec<RagChunk>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RagSearchResult {
    pub path: String,
    pub score: u32,
    pub snippet: String,
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
            chunks.push(RagChunk { path: entry.path.clone(), index, text: chunk });
        }
    }

    if tree.truncated {
        // GitHub itself truncated the tree response (an unusually large repo) --
        // real signal worth carrying, not swallowed, even though it doesn't
        // change v1's behavior beyond what files_seen already reports.
        eprintln!("devsystem-web: RAG sync for {repo_url} -- GitHub truncated the tree response, files_seen may undercount the real total");
    }

    Ok(RagIndex { repo_url: repo_url.to_string(), synced_at: now, branch, files_seen, files_indexed, chunks })
}

/// Real keyword search over a persisted index -- case-insensitive term overlap
/// scoring (count of query words present in the chunk, weighted slightly by
/// exact-phrase presence), not a fabricated relevance model. Good enough to
/// find the right doc file; not claiming to be more than that.
pub fn search(index: &RagIndex, query: &str, limit: usize) -> Vec<RagSearchResult> {
    let query_lower = query.to_ascii_lowercase();
    let terms: Vec<&str> = query_lower.split_whitespace().filter(|t| !t.is_empty()).collect();
    if terms.is_empty() {
        return Vec::new();
    }
    let mut scored: Vec<(u32, &RagChunk)> = index
        .chunks
        .iter()
        .filter_map(|c| {
            let text_lower = c.text.to_ascii_lowercase();
            let mut score = 0u32;
            for term in &terms {
                score += text_lower.matches(term).count() as u32;
            }
            if text_lower.contains(&query_lower) {
                score += 5;
            }
            (score > 0).then_some((score, c))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0));
    scored
        .into_iter()
        .take(limit)
        .map(|(score, c)| RagSearchResult { path: c.path.clone(), score, snippet: c.text.trim().chars().take(400).collect() })
        .collect()
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
                RagChunk { path: "a.md".into(), index: 0, text: "this document is about Noise_IK handshake details".into() },
                RagChunk { path: "b.md".into(), index: 0, text: "Noise appears here but handshake does not".into() },
            ],
        };
        let results = search(&index, "Noise_IK handshake", 10);
        assert_eq!(results[0].path, "a.md", "the real exact-phrase match should outrank a partial overlap");
    }

    #[test]
    fn search_returns_nothing_for_a_query_with_no_real_match() {
        let index = RagIndex { repo_url: "x".into(), synced_at: 0, branch: "main".into(), files_seen: 1, files_indexed: 1, chunks: vec![RagChunk { path: "a.md".into(), index: 0, text: "hello world".into() }] };
        assert!(search(&index, "nonexistent-term-xyz", 10).is_empty());
    }
}
