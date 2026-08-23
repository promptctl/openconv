//! Rust types for the ElevenLabs Conversational AI control protocol.
//!
//! # Where these types come from
//!
//! Every shape here is transcribed from `generated/types/asyncapi-types.ts` in the
//! published `@elevenlabs/types` npm package. That file is generated from ElevenLabs'
//! AsyncAPI spec and is the authoritative map of the wire; this crate is a derived
//! copy of it, and the fixture tests are what keep the copy honest.
//!
//! # How messages reach the client
//!
//! Under the WebRTC transport — the only transport Happy uses, since the SDK picks
//! `connectionType ?? (conversationToken ? "webrtc" : "websocket")` and Happy always
//! supplies a token — each message is a bare JSON object published on the LiveKit
//! data channel with `reliable: true`. There is no envelope, no topic, and no
//! batching: one `publishData` call carries one serialized [`ServerEvent`].
//!
//! # The tagging scheme, and why getting it wrong is invisible
//!
//! Every message is externally tagged: a `type` field naming the variant, plus **one
//! sibling field** whose name is specific to that variant — not `payload`, not `data`.
//! A VAD score is `{"type":"vad_score","vad_score_event":{"vad_score":0.83}}`. Serde's
//! internally-tagged struct variants produce exactly this shape, which is why both
//! event enums are `#[serde(tag = "type")]`.
//!
//! Both halves of that shape fail quietly if you get them wrong, which is why the
//! fixtures in `tests/` assert on serialized JSON rather than on round-trips alone:
//!
//! - A wrong `type` value reaches the client's `switch (parsedEvent.type)`, matches no
//!   case, and falls through to a debug callback nobody has registered. The session
//!   stays connected and does nothing.
//! - A wrong payload field name passes the client's `isValidSocketEvent` — which is
//!   only `!!event.type` — and dies inside the handler on an undefined access.
//!
//! # What is deliberately absent
//!
//! - **`UserAudio`** (`{"user_audio_chunk": ...}`) is the one message with no `type`
//!   field. Admitting it would force the enum to be untagged and cost every other
//!   variant its discriminator. It costs nothing to omit: the SDK's WebRTC transport
//!   drops it unsent (`if ("user_audio_chunk" in message) return`) because captured
//!   audio rides a LiveKit track instead.
//! - **The `Scribe*` messages** in the same TypeScript file are a different protocol —
//!   standalone speech-to-text over its own WebSocket, tagged `message_type` rather
//!   than `type`. openconv runs its own STT, so those shapes never cross this wire.

mod client;
mod server;

pub use client::{
    ClientEvent, ConversationConfigOverride, ConversationConfigOverrideAgent,
    ConversationConfigOverrideConversation, ConversationConfigOverrideTts,
    ConversationInitiationClientData, FeedbackScore, Language, PromptOverride, SourceInfo,
};
pub use server::{
    AgentResponseCorrectionEvent, AgentResponseEvent, AgentToolRequest, AgentToolResponse,
    AudioEvent, AudioFormat, ClientEventKind, ClientToolCall, ConversationInitiationMetadataEvent,
    ErrorCode, ErrorEvent, ErrorType, InterruptionEvent, McpConnectionStatus, McpIntegration,
    McpIntegrationType, McpToolCall, McpToolCallState, PingEvent, ServerEvent,
    TentativeAgentResponseInternalEvent, TentativeUserTranscriptionEvent, TextResponsePart,
    TextResponsePartKind, TurnProbabilityInternalEvent, UserTranscriptionEvent, VadScoreEvent,
};

use serde::{Deserialize, Serialize};

/// A monotonically increasing per-conversation event counter.
///
/// The client echoes it back on `pong` and attaches it to feedback, so it is one
/// value flowing in both directions rather than two unrelated numbers — the newtype
/// is what stops it being confused with the other bare numbers on the wire
/// (`ping_ms`, `vad_score`, `turn_probability`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EventId(pub u64);

impl From<u64> for EventId {
    fn from(raw: u64) -> Self {
        Self(raw)
    }
}

/// A free-form JSON object — the `Record<string, any>` of the TypeScript definitions.
///
/// Used where the protocol genuinely carries opaque data whose shape is set by
/// something other than this spec: tool parameters, dynamic variables, error details.
pub type JsonObject = serde_json::Map<String, serde_json::Value>;
