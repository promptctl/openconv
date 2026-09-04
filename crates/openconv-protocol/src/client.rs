//! Messages the client sends to the agent.

use serde::{Deserialize, Serialize};

use crate::{ClientEventKind, EventId, JsonObject};

/// A message the client publishes onto the LiveKit data channel.
///
/// Unlike [`ServerEvent`](crate::ServerEvent), most of these carry their payload as
/// fields sitting directly alongside `type` rather than nested under a per-variant
/// name — `{"type":"pong","event_id":42}`, not `{"type":"pong","pong_event":{…}}`.
/// The nesting is a property of each individual message, not of the direction, so it
/// is transcribed message by message from the spec.
///
/// There is no catch-all variant. A message this enum cannot name is protocol drift
/// between openconv and the client SDK, and deserialization fails loudly so the drift
/// is reported at the point it happens instead of being silently ignored.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientEvent {
    /// Answers a [`ServerEvent::Ping`](crate::ServerEvent::Ping), echoing its id.
    Pong { event_id: EventId },
    /// Typed input standing in for speech: it opens a turn exactly as talking would.
    UserMessage {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
    },
    /// A liveness signal — the user is present and interacting, so hold off on
    /// whatever the agent does when a conversation goes quiet.
    UserActivity,
    Feedback {
        event_id: EventId,
        score: FeedbackScore,
    },
    /// The outcome of a tool the agent asked the client to run. `result` is a string
    /// even when the tool produced structured data; the client serializes it.
    ClientToolResult {
        tool_call_id: String,
        result: String,
        is_error: bool,
    },
    McpToolApprovalResult {
        tool_call_id: String,
        is_approved: bool,
    },
    /// Context for the agent to absorb without treating it as the user's turn — the
    /// distinction that keeps background session events from being answered aloud.
    ContextualUpdate { text: String },
    /// The first message of every conversation, carrying the per-session
    /// configuration the agent applies before its opening turn.
    ///
    /// Boxed and named, unlike the other variants: this payload is an order of
    /// magnitude larger than the rest, and it outlives the message — the agent holds
    /// onto it for the whole conversation, so it wants to be a value that can be
    /// passed around on its own.
    #[serde(rename = "conversation_initiation_client_data")]
    ConversationInitiation(Box<ConversationInitiationClientData>),
}

/// The per-session configuration accompanying a conversation's first message.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ConversationInitiationClientData {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_config_override: Option<ConversationConfigOverride>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_llm_extra_body: Option<JsonObject>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dynamic_variables: Option<JsonObject>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_info: Option<SourceInfo>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackScore {
    Like,
    Dislike,
}

/// Per-session overrides of the agent's configured defaults.
///
/// The SDK builds all three sub-objects whenever the caller passes any override at
/// all, so `tts` and `conversation` routinely arrive as empty objects. Every field is
/// optional for that reason, not merely because the spec marks them so.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ConversationConfigOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<ConversationConfigOverrideAgent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tts: Option<ConversationConfigOverrideTts>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation: Option<ConversationConfigOverrideConversation>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ConversationConfigOverrideAgent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<Language>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<PromptOverride>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_mcp_server_ids: Option<Vec<String>>,
}

