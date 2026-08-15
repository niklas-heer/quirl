use crate::{ErrorCode, ShellError};
use serde::{Deserialize, Serialize};

pub const PROTOCOL_FREEZE_VERSION: u32 = 1;
pub const COMMON_ABI_SCHEMA_VERSION: u32 = 1;
pub const COMMON_ABI_SCHEMA_DESCRIPTOR: &str = "quirl.abi@1{Value:null|bool|i64|f64|string|list<Value>|record<string,Value>;Stream:bounded ordered Value sequence with cancellation;Result:ok(Value)|error(ShellError);ShellError{unknown_fields:currently-accepted;code:enum[invalid_command,invalid_argument,data,io,process_spawn,script_read,lua,validation,resource_limit];message:string;labels:array<{source:null|string,start:usize,end:usize,message:string}>;context:array<string>;help:array<string>;command:null|string;exit_status:null|i32}}";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CompatibilityPolicy {
    /// Serialized shape and semantics cannot change without a new major version.
    FrozenMajor,
    /// Readers accept an explicitly enumerated migration floor through current.
    MigratedRange,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct VersionPolicy {
    pub current: u32,
    pub oldest_readable: u32,
    pub compatibility: CompatibilityPolicy,
}

impl VersionPolicy {
    pub const fn frozen(current: u32) -> Self {
        Self {
            current,
            oldest_readable: current,
            compatibility: CompatibilityPolicy::FrozenMajor,
        }
    }

    pub const fn migrated(current: u32, oldest_readable: u32) -> Self {
        Self {
            current,
            oldest_readable,
            compatibility: CompatibilityPolicy::MigratedRange,
        }
    }

    pub fn validate(self, name: &str, found: u32) -> Result<(), ShellError> {
        if found < self.oldest_readable || found > self.current {
            return Err(ShellError::new(
                ErrorCode::Validation,
                format!(
                    "{name} schema version {found} is outside the readable range {}..={}",
                    self.oldest_readable, self.current
                ),
            )
            .with_help(if found > self.current {
                "Upgrade Quirl before reading a document produced by a newer schema"
            } else {
                "Migrate the document with a Quirl release that still supports its schema"
            }));
        }
        Ok(())
    }
}

/// Canonical, deterministic identity hash for a complete structural descriptor.
/// This is a compatibility fingerprint, not a cryptographic integrity claim.
pub fn schema_fingerprint(descriptor: &str) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in descriptor.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("fnv1a64:{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_policy_rejects_future_and_expired_documents() {
        let policy = VersionPolicy::migrated(4, 2);
        assert!(policy.validate("catalog", 2).is_ok());
        assert!(policy.validate("catalog", 4).is_ok());
        assert!(policy.validate("catalog", 1).is_err());
        assert!(policy.validate("catalog", 5).is_err());

        let frozen = VersionPolicy::frozen(1);
        assert!(frozen.validate("extension", 1).is_ok());
        assert!(frozen.validate("extension", 0).is_err());
        assert!(frozen.validate("extension", 2).is_err());
    }

    #[test]
    fn common_abi_descriptor_has_a_frozen_identity() {
        assert_eq!(
            schema_fingerprint(COMMON_ABI_SCHEMA_DESCRIPTOR),
            "fnv1a64:e46e0983e50da1af"
        );
    }
}
