//! Answers one line out loud, and times the thing the speech path exists to do.
//!
//! Isolates the reply path from the transport, the way `transcribe_wav` isolates the
//! listening path. When a live call sounds slow or silent, this answers in one step
//! whether the problem is the room or the pipeline — and it prints the number the
//! pipeline is built around: whether the caller starts hearing an answer before the
//! model has finished writing one.
//!
//!   ANTHROPIC_API_KEY=... cargo run -p openconv-agent --example answer_aloud -- \
//!     "what did the tests say" /tmp/reply.wav
//!
//! Needs a text-to-speech server reachable at `OPENCONV_TTS_URL` (default
//! `http://127.0.0.1:11000`). The WAV it writes is what the caller would have heard.

use openconv_agent::audio::SAMPLE_RATE;
use openconv_agent::speak::Voicing;
use openconv_agent::clause::Clauses;
use openconv_agent::llm::{Claude, Llm, Piece, Turn};
use openconv_agent::speak::collect;
use openconv_agent::tts::Tts;
use futures_util::StreamExt;
use std::sync::Arc;
use std::time::Instant;

const PROMPT: &str = "You are a voice assistant. Reply in two or three short spoken \
                      sentences. No markdown, no lists.";

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let line = args.next().expect("usage: answer_aloud <what the caller said> [out.wav]");
    let out = args.next().unwrap_or_else(|| "/tmp/answer_aloud.wav".to_owned());

    let llm = Claude::new(
        std::env::var("ANTHROPIC_API_KEY").expect("ANTHROPIC_API_KEY"),
        std::env::var("OPENCONV_LLM_MODEL").unwrap_or_else(|_| "claude-opus-5".to_owned()),
    )
    .await
    .expect("the API describes the configured model");
    let tts = Arc::new(Tts::new(
        std::env::var("OPENCONV_TTS_URL").unwrap_or_else(|_| "http://127.0.0.1:11000".to_owned()),
        std::env::var("OPENCONV_TTS_VOICE").unwrap_or_else(|_| "21m00Tcm4TlvDq8ikWAM".to_owned()),
    ));
    // Every axis reachable, because this is the tool for diagnosing any of them against
    // a real server. Unset means the client asked for nothing, which is what a
    // conversation that overrode nothing sends.
    let voicing = Voicing {
        voice_id: std::env::var("OPENCONV_TTS_VOICE").ok(),
        model_id: std::env::var("OPENCONV_TTS_MODEL").ok(),
        // Parsed through the published union rather than passed as text, so this tool
        // answers the same way the agent does: a code outside the list is a client that
        // has outrun this crate, and it fails here rather than reaching the server as a
        // language nothing speaks. Loudly, because the wav this writes would otherwise
        // be a perfectly fluent recording of the wrong phonemes.
        language: std::env::var("OPENCONV_TTS_LANGUAGE").ok().map(|code| {
            serde_json::from_value(serde_json::Value::String(code.clone())).unwrap_or_else(|_| {
                panic!("OPENCONV_TTS_LANGUAGE={code:?} is not a language this crate publishes")
            })
        }),
    };

    let turns = [Turn::Caller(line.clone())];
    let started = Instant::now();
    // No tools: this example measures how fast the first clause is spoken, and a model
    // that answered by calling something would have nothing to say.
    let mut reply = llm.respond(PROMPT, &turns, &[]);

    let mut clauses = Clauses::new();
    let mut synthesis = Vec::new();
    let mut text = String::new();
    let mut first_clause_at = None;

    while let Some(piece) = reply.next().await {
        let Piece::Say(said) = piece.expect("the model answered") else { continue };
        text.push_str(&said);

        for clause in clauses.push(&said) {
            first_clause_at.get_or_insert_with(|| started.elapsed());
            println!("[{:>6.2}s] clause: {clause}", started.elapsed().as_secs_f32());
            synthesis.push(spawn(&tts, voicing.clone(), clause));
        }
    }
    if let Some(clause) = clauses.flush() {
        first_clause_at.get_or_insert_with(|| started.elapsed());
        println!("[{:>6.2}s] clause: {clause}", started.elapsed().as_secs_f32());
        synthesis.push(spawn(&tts, voicing.clone(), clause));
    }

    let wrote_at = started.elapsed();
    println!("\n[{:>6.2}s] the model finished writing", wrote_at.as_secs_f32());

    // Awaited in dispatch order, the same guarantee `speak` gives the track.
    let mut samples = Vec::new();
    let mut audible_at = None;
    for (index, task) in synthesis.into_iter().enumerate() {
        let clause = task.await.expect("synthesis task").expect("the server answered");
        let at = started.elapsed();
        audible_at.get_or_insert(at);
        println!(
            "[{:>6.2}s] clause {index} synthesized: {:.2}s of audio",
            at.as_secs_f32(),
            clause.len() as f32 / SAMPLE_RATE as f32
        );
        samples.extend(clause);
    }

    let audible_at = audible_at.expect("something was said");
    write_wav(&out, &samples);

    println!("\n{}", "-".repeat(64));
    println!("said: {text}");
    println!(
        "{:.2}s of speech at {SAMPLE_RATE} Hz -> {out}",
        samples.len() as f32 / SAMPLE_RATE as f32
    );
    println!(
        "first clause written at {:.2}s, model finished at {:.2}s, first audio ready at {:.2}s",
        first_clause_at.expect("a clause").as_secs_f32(),
        wrote_at.as_secs_f32(),
        audible_at.as_secs_f32(),
    );

    // The ticket's own bar, checked rather than asserted in a summary.
    match audible_at < wrote_at {
        true => println!(
            "PASS: audio was ready {:.2}s before the model finished writing",
            (wrote_at - audible_at).as_secs_f32()
        ),
        false => println!(
            "SLOWER THAN THE MODEL: audio was ready {:.2}s after it finished writing",
            (audible_at - wrote_at).as_secs_f32()
        ),
    }
}

fn spawn(
    tts: &Arc<Tts>,
    voicing: Voicing,
    clause: String,
) -> tokio::task::JoinHandle<Result<Vec<i16>, openconv_agent::tts::TtsError>> {
    let tts = tts.clone();
    // Gathered rather than queued: this measures the path, it does not drive a track.
    tokio::spawn(async move { collect(tts.synthesize(&voicing, &clause).await?).await })
}

/// A 16-bit mono WAV, so the result can be played rather than described.
fn write_wav(path: &str, samples: &[i16]) {
    let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
    let mut wav = Vec::with_capacity(44 + data.len());

    wav.extend(b"RIFF");
    wav.extend(((36 + data.len()) as u32).to_le_bytes());
    wav.extend(b"WAVEfmt ");
    wav.extend(16u32.to_le_bytes()); // PCM header length
    wav.extend(1u16.to_le_bytes()); // uncompressed
    wav.extend(1u16.to_le_bytes()); // mono
    wav.extend(SAMPLE_RATE.to_le_bytes());
    wav.extend((SAMPLE_RATE * 2).to_le_bytes()); // bytes per second
    wav.extend(2u16.to_le_bytes()); // bytes per frame
    wav.extend(16u16.to_le_bytes()); // bits per sample
    wav.extend(b"data");
    wav.extend((data.len() as u32).to_le_bytes());
    wav.extend(data);

    std::fs::write(path, wav).expect("write the wav");
}