/// A one-field wrapper in the spec, kept as one so the wire stays
/// `{"prompt":{"prompt":"…"}}` — which is what the SDK actually sends.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PromptOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ConversationConfigOverrideTts {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice_id: Option<String>,
    /// Which engine synthesizes, where `voice_id` says which voice.
    ///
    /// **An extension, and the only field here not transcribed from the published
    /// types.** `ConversationConfigOverrideTts` in `@elevenlabs/types` declares
    /// `voice_id`, `stability`, `speed` and `similarity_boost` and nothing else — the
    /// sole `model_id` in that file is on `Config`, which configures audio input. So
    /// `scripts/check-against-published-types.mjs` reports this field as undeclared, and
    /// that report is correct rather than a fault in the script.
    ///
    /// It is here because the engine is a per-conversation choice on this deployment:
    /// elvenspeak serves several engines behind one endpoint and picks between them by
    /// `model_id`, and with nowhere to put one a caller cannot reach any but the
    /// default. Optional and skipped when unset, so a payload from the real SDK parses
    /// unchanged and one openconv sends is byte-identical to before unless a caller
    /// asks for an engine.
    ///
    /// Carried through untranslated for the same reason `voice_id` is: the
    /// text-to-speech server owns what an id means, including which ids it refuses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stability: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub similarity_boost: Option<f64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ConversationConfigOverrideConversation {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_only: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_events: Option<Vec<ClientEventKind>>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SourceInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// The languages the agent may be switched to.
///
/// A closed set, because the published type is a closed union: the client picks from
/// this list or omits the field entirely to let the agent auto-detect. A code outside
/// it is a client that has outrun this crate, and failing to parse says so.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    En,
    Ja,
    Zh,
    De,
    Hi,
    Fr,
    Ko,
    Pt,
    #[serde(rename = "pt-br")]
    PtBr,
    It,
    Es,
    Id,
    Nl,
    Tr,
    Pl,
    Sv,
    Bg,
    Ro,
    Ar,
    Cs,
    El,
    Fi,
    Ms,
    Da,
    Ta,
    Uk,
    Ru,
    Hu,
    Hr,
    Sk,
    No,
    Vi,
    Tl,
}

impl Language {
    /// Every language a client may ask for, for anything that has to *offer* the choice.
    ///
    /// The browser page builds its language control from this, by way of `/call/config`.
    /// It cannot ship the list in its own markup: a code outside the closed union fails
    /// the whole message to deserialize, so one stale option in a dropdown does not
    /// degrade to a wrong language — it silently drops the prompt, the voice and the
    /// first message along with it, and the conversation runs on the deployment default
    /// with nothing anywhere saying why. Served from here so the page offers exactly what
    /// this crate accepts. [LAW:one-source-of-truth]
    ///
    /// Be straight about what does and does not check this. Rust cannot enumerate an
    /// enum's variants without a derive this crate does not depend on, so this is a list
    /// a person maintains, not one the compiler fills in. What holds it down is
    /// `closed_vocabularies_match_their_published_names`, which asserts this list and the
    /// wire codes that test transcribes cover each other exactly — so a variant reaching one
    /// of the two and not the other is a failing test rather than a quiet difference. A
    /// variant added to the enum and to *neither* is caught by nothing, which was equally
    /// true before this constant existed.
    pub const ALL: &'static [Language] = &[
        Language::En, Language::Ja, Language::Zh, Language::De, Language::Hi, Language::Fr,
        Language::Ko, Language::Pt, Language::PtBr, Language::It, Language::Es, Language::Id,
        Language::Nl, Language::Tr, Language::Pl, Language::Sv, Language::Bg, Language::Ro,
        Language::Ar, Language::Cs, Language::El, Language::Fi, Language::Ms, Language::Da,
        Language::Ta, Language::Uk, Language::Ru, Language::Hu, Language::Hr, Language::Sk,
        Language::No, Language::Vi, Language::Tl,
    ];

    /// The code this language travels as on the wire.
    ///
    /// Read back out of serde rather than written out again here. The mapping from
    /// variant to code is already stated once, by the `rename_all` on the enum and the
    /// one `rename` that departs from it — and a second copy of thirty-two entries is a
    /// table that goes wrong in exactly one row, silently, for whichever language nobody
    /// is testing in. [LAW:one-source-of-truth]
    ///
    /// Costs an allocation, which is what buys the guarantee.
    pub fn code(self) -> String {
        let serde_json::Value::String(code) = serde_json::to_value(self).expect("serializes")
        else {
            unreachable!("a unit variant with no data serializes as a string")
        };
        code
    }
}
