//! Fixture tests pinning every message to its published JSON shape.
//!
//! Each fixture is transcribed by hand from `generated/types/asyncapi-types.ts` in
//! `@elevenlabs/types` and compared against what serde produces. Round-tripping alone
//! would not catch anything here — a message with every field renamed round-trips
//! perfectly and is still ignored by the client — so the assertion that matters is
//! the one against the literal JSON.

use openconv_protocol::*;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

/// Checks a value against its published shape in both directions: that serializing
/// produces exactly the fixture, and that the fixture parses back to the same value.
///
/// Comparison is on parsed JSON rather than text, so key order — which carries no
/// meaning on the wire — is free to differ while every field name, nesting level, and
/// omitted-versus-null decision is held exactly.
#[track_caller]
fn matches_published_shape<T>(value: T, fixture: Value)
where
    T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug,
{
    assert_eq!(
        serde_json::to_value(&value).expect("serializes"),
        fixture,
        "serialized shape differs from the published one"
    );
    assert_eq!(
        serde_json::from_value::<T>(fixture).expect("parses"),
        value,
        "the published shape did not parse back to the same value"
    );
}

/// One value of every agent-to-client message, paired with its published shape.
///
/// Shared by the shape test and the subscribable-kinds test so both read the same
/// list rather than each keeping its own idea of what the variants are.
fn server_fixtures() -> Vec<(ServerEvent, Value)> {
    vec![
        (
            ServerEvent::Audio {
                audio_event: AudioEvent {
                    audio_base_64: "UklGRg==".to_owned(),
                    event_id: EventId(1),
                },
            },
            json!({"type": "audio", "audio_event": {"audio_base_64": "UklGRg==", "event_id": 1}}),
        ),
        (
            ServerEvent::UserTranscript {
                user_transcription_event: UserTranscriptionEvent {
                    user_transcript: "run the tests".to_owned(),
                    event_id: EventId(2),
                },
            },
            json!({
                "type": "user_transcript",
                "user_transcription_event": {"user_transcript": "run the tests", "event_id": 2}
            }),
        ),
        (
            ServerEvent::TentativeUserTranscript {
                tentative_user_transcription_event: TentativeUserTranscriptionEvent {
                    user_transcript: "run the".to_owned(),
                    event_id: EventId(3),
                },
            },
            json!({
                "type": "tentative_user_transcript",
                "tentative_user_transcription_event": {"user_transcript": "run the", "event_id": 3}
            }),
        ),
        (
            ServerEvent::AgentResponse {
                agent_response_event: AgentResponseEvent {
                    agent_response: "Running them now.".to_owned(),
                    event_id: EventId(4),
                },
            },
            json!({
                "type": "agent_response",
                "agent_response_event": {"agent_response": "Running them now.", "event_id": 4}
            }),
        ),
        (
            ServerEvent::AgentResponseCorrection {
                agent_response_correction_event: AgentResponseCorrectionEvent {
                    original_agent_response: "Running them now.".to_owned(),
                    corrected_agent_response: "Running them.".to_owned(),
                    event_id: EventId(5),
                },
            },
            json!({
                "type": "agent_response_correction",
                "agent_response_correction_event": {
                    "original_agent_response": "Running them now.",
                    "corrected_agent_response": "Running them.",
                    "event_id": 5
                }
            }),
        ),
        (
            ServerEvent::AgentChatResponsePart {
                text_response_part: TextResponsePart {
                    text: "Run".to_owned(),
                    kind: TextResponsePartKind::Delta,
                },
            },
            json!({
                "type": "agent_chat_response_part",
                "text_response_part": {"text": "Run", "type": "delta"}
            }),
        ),
        (
            ServerEvent::Interruption {
                interruption_event: InterruptionEvent {
                    event_id: EventId(6),
                },
            },
            json!({"type": "interruption", "interruption_event": {"event_id": 6}}),
        ),
        (
            ServerEvent::ConversationMetadata {
                conversation_initiation_metadata_event: ConversationInitiationMetadataEvent {
                    conversation_id: "conv_abc123".to_owned(),
                    agent_output_audio_format: AudioFormat::Pcm24000,
                    user_input_audio_format: AudioFormat::Pcm16000,
                },
            },
            json!({
                "type": "conversation_initiation_metadata",
                "conversation_initiation_metadata_event": {
                    "conversation_id": "conv_abc123",
                    "agent_output_audio_format": "pcm_24000",
                    "user_input_audio_format": "pcm_16000"
                }
            }),
        ),
        (
            ServerEvent::ClientToolCall {
                client_tool_call: ClientToolCall {
                    tool_name: "sendMessageToSession".to_owned(),
                    tool_call_id: "call_1".to_owned(),
                    parameters: json!({"message": "run the tests"})
                        .as_object()
                        .expect("object")
                        .clone(),
                    event_id: EventId(7),
                },
            },
            json!({
                "type": "client_tool_call",
                "client_tool_call": {
                    "tool_name": "sendMessageToSession",
                    "tool_call_id": "call_1",
                    "parameters": {"message": "run the tests"},
                    "event_id": 7
                }
            }),
        ),
        (
            ServerEvent::AgentToolRequest {
                agent_tool_request: AgentToolRequest {
                    tool_name: "lookup".to_owned(),
                    tool_call_id: "call_2".to_owned(),
                    tool_type: "webhook".to_owned(),
                    event_id: EventId(8),
                },
            },
            json!({
                "type": "agent_tool_request",
                "agent_tool_request": {
                    "tool_name": "lookup",
                    "tool_call_id": "call_2",
                    "tool_type": "webhook",
                    "event_id": 8
                }
            }),
        ),
        (
            ServerEvent::AgentToolResponse {
                agent_tool_response: AgentToolResponse {
                    tool_name: "lookup".to_owned(),
                    tool_call_id: "call_2".to_owned(),
                    tool_type: "webhook".to_owned(),
                    is_error: false,
                    is_called: true,
                    event_id: EventId(9),
                },
            },
            json!({
                "type": "agent_tool_response",
                "agent_tool_response": {
                    "tool_name": "lookup",
                    "tool_call_id": "call_2",
                    "tool_type": "webhook",
                    "is_error": false,
                    "is_called": true,
                    "event_id": 9
                }
            }),
        ),
        (
            ServerEvent::McpToolCall {
                mcp_tool_call: McpToolCall {
                    service_id: "svc_1".to_owned(),
                    tool_call_id: "call_3".to_owned(),
                    tool_name: "search".to_owned(),
                    tool_description: Some("Search the docs".to_owned()),
                    parameters: json!({"q": "livekit"}).as_object().expect("object").clone(),
                    timestamp: "2026-08-23T06:00:00Z".to_owned(),
                    state: McpToolCallState::AwaitingApproval {
                        approval_timeout_secs: 30,
                    },
                },
            },
            json!({
                "type": "mcp_tool_call",
                "mcp_tool_call": {
                    "service_id": "svc_1",
                    "tool_call_id": "call_3",
                    "tool_name": "search",
                    "tool_description": "Search the docs",
                    "parameters": {"q": "livekit"},
                    "timestamp": "2026-08-23T06:00:00Z",
                    "state": "awaiting_approval",
                    "approval_timeout_secs": 30
                }
            }),
        ),
        (
            ServerEvent::McpConnectionStatus {
                mcp_connection_status: McpConnectionStatus {
                    integrations: vec![McpIntegration {
                        integration_id: "int_1".to_owned(),
                        integration_type: McpIntegrationType::McpServer,
                        is_connected: true,
                        tool_count: 4,
                    }],
                },
            },
            json!({
                "type": "mcp_connection_status",
                "mcp_connection_status": {
                    "integrations": [{
                        "integration_id": "int_1",
                        "integration_type": "mcp_server",
                        "is_connected": true,
                        "tool_count": 4
                    }]
                }
            }),
        ),
        (
            ServerEvent::VadScore {
                vad_score_event: VadScoreEvent { vad_score: 0.83 },
            },
            json!({"type": "vad_score", "vad_score_event": {"vad_score": 0.83}}),
        ),
        (
            ServerEvent::Ping {
                ping_event: PingEvent {
                    event_id: EventId(10),
                    ping_ms: Some(12.5),
                },
            },
            json!({"type": "ping", "ping_event": {"event_id": 10, "ping_ms": 12.5}}),
        ),
        (
            ServerEvent::AsrInitiationMetadata {
                asr_initiation_metadata_event: json!({"model": "whisper"})
                    .as_object()
                    .expect("object")
                    .clone(),
            },
            json!({
                "type": "asr_initiation_metadata",
                "asr_initiation_metadata_event": {"model": "whisper"}
            }),
        ),
        (
            ServerEvent::InternalTurnProbability {
                turn_probability_internal_event: TurnProbabilityInternalEvent {
                    turn_probability: 0.42,
                },
            },
            json!({
                "type": "internal_turn_probability",
                "turn_probability_internal_event": {"turn_probability": 0.42}
            }),
        ),
        (
            ServerEvent::InternalTentativeAgentResponse {
                tentative_agent_response_internal_event: TentativeAgentResponseInternalEvent {
                    tentative_agent_response: "Running".to_owned(),
                },
            },
            json!({
                "type": "internal_tentative_agent_response",
                "tentative_agent_response_internal_event": {"tentative_agent_response": "Running"}
            }),
        ),
        (
            ServerEvent::Error {
                error_event: ErrorEvent {
                    code: ErrorCode::InternalError,
                    message: Some("llm timed out".to_owned()),
                    error_type: Some(ErrorType::LlmTimeout),
                    reason: None,
                    debug_message: None,
                    details: None,
                },
            },
            json!({
                "type": "error",
                "error_event": {
                    "code": 1011,
                    "message": "llm timed out",
                    "error_type": "llm_timeout"
                }
            }),
        ),
    ]
}

