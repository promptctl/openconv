//! What every handler is given, and the one place it is assembled.
//!
//! Its own module because both halves of the HTTP surface need it — [`crate::api`] to
//! mint and meter, [`crate::web`] to tell the browser client which SFU to dial — and a
//! state type living inside either one would make the other depend upwards on it.

use crate::config::{Config, XiApiKey};
use crate::livekit::LiveKit;
use crate::store::ConversationLog;
use crate::webhook::Webhooks;
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub livekit: Arc<LiveKit>,
    pub log: Arc<ConversationLog>,
    pub webhooks: Arc<Webhooks>,
    /// Handed to every agent this process starts. Loaded once — see
    /// [`openconv_agent::Services`].
    pub agents: Arc<openconv_agent::Services>,
    pub xi_api_key: XiApiKey,
}

impl AppState {
    pub fn new(
        config: &Config,
        livekit: LiveKit,
        log: ConversationLog,
        agents: Arc<openconv_agent::Services>,
    ) -> Self {
        Self {
            livekit: Arc::new(livekit),
            log: Arc::new(log),
            agents,
            webhooks: Arc::new(Webhooks::new(
                &config.livekit_api_key,
                &config.livekit_api_secret,
            )),
            xi_api_key: config.xi_api_key.clone(),
        }
    }
}
