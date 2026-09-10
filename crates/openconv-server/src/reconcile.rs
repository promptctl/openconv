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
use crate::livekit::{LiveKit, LiveKitError};
use crate::record::{now_unix_secs, ConversationEvent};
use crate::store::{ConversationLog, LogError};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

/// Whatever can say which rooms are open right now.
///
/// A seam for one reason: the order in which a sweep takes its two readings is the whole
/// correctness argument below, and against the concrete client that order is untestable —
/// nothing can start a conversation in the gap between the two awaits. Behind this trait a
/// test can, which is what turns "the reads are the right way round" from a comment into
/// something that fails when it stops being true. [LAW:verifiable-goals]
pub trait RoomSource {
    fn open_rooms(&self) -> impl Future<Output = Result<HashSet<String>, LiveKitError>> + Send;
}

impl RoomSource for LiveKit {
    fn open_rooms(&self) -> impl Future<Output = Result<HashSet<String>, LiveKitError>> + Send {
        self.live_rooms()
    }
}

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
    rooms: &impl RoomSource,
    observed_at_unix_secs: i64,
) -> Result<usize, SweepError> {
    // The log first and the room list second, which is the only order that is safe.
    //
    // A conversation's room is created before its `Started` line is appended
    // (`api::conversation_token`), so anything carrying a `Started` in this snapshot had a
    // room strictly before the snapshot was taken, and a room list asked for afterwards is
    // guaranteed to show it. Asking the SFU first inverts that: a call born between the two
    // reads is in the log and missing from the rooms, and gets written off mid-call — into
    // an append-only log, so permanently. [LAW:no-ambient-temporal-coupling]
    let events = log.read_all().await.map_err(SweepError::ReadingLog)?;
    let live_rooms = rooms.open_rooms().await.map_err(SweepError::AskingLiveKit)?;

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
pub fn run_periodically<R: RoomSource + Send + Sync + 'static>(
    log: Arc<ConversationLog>,
    rooms: Arc<R>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(SWEEP_INTERVAL);
        loop {
            ticker.tick().await;
            match sweep(&log, &*rooms, now_unix_secs()).await {
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
        started_with_id(ConversationId::parse(id).expect("a well-formed id"))
    }

    fn started_with_id(conversation_id: ConversationId) -> ConversationEvent {
        ConversationEvent::Started(ConversationRecord {
            conversation_id,
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

    /// A room list that reports nothing, and starts a conversation while it is being asked.
    ///
    /// Stands in for the only moment that matters: a caller minting a token in the gap
    /// between a sweep's two readings. The room it creates is not in the list this returns,
    /// exactly as a room created after the SFU was asked would not be.
    struct SfuThatStartsACallWhileAnswering {
        log: Arc<ConversationLog>,
        conversation: ConversationId,
    }

    impl RoomSource for SfuThatStartsACallWhileAnswering {
        async fn open_rooms(&self) -> Result<HashSet<String>, LiveKitError> {
            self.log
                .append(&started_with_id(self.conversation.clone()))
                .await
                .expect("the log accepts the conversation that just started");
            Ok(HashSet::new())
        }
    }

    fn temp_log() -> ConversationLog {
        ConversationLog::new(std::env::temp_dir().join(format!(
            "openconv-reconcile-test-{}-{}.jsonl",
            std::process::id(),
            ConversationId::generate()
        )))
    }

    #[test]
    fn a_conversation_whose_room_is_gone_is_abandoned() {
        let events = [started("conv_aaa")];
        assert_eq!(abandoned(&events, &rooms(&[])), vec![id("conv_aaa")]);
    }

    /// The race this ordering exists to close, and the reason the two reads may not be
    /// swapped back: a call born mid-sweep must not be written off.
    ///
    /// Reading the log first is what makes it safe — the conversation this SFU starts while
    /// answering cannot be in a snapshot taken before it existed. With the readings the
    /// other way round it is in the log and absent from the rooms, and the sweep closes a
    /// call that is still running, permanently.
    #[tokio::test]
    async fn a_conversation_that_starts_mid_sweep_is_not_written_off() {
        let log = Arc::new(temp_log());
        let born = ConversationId::generate();
        let sfu = SfuThatStartsACallWhileAnswering { log: log.clone(), conversation: born.clone() };

        let closed = sweep(&log, &sfu, 1_700_000_100).await.expect("the sweep runs");

        assert_eq!(closed, 0, "a call that started mid-sweep was written off");
        let events = log.read_all().await.expect("the log reads back");
        assert!(
            !events.iter().any(|event| matches!(
                event,
                ConversationEvent::Abandoned { conversation_id, .. } if *conversation_id == born
            )),
            "the log holds an Abandoned for a conversation that had only just started",
        );
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