/// One value of every client-to-agent message, paired with its published shape.
fn client_fixtures() -> Vec<(ClientEvent, Value)> {
    vec![
        (
            ClientEvent::Pong {
                event_id: EventId(10),
            },
            json!({"type": "pong", "event_id": 10}),
        ),
        (
            ClientEvent::UserMessage {
                text: Some("run the tests".to_owned()),
            },
            json!({"type": "user_message", "text": "run the tests"}),
        ),
        (ClientEvent::UserActivity, json!({"type": "user_activity"})),
        (
            ClientEvent::Feedback {
                event_id: EventId(4),
                score: FeedbackScore::Like,
            },
            json!({"type": "feedback", "event_id": 4, "score": "like"}),
        ),
        (
            ClientEvent::ClientToolResult {
                tool_call_id: "call_1".to_owned(),
                result: "delivered".to_owned(),
                is_error: false,
            },
            json!({
                "type": "client_tool_result",
                "tool_call_id": "call_1",
                "result": "delivered",
                "is_error": false
            }),
        ),
        (
            ClientEvent::McpToolApprovalResult {
                tool_call_id: "call_3".to_owned(),
                is_approved: true,
            },
            json!({
                "type": "mcp_tool_approval_result",
                "tool_call_id": "call_3",
                "is_approved": true
            }),
        ),
        (
            ClientEvent::ContextualUpdate {
                text: "the agent finished editing main.rs".to_owned(),
            },
            json!({
                "type": "contextual_update",
                "text": "the agent finished editing main.rs"
            }),
        ),
        (
            // The exact shape Happy's RealtimeVoiceSession produces once the SDK's
            // constructOverrides has camel-to-snake mapped it: a prompt override, a
            // first message, a language, and two dynamic variables.
            ClientEvent::ConversationInitiation(Box::new(ConversationInitiationClientData {
                conversation_config_override: Some(ConversationConfigOverride {
                    agent: Some(ConversationConfigOverrideAgent {
                        first_message: Some("What are we working on?".to_owned()),
                        language: Some(Language::En),
                        prompt: Some(PromptOverride {
                            prompt: Some("You are a voice bridge.".to_owned()),
                        }),
                        native_mcp_server_ids: None,
                    }),
                    tts: Some(ConversationConfigOverrideTts::default()),
                    conversation: Some(ConversationConfigOverrideConversation::default()),
                }),
                custom_llm_extra_body: None,
                dynamic_variables: Some(
                    json!({"sessionId": "sess_1", "initialConversationContext": ""})
                        .as_object()
                        .expect("object")
                        .clone(),
                ),
                user_id: Some("user_1".to_owned()),
                source_info: None,
            })),
            json!({
                "type": "conversation_initiation_client_data",
                "conversation_config_override": {
                    "agent": {
                        "first_message": "What are we working on?",
                        "language": "en",
                        "prompt": {"prompt": "You are a voice bridge."}
                    },
                    "tts": {},
                    "conversation": {}
                },
                "dynamic_variables": {"sessionId": "sess_1", "initialConversationContext": ""},
                "user_id": "user_1"
            }),
        ),
    ]
}

