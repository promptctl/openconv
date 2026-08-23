//! Turning the conversation log into the answer `GET /v1/convai/conversations` gives.
//!
//! Everything here is a pure function of the events, the query, and the current time.
//! Reading the log is [`crate::store`]'s job and serving the result is [`crate::api`]'s,
//! which leaves the part with the actual judgement in it — how long a call lasted, and
//! which calls a query is asking about — testable against fixed inputs with no clock,
//! no filesystem, and no mocks.

use crate::conversation::ConversationId;
use crate::record::{AgentId, ConversationEvent, HappyUserId};
use serde::Serialize;
use std::collections::HashMap;

/// The longest a conversation that never reported an end is credited with.
///
/// An unfinished conversation is either happening right now or its `room_finished`
/// webhook was lost, and the two are indistinguishable from the log. Both are charged
/// for the time elapsed since they started, because reporting zero would make dropping
/// the webhook — or simply holding a call open — a way to use the service for free.
/// The cap is the participant token's lifetime: LiveKit re-presents that token on
/// every reconnect, so no conversation can outlive it, and anything older is certainly
/// over rather than merely unreported.
const MAX_UNREPORTED_DURATION_SECS: i64 = 6 * 3600;

/// A conversation as this endpoint reports it.
///
/// `call_duration_secs` is the field Happy actually reads — it sums them across the
/// window to gate usage — and the array length is the other thing it uses. Those two
/// are pinned by a real consumer. The remaining fields mirror what ElevenLabs' own
/// conversation list returns so a client written against their API finds what it
/// expects; unlike the control protocol, that shape ships in no published type, so it
/// is matched from their documentation rather than read from a generated one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Conversation {
    pub conversation_id: ConversationId,
    pub agent_id: AgentId,
    pub start_time_unix_secs: i64,
    pub call_duration_secs: i64,
    pub status: ConversationStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ConversationStatus {
    /// Started, with no end reported yet.
    InProgress,
    /// LiveKit reported the room closed.
    Done,
}

/// The response body, shaped like ElevenLabs' so a client written against their list
/// endpoint can page through this one the same way.
#[derive(Debug, Serialize)]
pub struct ConversationPage {
    pub conversations: Vec<Conversation>,
    pub has_more: bool,
}

/// What a caller is asking for.
///
/// Both filters are optional in the type because both are optional on the wire, and
/// absent means "unfiltered" rather than "match nothing" — the distinction that decides
/// whether an operator running the endpoint by hand sees everything or nothing.
#[derive(Clone, Debug, Default)]
pub struct UsageQuery {
    pub user_id: Option<HappyUserId>,
    pub created_after_unix_secs: Option<i64>,
    pub page_size: Option<usize>,
}

/// Happy asks for 100 and ElevenLabs caps there too, so an unbounded default would
/// only ever surprise somebody.
const DEFAULT_PAGE_SIZE: usize = 100;
const MAX_PAGE_SIZE: usize = 100;

/// Folds the log into the conversations a query asks for, newest first.
pub fn conversations(
    events: &[ConversationEvent],
    query: &UsageQuery,
    now_unix_secs: i64,
) -> ConversationPage {
    // Ends are collected first so the fold over starts can answer "did this one
    // finish" without scanning, and so an end that arrives before its start — which a
    // truncated or reordered log can produce — simply finds no start to attach to
    // rather than inventing a conversation with no user.
    let ends: HashMap<&ConversationId, i64> = events
        .iter()
        .filter_map(|event| match event {
            ConversationEvent::Finished { conversation_id, ended_at_unix_secs } => {
                Some((conversation_id, *ended_at_unix_secs))
            }
            ConversationEvent::Started(_) => None,
        })
        .collect();

    let mut matched: Vec<Conversation> = events
        .iter()
        .filter_map(|event| match event {
            ConversationEvent::Started(record) => Some(record),
            ConversationEvent::Finished { .. } => None,
        })
        .filter(|record| query.matches(record.happy_user.as_ref(), record.started_at_unix_secs))
        .map(|record| {
            let (ended_at, status) = ends.get(&record.conversation_id).map_or(
                (now_unix_secs, ConversationStatus::InProgress),
                |ended| (*ended, ConversationStatus::Done),
            );

            Conversation {
                conversation_id: record.conversation_id.clone(),
                agent_id: record.agent_id.clone(),
                start_time_unix_secs: record.started_at_unix_secs,
                call_duration_secs: duration_secs(record.started_at_unix_secs, ended_at, status),
                status,
            }
        })
        .collect();

    // Newest first, matching how ElevenLabs returns them and how anyone reading a
    // usage list expects to read it.
    matched.sort_by(|a, b| b.start_time_unix_secs.cmp(&a.start_time_unix_secs));

    let page_size = query.page_size.unwrap_or(DEFAULT_PAGE_SIZE).min(MAX_PAGE_SIZE);
    let has_more = matched.len() > page_size;
    matched.truncate(page_size);

    ConversationPage { conversations: matched, has_more }
}

