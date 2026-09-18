use schemars::{json_schema, JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::{borrow::Cow, fmt, str::FromStr};
use uuid::Uuid;

macro_rules! id_type {
    ($name:ident) => {
        #[derive(
            Clone,
            Copy,
            Debug,
            Deserialize,
            Eq,
            Hash,
            JsonSchema,
            Ord,
            PartialEq,
            PartialOrd,
            Serialize,
        )]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl $name {
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }

            pub const fn from_uuid(value: Uuid) -> Self {
                Self(value)
            }

            pub const fn as_uuid(&self) -> &Uuid {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }

        impl FromStr for $name {
            type Err = uuid::Error;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Uuid::parse_str(value).map(Self)
            }
        }
    };
}

id_type!(WorkspaceId);
id_type!(SessionId);
id_type!(TurnId);
id_type!(ItemId);
id_type!(AgentId);
id_type!(TaskId);
id_type!(AttemptId);
id_type!(OperationId);
id_type!(EventId);
id_type!(CorrelationId);
id_type!(ArtifactId);
id_type!(FindingId);
id_type!(ApprovalId);
id_type!(FragmentId);
id_type!(ContextViewId);
id_type!(CheckpointId);
id_type!(SubscriptionId);
id_type!(RequestId);
id_type!(GrantId);
id_type!(MemoryId);

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum Principal {
    User(String),
    Agent(AgentId),
    System,
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
#[serde(try_from = "ResourceRefWire")]
pub struct ResourceRef {
    scheme: String,
    value: String,
}

#[derive(Deserialize, JsonSchema)]
struct ResourceRefWire {
    scheme: String,
    value: String,
}

impl ResourceRef {
    pub fn new(
        scheme: impl Into<String>,
        value: impl Into<String>,
    ) -> Result<Self, ResourceRefError> {
        let scheme = scheme.into();
        let value = value.into();
        let valid_scheme = scheme.bytes().enumerate().all(|(index, byte)| {
            matches!(
                (index, byte),
                (0, b'a'..=b'z')
                    | (_, b'a'..=b'z' | b'0'..=b'9' | b'+' | b'-' | b'.')
            )
        });
        if !valid_scheme {
            return Err(ResourceRefError::InvalidScheme);
        }
        if value.is_empty() {
            return Err(ResourceRefError::EmptyValue);
        }
        Ok(Self { scheme, value })
    }

    pub fn scheme(&self) -> &str {
        &self.scheme
    }

    pub fn value(&self) -> &str {
        &self.value
    }
}

impl TryFrom<ResourceRefWire> for ResourceRef {
    type Error = ResourceRefError;

    fn try_from(value: ResourceRefWire) -> Result<Self, Self::Error> {
        Self::new(value.scheme, value.value)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResourceRefError {
    InvalidScheme,
    EmptyValue,
}

impl fmt::Display for ResourceRefError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidScheme => "resource scheme must match [a-z][a-z0-9+.-]*",
            Self::EmptyValue => "resource value cannot be empty",
        })
    }
}

impl std::error::Error for ResourceRefError {}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct StateVersion([u8; 32]);

impl StateVersion {
    pub const fn from_digest(digest: [u8; 32]) -> Self {
        Self(digest)
    }

    pub const fn digest(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for StateVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl FromStr for StateVersion {
    type Err = StateVersionError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() != 64 {
            return Err(StateVersionError);
        }
        let mut digest = [0; 32];
        for (index, byte) in digest.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)
                .map_err(|_| StateVersionError)?;
        }
        Ok(Self(digest))
    }
}

impl Serialize for StateVersion {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for StateVersion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// Hand-written because the wire form is a hex string, not the byte array.
impl JsonSchema for StateVersion {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("StateVersion")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "pattern": "^[0-9a-f]{64}$",
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StateVersionError;

impl fmt::Display for StateVersionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("state version must be a 64-character hexadecimal digest")
    }
}

impl std::error::Error for StateVersionError {}

#[derive(
    Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(transparent)]
pub struct WorkspaceVersion(pub StateVersion);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domain_values_have_stable_round_trips() {
        let session = SessionId::new();
        assert_eq!(
            session,
            serde_json::from_str(&serde_json::to_string(&session).unwrap()).unwrap()
        );

        let principal = Principal::Agent(AgentId::new());
        assert_eq!(
            principal,
            serde_json::from_str(&serde_json::to_string(&principal).unwrap()).unwrap()
        );

        let resource = ResourceRef::new("workspace", "src/lib.rs").unwrap();
        assert_eq!(resource.scheme(), "workspace");
        assert_eq!(
            resource,
            serde_json::from_str(&serde_json::to_string(&resource).unwrap()).unwrap()
        );
        assert!(ResourceRef::new("Workspace", "src/lib.rs").is_err());

        let state = StateVersion::from_digest([0xab; 32]);
        assert_eq!(state.to_string(), "ab".repeat(32));
        assert_eq!(
            state,
            serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap()
        );
        assert_eq!(state, state.to_string().parse().unwrap());
    }
}
