//! What every handler is given, and the one place it is assembled.
//!
//! Its own module because both halves of the HTTP surface need it — [`crate::api`] to
//! mint and meter, [`crate::web`] to tell the browser client which SFU to dial — and a
//! state type living inside either one would make the other depend upwards on it.

use crate::config::{Config, XiApiKey};
use crate::livekit::LiveKit;
use crate::store::ConversationLog;
use crate::webhook::Webhooks;
use openconv_agent::tts::Tts;
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub livekit: Arc<LiveKit>,
    pub log: Arc<ConversationLog>,
    pub webhooks: Arc<Webhooks>,
    /// Handed to every agent this process starts. Loaded once — see
    /// [`openconv_agent::Services`].
    pub agents: Arc<openconv_agent::Services>,
    /// The very same client [`Self::agents`] speaks through, held at its own type
    /// rather than as the [`Synthesizer`] a conversation narrows it to.
    ///
    /// One value behind two handles, not two clients: they are cloned from one `Arc` in
    /// `main`, so there is nothing here that can come to disagree with what a call
    /// actually sounds like. [LAW:one-source-of-truth] The narrowing is the point — an
    /// agent speaks and needs nothing else, while the browser client has to ask what
    /// this deployment can be asked for, and widening the trait to carry that would put
    /// a method on every test double that no conversation ever calls.
    ///
    /// [`Synthesizer`]: openconv_agent::speak::Synthesizer
    pub tts: Arc<Tts>,
    pub xi_api_key: XiApiKey,
}

impl AppState {
    pub fn new(
        config: &Config,
        livekit: LiveKit,
        log: ConversationLog,
        agents: Arc<openconv_agent::Services>,
        tts: Arc<Tts>,
    ) -> Self {
        Self {
            livekit: Arc::new(livekit),
            log: Arc::new(log),
            agents,
            tts,
            webhooks: Arc::new(Webhooks::new(
                &config.livekit_api_key,
                &config.livekit_api_secret,
            )),
            xi_api_key: config.xi_api_key.clone(),
        }
    }
}