#[test]
fn server_events_match_published_shapes() {
    for (event, fixture) in server_fixtures() {
        matches_published_shape(event, fixture);
    }
}

#[test]
fn client_events_match_published_shapes() {
    for (event, fixture) in client_fixtures() {
        matches_published_shape(event, fixture);
    }
}

/// The exact message the browser client publishes to carry a voice, read as the agent
/// reads it.
///
/// Pinned here rather than trusted, because `web/caller.js` sends `voice_id` explicitly
/// and sends it as `null` whenever the caller picked no particular voice — and nothing on
/// that page can tell an explicit null being read as "none" from an explicit null being
/// read as some other thing. What the page depends on is that the two spellings of "no
/// voice named", omitted and null, settle to the same [`Voicing`] the agent had before
/// any configuration arrived.
///
/// The other half of the dependency is asserted alongside: an initiation message
/// carrying nothing but a voice leaves the prompt, the first message and the language
/// exactly as an unconfigured conversation has them, which is what makes this page's
/// message additive rather than a conversation it quietly reshaped.
///
/// [`Voicing`]: openconv-agent's `speak::Voicing`
#[test]
fn a_voice_the_browser_client_did_not_name_is_no_voice_rather_than_an_empty_one() {
    let named: ClientEvent = serde_json::from_value(json!({
        "type": "conversation_initiation_client_data",
        "conversation_config_override": {"tts": {"voice_id": "af_heart"}}
    }))
    .expect("the page's message with a voice picked");

    let unnamed: ClientEvent = serde_json::from_value(json!({
        "type": "conversation_initiation_client_data",
        "conversation_config_override": {"tts": {"voice_id": null}}
    }))
    .expect("the page's message with no voice picked");

    let omitted: ClientEvent = serde_json::from_value(json!({
        "type": "conversation_initiation_client_data",
        "conversation_config_override": {"tts": {}}
    }))
    .expect("a message that never mentions a voice");

    let tts = |event: &ClientEvent| match event {
        ClientEvent::ConversationInitiation(data) => data
            .conversation_config_override
            .clone()
            .expect("an override")
            .tts
            .expect("a tts override"),
        other => panic!("not an initiation message: {other:?}"),
    };

    assert_eq!(tts(&named).voice_id.as_deref(), Some("af_heart"));
    assert_eq!(tts(&unnamed).voice_id, None);
    assert_eq!(tts(&unnamed), tts(&omitted));

    // Everything the page does not send stays unsaid, so nothing but the voice is
    // decided by this message.
    let ClientEvent::ConversationInitiation(data) = &named else {
        panic!("not an initiation message");
    };
    let overrides = data.conversation_config_override.clone().expect("an override");
    assert_eq!(overrides.agent, None);
    assert_eq!(overrides.conversation, None);
    assert_eq!(data.dynamic_variables, None);
}

