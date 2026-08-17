use crate::{ErrorCode, ShellError};
use serde::{Deserialize, Serialize};

/// Project-wide major version at which the initial machine contracts were frozen.
pub const PROTOCOL_FREEZE_VERSION: u32 = 1;
/// Current version of the value, stream, result, and error ABI shared by crates.
pub const COMMON_ABI_SCHEMA_VERSION: u32 = 2;
/// Canonical structural description used to fingerprint the common ABI.
///
/// Any serialized-shape or semantic change must update this descriptor and the
/// corresponding schema version; [`schema_fingerprint`] derives its identity.
pub const COMMON_ABI_SCHEMA_DESCRIPTOR: &str = "quirl.abi@2{StructuredValue:adjacent-tag(type,value)[nothing|bool(bool)|int(i64)|uint(u64)|decimal(string)|string(string)|list(array<StructuredValue>)|record(map<string,StructuredValue>)|path(string)|duration{nanoseconds:u64}|size{bytes:u64}|date_time(string)|pattern(string)];Stream:bounded ordered StructuredValue sequence with cancellation;Result:ok(StructuredValue)|error(ShellError);ShellError{unknown_fields:currently-accepted;code:enum[invalid_command,invalid_argument,data,io,process_spawn,script_read,lua,validation,resource_limit];message:string;labels:array<{source:null|string,start:u64,end:u64,message:string}>;context:array<string>;help:array<string>;command:null|string;exit_status:null|i32}}";
/// Historical common ABI descriptor retained for version-1 identity checks.
pub const COMMON_ABI_SCHEMA_V1_DESCRIPTOR: &str = "quirl.abi@1{Value:null|bool|i64|f64|string|list<Value>|record<string,Value>;Stream:bounded ordered Value sequence with cancellation;Result:ok(Value)|error(ShellError);ShellError{unknown_fields:currently-accepted;code:enum[invalid_command,invalid_argument,data,io,process_spawn,script_read,lua,validation,resource_limit];message:string;labels:array<{source:null|string,start:usize,end:usize,message:string}>;context:array<string>;help:array<string>;command:null|string;exit_status:null|i32}}";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
/// Compatibility rule used when accepting a versioned machine document.
pub enum CompatibilityPolicy {
    /// Serialized shape and semantics cannot change without a new major version.
    FrozenMajor,
    /// Readers accept an explicitly enumerated migration floor through current.
    MigratedRange,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
/// Inclusive readable-version range and its advertised compatibility rule.
///
/// Writers emit [`Self::current`]. Readers accept values from
/// [`Self::oldest_readable`] through `current`, inclusive.
pub struct VersionPolicy {
    /// Version emitted by the current writer and newest version this reader accepts.
    pub current: u32,
    /// Oldest version for which the reader has an explicit compatibility path.
    pub oldest_readable: u32,
    /// Stability promise explaining whether older readable versions are migrated.
    pub compatibility: CompatibilityPolicy,
}

impl VersionPolicy {
    /// Construct a policy that accepts exactly one frozen major version.
    pub const fn frozen(current: u32) -> Self {
        Self {
            current,
            oldest_readable: current,
            compatibility: CompatibilityPolicy::FrozenMajor,
        }
    }

    /// Construct a policy for an explicitly migrated inclusive version range.
    ///
    /// Callers must keep `oldest_readable <= current`; an inverted range rejects
    /// every version when passed to [`Self::validate`].
    pub const fn migrated(current: u32, oldest_readable: u32) -> Self {
        Self {
            current,
            oldest_readable,
            compatibility: CompatibilityPolicy::MigratedRange,
        }
    }

    /// Check that `found` lies in this policy's inclusive readable range.
    ///
    /// `name` identifies the schema in diagnostics. Out-of-range versions return
    /// [`ErrorCode::Validation`] with upgrade or migration guidance.
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
            schema_fingerprint(COMMON_ABI_SCHEMA_V1_DESCRIPTOR),
            "fnv1a64:e46e0983e50da1af"
        );
        assert_eq!(
            schema_fingerprint(COMMON_ABI_SCHEMA_DESCRIPTOR),
            "fnv1a64:254f7565de75d691"
        );
    }

    #[test]
    fn common_abi_v1_fails_closed_and_current_value_shape_is_typed() {
        assert!(VersionPolicy::frozen(COMMON_ABI_SCHEMA_VERSION)
            .validate("common ABI", 1)
            .is_err());
        let value = crate::StructuredValue::Duration { nanoseconds: 42 };
        assert_eq!(
            serde_json::to_value(value).unwrap(),
            serde_json::json!({
                "type": "duration",
                "value": {"nanoseconds": 42},
            })
        );
    }
}
