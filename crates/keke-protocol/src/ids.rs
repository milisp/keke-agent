//! Opaque, typed identifiers.
//!
//! Every cross-boundary id gets its own newtype so a `ThreadId` can never be
//! passed where a `TurnId` is expected. All of them are UUIDv7 so their string
//! form sorts chronologically, which the rollout log relies on.

use std::fmt;

use serde::Deserialize;
use serde::Serialize;
use uuid::Uuid;

macro_rules! typed_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            /// Mint a fresh, chronologically sortable id.
            #[must_use]
            pub fn new() -> Self {
                Self(Uuid::now_v7())
            }

            #[must_use]
            pub fn as_uuid(&self) -> Uuid {
                self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl From<Uuid> for $name {
            fn from(value: Uuid) -> Self {
                Self(value)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0, f)
            }
        }
    };
}

typed_id!(
    /// A persisted conversation, one rollout log file.
    SessionId
);
typed_id!(
    /// A branch of a session; forking a session mints a new thread.
    ThreadId
);
typed_id!(
    /// One user input and everything the agent does in response to it.
    TurnId
);

/// A model-assigned tool call identifier.
///
/// Unlike the ids above this is *not* minted locally: providers hand back their
/// own opaque strings and the result must echo them verbatim, so this is a
/// string newtype rather than a UUID.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ToolCallId(String);

impl ToolCallId {
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ToolCallId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
