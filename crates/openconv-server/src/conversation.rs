//! The conversation identifier, and the room-naming invariant it exists to enforce.
//!
//! # Why this is a type and not a `String`
//!
//! Two independent consumers recover the conversation ID by running the regex
//! `(conv_[a-zA-Z0-9]+)` over a longer string: Happy's server reads it out of the
//! minted JWT's `video.room` claim (`voiceRoutes.ts`), and the ElevenLabs client SDK
//! reads it out of the name of the room it joined. Neither one raises an error when
//! the pattern does not match — Happy returns a 500 with "Failed to get conversation
//! ID" only if the match is empty, and a *partial* match raises nothing at all.
//!
//! That makes the naming rule the rare invariant whose violation is invisible, so it
//! is worth spending a type on. Every value of [`ConversationId`] satisfies the regex
//! by construction, and the room name is not merely derived from the ID — it *is* the
//! ID, so the two cannot drift.

use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;

/// The alphabet the random suffix is drawn from.
///
/// Strictly `[a-zA-Z0-9]`, matching the consumers' character class exactly — and that
/// is load-bearing rather than cosmetic. A suffix drawn from a wider alphabet does not
/// fail the regex, it *truncates* it: the base64url-shaped `conv_ab-cd` yields
/// `conv_ab` on both sides, a perfectly well-formed ID that names a room nobody
/// created. Usage tracking would then attribute every call to a room that never
/// existed, with no error raised anywhere along the way. Keeping generation inside
/// this module is what makes that outcome unrepresentable.
const SUFFIX_ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";

/// Length of the random suffix. 22 alphanumeric characters carry ~131 bits, so
/// collisions stay negligible without a coordinating counter.
const SUFFIX_LEN: usize = 22;

/// The prefix both consumers anchor their regex on.
const PREFIX: &str = "conv_";

/// A conversation identifier, guaranteed to match `conv_[a-zA-Z0-9]+`.
///
/// This doubles as the LiveKit room name. See the module docs for why the two are one
/// value rather than two.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct ConversationId(String);

impl ConversationId {
    /// Mints a fresh identifier. The only way to create one from nothing.
    pub fn generate() -> Self {
        use rand::Rng;

        let mut rng = rand::rng();
        let suffix: String = (0..SUFFIX_LEN)
            .map(|_| SUFFIX_ALPHABET[rng.random_range(0..SUFFIX_ALPHABET.len())] as char)
            .collect();
        Self(format!("{PREFIX}{suffix}"))
    }

    /// The checkpoint for identifiers arriving from outside — a LiveKit room name, a
    /// query parameter, a line read back out of the conversation log.
    ///
    /// Deliberately stricter than the consumers' regex: they search for the pattern
    /// anywhere inside a longer string, whereas this demands the whole input be the
    /// identifier. As the producer of these names we know the difference between
    /// "contains an ID" and "is an ID", and accepting the looser form here would let a
    /// room name and the ID recovered from it disagree.
    pub fn parse(candidate: &str) -> Result<Self, MalformedConversationId> {
        let suffix = candidate
            .strip_prefix(PREFIX)
            .ok_or_else(|| MalformedConversationId(candidate.to_owned()))?;

        let well_formed =
            !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_alphanumeric());

        well_formed
            .then(|| Self(candidate.to_owned()))
            .ok_or_else(|| MalformedConversationId(candidate.to_owned()))
    }

    /// The identifier as it appears on the wire and as the LiveKit room name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ConversationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Deserialization is a boundary, so it routes through [`ConversationId::parse`]
/// rather than accepting any string. Without this, reading the conversation log back
/// would reintroduce exactly the unchecked values the type exists to exclude.
impl<'de> Deserialize<'de> for ConversationId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}

/// A string that does not match `conv_[a-zA-Z0-9]+`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MalformedConversationId(pub String);

impl fmt::Display for MalformedConversationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:?} is not a conversation id: expected {PREFIX} followed by one or more \
             ASCII alphanumeric characters",
            self.0
        )
    }
}

impl std::error::Error for MalformedConversationId {}

#[cfg(test)]
mod tests {
    use super::*;

    /// The property both consumers actually depend on: running their regex over the
    /// room name recovers the whole identifier, not a prefix of it.
    ///
    /// Implemented as a scan rather than with the `regex` crate so the test states the
    /// consumers' rule directly instead of trusting a second transcription of it.
    fn recover_like_a_consumer(haystack: &str) -> Option<&str> {
        let start = haystack.find(PREFIX)?;
        let suffix_start = start + PREFIX.len();
        let end = haystack[suffix_start..]
            .find(|c: char| !c.is_ascii_alphanumeric())
            .map_or(haystack.len(), |offset| suffix_start + offset);

        (end > suffix_start).then(|| &haystack[start..end])
    }

    #[test]
    fn generated_ids_survive_the_consumer_regex_intact() {
        for _ in 0..1_000 {
            let id = ConversationId::generate();
            assert_eq!(
                recover_like_a_consumer(id.as_str()),
                Some(id.as_str()),
                "consumer recovered a different value than the room name"
            );
        }
    }

    /// The specific near-miss this type exists to prevent: a suffix that keeps the
    /// regex matching while silently shortening what the consumer recovers.
    #[test]
    fn a_non_alphanumeric_suffix_would_truncate_rather_than_fail() {
        assert_eq!(recover_like_a_consumer("conv_ab-cd"), Some("conv_ab"));
        assert_eq!(ConversationId::parse("conv_ab-cd"), Err(MalformedConversationId("conv_ab-cd".to_owned())));
    }

    #[test]
    fn generated_ids_round_trip_through_parse() {
        let id = ConversationId::generate();
        assert_eq!(ConversationId::parse(id.as_str()).as_ref(), Ok(&id));
    }

    #[test]
    fn generation_does_not_repeat() {
        let ids: std::collections::HashSet<_> =
            (0..1_000).map(|_| ConversationId::generate()).collect();
        assert_eq!(ids.len(), 1_000);
    }

    #[test]
    fn rejects_strings_that_are_not_identifiers() {
        for candidate in ["", "conv_", "room_1730", "conv", "xconv_abc", "conv_ab_cd"] {
            assert!(
                ConversationId::parse(candidate).is_err(),
                "{candidate:?} was accepted as a conversation id"
            );
        }
    }

    #[test]
    fn deserialization_rejects_what_parse_rejects() {
        assert!(serde_json::from_str::<ConversationId>("\"room_1730\"").is_err());
        assert_eq!(
            serde_json::from_str::<ConversationId>("\"conv_abc123\"").unwrap(),
            ConversationId::parse("conv_abc123").unwrap()
        );
    }
}
