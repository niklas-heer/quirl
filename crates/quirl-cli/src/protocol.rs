//! Composition-level inventory for Quirl's frozen public machine contracts.
//!
//! Schema definitions stay in their lowest owning crates. This module only
//! assembles their identities so one golden fixture catches cross-crate drift.

use quirl_core::{schema_fingerprint, CompatibilityPolicy};
use serde::Serialize;

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct FrozenContract {
    name: &'static str,
    owner: &'static str,
    current_version: String,
    oldest_readable: String,
    compatibility: CompatibilityPolicy,
    schema_hash: String,
    status: &'static str,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ProtocolFreezeManifest {
    document_type: &'static str,
    schema_version: u32,
    product_line: &'static str,
    contracts: Vec<FrozenContract>,
}

#[allow(dead_code)]
fn current_manifest() -> Result<ProtocolFreezeManifest, quirl_core::ShellError> {
    let mut contracts = vec![
        contract(
            "agent.catalog",
            "quirl-contract",
            quirl_contract::AGENT_SCHEMA_VERSION,
            quirl_contract::AGENT_SCHEMA_VERSION,
            CompatibilityPolicy::FrozenMajor,
            quirl_contract::agent_catalog_schema_hash(),
            "frozen",
        ),
        contract(
            "agent.context",
            "quirl-contract",
            quirl_contract::AGENT_SCHEMA_VERSION,
            quirl_contract::AGENT_SCHEMA_VERSION,
            CompatibilityPolicy::FrozenMajor,
            quirl_contract::agent_context_schema_hash(),
            "frozen",
        ),
        contract(
            "agent.manifest",
            "quirl-contract",
            quirl_contract::AGENT_SCHEMA_VERSION,
            quirl_contract::AGENT_SCHEMA_VERSION,
            CompatibilityPolicy::FrozenMajor,
            quirl_contract::agent_manifest_schema_hash(),
            "frozen",
        ),
        contract(
            "catalog",
            "quirl-catalog",
            quirl_catalog::CATALOG_SCHEMA_VERSION,
            quirl_catalog::CATALOG_OLDEST_READABLE_VERSION,
            CompatibilityPolicy::MigratedRange,
            schema_fingerprint(quirl_catalog::CATALOG_SCHEMA_DESCRIPTOR),
            "frozen",
        ),
        contract(
            "command_grammar",
            "quirl-syntax",
            quirl_syntax::GRAMMAR_PROTOCOL_VERSION,
            quirl_syntax::GRAMMAR_PROTOCOL_VERSION,
            CompatibilityPolicy::FrozenMajor,
            schema_fingerprint(quirl_syntax::GRAMMAR_SCHEMA_DESCRIPTOR),
            "preview_subset",
        ),
        contract(
            "common_abi",
            "quirl-core",
            quirl_core::COMMON_ABI_SCHEMA_VERSION,
            quirl_core::COMMON_ABI_SCHEMA_VERSION,
            CompatibilityPolicy::FrozenMajor,
            schema_fingerprint(quirl_core::COMMON_ABI_SCHEMA_DESCRIPTOR),
            "frozen",
        ),
        contract(
            "completion",
            "quirl-catalog",
            quirl_catalog::COMPLETION_PROTOCOL_VERSION,
            quirl_catalog::COMPLETION_PROTOCOL_VERSION,
            CompatibilityPolicy::FrozenMajor,
            schema_fingerprint(quirl_catalog::COMPLETION_SCHEMA_DESCRIPTOR),
            "rust_shape_frozen",
        ),
        contract(
            "compatibility_matrix",
            "quirl-syntax",
            quirl_syntax::COMPATIBILITY_MATRIX_SCHEMA_VERSION,
            quirl_syntax::COMPATIBILITY_MATRIX_SCHEMA_VERSION,
            CompatibilityPolicy::FrozenMajor,
            schema_fingerprint(quirl_syntax::COMPATIBILITY_MATRIX_JSON),
            "frozen_disposition",
        ),
        contract(
            "config",
            "quirl-lua",
            quirl_lua::CONFIG_SCHEMA_VERSION,
            quirl_lua::CONFIG_OLDEST_READABLE_VERSION,
            CompatibilityPolicy::MigratedRange,
            quirl_lua::config_schema_hash(),
            "legacy_unversioned_migrates",
        ),
        contract(
            "extension",
            "quirl-core",
            quirl_core::EXTENSION_PROTOCOL_VERSION,
            quirl_core::EXTENSION_PROTOCOL_VERSION,
            CompatibilityPolicy::FrozenMajor,
            quirl_core::extension_schema_hash(),
            "frozen",
        ),
        contract(
            "package",
            "quirl-contract",
            quirl_contract::PACKAGE_SCHEMA_VERSION,
            quirl_contract::PACKAGE_SCHEMA_VERSION,
            CompatibilityPolicy::FrozenMajor,
            quirl_contract::package_manifest_schema_hash(),
            "frozen_current_shape",
        ),
        contract(
            "picker",
            "quirl-picker",
            quirl_picker::PICKER_PROTOCOL_VERSION,
            quirl_picker::PICKER_PROTOCOL_VERSION,
            CompatibilityPolicy::FrozenMajor,
            schema_fingerprint(quirl_picker::PICKER_SCHEMA_DESCRIPTOR),
            "rust_shape_frozen",
        ),
        contract(
            "plugin_lock",
            "quirl-plugin",
            quirl_plugin::LOCK_SCHEMA_VERSION,
            1,
            CompatibilityPolicy::MigratedRange,
            quirl_plugin::plugin_lock_schema_hash(),
            "v1_migrates_to_v2",
        ),
        contract(
            "plugin_manifest",
            "quirl-plugin",
            quirl_plugin::PLUGIN_SCHEMA_VERSION,
            quirl_plugin::PLUGIN_SCHEMA_VERSION,
            CompatibilityPolicy::FrozenMajor,
            quirl_plugin::plugin_manifest_schema_hash(),
            "frozen",
        ),
        contract(
            "recovery",
            "quirl-cli",
            super::recovery::RECOVERY_SCHEMA_VERSION,
            super::recovery::RECOVERY_OLDEST_READABLE_VERSION,
            CompatibilityPolicy::MigratedRange,
            schema_fingerprint(super::recovery::RECOVERY_SCHEMA_DESCRIPTOR),
            "v1_migrates_to_v2",
        ),
        contract(
            "runner",
            "quirl-process",
            quirl_process::RUNNER_PROTOCOL_VERSION,
            quirl_process::RUNNER_PROTOCOL_VERSION,
            CompatibilityPolicy::FrozenMajor,
            quirl_process::runner_schema_hash(),
            "text_outcome_only",
        ),
        contract_text(
            "wasm_world",
            "quirl-plugin",
            "0.1.0",
            "0.1.0",
            CompatibilityPolicy::FrozenMajor,
            quirl_plugin::wasm_world_hash(),
            "nonexecuting_boundary",
        ),
    ];
    contracts.sort_by(|left, right| left.name.cmp(right.name));
    Ok(ProtocolFreezeManifest {
        document_type: "quirl.protocol.freeze",
        schema_version: quirl_core::PROTOCOL_FREEZE_VERSION,
        product_line: "0.1-to-1.0-freeze-candidate",
        contracts,
    })
}

fn contract(
    name: &'static str,
    owner: &'static str,
    current: u32,
    oldest: u32,
    compatibility: CompatibilityPolicy,
    schema_hash: String,
    status: &'static str,
) -> FrozenContract {
    contract_text(
        name,
        owner,
        &current.to_string(),
        &oldest.to_string(),
        compatibility,
        schema_hash,
        status,
    )
}

fn contract_text(
    name: &'static str,
    owner: &'static str,
    current: &str,
    oldest: &str,
    compatibility: CompatibilityPolicy,
    schema_hash: String,
    status: &'static str,
) -> FrozenContract {
    FrozenContract {
        name,
        owner,
        current_version: current.to_owned(),
        oldest_readable: oldest.to_owned(),
        compatibility,
        schema_hash,
        status,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_freeze_manifest_matches_the_reviewed_golden_fixture() {
        let actual = serde_json::to_value(current_manifest().unwrap()).unwrap();
        let expected: serde_json::Value =
            serde_json::from_str(include_str!("../../../docs/protocol-freeze-v1.json")).unwrap();
        assert_eq!(actual, expected);
    }
}
