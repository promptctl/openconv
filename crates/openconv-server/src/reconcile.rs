//! Closing the conversations the SFU has already forgotten.
//!
//! A conversation ends when `room_finished` arrives, and a delivery that never arrives
//! leaves it open forever — reading as a call still in progress, accruing against its
//! caller up to the cap and never past it. Nothing in the webhook path can notice that,
//! because the thing it would have to notice is a message it did not receive.
//!
//! [LAW:one-source-of-truth] So the log is re-read against the thing it is a record of.
//! LiveKit's room list is the territory for "is this call still happening"; the log is the
//! map, and a webhook is only the mechanism that keeps them in step. This is the pass that
//! puts them back in step when that mechanism drops something.
//!
//! The decision is a pure function of the log, the room list, and the clock
//! ([`abandoned`]); the two effects it needs — asking the SFU, appending to the log —
//! stay in [`sweep`], at the edge. [LAW:effects-at-boundaries]

use crate::conversation::ConversationId;
use crate::livekit::LiveKit;
use crate::record::{now_unix_secs, ConversationEvent};
use crate::store::{ConversationLog, LogError};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

/// How often the log is checked against the SFU.
///
/// Lateness costs a conversation reading as in progress for up to an hour longer than it
/// was, which nothing acts on. Frequency costs a Twirp call per interval. Neither number
/// is delicate, and this one is far below the six-hour cap that is the actual consequence
/// of never sweeping at all.
const SWEEP_INTERVAL: Duration = Duration::from_secs(3600);

/// The conversations that started, never ended, and have no room on the SFU any more.
///
/// A conversation with a live room is left alone however old it looks: it is a call in
/// progress, which is the one state that must not be closed out from underneath.
pub fn abandoned(events: &[ConversationEvent], live_rooms: &HashSet<String>) -> Vec<ConversationId> {
    // Ended is every kind of ended, so a second sweep over a log the first one wrote does
    // not append a second `Abandoned` for the same conversation. That idempotence is what
    // makes this safe to run on a schedule rather than once by hand.
    let ended: HashSet<&ConversationId> = events
        .iter()
        .filter_map(|event| match event {
            ConversationEvent::Finished { conversation_id, .. }
            | ConversationEvent::Abandoned { conversation_id, .. } => Some(conversation_id),
            ConversationEvent::Started(_) => None,
        })
        .collect();

    // Keyed, because a log can hold two starts for one id — a retried mint, a replayed
    // line — and two identical `Abandoned` events help nobody.
    let mut open: HashMap<&ConversationId, ()> = HashMap::new();
    for event in events {
        let ConversationEvent::Started(record) = event else { continue };
        let id = &record.conversation_id;
        if !ended.contains(id) && !live_rooms.contains(id.as_str()) {
            open.insert(id, ());
        }
    }

    open.into_keys().cloned().collect()
}

/// Asks the SFU what is still open and writes off everything else.
///
/// Returns how many conversations were closed, which is the number worth logging: a sweep
/// that closes nothing is the steady state, and one that closes many says deliveries have
/// been going missing.
///
/// [LAW:no-silent-failure] An SFU that cannot be reached is an error and not an empty room
/// list. The two are a keystroke apart here and could not be further apart in effect:
/// treating "I could not ask" as "no rooms are open" would end every call in progress,
/// including the ones happening at that moment.
pub async fn sweep(
    log: &ConversationLog,
    livekit: &LiveKit,
    observed_at_unix_secs: i64,
) -> Result<usize, SweepError> {
    let live_rooms = livekit.live_rooms().await.map_err(SweepError::AskingLiveKit)?;
    let events = log.read_all().await.map_err(SweepError::ReadingLog)?;

    let closing = abandoned(&events, &live_rooms);
    for conversation_id in &closing {
        log.append(&ConversationEvent::Abandoned {
            conversation_id: conversation_id.clone(),
            observed_at_unix_secs,
        })
        .await
        .map_err(SweepError::WritingLog)?;
    }

    Ok(closing.len())
}