/// Guards the two enumerations of the same vocabulary against drifting apart: a
/// subscribable kind that names no real message would let a client silently subscribe
/// to nothing.
#[test]
fn subscribable_kinds_name_real_events() {
    let published_types: Vec<String> = server_fixtures()
        .iter()
        .map(|(_, fixture)| fixture["type"].as_str().expect("tagged").to_owned())
        .collect();

    let subscribable = [
        ClientEventKind::Audio,
        ClientEventKind::AgentResponse,
        ClientEventKind::AgentResponseCorrection,
        ClientEventKind::AgentChatResponsePart,
        ClientEventKind::Interruption,
        ClientEventKind::UserTranscript,
        ClientEventKind::TentativeUserTranscript,
        ClientEventKind::ConversationInitiationMetadata,
        ClientEventKind::ClientToolCall,
        ClientEventKind::AgentToolRequest,
        ClientEventKind::AgentToolResponse,
        ClientEventKind::McpToolCall,
        ClientEventKind::McpConnectionStatus,
        ClientEventKind::VadScore,
        ClientEventKind::Ping,
        ClientEventKind::AsrInitiationMetadata,
        ClientEventKind::InternalTurnProbability,
        ClientEventKind::InternalTentativeAgentResponse,
    ];

    for kind in subscribable {
        let name = serde_json::to_value(kind).expect("serializes");
        let name = name.as_str().expect("string").to_owned();
        assert!(
            published_types.contains(&name),
            "subscribable kind {name:?} names no ServerEvent variant"
        );
    }
}

