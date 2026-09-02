//! Prints one serialized value of every message, for cross-checking against the
//! published TypeScript definitions.
//!
//! The fixtures in `tests/fixtures.rs` are transcribed by hand, so something has to
//! check the transcription. That cannot be a test: the TypeScript lives in a
//! `node_modules` tree in another repository, which CI does not have. So this dumps
//! the shapes and leaves the comparison to whoever has the package checked out —
//! `cargo run --example dump_fixtures` beside `scripts/check-against-published-types.mjs`.

use openconv_protocol::*;
use serde_json::json;

fn main() {
    let messages: Vec<serde_json::Value> = fixtures()
        .into_iter()
        .map(|value| serde_json::to_value(value).expect("serializes"))
        .collect();
    println!(
        "{}",
        serde_json::to_string_pretty(&messages).expect("serializes")
    );
}

/// Every message, as the maximal value of its variant: every optional field present,
/// so the cross-check sees the full field set rather than whichever subset a
/// minimal example happened to include.
fn fixtures() -> Vec<serde_json::Value> {
    let object = |value: serde_json::Value| value.as_object().expect("object").clone();

    let server = vec![
        ServerEvent::Audio {
            audio_event: AudioEvent {
                audio_base_64: "UklGRg==".to_owned(),
                event_id: EventId(1),
            },
        },
        ServerEvent::UserTranscript {
            user_transcription_event: UserTranscriptionEvent {
                user_transcript: "x".to_owned(),
                event_id: EventId(1),
            },
        },
        ServerEvent::TentativeUserTranscript {
            tentative_user_transcription_event: TentativeUserTranscriptionEvent {
                user_transcript: "x".to_owned(),
                event_id: EventId(1),
            },
        },
        ServerEvent::AgentResponse {
            agent_response_event: AgentResponseEvent {
                agent_response: "x".to_owned(),
                event_id: EventId(1),
            },
        },
        ServerEvent::AgentResponseCorrection {
            agent_response_correction_event: AgentResponseCorrectionEvent {
                original_agent_response: "x".to_owned(),
                corrected_agent_response: "y".to_owned(),
                event_id: EventId(1),
            },
        },
        ServerEvent::AgentChatResponsePart {
            text_response_part: TextResponsePart {
                text: "x".to_owned(),
                kind: TextResponsePartKind::Delta,
            },
        },
        ServerEvent::Interruption {
            interruption_event: InterruptionEvent {
                event_id: EventId(1),
            },
        },
        ServerEvent::ConversationMetadata {
            conversation_initiation_metadata_event: ConversationInitiationMetadataEvent {
                conversation_id: "conv_1".to_owned(),
                agent_output_audio_format: AudioFormat::Pcm24000,
                user_input_audio_format: AudioFormat::Pcm16000,
            },
        },
        ServerEvent::ClientToolCall {
            client_tool_call: ClientToolCall {
                tool_name: "x".to_owned(),
                tool_call_id: "x".to_owned(),
                parameters: object(json!({})),
                event_id: EventId(1),
            },
        },
        ServerEvent::AgentToolRequest {
            agent_tool_request: AgentToolRequest {
                tool_name: "x".to_owned(),
                tool_call_id: "x".to_owned(),
                tool_type: "x".to_owned(),
                event_id: EventId(1),
            },
        },
        ServerEvent::AgentToolResponse {
            agent_tool_response: AgentToolResponse {
                tool_name: "x".to_owned(),
                tool_call_id: "x".to_owned(),
                tool_type: "x".to_owned(),
                is_error: false,
                is_called: true,
                event_id: EventId(1),
            },
        },
        ServerEvent::McpToolCall {
            mcp_tool_call: McpToolCall {
                service_id: "x".to_owned(),
                tool_call_id: "x".to_owned(),
                tool_name: "x".to_owned(),
                tool_description: Some("x".to_owned()),
                parameters: object(json!({})),
                timestamp: "x".to_owned(),
                state: McpToolCallState::AwaitingApproval {
                    approval_timeout_secs: 1,
                },
            },
        },
        ServerEvent::McpConnectionStatus {
            mcp_connection_status: McpConnectionStatus {
                integrations: vec![McpIntegration {
                    integration_id: "x".to_owned(),
                    integration_type: McpIntegrationType::McpServer,
                    is_connected: true,
                    tool_count: 1,
                }],
            },
        },
        ServerEvent::VadScore {
            vad_score_event: VadScoreEvent { vad_score: 0.5 },
        },
        ServerEvent::Ping {
            ping_event: PingEvent {
                event_id: EventId(1),
                ping_ms: Some(1.0),
            },
        },
        ServerEvent::AsrInitiationMetadata {
            asr_initiation_metadata_event: object(json!({})),
        },
        ServerEvent::InternalTurnProbability {
            turn_probability_internal_event: TurnProbabilityInternalEvent {
                turn_probability: 0.5,
            },
        },
        ServerEvent::InternalTentativeAgentResponse {
            tentative_agent_response_internal_event: TentativeAgentResponseInternalEvent {
                tentative_agent_response: "x".to_owned(),
            },
        },
        ServerEvent::Error {
            error_event: ErrorEvent {
                code: ErrorCode::InternalError,
                message: Some("x".to_owned()),
                error_type: Some(ErrorType::LlmTimeout),
                reason: Some("x".to_owned()),
                debug_message: Some("x".to_owned()),
                details: Some(object(json!({}))),
            },
        },
    ];

    let client = vec![
        ClientEvent::Pong {
            event_id: EventId(1),
        },
        ClientEvent::UserMessage {
            text: Some("x".to_owned()),
        },
        ClientEvent::UserActivity,
        ClientEvent::Feedback {
            event_id: EventId(1),
            score: FeedbackScore::Like,
        },
        ClientEvent::ClientToolResult {
            tool_call_id: "x".to_owned(),
            result: "x".to_owned(),
            is_error: false,
        },
        ClientEvent::McpToolApprovalResult {
            tool_call_id: "x".to_owned(),
            is_approved: true,
        },
        ClientEvent::ContextualUpdate {
            text: "x".to_owned(),
        },
        ClientEvent::ConversationInitiation(Box::new(ConversationInitiationClientData {
            conversation_config_override: Some(ConversationConfigOverride {
                agent: Some(ConversationConfigOverrideAgent {
                    first_message: Some("x".to_owned()),
                    language: Some(Language::En),
                    prompt: Some(PromptOverride {
                        prompt: Some("x".to_owned()),
                    }),
                    native_mcp_server_ids: Some(vec!["x".to_owned()]),
                }),
                tts: Some(ConversationConfigOverrideTts {
                    voice_id: Some("x".to_owned()),
                    // Populated even though `check-against-published-types` reports it
                    // as undeclared, because it is: `model_id` is openconv's own
                    // extension to this object and the script is right to say so. One
                    // known finding, reported every run, is worth more than a field
                    // hidden from the check that exists to find exactly this.
                    model_id: Some("x".to_owned()),
                    stability: Some(0.5),
                    speed: Some(1.0),
                    similarity_boost: Some(0.5),
                }),
                conversation: Some(ConversationConfigOverrideConversation {
                    text_only: Some(false),
                    client_events: Some(vec![ClientEventKind::VadScore]),
                }),
            }),
            custom_llm_extra_body: Some(object(json!({}))),
            dynamic_variables: Some(object(json!({}))),
            user_id: Some("x".to_owned()),
            source_info: Some(SourceInfo {
                source: Some("x".to_owned()),
                version: Some("x".to_owned()),
            }),
        })),
    ];

    server
        .into_iter()
        .map(|event| serde_json::to_value(event).expect("serializes"))
        .chain(
            client
                .into_iter()
                .map(|event| serde_json::to_value(event).expect("serializes")),
        )
        .collect()
}
