//! What is known about a conversation at the moment it starts.
//!
//! A [`ConversationRecord`] is written twice — onto the LiveKit room as metadata, so
//! the agent learns who it is serving when it joins, and into the append-only log, so
//! `GET /v1/convai/conversations` can bill for it after the room is gone. Those are
//! two renderings of one value built in one place, never two values kept in step, so
//! there is no way for them to disagree about a conversation.

use crate::conversation::ConversationId;
use serde::{Deserialize, Serialize};
use std::fmt;

/// The agent the caller asked for, from the `agent_id` query parameter.
///
/// A newtype because it travels beside [`HappyUserId`] as a second bare string, and
/// swapping the two would produce a conversation that is well-formed, billable to the
/// wrong party, and served by an agent nobody selected — with nothing to notice it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentId(String);

/// Happy's per-user identifier, from the `participant_name` query parameter.
///
/// Happy derives it as `u_<hmac>` and later filters usage with `?user_id=<this>`, so
/// it is the join key between minting a token and billing for the call.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HappyUserId(String);

macro_rules! string_newtype {
    ($name:ident) => {
        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

string_newtype!(AgentId);
string_newtype!(HappyUserId);

/// A conversation, as of the moment its room was created.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationRecord {
    pub conversation_id: ConversationId,
    pub agent_id: AgentId,
    /// Absent on the bring-your-own-key path, which mints a token without a
    /// `participant_name` (see the BYO handler in Happy's `voiceRoutes.ts`). Such a
    /// conversation is unattributable by design: the caller pays their own provider,
    /// so no usage is gated on it.
    pub happy_user: Option<HappyUserId>,
    /// Seconds since the Unix epoch. Matches the unit ElevenLabs reports conversation
    /// timestamps in, so the usage endpoint can serve this value without conversion.
    pub started_at_unix_secs: i64,
}

impl ConversationRecord {
    /// Builds the record. The clock is a parameter rather than a call to
    /// [`std::time::SystemTime`] so that constructing a record stays pure and its
    /// serialization is testable against a fixed instant.
    pub fn start(
        conversation_id: ConversationId,
        agent_id: AgentId,
        happy_user: Option<HappyUserId>,
        started_at_unix_secs: i64,
    ) -> Self {
        Self { conversation_id, agent_id, happy_user, started_at_unix_secs }
    }
}

/// Reads the wall clock. The one impure step in building a record, kept apart from
/// [`ConversationRecord::start`] so the rest stays a pure function of its inputs.
pub fn now_unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock is before the Unix epoch")
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> ConversationRecord {
        ConversationRecord::start(
            ConversationId::parse("conv_abc123").unwrap(),
            AgentId::new("agent_happy"),
            Some(HappyUserId::new("u_deadbeef")),
            1_700_000_000,
        )
    }

    #[test]
    fn a_record_round_trips_through_json() {
        let json = serde_json::to_string(&sample()).unwrap();
        assert_eq!(serde_json::from_str::<ConversationRecord>(&json).unwrap(), sample());
    }

    #[test]
    fn the_byo_path_records_no_user() {
        let record = ConversationRecord::start(
            ConversationId::generate(),
            AgentId::new("agent_happy"),
            None,
            1_700_000_000,
        );
        let json = serde_json::to_value(&record).unwrap();
        assert_eq!(json["happy_user"], serde_json::Value::Null);
        assert_eq!(serde_json::from_value::<ConversationRecord>(json).unwrap(), record);
    }
}