/// Every `state` of an MCP tool call keeps only the fields that state owns, flattened
/// alongside the shared ones rather than nested under a wrapper.
#[test]
fn mcp_tool_call_states_flatten_beside_the_shared_fields() {
    let states = [
        (McpToolCallState::Loading, json!({"state": "loading"})),
        (
            McpToolCallState::Success {
                result: vec![json!({"text": "ok"}).as_object().expect("object").clone()],
            },
            json!({"state": "success", "result": [{"text": "ok"}]}),
        ),
        (
            McpToolCallState::Failure {
                error_message: "upstream refused".to_owned(),
            },
            json!({"state": "failure", "error_message": "upstream refused"}),
        ),
    ];

    for (state, state_fields) in states {
        let call = McpToolCall {
            service_id: "svc_1".to_owned(),
            tool_call_id: "call_3".to_owned(),
            tool_name: "search".to_owned(),
            tool_description: None,
            parameters: serde_json::Map::new(),
            timestamp: "2026-08-23T06:00:00Z".to_owned(),
            state,
        };

        let mut fixture = json!({
            "service_id": "svc_1",
            "tool_call_id": "call_3",
            "tool_name": "search",
            "parameters": {},
            "timestamp": "2026-08-23T06:00:00Z"
        });
        for (key, value) in state_fields.as_object().expect("object") {
            fixture[key] = value.clone();
        }

        matches_published_shape(call, fixture);
    }
}

/// Absent optional fields are omitted, never sent as `null`. The client reads
/// `ping_event.ping_ms` to decide whether to warn about latency, and an explicit null
/// is a different fact from an absent field.
#[test]
fn absent_optional_fields_are_omitted_not_nulled() {
    matches_published_shape(
        ServerEvent::Ping {
            ping_event: PingEvent {
                event_id: EventId(11),
                ping_ms: None,
            },
        },
        json!({"type": "ping", "ping_event": {"event_id": 11}}),
    );

    matches_published_shape(
        ClientEvent::UserMessage { text: None },
        json!({"type": "user_message"}),
    );
}

/// Protocol drift is reported, not absorbed. A message this crate cannot name must
/// fail to parse so the caller can log it, rather than decoding into a catch-all that
/// looks like a handled message.
#[test]
fn unnameable_messages_fail_to_parse() {
    let drifted = json!({"type": "some_future_event", "some_future_payload": {}});
    let parsed = serde_json::from_value::<ClientEvent>(drifted);
    assert!(parsed.is_err(), "unknown message type parsed successfully");
}

