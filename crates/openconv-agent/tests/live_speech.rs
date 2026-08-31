//! Checks the speech path against a real text-to-speech server (elvenspeak today).
//!
//! Ignored by default, because it needs a server: run it with
//!
//!   OPENCONV_TTS_URL=http://127.0.0.1:11000 cargo test -p openconv-agent --test live_speech -- --ignored --nocapture
//!
//! The unit tests cover the awkward parts against fixed bytes; these cover the thing
//! fixtures cannot — that the service still answers in the shape this code decodes, and
//! how long it takes to do it. That second number is the one the whole streaming design
//! is arranged around, so it is worth being able to measure rather than assume.

use openconv_agent::audio::SAMPLE_RATE;
use openconv_agent::clause::Clauses;
use futures_util::StreamExt;
use openconv_agent::speak::collect;
use openconv_agent::tts::Tts;
use std::time::Instant;

fn tts() -> Tts {
    Tts::new(
        std::env::var("OPENCONV_TTS_URL").unwrap_or_else(|_| "http://127.0.0.1:11000".to_owned()),
        std::env::var("OPENCONV_TTS_VOICE").unwrap_or_else(|_| "21m00Tcm4TlvDq8ikWAM".to_owned()),
    )
}

fn seconds(samples: &[i16]) -> f32 {
    samples.len() as f32 / SAMPLE_RATE as f32
}

/// The whole client, end to end: a clause goes out, audible speech comes back at the
/// rate the agent's track publishes.
#[tokio::test]
#[ignore = "needs a running text-to-speech server"]
async fn a_clause_comes_back_as_audible_audio_at_the_tracks_rate() {
    let spoken = "Both suites passed on the first attempt.";

    let started = Instant::now();
    let speech = tts().synthesize(None, spoken).await.expect("the server answered");
    let samples = collect(speech).await.expect("the audio decoded");
    let took = started.elapsed();

    println!("{:?} for {:.2}s of audio", took, seconds(&samples));

    // Speech, not silence and not a click: a real utterance of roughly this length.
    assert!(
        (1.0..8.0).contains(&seconds(&samples)),
        "a seven-word clause became {:.2}s of audio",
        seconds(&samples)
    );
    assert!(
        samples.iter().any(|&sample| sample.abs() > i16::MAX / 50),
        "decoded to something inaudible"
    );

    // A rate mismatch is the failure that is inaudible in a test and obvious on a call:
    // the words are all there, an octave off. Speaking rate is the only way to catch it
    // from samples alone — seven words do not fit in two seconds.
    let words_per_second = spoken.split_whitespace().count() as f32 / seconds(&samples);
    assert!(
        (1.0..5.0).contains(&words_per_second),
        "{words_per_second:.1} words per second — the audio is being played at the wrong rate"
    );
}

/// What the streaming design buys, measured rather than assumed.
///
/// Prints the two numbers that decide whether cutting a reply into clauses is worth it:
/// what a clause costs to synthesize, and how much of that is fixed overhead rather than
/// proportional to the words. When the fixed part dominates, the reply should be cut
/// into fewer, larger pieces; when the proportional part does, into as many as it takes
/// to get the first one speaking. This is how you find out which world you are in.
///
/// The name of this test used to assert the answer — `the_cost_of_a_clause_is_mostly_fixed`
/// — which was true of elvenreader-server and false of elvenspeak, while the test went
/// on passing either way because it measures rather than asserts the ratio. A test whose
/// name states a finding is a second copy of that finding that no assertion keeps honest,
/// so it now names the question instead.
#[tokio::test]
#[ignore = "needs a running text-to-speech server"]
async fn what_a_clause_costs_fixed_versus_per_second() {
    let tts = tts();

    let short = Instant::now();
    let short_audio = collect(tts.synthesize(None, "They passed.").await.expect("short clause"))
        .await
        .expect("short clause audio");
    let short = short.elapsed();

    let long = Instant::now();
    let long_audio = collect(
        tts.synthesize(
            None,
            "Both suites passed on the first attempt, and the whole run took a little \
             under four minutes from a cold cache.",
        )
        .await
        .expect("long clause"),
    )
    .await
    .expect("long clause audio");
    let long = long.elapsed();

    println!("short: {short:?} for {:.2}s of audio", seconds(&short_audio));
    println!("long:  {long:?} for {:.2}s of audio", seconds(&long_audio));
    println!(
        "roughly {:.1}s fixed per request, {:.2}s per second of speech",
        short.as_secs_f32(),
        (long.as_secs_f32() - short.as_secs_f32())
            / (seconds(&long_audio) - seconds(&short_audio)).max(0.01)
    );

    assert!(long_audio.len() > short_audio.len(), "the longer clause was not longer");
}

/// Where the first clause of a reply lands, against a recorded one.
///
/// The timings are from a real `claude-opus-5` reply at low effort, taken off the wire:
/// two text deltas and a stream that ends shortly after the second. Recorded rather than
/// live so this measures the speech path rather than the model's mood — and because the
/// interesting quantity is the *gap*, which is a property of the API, not of one answer.
#[tokio::test]
#[ignore = "needs a running text-to-speech server"]
async fn the_first_clause_is_measured_against_when_the_reply_ended() {
    // (arrival in seconds, text) — a recorded reply, and when it finished.
    let recorded = [
        (1.42, "The suite covers the parser, the endpointer and the usage endpoint. "),
        (2.06, "A full run takes a little under four minutes."),
    ];
    let reply_ended = 2.09;

    let mut clauses = Clauses::new();
    let mut first_clause = None;
    for (at, text) in recorded {
        for clause in clauses.push(text) {
            first_clause.get_or_insert((at, clause));
        }
    }
    let (written_at, clause) = first_clause
        .or_else(|| clauses.flush().map(|c| (reply_ended, c)))
        .expect("the reply produced a clause");

    let started = Instant::now();
    let mut speech = tts().synthesize(None, &clause).await.expect("the server answered");

    // Time to the *first* stretch of audio, not the last — that is the moment the caller
    // starts hearing something, and the whole reason this decodes as the bytes arrive.
    let first = speech.next().await.expect("some audio").expect("it decoded");
    let to_first = started.elapsed().as_secs_f32();
    let rest = collect(speech).await.expect("the rest decoded");
    let to_last = started.elapsed().as_secs_f32();

    let audible_at = written_at + to_first;
    println!("first clause written at {written_at:.2}s: {clause}");
    println!(
        "first audio after {to_first:.2}s, last after {to_last:.2}s \
         ({:.2}s of speech) — decoding on the way in saved {:.2}s",
        seconds(&first) + seconds(&rest),
        to_last - to_first
    );
    println!("audio is ready at {audible_at:.2}s; the model finished writing at {reply_ended:.2}s");
    println!(
        "{}",
        match audible_at < reply_ended {
            true => format!("audio leads the model by {:.2}s", reply_ended - audible_at),
            false => format!(
                "synthesis is the bottleneck: audio trails the model by {:.2}s",
                audible_at - reply_ended
            ),
        }
    );

    // What streaming the *text* buys: the gap it removes, which is real whatever
    // synthesis costs.
    assert!(
        written_at < reply_ended,
        "the first clause was not speakable before the reply ended, so cutting bought nothing"
    );
    // Deliberately not asserted: how much of the request is spent receiving audio the
    // caller could already be hearing. That is the server's to improve, and the printed
    // `to_first` / `to_last` above is how you find out whether it has. Against
    // elvenspeak it has — first audio lands in milliseconds and the gap is a few
    // hundredths of a second, where elvenreader-server delivered the whole body in one
    // burst at the end.
}
