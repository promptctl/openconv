//! Prints the full error chain from one room-service call.
//!
//! `ServiceError`'s Display collapses to "error sending request for url (...)", which
//! reads identically whether the host is unreachable, the TLS handshake failed, or the
//! process was denied a socket. This walks `source()` so the actual cause is visible.
//!
//!   LIVEKIT_API_KEY=... LIVEKIT_API_SECRET=... cargo run -p openconv-server --example livekit_probe

use livekit_api::services::room::RoomClient;

#[tokio::main]
async fn main() {
    let url = std::env::var("LIVEKIT_URL")
        .unwrap_or_else(|_| "https://livekit.sanctuary.gdn".to_owned());
    let key = std::env::var("LIVEKIT_API_KEY").expect("LIVEKIT_API_KEY");
    let secret = std::env::var("LIVEKIT_API_SECRET").expect("LIVEKIT_API_SECRET");

    match RoomClient::with_api_key(&url, &key, &secret).list_rooms(Vec::new()).await {
        Ok(rooms) => println!("ok: {} room(s)", rooms.len()),
        Err(error) => {
            eprintln!("failed: {error}");
            let mut source = std::error::Error::source(&error);
            while let Some(cause) = source {
                eprintln!("  caused by: {cause}");
                source = cause.source();
            }
            std::process::exit(1);
        }
    }
}