/// Spells out every value of the closed vocabularies the message fixtures only
/// sample. `AudioFormat` is here because deriving its names produced `pcm24000` for
/// `Pcm24000` — `rename_all` finds word boundaries at case changes and digits have
/// none — and the rest are here because a hand-transcribed list of twenty-six names
/// is exactly where a typo hides.
#[test]
fn closed_vocabularies_match_their_published_names() {
    let formats = [
        (AudioFormat::Pcm8000, "pcm_8000"),
        (AudioFormat::Pcm16000, "pcm_16000"),
        (AudioFormat::Pcm22050, "pcm_22050"),
        (AudioFormat::Pcm24000, "pcm_24000"),
        (AudioFormat::Pcm44100, "pcm_44100"),
        (AudioFormat::Pcm48000, "pcm_48000"),
        (AudioFormat::Ulaw8000, "ulaw_8000"),
    ];
    for (format, name) in formats {
        matches_published_shape(format, json!(name));
    }

    let parts = [
        (TextResponsePartKind::Start, "start"),
        (TextResponsePartKind::Delta, "delta"),
        (TextResponsePartKind::Stop, "stop"),
    ];
    for (part, name) in parts {
        matches_published_shape(part, json!(name));
    }

    let integrations = [
        (McpIntegrationType::McpServer, "mcp_server"),
        (McpIntegrationType::McpIntegration, "mcp_integration"),
    ];
    for (integration, name) in integrations {
        matches_published_shape(integration, json!(name));
    }

    let scores = [
        (FeedbackScore::Like, "like"),
        (FeedbackScore::Dislike, "dislike"),
    ];
    for (score, name) in scores {
        matches_published_shape(score, json!(name));
    }

    let error_types = [
        (ErrorType::Unknown, "unknown"),
        (ErrorType::InvalidMessage, "invalid_message"),
        (ErrorType::TelephonyAgentError, "telephony_agent_error"),
        (ErrorType::McpToolError, "mcp_tool_error"),
        (ErrorType::McpHttpsError, "mcp_https_error"),
        (ErrorType::ValueError, "value_error"),
        (ErrorType::MissingFields, "missing_fields"),
        (ErrorType::OverrideError, "override_error"),
        (
            ErrorType::MissingDynamicVariableTransfer,
            "missing_dynamic_variable_transfer",
        ),
        (
            ErrorType::MissingDynamicVariable,
            "missing_dynamic_variable",
        ),
        (ErrorType::WebsocketDisconnect, "websocket_disconnect"),
        (ErrorType::SafetyViolation, "safety_violation"),
        (ErrorType::LlmTimeout, "llm_timeout"),
        (
            ErrorType::TransportReceiveTimeout,
            "transport_receive_timeout",
        ),
        (ErrorType::AsyncioTimeout, "asyncio_timeout"),
        (ErrorType::HttpException, "http_exception"),
        (ErrorType::MaxDurationExceeded, "max_duration_exceeded"),
        (ErrorType::LlmError, "llm_error"),
        (ErrorType::CustomLlmError, "custom_llm_error"),
        (ErrorType::CascadeBrainError, "cascade_brain_error"),
        (ErrorType::AsrTranscriptionError, "asr_transcription_error"),
        (ErrorType::VadError, "vad_error"),
        (ErrorType::TurnProbabilityError, "turn_probability_error"),
        (ErrorType::TtsCascadeError, "tts_cascade_error"),
        (ErrorType::RedisTimeoutError, "redis_timeout_error"),
        (ErrorType::UnknownWebsocketCrash, "unknown_websocket_crash"),
    ];
    for (error_type, name) in error_types {
        matches_published_shape(error_type, json!(name));
    }

    let languages = [
        (Language::En, "en"),
        (Language::Ja, "ja"),
        (Language::Zh, "zh"),
        (Language::De, "de"),
        (Language::Hi, "hi"),
        (Language::Fr, "fr"),
        (Language::Ko, "ko"),
        (Language::Pt, "pt"),
        (Language::PtBr, "pt-br"),
        (Language::It, "it"),
        (Language::Es, "es"),
        (Language::Id, "id"),
        (Language::Nl, "nl"),
        (Language::Tr, "tr"),
        (Language::Pl, "pl"),
        (Language::Sv, "sv"),
        (Language::Bg, "bg"),
        (Language::Ro, "ro"),
        (Language::Ar, "ar"),
        (Language::Cs, "cs"),
        (Language::El, "el"),
        (Language::Fi, "fi"),
        (Language::Ms, "ms"),
        (Language::Da, "da"),
        (Language::Ta, "ta"),
        (Language::Uk, "uk"),
        (Language::Ru, "ru"),
        (Language::Hu, "hu"),
        (Language::Hr, "hr"),
        (Language::Sk, "sk"),
        (Language::No, "no"),
        (Language::Vi, "vi"),
        (Language::Tl, "tl"),
    ];
    for (language, name) in languages {
        matches_published_shape(language, json!(name));

        // The same code, read back the way anything that has to *print* a language gets
        // it. Checked here rather than against its own list, so the accessor cannot claim
        // a code the wire does not carry — `pt-br` being the row where a hand-written
        // table would say `ptbr` and nobody would notice until a Brazilian caller did.
        assert_eq!(language.code(), name);
    }

    // The list above transcribes the published union; `Language::ALL` is what the page is
    // offered. Neither can enumerate the enum on its own, so they are held to each other:
    // a language reaching one and not the other fails here instead of becoming an option
    // nobody can pick, or a code nothing ever checked.
    let transcribed: Vec<Language> = languages.iter().map(|(language, _)| *language).collect();
    assert_eq!(transcribed, Language::ALL, "the offered languages and the checked ones differ");
}

/// The four admitted error codes travel as numbers; anything else is rejected rather
/// than passed through as an opaque integer.
#[test]
fn error_codes_travel_as_their_published_numbers() {
    let pairs = [
        (ErrorCode::NormalClosure, 1000),
        (ErrorCode::ProtocolError, 1002),
        (ErrorCode::PolicyViolation, 1008),
        (ErrorCode::InternalError, 1011),
    ];
    for (code, number) in pairs {
        matches_published_shape(code, json!(number));
    }

    assert!(serde_json::from_value::<ErrorCode>(json!(1006)).is_err());
}
