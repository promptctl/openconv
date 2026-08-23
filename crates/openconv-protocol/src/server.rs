//! Messages the agent sends to the client.

use serde::{Deserialize, Serialize};

use crate::{EventId, JsonObject};

/// A message published by the agent onto the LiveKit data channel.
///
/// Internally tagged, so each variant serializes as its `type` plus the one
/// per-variant payload field the client's handler reads by name.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerEvent {
    /// Synthesized speech. Present for protocol completeness only: under WebRTC the
    /// client drops this message on arrival (`if (message.type === "audio") return`)
    /// because agent audio arrives on a published track instead.
    Audio {
        audio_event: AudioEvent,
    },
    UserTranscript {
        user_transcription_event: UserTranscriptionEvent,
    },
    TentativeUserTranscript {
        tentative_user_transcription_event: TentativeUserTranscriptionEvent,
    },
    AgentResponse {
        agent_response_event: AgentResponseEvent,
    },
    AgentResponseCorrection {
        agent_response_correction_event: AgentResponseCorrectionEvent,
    },
    AgentChatResponsePart {
        text_response_part: TextResponsePart,
    },
    Interruption {
        interruption_event: InterruptionEvent,
    },
    #[serde(rename = "conversation_initiation_metadata")]
    ConversationMetadata {
        conversation_initiation_metadata_event: ConversationInitiationMetadataEvent,
    },
    ClientToolCall {
        client_tool_call: ClientToolCall,
    },
    AgentToolRequest {
        agent_tool_request: AgentToolRequest,
    },
    AgentToolResponse {
        agent_tool_response: AgentToolResponse,
    },
    McpToolCall {
        mcp_tool_call: McpToolCall,
    },
    McpConnectionStatus {
        mcp_connection_status: McpConnectionStatus,
    },
    VadScore {
        vad_score_event: VadScoreEvent,
    },
    Ping {
        ping_event: PingEvent,
    },
    /// The payload is spec'd as a free-form object, so it stays one.
    AsrInitiationMetadata {
        asr_initiation_metadata_event: JsonObject,
    },
    InternalTurnProbability {
        turn_probability_internal_event: TurnProbabilityInternalEvent,
    },
    InternalTentativeAgentResponse {
        tentative_agent_response_internal_event: TentativeAgentResponseInternalEvent,
    },
    Error {
        error_event: ErrorEvent,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AudioEvent {
    pub audio_base_64: String,
    pub event_id: EventId,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UserTranscriptionEvent {
    pub user_transcript: String,
    pub event_id: EventId,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TentativeUserTranscriptionEvent {
    pub user_transcript: String,
    pub event_id: EventId,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentResponseEvent {
    pub agent_response: String,
    pub event_id: EventId,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentResponseCorrectionEvent {
    pub original_agent_response: String,
    pub corrected_agent_response: String,
    pub event_id: EventId,
}

/// One frame of a streamed agent reply. Note the inner `type` field: this payload
/// carries a discriminator of its own, unrelated to the message's outer `type`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TextResponsePart {
    pub text: String,
    #[serde(rename = "type")]
    pub kind: TextResponsePartKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextResponsePartKind {
    Start,
    Delta,
    Stop,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InterruptionEvent {
    pub event_id: EventId,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ConversationInitiationMetadataEvent {
    pub conversation_id: String,
    pub agent_output_audio_format: AudioFormat,
    pub user_input_audio_format: AudioFormat,
}

/// The generator emitted a separate type alias per field, but both admit exactly the
/// same seven formats, so one type serves both.
///
/// Each name is renamed explicitly rather than left to `rename_all`, which derives
/// word boundaries from case changes and so would emit `pcm24000` for `Pcm24000`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AudioFormat {
    #[serde(rename = "pcm_8000")]
    Pcm8000,
    #[serde(rename = "pcm_16000")]
    Pcm16000,
    #[serde(rename = "pcm_22050")]
    Pcm22050,
    #[serde(rename = "pcm_24000")]
    Pcm24000,
    #[serde(rename = "pcm_44100")]
    Pcm44100,
    #[serde(rename = "pcm_48000")]
    Pcm48000,
    #[serde(rename = "ulaw_8000")]
    Ulaw8000,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ClientToolCall {
    pub tool_name: String,
    pub tool_call_id: String,
    pub parameters: JsonObject,
    pub event_id: EventId,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentToolRequest {
    pub tool_name: String,
    pub tool_call_id: String,
    pub tool_type: String,
    pub event_id: EventId,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentToolResponse {
    pub tool_name: String,
    pub tool_call_id: String,
    pub tool_type: String,
    pub is_error: bool,
    pub is_called: bool,
    pub event_id: EventId,
}

/// The TypeScript spells this as a four-way union of near-identical objects. The
/// common fields are shared here and the four `state` arms keep only what is genuinely
/// theirs, so a `success` call cannot carry an `error_message` and an
/// `awaiting_approval` call cannot omit its timeout.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct McpToolCall {
    pub service_id: String,
    pub tool_call_id: String,
    pub tool_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_description: Option<String>,
    pub parameters: JsonObject,
    pub timestamp: String,
    #[serde(flatten)]
    pub state: McpToolCallState,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum McpToolCallState {
    Loading,
    AwaitingApproval { approval_timeout_secs: u64 },
    Success { result: Vec<JsonObject> },
    Failure { error_message: String },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct McpConnectionStatus {
    pub integrations: Vec<McpIntegration>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct McpIntegration {
    pub integration_id: String,
    pub integration_type: McpIntegrationType,
    pub is_connected: bool,
    pub tool_count: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpIntegrationType {
    McpServer,
    McpIntegration,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VadScoreEvent {
    pub vad_score: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PingEvent {
    pub event_id: EventId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ping_ms: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TurnProbabilityInternalEvent {
    pub turn_probability: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TentativeAgentResponseInternalEvent {
    pub tentative_agent_response: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ErrorEvent {
    pub code: ErrorCode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_type: Option<ErrorType>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub debug_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<JsonObject>,
}

/// The four WebSocket close codes the spec admits, carried as numbers on the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "u16", into = "u16")]
pub enum ErrorCode {
    NormalClosure,
    ProtocolError,
    PolicyViolation,
    InternalError,
}

impl From<ErrorCode> for u16 {
    fn from(code: ErrorCode) -> Self {
        match code {
            ErrorCode::NormalClosure => 1000,
            ErrorCode::ProtocolError => 1002,
            ErrorCode::PolicyViolation => 1008,
            ErrorCode::InternalError => 1011,
        }
    }
}

impl TryFrom<u16> for ErrorCode {
    type Error = UnknownErrorCode;

    fn try_from(raw: u16) -> Result<Self, Self::Error> {
        match raw {
            1000 => Ok(Self::NormalClosure),
            1002 => Ok(Self::ProtocolError),
            1008 => Ok(Self::PolicyViolation),
            1011 => Ok(Self::InternalError),
            other => Err(UnknownErrorCode(other)),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnknownErrorCode(pub u16);

impl std::fmt::Display for UnknownErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unknown ConvAI error code {}", self.0)
    }
}

impl std::error::Error for UnknownErrorCode {}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorType {
    Unknown,
    InvalidMessage,
    TelephonyAgentError,
    McpToolError,
    McpHttpsError,
    ValueError,
    MissingFields,
    OverrideError,
    MissingDynamicVariableTransfer,
    MissingDynamicVariable,
    WebsocketDisconnect,
    SafetyViolation,
    LlmTimeout,
    TransportReceiveTimeout,
    AsyncioTimeout,
    HttpException,
    MaxDurationExceeded,
    LlmError,
    CustomLlmError,
    CascadeBrainError,
    AsrTranscriptionError,
    VadError,
    TurnProbabilityError,
    TtsCascadeError,
    RedisTimeoutError,
    UnknownWebsocketCrash,
}

/// The event kinds a client may subscribe to in `conversation_config_override`.
///
/// Deliberately not the same set as [`ServerEvent`]: `error` is absent because it is
/// never opt-in. Every other name here must name a real [`ServerEvent`] tag, which the
/// `subscribable_kinds_name_real_events` test checks rather than trusting.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientEventKind {
    Audio,
    AgentResponse,
    AgentResponseCorrection,
    AgentChatResponsePart,
    Interruption,
    UserTranscript,
    TentativeUserTranscript,
    ConversationInitiationMetadata,
    ClientToolCall,
    AgentToolRequest,
    AgentToolResponse,
    McpToolCall,
    McpConnectionStatus,
    VadScore,
    Ping,
    AsrInitiationMetadata,
    InternalTurnProbability,
    InternalTentativeAgentResponse,
}