/// The one owner of when a sweep happens.
///
/// [LAW:no-ambient-temporal-coupling] A schedule with a name, rather than a tidy-up
/// hanging off whichever request happens to arrive: the work is nobody's request, and
/// attaching it to a caller's would make a user's page load pay for a Twirp round trip
/// and leave the log unswept on a quiet day — exactly the day a lost delivery is least
/// likely to be noticed.
///
/// Swept once at startup and then on the interval. The first pass is where a delivery
/// lost while this process was down gets caught, which is the common way to lose one.
///
/// A failed sweep is logged and the schedule continues. The log being briefly out of step
/// with the SFU is a smaller problem than a voice service that stops answering calls
/// because it could not tidy its ledger — but it is still said out loud, because a sweep
/// that has been failing since Tuesday is a thing somebody needs to know.
pub fn run_periodically(log: Arc<ConversationLog>, livekit: Arc<LiveKit>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(SWEEP_INTERVAL);
        loop {
            ticker.tick().await;
            match sweep(&log, &livekit, now_unix_secs()).await {
                Ok(0) => tracing::debug!("conversation log agrees with the SFU"),
                Ok(closed) => tracing::info!(
                    closed,
                    "closed conversations whose rooms are gone and whose ends were never reported"
                ),
                Err(error) => tracing::error!("conversation log sweep failed: {error}"),
            }
        }
    })
}

/// Why a sweep could not be completed.
#[derive(Debug)]
pub enum SweepError {
    AskingLiveKit(crate::livekit::LiveKitError),
    ReadingLog(LogError),
    WritingLog(LogError),
}

impl std::fmt::Display for SweepError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AskingLiveKit(error) => write!(f, "could not ask LiveKit which rooms are open: {error}"),
            Self::ReadingLog(error) => write!(f, "could not read the conversation log: {error}"),
            Self::WritingLog(error) => write!(f, "could not write to the conversation log: {error}"),
        }
    }
}

impl std::error::Error for SweepError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::{AgentId, ConversationRecord, HappyUserId};

    fn started(id: &str) -> ConversationEvent {
        ConversationEvent::Started(ConversationRecord {
            conversation_id: ConversationId::parse(id).expect("a well-formed id"),
            agent_id: AgentId::new("agent_happy"),
            happy_user: Some(HappyUserId::new("u_someone")),
            started_at_unix_secs: 1_700_000_000,
        })
    }

    fn id(value: &str) -> ConversationId {
        ConversationId::parse(value).expect("a well-formed id")
    }

    fn rooms(names: &[&str]) -> HashSet<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn a_conversation_whose_room_is_gone_is_abandoned() {
        let events = [started("conv_aaa")];
        assert_eq!(abandoned(&events, &rooms(&[])), vec![id("conv_aaa")]);
    }

    /// The state that must survive a sweep untouched, whatever else it does: a call
    /// happening right now looks identical to a lost webhook from the log alone, and the
    /// room list is the only thing that tells them apart.
    #[test]
    fn a_conversation_still_in_its_room_is_left_alone() {
        let events = [started("conv_aaa")];
        assert!(abandoned(&events, &rooms(&["conv_aaa"])).is_empty());
    }

    #[test]
    fn a_conversation_the_sfu_already_reported_is_not_swept_again() {
        let events = [
            started("conv_aaa"),
            ConversationEvent::Finished {
                conversation_id: id("conv_aaa"),
                ended_at_unix_secs: 1_700_000_060,
            },
        ];
        assert!(abandoned(&events, &rooms(&[])).is_empty());
    }

    /// What makes this safe on a timer: the second pass over the first pass's own output
    /// has nothing left to do.
    #[test]
    fn sweeping_twice_writes_nothing_the_second_time() {
        let events = [
            started("conv_aaa"),
            ConversationEvent::Abandoned {
                conversation_id: id("conv_aaa"),
                observed_at_unix_secs: 1_700_000_060,
            },
        ];
        assert!(abandoned(&events, &rooms(&[])).is_empty());
    }

    #[test]
    fn a_replayed_start_is_one_conversation_and_gets_one_event() {
        let events = [started("conv_aaa"), started("conv_aaa")];
        assert_eq!(abandoned(&events, &rooms(&[])), vec![id("conv_aaa")]);
    }

    #[test]
    fn only_the_conversations_without_rooms_are_closed() {
        let events = [started("conv_aaa"), started("conv_bbb"), started("conv_ccc")];
        let mut closing = abandoned(&events, &rooms(&["conv_bbb"]));
        closing.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        assert_eq!(closing, vec![id("conv_aaa"), id("conv_ccc")]);
    }
}
