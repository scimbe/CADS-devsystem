//! Real per-run build artifacts (issue #36, #382 goal doc): a run had no way
//! to carry a real, downloadable, verifiable build output -- only free-text
//! `feedback` on an iteration, so a claim like "APK built, sha256 abc123,
//! commit deadbeef" could never actually be checked, and
//! `webconference-android` requirement #5 ("SHALL produce a downloadable,
//! installable release APK artifact that is traceable to the exact source
//! commit it was built from") was structurally unsatisfiable, not merely
//! unfinished. Mirrors `rag.rs`'s own persisted-JSON-file-per-run pattern (a
//! separate `artifacts.json`, not folded into `state.json`) -- these are real
//! files with their own real bytes on disk, a genuinely different storage
//! shape from anything `RunState` already models.

use serde::{Deserialize, Serialize};

/// One real, downloadable build artifact. Every field a reviewer needs to
/// actually verify the claim requirement #5 exists to enforce, not trust as
/// prose: `sha256` is computed server-side from the real uploaded bytes (see
/// `main.rs::upload_artifact`), never accepted as a client-supplied value --
/// the same "server computes/stamps, client never dictates" discipline
/// `submitted_by`/`created_by`/`confirmed_by` already established for every
/// other real provenance field in this codebase.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Artifact {
    pub id: String,
    pub filename: String,
    pub sha256: String,
    pub size_bytes: u64,
    #[serde(default)]
    pub source_commit: Option<String>,
    #[serde(default)]
    pub version_name: Option<String>,
    #[serde(default)]
    pub version_code: Option<String>,
    #[serde(default)]
    pub signing_identity: Option<String>,
    /// The real iteration number this artifact was produced by -- cross-checked
    /// against the run's own real `state.history` at upload time (see
    /// `main.rs::upload_artifact`), not accepted as an unverified claim, so
    /// "traceable to the exact source commit" means a real, checkable link to
    /// a real iteration record, not just a number typed into a form.
    pub producing_iteration: u32,
    pub producing_stage: String,
    pub uploaded_at: u64,
    /// Real, gate-verified identity (`X-Gate-Email`), honestly `None` for a
    /// header-less upload -- same convention as `created_by`/`confirmed_by`.
    #[serde(default)]
    pub uploaded_by: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ArtifactIndex {
    #[serde(default)]
    pub artifacts: Vec<Artifact>,
}
