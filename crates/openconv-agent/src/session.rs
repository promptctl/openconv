//! What the client asked this conversation to be.
//!
//! The SDK sends a `conversation_initiation_client_data` message carrying a system
//! prompt override, a first message, a language, and dynamic variables. Honouring all
//! of it matters more than it looks: an agent that ignores the override still holds a
//! perfectly fluent conversation — it simply knows nothing about the coding session it
//! was supposed to be driving. Nothing fails, nothing logs, and the only symptom is an
//! assistant that is confidently unhelpful.
//!
//! Everything here is a pure transformation of that message into the settled
//! configuration a turn runs against.

use crate::speak::Voicing;
use openconv_protocol::{ConversationInitiationClientData, JsonObject};

/// A conversation's configuration, after the client's overrides have been applied.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionConfig {
    /// The system prompt, with dynamic variables already substituted.
    pub system_prompt: String,
    /// What the agent says before the caller says anything, if the client asked for one.
    pub first_message: Option<String>,
    /// The voice the client picked, the engine it asked to hear it in, and the language
    /// it asked to be spoken.
    ///
    /// The language used to sit beside this as a field of its own, written here and read
    /// nowhere — so an operator who configured `language: es` configured nothing, and the
    /// reply came back Spanish text read with English phonemes, which plays perfectly and
    /// is nonsense. It is a fact about how the reply is *spoken*, so it belongs in the one
    /// value that reaches the synthesizer, not in a second home that can hold a different
    /// answer.
    ///
    /// Carried through untranslated: the text-to-speech server owns the tables that resolve
    /// any of the three onto something it can serve, including the fallback for voices it
    /// has never heard of, the refusal for an engine it is not running, and the substitute
    /// voice for a language it cannot speak. See [`crate::tts`] for why no table is copied
    /// here.
    pub voicing: Voicing,
}

impl SessionConfig {
    /// Settles the client's request against a default prompt.
    ///
    /// The default is used only when the client sends no override — it is a fallback,
    /// not a base to append to. ElevenLabs treats an override as a replacement, and a
    /// client that carefully replaced the prompt would be surprised to find ours still
    /// in front of it.
    pub fn settle(default_prompt: &str, client: ConversationInitiationClientData) -> Self {
        let overrides = client.conversation_config_override.unwrap_or_default();
        let agent = overrides.agent.unwrap_or_default();

        let prompt = agent
            .prompt
            .and_then(|prompt| prompt.prompt)
            .unwrap_or_else(|| default_prompt.to_owned());

        let variables = client.dynamic_variables.unwrap_or_default();

        Self {
            system_prompt: substitute(&prompt, &variables),
            first_message: agent.first_message.filter(|message| !message.trim().is_empty()),
            voicing: {
                let tts = overrides.tts.unwrap_or_default();
                // Blank is unsaid on both id axes. A settings screen that stores an
                // empty string means the client picked nothing, and forwarding it would
                // ask the server to resolve `""`. The language needs no such filter: it
                // arrives already parsed out of a closed union, so the only way to say
                // nothing is to omit it.
                Voicing {
                    voice_id: tts.voice_id.filter(|voice| !voice.trim().is_empty()),
                    model_id: tts.model_id.filter(|model| !model.trim().is_empty()),
                    // The client sends this under `agent`, beside the prompt, rather
                    // than under `tts` where the other two are — so it is read from
                    // there and settled here, which is where every axis of the voicing
                    // is settled regardless of which half of the message carried it.
                    language: agent.language,
                }
            },
        }
    }
}

/// Replaces `{{name}}` placeholders with the client's dynamic variables.
///
/// Happy passes `sessionId` and `initialConversationContext` this way, so the
/// substitution is what connects the agent to the coding session it is driving.
///
/// Unknown placeholders are left standing rather than blanked. A prompt that still
/// reads `{{sessionId}}` is visibly wrong to anyone who looks at it; one where the
/// placeholder was silently replaced with nothing looks correct and merely omits the
/// only fact that made the conversation useful.
fn substitute(template: &str, variables: &JsonObject) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;

    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];

        let Some(end) = after.find("}}") else {
            // An unclosed brace is literal text, not a placeholder — emit it and
            // everything after it verbatim. Emitting only the tail here would drop the
            // braces themselves.
            out.push_str(&rest[start..]);
            return out;
        };

        let name = after[..end].trim();
        match variables.get(name) {
            Some(value) => out.push_str(&render(value)),
            None => {
                tracing::warn!(variable = name, "prompt references an unset dynamic variable");
                out.push_str(&rest[start..start + 2 + end + 2]);
            }
        }
        rest = &after[end + 2..];
    }

    out.push_str(rest);
    out
}

