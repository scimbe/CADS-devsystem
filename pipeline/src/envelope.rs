//! The zylos envelope (`docs/role-contracts.md`), implemented for real: until now it
//! was documentation only -- every [`IterationRecord`] produced actual results, but
//! nothing shaped them into the mem0-filterable form the docs promise. This is the
//! `devsystem.remember` stage's first real piece: turning a run's iterations into a
//! durable, structured log instead of only living in `state.json`'s free-text fields.

use crate::IterationRecord;
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::Path;

/// The envelope payload itself (`docs/role-contracts.md` schema v1) -- what a stage
/// actually produced, independent of which run/stage/role it came from.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ZylosEnvelope {
    pub task: String,
    pub key_findings: Vec<String>,
    pub constraints: Vec<String>,
    pub output_format: String,
}

/// `unreviewed` (default, on write) or `governed` (promoted after a human review) --
/// borrowed from ECC's own vault/governed-artifact split, per the docs.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Trust {
    #[default]
    Unreviewed,
    Governed,
}

/// The envelope plus its mem0-filterable metadata (`run_id`/`stage`/`role`/`trust`) --
/// the actual unit a `mem0.search(filters={...})` call would match against.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EnvelopeRecord {
    pub run_id: String,
    pub stage: String,
    pub role: String,
    pub trust: Trust,
    pub envelope: ZylosEnvelope,
}

/// Derive an [`EnvelopeRecord`] from a real [`IterationRecord`] -- no invented content:
/// `key_findings` wraps the iteration's actual feedback text as-is (not split into
/// synthesized bullet points, which would risk misrepresenting what the stage said);
/// `constraints` are the real proposals' rationale (a proposal genuinely is something
/// the next stage must know about and account for).
pub fn envelope_from_iteration(record: &IterationRecord) -> EnvelopeRecord {
    let constraints = record.proposals.iter().map(|p| format!("{}: {}", p.stage_id, p.rationale)).collect();
    EnvelopeRecord {
        run_id: record.run_id.clone(),
        stage: record.stage.clone(),
        role: record.stage.strip_prefix("devsystem.").unwrap_or(&record.stage).to_string(),
        trust: Trust::Unreviewed,
        envelope: ZylosEnvelope {
            task: record.stage.clone(),
            key_findings: vec![record.feedback.clone()],
            constraints,
            output_format: "markdown".to_string(),
        },
    }
}

/// Append one [`EnvelopeRecord`] as a single JSON line to `path` (JSONL), creating the
/// file if needed. Ready to be loaded into mem0/Qdrant later without reshaping --
/// that backend isn't wired yet (README's status section), but the durable log format
/// it will consume already is.
pub fn append_to_memory_log(path: &Path, record: &EnvelopeRecord) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    let line = serde_json::to_string(record).expect("EnvelopeRecord always serializes");
    writeln!(file, "{line}")
}

/// Read every entry ever appended to a run's `memory.jsonl`, in write order -- the
/// read side `append_to_memory_log` never had, which meant `devsystem.remember`'s
/// durable log was write-only in practice: real data landed on disk every iteration
/// but nothing (CLI or GUI) could ever look back at it. A missing file (a run with
/// no iterations yet) is a normal empty log, not an error; a line that fails to
/// parse is skipped rather than failing the whole read, since one malformed
/// historical line shouldn't hide every other real entry from a human trying to
/// review this run's memory.
pub fn read_memory_log(path: &Path) -> io::Result<Vec<EnvelopeRecord>> {
    let contents = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    Ok(contents.lines().filter(|line| !line.trim().is_empty()).filter_map(|line| serde_json::from_str(line).ok()).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StageProposal;

    fn record(proposals: Vec<StageProposal>) -> IterationRecord {
        IterationRecord {
            run_id: "run-envelope".into(),
            stage: "devsystem.review".into(),
            iteration: 4,
            feedback: "found and fixed allowBackup=true and raw-pixel padding".into(),
            proposals,
            succeeded: true,
        }
    }

    #[test]
    fn derives_task_role_and_key_findings_from_the_real_feedback_text() {
        let env = envelope_from_iteration(&record(vec![]));
        assert_eq!(env.run_id, "run-envelope");
        assert_eq!(env.stage, "devsystem.review");
        assert_eq!(env.role, "review", "role strips the devsystem. prefix per the tag convention");
        assert_eq!(env.trust, Trust::Unreviewed, "every fresh envelope starts unreviewed");
        assert_eq!(env.envelope.key_findings, vec!["found and fixed allowBackup=true and raw-pixel padding".to_string()]);
        assert!(env.envelope.constraints.is_empty());
        assert_eq!(env.envelope.output_format, "markdown");
    }

    #[test]
    fn a_proposals_rationale_becomes_a_real_constraint_for_the_next_stage() {
        let proposal = StageProposal {
            proposed_by: "devsystem.review".into(),
            stage_id: "devsystem.android_native_bridge".into(),
            tag: "android_native_bridge".into(),
            rationale: "reuse the audited Rust Noise_IK code".into(),
            use_existing_service: None,
            units: 1,
            price_ceiling: None,
        };
        let env = envelope_from_iteration(&record(vec![proposal]));
        assert_eq!(env.envelope.constraints, vec!["devsystem.android_native_bridge: reuse the audited Rust Noise_IK code".to_string()]);
    }

    #[test]
    fn trust_serializes_as_the_documented_lowercase_strings() {
        assert_eq!(serde_json::to_string(&Trust::Unreviewed).unwrap(), "\"unreviewed\"");
        assert_eq!(serde_json::to_string(&Trust::Governed).unwrap(), "\"governed\"");
    }

    #[test]
    fn appending_twice_produces_two_valid_independently_parseable_json_lines() {
        let dir = std::env::temp_dir().join(format!("devsystem-envelope-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("memory.jsonl");
        let _ = std::fs::remove_file(&path);

        let env1 = envelope_from_iteration(&record(vec![]));
        let mut rec2 = record(vec![]);
        rec2.iteration = 5;
        rec2.feedback = "second iteration's real feedback".into();
        let env2 = envelope_from_iteration(&rec2);

        append_to_memory_log(&path, &env1).unwrap();
        append_to_memory_log(&path, &env2).unwrap();

        let contents = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2);
        let parsed1: EnvelopeRecord = serde_json::from_str(lines[0]).unwrap();
        let parsed2: EnvelopeRecord = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(parsed1, env1);
        assert_eq!(parsed2, env2);

        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir(&dir).unwrap();
    }

    #[test]
    fn reading_a_nonexistent_memory_log_returns_an_empty_vec_not_an_error() {
        let path = std::env::temp_dir().join(format!("devsystem-envelope-missing-{}-{}", std::process::id(), line!()));
        let _ = std::fs::remove_file(&path);
        assert_eq!(read_memory_log(&path).unwrap(), Vec::new());
    }

    #[test]
    fn read_memory_log_round_trips_every_appended_entry_in_order() {
        let dir = std::env::temp_dir().join(format!("devsystem-envelope-readback-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("memory.jsonl");
        let _ = std::fs::remove_file(&path);

        let env1 = envelope_from_iteration(&record(vec![]));
        let mut rec2 = record(vec![]);
        rec2.iteration = 5;
        rec2.feedback = "second iteration's real feedback".into();
        let env2 = envelope_from_iteration(&rec2);

        append_to_memory_log(&path, &env1).unwrap();
        append_to_memory_log(&path, &env2).unwrap();

        let read_back = read_memory_log(&path).unwrap();
        assert_eq!(read_back, vec![env1, env2]);

        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir(&dir).unwrap();
    }
}