/// How long a call is credited with.
///
/// Clamped at both ends. Clocks moving backwards would otherwise produce a negative
/// duration that *subtracts* from a user's usage, and an unfinished conversation is
/// capped so a lost webhook cannot accrue against a user forever.
fn duration_secs(started_at: i64, ended_at: i64, status: ConversationStatus) -> i64 {
    let elapsed = (ended_at - started_at).max(0);
    match status {
        ConversationStatus::Done => elapsed,
        ConversationStatus::InProgress => elapsed.min(MAX_UNREPORTED_DURATION_SECS),
    }
}

impl UsageQuery {
    fn matches(&self, happy_user: Option<&HappyUserId>, started_at_unix_secs: i64) -> bool {
        // A conversation with no user is the bring-your-own-key path. It matches no
        // `user_id`, which is correct: nobody's allowance is spent on a call the caller
        // paid their own provider for.
        let user_matches = self.user_id.as_ref().is_none_or(|wanted| happy_user == Some(wanted));

        let window_matches = self
            .created_after_unix_secs
            .is_none_or(|cutoff| started_at_unix_secs >= cutoff);

        user_matches && window_matches
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::ConversationRecord;

    const NOW: i64 = 1_700_000_000;

    fn started(id: &str, user: Option<&str>, at: i64) -> ConversationEvent {
        ConversationEvent::Started(ConversationRecord::start(
            ConversationId::parse(id).unwrap(),
            AgentId::new("agent_happy"),
            user.map(HappyUserId::new),
            at,
        ))
    }

    fn finished(id: &str, at: i64) -> ConversationEvent {
        ConversationEvent::Finished {
            conversation_id: ConversationId::parse(id).unwrap(),
            ended_at_unix_secs: at,
        }
    }

    fn query(user: Option<&str>, after: Option<i64>) -> UsageQuery {
        UsageQuery {
            user_id: user.map(HappyUserId::new),
            created_after_unix_secs: after,
            page_size: None,
        }
    }

    /// The ticket's acceptance criterion, stated as a test: two completed sessions for
    /// one user with plausible durations, and a cutoff between them returning only the
    /// later one.
    #[test]
    fn two_completed_sessions_then_a_cutoff_between_them() {
        let events = vec![
            started("conv_aaa", Some("u_alice"), NOW - 1000),
            finished("conv_aaa", NOW - 940), // 60s
            started("conv_bbb", Some("u_alice"), NOW - 500),
            finished("conv_bbb", NOW - 380), // 120s
        ];

        let page = conversations(&events, &query(Some("u_alice"), None), NOW);
        assert_eq!(page.conversations.len(), 2);
        assert_eq!(
            page.conversations.iter().map(|c| c.call_duration_secs).sum::<i64>(),
            180
        );

        let later = conversations(&events, &query(Some("u_alice"), Some(NOW - 600)), NOW);
        assert_eq!(later.conversations.len(), 1);
        assert_eq!(later.conversations[0].conversation_id.as_str(), "conv_bbb");
        assert_eq!(later.conversations[0].call_duration_secs, 120);
    }

    #[test]
    fn one_users_calls_are_invisible_to_another() {
        let events = vec![
            started("conv_aaa", Some("u_alice"), NOW - 100),
            finished("conv_aaa", NOW - 40),
            started("conv_bbb", Some("u_bob"), NOW - 100),
            finished("conv_bbb", NOW - 90),
        ];

        let alice = conversations(&events, &query(Some("u_alice"), None), NOW);
        assert_eq!(alice.conversations.len(), 1);
        assert_eq!(alice.conversations[0].call_duration_secs, 60);
    }

    #[test]
    fn a_byo_conversation_is_billed_to_nobody() {
        let events = vec![started("conv_aaa", None, NOW - 100), finished("conv_aaa", NOW - 40)];

        assert!(conversations(&events, &query(Some("u_alice"), None), NOW).conversations.is_empty());
        // ...but an unfiltered query still shows it, so it is hidden from billing
        // rather than missing from the record.
        assert_eq!(conversations(&events, &query(None, None), NOW).conversations.len(), 1);
    }

    #[test]
    fn an_unfinished_conversation_is_charged_for_time_elapsed() {
        let events = vec![started("conv_aaa", Some("u_alice"), NOW - 90)];
        let page = conversations(&events, &query(Some("u_alice"), None), NOW);

        assert_eq!(page.conversations[0].call_duration_secs, 90);
        assert_eq!(page.conversations[0].status, ConversationStatus::InProgress);
    }

    /// A lost `room_finished` webhook must not accrue against a user forever.
    #[test]
    fn an_abandoned_conversation_stops_accruing_at_the_cap() {
        let events = vec![started("conv_aaa", Some("u_alice"), NOW - 400 * 86400)];
        let page = conversations(&events, &query(Some("u_alice"), None), NOW);

        assert_eq!(page.conversations[0].call_duration_secs, MAX_UNREPORTED_DURATION_SECS);
    }

    /// A backwards clock must never hand usage back to a user.
    #[test]
    fn durations_are_never_negative() {
        let events = vec![
            started("conv_aaa", Some("u_alice"), NOW),
            finished("conv_aaa", NOW - 500),
        ];
        let page = conversations(&events, &query(Some("u_alice"), None), NOW);

        assert_eq!(page.conversations[0].call_duration_secs, 0);
    }

    #[test]
    fn an_end_with_no_start_yields_no_conversation() {
        let page = conversations(&[finished("conv_orphan", NOW)], &query(None, None), NOW);
        assert!(page.conversations.is_empty());
    }

    #[test]
    fn conversations_come_back_newest_first() {
        let events = vec![
            started("conv_aaa", Some("u_alice"), NOW - 100),
            started("conv_bbb", Some("u_alice"), NOW - 300),
            started("conv_ccc", Some("u_alice"), NOW - 200),
        ];
        let page = conversations(&events, &query(Some("u_alice"), None), NOW);

        let order: Vec<_> = page.conversations.iter().map(|c| c.conversation_id.as_str()).collect();
        assert_eq!(order, ["conv_aaa", "conv_ccc", "conv_bbb"]);
    }

    #[test]
    fn a_page_is_capped_and_says_when_more_remain() {
        let events: Vec<_> = (0..150)
            .map(|i| started(&format!("conv_x{i:03}"), Some("u_alice"), NOW - i))
            .collect();

        let page = conversations(&events, &query(Some("u_alice"), None), NOW);
        assert_eq!(page.conversations.len(), 100);
        assert!(page.has_more);

        let small = conversations(
            &events,
            &UsageQuery { page_size: Some(10), ..query(Some("u_alice"), None) },
            NOW,
        );
        assert_eq!(small.conversations.len(), 10);
        assert!(small.has_more);
    }

    #[test]
    fn an_empty_log_is_a_real_answer() {
        let page = conversations(&[], &query(Some("u_alice"), None), NOW);
        assert!(page.conversations.is_empty());
        assert!(!page.has_more);
    }
}