/// Renders a variable for insertion into a prompt.
///
/// Strings go in bare — a JSON-quoted `"abc123"` where a session ID belongs would be
/// read by the model as part of the value. Everything else keeps its JSON form, which
/// is the honest rendering of a structure that has no plain-text equivalent.
fn render(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openconv_protocol::{
        ConversationConfigOverride, ConversationConfigOverrideAgent, ConversationConfigOverrideTts,
        Language, PromptOverride,
    };

    fn client_data(agent: ConversationConfigOverrideAgent) -> ConversationInitiationClientData {
        ConversationInitiationClientData {
            conversation_config_override: Some(ConversationConfigOverride {
                agent: Some(agent),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn variables(pairs: &[(&str, &str)]) -> JsonObject {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), serde_json::Value::String((*v).to_owned())))
            .collect()
    }

    /// Every caller now sends the same message shape whether it overrides anything or
    /// not, with `null` standing where nothing was asked for — see `web/conversation.js`,
    /// which is the one place any of them builds it. That only holds together if a
    /// message full of nulls means "use the defaults", so this is the claim that lets the
    /// acceptance runs which used to send *nothing* take the same path as the ones which
    /// always configured: settling this payload has to land exactly where
    /// `crates/openconv-agent/src/lib.rs` starts a conversation before any client speaks.
    #[test]
    fn a_message_that_overrides_nothing_settles_where_a_conversation_starts() {
        let sent: ConversationInitiationClientData = serde_json::from_str(
            r#"{
                "conversation_config_override": {
                    "agent": {"prompt": {"prompt": null}, "first_message": null, "language": null},
                    "tts": {"voice_id": null, "model_id": null}
                },
                "dynamic_variables": null
            }"#,
        )
        .expect("the message every caller sends");

        assert_eq!(
            SessionConfig::settle("default prompt", sent),
            SessionConfig::settle("default prompt", Default::default()),
        );
    }

    /// The same message with every setting filled in, which is the other half of the
    /// contract: that each one lands where the caller meant it to and not merely that the
    /// message parses. A field renamed on this side goes on deserializing perfectly — it
    /// simply stops being read, and the override silently reverts to the default, which is
    /// this ticket's own bug wearing a different hat. Here that shows up as a failed
    /// assertion instead of as a conversation running on a prompt nobody chose.
    ///
    /// Written out as the wire bytes rather than built from the types, because the types
    /// are what is under test. `web/conversation.js` is the only thing that produces this
    /// shape and `web/conversation.test.mjs` pins the identical bytes from that side, so
    /// the two ends of one wire are each nailed down where they can be checked without the
    /// other running. [LAW:one-source-of-truth]
    #[test]
    fn every_setting_a_caller_can_express_reaches_the_conversation() {
        let sent: ConversationInitiationClientData = serde_json::from_str(
            r#"{
                "conversation_config_override": {
                    "agent": {
                        "prompt": {"prompt": "you are a voice interface"},
                        "first_message": "ready when you are",
                        "language": "es"
                    },
                    "tts": {"voice_id": "bm_george", "model_id": "piper"}
                },
                "dynamic_variables": {"sessionId": "sess_42"}
            }"#,
        )
        .expect("the message every caller sends");

        let config = SessionConfig::settle("unused default", sent);

        assert_eq!(config.system_prompt, "you are a voice interface");
        assert_eq!(config.first_message.as_deref(), Some("ready when you are"));
        assert_eq!(config.voicing.language, Some(Language::Es));
        assert_eq!(config.voicing.voice_id.as_deref(), Some("bm_george"));
        assert_eq!(config.voicing.model_id.as_deref(), Some("piper"));
    }

    /// An explicit null and an omitted field are the same answer, which is what lets the
    /// one builder emit a fixed shape rather than assembling itself out of whichever
    /// settings happen to be set. Asserted on the axis where getting it wrong is silent:
    /// `""` would reach the text-to-speech server as a voice id to resolve.
    #[test]
    fn a_null_and_a_blank_both_mean_no_particular_voice() {
        let voiced = |json: &str| {
            let sent: ConversationInitiationClientData =
                serde_json::from_str(json).expect("a client message");
            SessionConfig::settle("unused", sent).voicing
        };

        let nothing = voiced(r#"{"conversation_config_override": {"tts": {"voice_id": null}}}"#);
        assert_eq!(nothing.voice_id, None);
        assert_eq!(voiced(r#"{"conversation_config_override": {"tts": {"voice_id": ""}}}"#), nothing);
        assert_eq!(voiced(r#"{"conversation_config_override": {"tts": {}}}"#), nothing);
    }

    #[test]
    fn an_absent_override_leaves_the_default_prompt() {
        let config = SessionConfig::settle("default prompt", Default::default());
        assert_eq!(config.system_prompt, "default prompt");
        assert_eq!(config.first_message, None);
    }

    /// The failure this whole module exists to prevent: an override that is accepted
    /// and then not used.
    #[test]
    fn an_override_replaces_the_default_rather_than_appending_to_it() {
        let config = SessionConfig::settle(
            "default prompt",
            client_data(ConversationConfigOverrideAgent {
                prompt: Some(PromptOverride { prompt: Some("you are a voice interface".into()) }),
                ..Default::default()
            }),
        );

        assert_eq!(config.system_prompt, "you are a voice interface");
        assert!(!config.system_prompt.contains("default prompt"));
    }

    #[test]
    fn dynamic_variables_are_substituted_into_the_prompt() {
        let mut client = client_data(ConversationConfigOverrideAgent {
            prompt: Some(PromptOverride {
                prompt: Some("session {{sessionId}}\n\n{{initialConversationContext}}".into()),
            }),
            ..Default::default()
        });
        client.dynamic_variables = Some(variables(&[
            ("sessionId", "sess_42"),
            ("initialConversationContext", "user asked about tests"),
        ]));

        let config = SessionConfig::settle("unused", client);
        assert_eq!(config.system_prompt, "session sess_42\n\nuser asked about tests");
    }

    #[test]
    fn variables_are_substituted_into_the_default_prompt_too() {
        let client = ConversationInitiationClientData {
            dynamic_variables: Some(variables(&[("sessionId", "sess_9")])),
            ..Default::default()
        };

        let config = SessionConfig::settle("driving {{sessionId}}", client);
        assert_eq!(config.system_prompt, "driving sess_9");
    }

    /// Left standing on purpose — a visible `{{...}}` is a bug someone can see, and
    /// blanking it produces a prompt that reads correctly and says nothing.
    #[test]
    fn an_unset_variable_is_left_visible_rather_than_blanked() {
        let config = SessionConfig::settle("session {{sessionId}} here", Default::default());
        assert_eq!(config.system_prompt, "session {{sessionId}} here");
    }

    #[test]
    fn braces_that_are_not_placeholders_survive_intact() {
        for prompt in ["use {{ to open", "a { single } brace", "no braces at all"] {
            let config = SessionConfig::settle(prompt, Default::default());
            assert_eq!(config.system_prompt, prompt);
        }
    }

    #[test]
    fn a_non_string_variable_keeps_its_json_form() {
        let mut vars = JsonObject::new();
        vars.insert("count".to_owned(), serde_json::json!(3));
        vars.insert("open".to_owned(), serde_json::json!(true));
        let client = ConversationInitiationClientData {
            dynamic_variables: Some(vars),
            ..Default::default()
        };

        let config = SessionConfig::settle("{{count}} sessions, open={{open}}", client);
        assert_eq!(config.system_prompt, "3 sessions, open=true");
    }

    #[test]
    fn a_first_message_is_carried_through_but_blank_is_not() {
        let with = SessionConfig::settle(
            "p",
            client_data(ConversationConfigOverrideAgent {
                first_message: Some("Hi, what are we working on?".into()),
                ..Default::default()
            }),
        );
        assert_eq!(with.first_message.as_deref(), Some("Hi, what are we working on?"));

        let blank = SessionConfig::settle(
            "p",
            client_data(ConversationConfigOverrideAgent {
                first_message: Some("   ".into()),
                ..Default::default()
            }),
        );
        assert_eq!(blank.first_message, None, "whitespace is not a greeting");
    }

    fn tts_override(tts: ConversationConfigOverrideTts) -> ConversationInitiationClientData {
        ConversationInitiationClientData {
            conversation_config_override: Some(ConversationConfigOverride {
                tts: Some(tts),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// Carried through exactly as sent — resolving it is the text-to-speech server's job, and
    /// an ID rewritten here would be a second opinion about which voice this is.
    #[test]
    fn the_clients_voice_is_carried_through_untranslated() {
        let config = SessionConfig::settle(
            "p",
            tts_override(ConversationConfigOverrideTts {
                voice_id: Some("21m00Tcm4TlvDq8ikWAM".into()),
                ..Default::default()
            }),
        );
        assert_eq!(config.voicing.voice_id.as_deref(), Some("21m00Tcm4TlvDq8ikWAM"));
    }

    /// No voice and a blank voice mean the same thing — use whatever the service
    /// defaults to — and collapsing them here saves every caller the check.
    #[test]
    fn an_absent_or_blank_voice_leaves_the_default_standing() {
        assert_eq!(SessionConfig::settle("p", Default::default()).voicing.voice_id, None);

        let blank = SessionConfig::settle(
            "p",
            tts_override(ConversationConfigOverrideTts { voice_id: Some("  ".into()), ..Default::default() }),
        );
        assert_eq!(blank.voicing.voice_id, None);
    }

    #[test]
    fn the_clients_engine_is_carried_through_untranslated() {
        // The second axis, and the one this deployment has more than one of: elvenspeak
        // serves several engines behind one endpoint and picks between them by
        // `model_id`. Untranslated for the same reason the voice is — which engine an id
        // names, and whether this deployment runs it, is that server's answer.
        let config = SessionConfig::settle(
            "p",
            tts_override(ConversationConfigOverrideTts {
                model_id: Some("kokoro".into()),
                ..Default::default()
            }),
        );
        assert_eq!(config.voicing.model_id.as_deref(), Some("kokoro"));
    }

    #[test]
    fn an_absent_or_blank_engine_leaves_the_default_standing() {
        let unset = SessionConfig::settle("p", Default::default());
        assert_eq!(unset.voicing.model_id, None);

        let blank = SessionConfig::settle(
            "p",
            tts_override(ConversationConfigOverrideTts {
                model_id: Some("  ".into()),
                ..Default::default()
            }),
        );
        assert_eq!(blank.voicing.model_id, None);
    }

    /// The read that did not exist, from the settling end.
    ///
    /// `language` was copied into a field of `SessionConfig` and read nowhere — grep for
    /// it before this change and the write is the only hit — so an operator who
    /// configured `language: es` configured nothing at all. It now lands in the one
    /// value that reaches the synthesizer, which is the only place a fact about how the
    /// reply is spoken can be read from.
    ///
    /// Carried untranslated like the two ids beside it, though for the opposite reason:
    /// those are the server's vocabulary and this is the published one, and either way
    /// the crate that re-spells it is the crate that gets it wrong.
    #[test]
    fn the_clients_language_reaches_the_voicing_rather_than_stopping_here() {
        let config = SessionConfig::settle(
            "p",
            client_data(ConversationConfigOverrideAgent {
                language: Some(Language::Es),
                ..Default::default()
            }),
        );
        assert_eq!(config.voicing.language, Some(Language::Es));
    }

    /// An unconfigured agent keeps saying nothing, which is not the same as saying English.
    ///
    /// The tempting default is right here: every conversation before this field was read
    /// was in English, so `unwrap_or(Language::En)` looks like a no-op. It is not — the
    /// server picks the voice when nobody names a language, and an `en` sent on the
    /// agent's behalf takes that decision away from the deployment that was making it.
    #[test]
    fn an_agent_that_configured_no_language_asks_for_none() {
        assert_eq!(SessionConfig::settle("p", Default::default()).voicing.language, None);
    }

    /// The language arrives under `agent` and the ids under `tts`, and all three settle.
    ///
    /// Two halves of the client's message reaching one value is the shape this test is
    /// about: a settling that read the voicing only out of the `tts` override would drop
    /// the language of a client that sent both, and every test above would still pass.
    #[test]
    fn a_language_and_a_voice_arriving_in_different_halves_both_land() {
        let config = SessionConfig::settle(
            "p",
            ConversationInitiationClientData {
                conversation_config_override: Some(ConversationConfigOverride {
                    agent: Some(ConversationConfigOverrideAgent {
                        language: Some(Language::Es),
                        ..Default::default()
                    }),
                    tts: Some(ConversationConfigOverrideTts {
                        voice_id: Some("es_MX-claude-high".into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
        );

        assert_eq!(config.voicing.language, Some(Language::Es));
        assert_eq!(config.voicing.voice_id.as_deref(), Some("es_MX-claude-high"));
    }

    #[test]
    fn the_two_axes_are_settled_independently() {
        // They arrive in one object and are two decisions. A client that picked a voice
        // and no engine must not have the voice read as an engine, nor either dropped
        // because the other was absent.
        let voice_only = SessionConfig::settle(
            "p",
            tts_override(ConversationConfigOverrideTts {
                voice_id: Some("21m00Tcm4TlvDq8ikWAM".into()),
                ..Default::default()
            }),
        );
        assert_eq!(
            voice_only.voicing.voice_id.as_deref(),
            Some("21m00Tcm4TlvDq8ikWAM")
        );
        assert_eq!(voice_only.voicing.model_id, None);

        let both = SessionConfig::settle(
            "p",
            tts_override(ConversationConfigOverrideTts {
                voice_id: Some("af_heart".into()),
                model_id: Some("kokoro".into()),
                ..Default::default()
            }),
        );
        assert_eq!(both.voicing.voice_id.as_deref(), Some("af_heart"));
        assert_eq!(both.voicing.model_id.as_deref(), Some("kokoro"));
    }
}
