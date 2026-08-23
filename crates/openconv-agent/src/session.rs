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

use openconv_protocol::{ConversationInitiationClientData, JsonObject, Language};

/// A conversation's configuration, after the client's overrides have been applied.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionConfig {
    /// The system prompt, with dynamic variables already substituted.
    pub system_prompt: String,
    /// What the agent says before the caller says anything, if the client asked for one.
    pub first_message: Option<String>,
    pub language: Option<Language>,
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
            language: agent.language,
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
        ConversationConfigOverride, ConversationConfigOverrideAgent, PromptOverride,
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
}
