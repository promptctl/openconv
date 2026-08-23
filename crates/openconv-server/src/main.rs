//! Brings the service up, and refuses to come up at all if anything it needs is
//! missing — a voice call failing at 2am should never be the first sign that a
//! credential was never set.

use openconv_server::api::{router, AppState};
use openconv_server::config::Config;
use openconv_server::livekit::LiveKit;
use openconv_server::store::ConversationLog;
use std::process::ExitCode;
use std::sync::Arc;

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "openconv_server=info,tower_http=info".into()),
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

async fn serve() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::from_env()?;
    let log = ConversationLog::new(&config.conversation_log);

    // Fail here rather than on the first call: an unwritable log means conversations
    // cannot be billed, and finding that out at startup costs nothing.
    log.read_all().await?;

    // Loaded here rather than on the first utterance: a missing or corrupt model must
    // stop the process, not produce a service that answers calls and cannot hear.
    let agents = Arc::new(openconv_agent::Services {
        transcriber: Arc::new(openconv_agent::transcribe::Transcriber::load(
            &config.whisper_model,
        )?),
    });

    let state = AppState::new(&config, LiveKit::new(&config), log, agents);
    let listener = tokio::net::TcpListener::bind(config.bind).await?;

    tracing::info!(
        bind = %config.bind,
        livekit = %config.livekit_url,
        conversation_log = %config.conversation_log.display(),
        "openconv listening"
    );

    axum::serve(listener, router(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("shutting down");
        })
        .await?;

    Ok(())
}
