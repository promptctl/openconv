//! Transcribes a 16 kHz mono WAV, so the speech path can be exercised without a room.
//!
//! Isolates the model from the transport. When a live call produces no transcript this
//! answers, in one step, whether the problem is the audio or the agent:
//!
//!   say -o /tmp/s.wav --data-format=LEI16@16000 "hello can you hear me"
//!   cargo run -p openconv-agent --example transcribe_wav -- /tmp/s.wav

use openconv_agent::endpoint::{to_f32, SAMPLE_RATE};
use openconv_agent::transcribe::{Transcript, Transcriber};

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let wav = args.next().expect("usage: transcribe_wav <file.wav> [model.bin]");
    let model = args.next().unwrap_or_else(|| {
        format!("{}/.cache/openconv/models/ggml-base.en.bin", std::env::var("HOME").unwrap())
    });

    let samples = read_wav(&wav);
    println!("{wav}: {:.2}s", samples.len() as f32 / SAMPLE_RATE as f32);

    let loading = std::time::Instant::now();
    let transcriber = Transcriber::load(model.as_ref()).expect("model");
    println!("model ready in {:?}", loading.elapsed());

    // Twice, because the first call through Metal pays a large one-off cost to compile
    // and load the shader library. Reporting only that number would badly misrepresent
    // what a caller actually waits for on every utterance after the first.
    for attempt in 1..=2 {
        let started = std::time::Instant::now();
        let heard = transcriber.transcribe(samples.clone()).await.expect("transcription");
        let took = started.elapsed();
        let realtime = samples.len() as f32 / SAMPLE_RATE as f32 / took.as_secs_f32();

        match heard {
            Transcript::Speech(text) => {
                println!("run {attempt}: {took:?} ({realtime:.1}x realtime): {text}")
            }
            Transcript::Nothing => println!("run {attempt}: {took:?} — heard nothing"),
        }
    }
}

/// Reads the one WAV shape this tool accepts, and refuses anything else rather than
/// reinterpreting its bytes as something they are not.
///
/// Both chunks are located by walking the file rather than read from fixed offsets.
/// A WAV is a chunk list and writers put what they like in it — `say` emits a `LIST`
/// ahead of the audio — so fixed offsets read metadata as a sample rate, and metadata
/// as a burst of noise at the front of the recording.
fn read_wav(path: &str) -> Vec<f32> {
    let bytes = std::fs::read(path).expect("read wav");
    assert_eq!(&bytes[0..4], b"RIFF", "{path} is not a RIFF file");
    assert_eq!(&bytes[8..12], b"WAVE", "{path} is not a WAVE file");

    let mut format = None;
    let mut offset = 12;

    while offset + 8 <= bytes.len() {
        let id = &bytes[offset..offset + 4];
        let size = u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap()) as usize;
        let body = offset + 8;

        match id {
            b"fmt " => {
                let channels = u16::from_le_bytes(bytes[body + 2..body + 4].try_into().unwrap());
                let rate = u32::from_le_bytes(bytes[body + 4..body + 8].try_into().unwrap());
                let bits = u16::from_le_bytes(bytes[body + 14..body + 16].try_into().unwrap());
                format = Some((channels, rate, bits));
            }
            b"data" => {
                let (channels, rate, bits) = format.expect("fmt chunk before data");
                assert_eq!(rate, SAMPLE_RATE, "need {SAMPLE_RATE} Hz, got {rate}");
                assert_eq!(channels, 1, "need mono, got {channels} channels");
                assert_eq!(bits, 16, "need 16-bit samples, got {bits}");

                let pcm: Vec<i16> = bytes[body..(body + size).min(bytes.len())]
                    .chunks_exact(2)
                    .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
                    .collect();
                return to_f32(&pcm);
            }
            _ => {}
        }
        // Chunks are word-aligned; an odd size is followed by a pad byte.
        offset = body + size + (size % 2);
    }
    panic!("no data chunk in {path}");
}
