//! The zylos envelope (`docs/role-contracts.md`), implemented for real: until now it
//! was documentation only -- every [`IterationRecord`] produced actual results, but
//! nothing shaped them into the mem0-filterable form the docs promise. This is the
//! `devsystem.remember` stage's first real piece: turning a run's iterations into a
//! durable, structured log instead of only living in `state.json`'s free-text fields.

use crate::runner::Requirement;
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
/// synthesized bullet points, which would risk misrepresenting what the stage said),
/// plus one real line per requirement this iteration actually claims to address
/// (`requirements` resolves `record.requirement_indices` to their real statement text
/// at the time this envelope is written -- a durable log entry should capture what the
/// statement actually said then, not a mutable reference that could drift or vanish
/// later; gap found+fixed 2026-08-05, same traceability data the check-in markdown and
/// GUI panels already surface, previously missing from the one other real human-viewed
/// surface -- the Memory Log panel's `key_findings` rendering); `constraints` are the
/// real proposals' rationale (a proposal genuinely is something the next stage must
/// know about and account for). `ZylosEnvelope`'s shape itself (`docs/role-contracts.md`
/// schema v1) is a fixed external contract -- deliberately not adding a new field to it,
/// folding this into `key_findings` instead since it genuinely is one.
pub fn envelope_from_iteration(record: &IterationRecord, requirements: &[Requirement]) -> EnvelopeRecord {
    let constraints = record.proposals.iter().map(|p| format!("{}: {}", p.stage_id, p.rationale)).collect();
    let mut key_findings = vec![record.feedback.clone()];
    for &i in &record.requirement_indices {
        match requirements.get(i) {
            Some(r) => key_findings.push(format!("Addressed requirement: {}", r.statement)),
            None => key_findings.push(format!("Addressed requirement #{i} (no longer exists)")),
        }
    }
    EnvelopeRecord {
        run_id: record.run_id.clone(),
        stage: record.stage.clone(),
        role: record.stage.strip_prefix("devsystem.").unwrap_or(&record.stage).to_string(),
        trust: Trust::Unreviewed,
        envelope: ZylosEnvelope {
            task: record.stage.clone(),
            key_findings,
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

/// Promote the entry at `index` (0-based, in `read_memory_log`'s write order) from
/// `Trust::Unreviewed` to `Trust::Governed` and persist every entry back to `path`.
/// `Trust::Governed` was documented ("promoted after a human review", modeled on
/// ECC's vault/governed-artifact split) but nothing anywhere ever actually set it --
/// this is the only place that should: an explicit human action through the GUI,
/// never automatic. Returns `NotFound` for an out-of-range index rather than
/// silently doing nothing, so a stale GUI reference fails loudly.
pub fn govern_memory_entry(path: &Path, index: usize) -> io::Result<Vec<EnvelopeRecord>> {
    let mut entries = read_memory_log(path)?;
    let entry = entries
        .get_mut(index)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("no memory entry at index {index}")))?;
    entry.trust = Trust::Governed;

    let mut file = std::fs::File::create(path)?;
    for e in &entries {
        let line = serde_json::to_string(e).expect("EnvelopeRecord always serializes");
        writeln!(file, "{line}")?;
    }
    Ok(entries)
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
            requirement_indices: Vec::new(),
        }
    }

    #[test]
    fn derives_task_role_and_key_findings_from_the_real_feedback_text() {
        let env = envelope_from_iteration(&record(vec![]), &[]);
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
        let env = envelope_from_iteration(&record(vec![proposal]), &[]);
        assert_eq!(env.envelope.constraints, vec!["devsystem.android_native_bridge: reuse the audited Rust Noise_IK code".to_string()]);
    }

    #[test]
    fn a_real_addressed_requirement_becomes_a_key_finding_with_its_actual_statement_text() {
        let requirements = vec![Requirement {
            statement: "WHEN a user sends a text message over an established channel, THE SYSTEM SHALL persist it locally before confirming delivery to the UI".into(),
            acceptance_criteria: vec!["message survives an app restart".into()],
            verified: false,
        }];
        let mut rec = record(vec![]);
        rec.requirement_indices = vec![0];
        let env = envelope_from_iteration(&rec, &requirements);
        assert_eq!(
            env.envelope.key_findings,
            vec![
                "found and fixed allowBackup=true and raw-pixel padding".to_string(),
                "Addressed requirement: WHEN a user sends a text message over an established channel, THE SYSTEM SHALL persist it locally before confirming delivery to the UI".to_string(),
            ]
        );
    }

    #[test]
    fn an_addressed_requirement_index_that_no_longer_exists_is_reported_honestly_not_silently_dropped() {
        let mut rec = record(vec![]);
        rec.requirement_indices = vec![7];
        let env = envelope_from_iteration(&rec, &[]);
        assert!(env.envelope.key_findings[1].contains("#7"));
        assert!(env.envelope.key_findings[1].contains("no longer exists"));
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

        let env1 = envelope_from_iteration(&record(vec![]), &[]);
        let mut rec2 = record(vec![]);
        rec2.iteration = 5;
        rec2.feedback = "second iteration's real feedback".into();
        let env2 = envelope_from_iteration(&rec2, &[]);

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
    fn governing_an_entry_persists_governed_and_leaves_other_entries_untouched() {
        let dir = std::env::temp_dir().join(format!("devsystem-envelope-govern-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("memory.jsonl");
        let _ = std::fs::remove_file(&path);

        let env1 = envelope_from_iteration(&record(vec![]), &[]);
        let mut rec2 = record(vec![]);
        rec2.iteration = 5;
        rec2.feedback = "second iteration's real feedback".into();
        let env2 = envelope_from_iteration(&rec2, &[]);
        append_to_memory_log(&path, &env1).unwrap();
        append_to_memory_log(&path, &env2).unwrap();

        let returned = govern_memory_entry(&path, 1).unwrap();
        assert_eq!(returned[0].trust, Trust::Unreviewed, "only the targeted entry should change");
        assert_eq!(returned[1].trust, Trust::Governed);

        let reread = read_memory_log(&path).unwrap();
        assert_eq!(reread, returned, "the promotion must actually persist to disk, not just the in-memory return value");

        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir(&dir).unwrap();
    }

    #[test]
    fn governing_an_out_of_range_index_fails_loudly_instead_of_silently_no_opping() {
        let dir = std::env::temp_dir().join(format!("devsystem-envelope-govern-oob-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("memory.jsonl");
        let _ = std::fs::remove_file(&path);
        append_to_memory_log(&path, &envelope_from_iteration(&record(vec![]), &[])).unwrap();

        let err = govern_memory_entry(&path, 5).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);

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

        let env1 = envelope_from_iteration(&record(vec![]), &[]);
        let mut rec2 = record(vec![]);
        rec2.iteration = 5;
        rec2.feedback = "second iteration's real feedback".into();
        let env2 = envelope_from_iteration(&rec2, &[]);

        append_to_memory_log(&path, &env1).unwrap();
        append_to_memory_log(&path, &env2).unwrap();

        let read_back = read_memory_log(&path).unwrap();
        assert_eq!(read_back, vec![env1, env2]);

        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir(&dir).unwrap();
    }
}
