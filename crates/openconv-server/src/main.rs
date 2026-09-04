//! Brings the service up, and refuses to come up at all if anything it needs is
//! missing — a voice call failing at 2am should never be the first sign that a
//! credential was never set.

use openconv_server::app::router;
use openconv_server::config::Config;
use openconv_server::livekit::LiveKit;
use openconv_server::state::AppState;
use openconv_server::store::ConversationLog;
use std::process::ExitCode;
use std::sync::Arc;

/// What the service says about itself when nothing sets `RUST_LOG`.
///
/// `openconv_agent` is on the list because it is where the conversation actually happens:
/// leaving it off meant the deployed service logged its HTTP endpoints and nothing about
/// any call served through them, and every question about a call — did the agent hear it,
/// did it answer, did the answer reach the room — had to be re-run locally with a filter
/// set by hand.
///
/// `libwebrtc` and `livekit` at warn for the same reason one level down. The transport
/// drops the caller's audio when a sink queue overflows and says so through the `log`
/// crate; filtered out, the only remaining evidence was a transcript that came back short,
/// which reads exactly like a speech model getting a word wrong. It was read that way for
/// days.
///
// [LAW:no-silent-failure] the alarms exist; the default filter is what switches them on.
const DEFAULT_LOG: &str = "openconv_server=info,openconv_agent=info,libwebrtc=warn,livekit=warn,tower_http=info";

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| DEFAULT_LOG.into()),
        )
        .init();

    match serve().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!("{error}");
            ExitCode::FAILURE
        }
    }
}

/// What the agent is when the client does not say.
///
/// Deliberately thin. Happy sends its own prompt in
/// `conversation_initiation_client_data`, and that override *replaces* this rather than
/// extending it — so anything substantive written here would be dead text in the only
/// deployment that matters, while still shaping every conversation started by a client
/// that forgot to configure one. What it does say is the part that is true of any voice
/// agent: replies are spoken aloud, so they have to be short.
const DEFAULT_PROMPT: &str = "\
You are a voice assistant. Your replies are spoken aloud, so keep them to one or two \
short sentences and use plain words that are easy to hear. Do not use markdown, lists, \
or formatting of any kind. If you do not understand what was said, ask for it again \
rather than guessing.";

async fn serve() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::from_env()?;
    let log = ConversationLog::new(&config.conversation_log);

    // Fail here rather than on the first call: an unwritable log means conversations
    // cannot be billed, and finding that out at startup costs nothing.
    log.read_all().await?;

    // Built before the bundle so that both the agents that speak through it and the
    // route that lists what it can speak in hold the same client, rather than two built
    // from the same string. [LAW:one-source-of-truth]
    let tts = Arc::new(openconv_agent::tts::Tts::new(
        config.tts_url.clone(),
        config.tts_voice.clone(),
    ));

    // Loaded here rather than on the first utterance: a missing or corrupt model must
    // stop the process, not produce a service that answers calls and cannot hear.
    let agents = Arc::new(openconv_agent::Services {
        transcriber: Arc::new(openconv_agent::transcribe::Transcriber::load(
            &config.whisper_model,
        )?),
        llm: Arc::new(
            openconv_agent::llm::Claude::new(
                config.anthropic_api_key.clone(),
                config.llm_model.clone(),
            )
            .await?,
        ),
        tts: tts.clone(),
        default_prompt: DEFAULT_PROMPT.into(),
    });

    let state = AppState::new(&config, LiveKit::new(&config), log, agents, tts);
    let listener = tokio::net::TcpListener::bind(config.bind).await?;

    tracing::info!(
        bind = %config.bind,
        livekit = %config.livekit_url,
        conversation_log = %config.conversation_log.display(),
        "openconv listening"
    );

    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown_requested())
        .await?;

    Ok(())
}

/// Resolves when the process has been asked to stop, by either of the signals that mean
/// it.
///
/// SIGINT is a developer's ctrl-c. SIGTERM is what every container runtime and service
/// manager sends first, giving a grace period before SIGKILL — so a process that listens
/// for only the first is killed outright on every redeploy, part-way through appending
/// to the conversation log that decides what a call cost.
async fn shutdown_requested() {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("the process cannot register a SIGTERM handler");

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = terminate.recv() => {}
    }

    tracing::info!("shutting down");
}
