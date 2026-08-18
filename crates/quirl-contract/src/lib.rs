//! Stable machine contracts for Quirl's agent and package surfaces.
//!
//! This crate owns serializable schemas and deterministic validation. The CLI
//! remains responsible for adapting the installed Lua `HOST_API` and for any
//! filesystem writes requested by a command.

#![cfg_attr(
    test,
    allow(
        dead_code_pub_in_binary,
        reason = "the libtest harness is an executable, but these public items remain library API"
    )
)]

mod agent;
mod package;
mod proposal;

pub use agent::{
    AGENT_CATALOG_SCHEMA_DESCRIPTOR, AGENT_CONTEXT_SCHEMA_DESCRIPTOR,
    AGENT_CONTEXT_SCHEMA_V1_DESCRIPTOR, AGENT_DOCUMENT_BYTES_MAX, AGENT_MANIFEST_SCHEMA_DESCRIPTOR,
    AGENT_SCHEMA_VERSION, AgentCatalog, AgentCommand, AgentContext, AgentDocumentKind,
    AgentManifest, AgentProvenance, AgentValidationAnchors, DEFAULT_TOKEN_BUDGET,
    DiagnosticSeverity, HostCapability, HostParameter, InstalledCapability, MINIMUM_TOKEN_BUDGET,
    ValidationDiagnostic, ValidationReport, agent_catalog_schema_hash, agent_context_schema_hash,
    agent_manifest_schema_hash, build_agent_catalog, build_agent_context, build_agent_manifest,
    render_context_markdown, validate_agent_document, validate_agent_document_with_anchors,
};
pub use package::{
    ArgumentKind, PACKAGE_BUILD_SCHEMA_DESCRIPTOR, PACKAGE_MANIFEST_BYTES_MAX,
    PACKAGE_SCHEMA_DESCRIPTOR, PACKAGE_SCHEMA_VERSION, PackageArgument, PackageBuild,
    PackageBuildOutcome, PackageCapabilitySection, PackageCommand, PackageContributions,
    PackageManifest, PackageMetadata, PackagePublishPlan, PackageSourceAudit, build_package,
    package_build_schema_hash, package_manifest_schema_hash, parse_package_manifest,
    validate_package_manifest,
};
pub use proposal::{
    COMMAND_PLANNING_INTENT_BYTES_MAX, COMMAND_PROPOSAL_ARGUMENTS_MAX,
    COMMAND_PROPOSAL_CATALOG_ARGUMENTS_MAX, COMMAND_PROPOSAL_EXPLANATION_BYTES_MAX,
    COMMAND_PROPOSAL_PRODUCER_BYTES_MAX, COMMAND_PROPOSAL_RENDER_BYTES_MAX,
    COMMAND_PROPOSAL_SCHEMA_DESCRIPTOR, COMMAND_PROPOSAL_SCHEMA_VERSION,
    COMMAND_PROPOSAL_SOURCE_BYTES_MAX, COMMAND_PROPOSAL_VALUE_BYTES_MAX,
    COMMAND_PROPOSAL_VALUES_BYTES_MAX, CommandPlanner, CommandPlanningRequest, CommandProposal,
    CommandProposalArgument, CommandProposalProvenance, CommandProposalRisk,
    CommandProposalRiskReason, CommandProposalSlot, CommandProposalSource, CommandProposalValue,
    CommandProposalValueKind, ValidatedCommandProposal, command_proposal_schema_hash,
};

/// Stable, dependency-free hash used in versioned schema identifiers.
///
/// The algorithm name is included in every rendered hash. This is an identity
/// checksum for freshness detection, not a cryptographic authenticity claim.
pub fn stable_hash(bytes: &[u8]) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("fnv1a64:{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_hash_is_named_and_reproducible() {
        assert_eq!(stable_hash(b"quirl"), stable_hash(b"quirl"));
        assert_ne!(stable_hash(b"quirl"), stable_hash(b"Quirl"));
        assert!(stable_hash(b"quirl").starts_with("fnv1a64:"));
    }
}
