//! The Development System's pipeline spec (#382): `plan -> test -> implement -> review ->
//! verify -> remember -> improve`, each stage a [`RequiredRole`] whose `service` is a
//! [`ServiceType::Custom`] name -- no CADS-Tunnel core change needed per new stage type,
//! the whole point of the `ServiceType::Custom` generalization this repo builds on.
//!
//! This crate is deliberately small: it defines the spec and proves (via `convene()`,
//! tested below with real signed offers, not just declared types) that CADS-Tunnel's
//! existing crew-auction primitive -- built for the flappy-demo crew (#171) -- genuinely
//! works unmodified for a completely different pipeline's roles. Discovery
//! (`/registry/agents`), channel wiring (Agent-Fabric), and escrow settlement are all
//! reused as-is from CADS-Tunnel/ct-agent; nothing about agent-to-agent plumbing is
//! reinvented here (see the coordination repo's README for the full picture).

use ct_common::channel::ServiceType;
use ct_common::pipeline::{PipelineSpec, RequiredRole, SelectionPolicy};

/// The seven pipeline-stage service names (#382 §3), each a `ServiceType::Custom` --
/// a pipeline-designer-level decision, not a CADS-Tunnel core one.
pub const STAGE_PLAN: &str = "devsystem.plan";
pub const STAGE_TEST: &str = "devsystem.test";
pub const STAGE_IMPLEMENT: &str = "devsystem.implement";
pub const STAGE_REVIEW: &str = "devsystem.review";
pub const STAGE_VERIFY: &str = "devsystem.verify";
pub const STAGE_REMEMBER: &str = "devsystem.remember";
pub const STAGE_IMPROVE: &str = "devsystem.improve";

/// All seven stage names, in pipeline order.
pub const ALL_STAGES: [&str; 7] =
    [STAGE_PLAN, STAGE_TEST, STAGE_IMPLEMENT, STAGE_REVIEW, STAGE_VERIFY, STAGE_REMEMBER, STAGE_IMPROVE];

/// Build the real [`PipelineSpec`] for one pipeline run, keyed by `run_id` (the
/// coordination repo's convention: one GitHub Issue per run, `run_id` matching the
/// issue number/slug). `operator_pubkey_hex` is the Agent-Fabric channel operator key
/// governing this run's role channels -- `None` while a run has no channels wired yet
/// (the #382 first slice's scope: the `plan` stage only, see [`plan_only_spec`]).
pub fn full_spec(run_id: &str, operator_pubkey_hex: Option<String>) -> PipelineSpec {
    PipelineSpec {
        id: format!("devsystem-{run_id}"),
        roles: ALL_STAGES
            .iter()
            .map(|stage| RequiredRole {
                service: ServiceType::Custom((*stage).to_string()),
                units: 1,
                tag: stage.strip_prefix("devsystem.").unwrap_or(stage).to_string(),
                selection_policy: None,
            })
            .collect(),
        operator_pubkey_hex,
        selection_policy: SelectionPolicy::LowestFloor,
    }
}

/// The #382 first-slice spec: **only** the `plan` role, matching the committed
/// sequencing ("stand up the coordination repo + generalize RequiredRole/convene() ...
/// + plan/Plan-Canvas stage only, before committing to the full seven-stage build").
/// The other six stages exist in [`full_spec`] but are not wired into any real run yet.
pub fn plan_only_spec(run_id: &str, operator_pubkey_hex: Option<String>) -> PipelineSpec {
    PipelineSpec {
        id: format!("devsystem-{run_id}"),
        roles: vec![RequiredRole {
            service: ServiceType::Custom(STAGE_PLAN.to_string()),
            units: 1,
            tag: "plan".to_string(),
            selection_policy: None,
        }],
        operator_pubkey_hex,
        selection_policy: SelectionPolicy::LowestFloor,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ct_common::channel::CapacityKind;
    use ct_common::pipeline::PipelineError;
    use ed25519_dalek::SigningKey;

    fn offer(seed: u8, services: Vec<ServiceType>, price: u64) -> ct_common::channel::CapacityOffer {
        let sk = SigningKey::from_bytes(&[seed; 32]);
        ct_common::channel::CapacityOffer::sign_new_with_services(
            &sk,
            CapacityKind::CloudApiQuota,
            vec!["claude".into()],
            1,
            price,
            "usd".into(),
            0,
            1_000_000,
            services,
        )
    }

    fn holder(seed: u8) -> [u8; 32] {
        SigningKey::from_bytes(&[seed; 32]).verifying_key().to_bytes()
    }

    #[test]
    fn full_spec_declares_all_seven_stages_as_distinct_custom_services() {
        let spec = full_spec("run-1", None);
        assert_eq!(spec.roles.len(), 7);
        let services: Vec<_> = spec.roles.iter().map(|r| r.service.clone()).collect();
        for (i, a) in services.iter().enumerate() {
            for (j, b) in services.iter().enumerate() {
                assert!(i == j || a != b, "every stage is its own distinct ServiceType::Custom -- no accidental aliasing");
            }
        }
        assert_eq!(spec.roles[0].tag, "plan");
        assert_eq!(spec.roles[6].tag, "improve");
    }

    #[test]
    fn plan_only_spec_convenes_for_real_when_a_plan_role_filler_is_online() {
        // The actual proof this crate exists to make: CADS-Tunnel's convene() -- built
        // for the flappy-demo crew, never touched for this -- genuinely clears a real
        // auction for a devsystem-declared, non-demo role, with a real signed offer.
        let spec = plan_only_spec("test-run", None);
        let plan_filler = offer(1, vec![ServiceType::Custom(STAGE_PLAN.to_string())], 10);

        let assignments = spec
            .convene(&[plan_filler], 100)
            .expect("a real, valid, online offer for the plan role convenes");
        assert_eq!(assignments.len(), 1);
        assert_eq!(assignments[0].service, ServiceType::Custom(STAGE_PLAN.to_string()));
        assert_eq!(assignments[0].provider, holder(1));
        assert_eq!(assignments[0].price, 10);
    }

    #[test]
    fn plan_only_spec_fails_closed_with_no_role_filler_online() {
        // The protocol's own promise (#382's opening quote): "if not enough agents are
        // online for an auction, the protocol raises an error" -- never a silent partial
        // run. Proven here with zero offers at all.
        let spec = plan_only_spec("test-run", None);
        assert_eq!(
            spec.convene(&[], 100),
            Err(PipelineError::UnfilledRole { service: ServiceType::Custom(STAGE_PLAN.to_string()) })
        );
    }

    #[test]
    fn an_offer_for_a_different_stage_never_fills_the_plan_role() {
        // Custom service types don't alias each other -- an implement-stage offer can't
        // accidentally clear the plan role just because both are ServiceType::Custom.
        let spec = plan_only_spec("test-run", None);
        let implement_filler = offer(2, vec![ServiceType::Custom(STAGE_IMPLEMENT.to_string())], 5);
        assert_eq!(
            spec.convene(&[implement_filler], 100),
            Err(PipelineError::UnfilledRole { service: ServiceType::Custom(STAGE_PLAN.to_string()) })
        );
    }
}
