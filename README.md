# openconv

An ElevenLabs Conversational AI-compatible server. Happy points at it instead of
`api.elevenlabs.io` and gets the same voice agent, self-hosted.

## What it has to do

Happy's server mints a conversation token, hands it to the ElevenLabs SDK running
in the app, and the SDK joins a room to talk to an agent. openconv replaces the far
side of that — two REST endpoints and the agent itself.

| Endpoint | Purpose |
|---|---|
| `GET /v1/convai/conversation/token` | Create a room, dispatch the agent into it, return a LiveKit JWT. The room name must contain a `conv_<id>`; Happy's server pulls that ID out with a regex. |
| `GET /v1/convai/conversations` | Past conversations with `call_duration_secs`, which Happy sums for usage gating. |

The agent joins each room and runs the turn loop: VAD, speech-to-text, LLM,
text-to-speech. The LLM can call two tools that execute back in the app —
`sendMessageToSession` and `processPermissionRequest`.

## Decisions already made

**Transport is LiveKit WebRTC, on web as well as native.** The ElevenLabs client
SDK picks its transport with `connectionType ?? (conversationToken ? "webrtc" :
"websocket")`, and both of Happy's paths supply a token. Its proprietary WebSocket
signaling protocol never comes into play, so there is nothing to reverse-engineer.

**Text-to-speech comes from elvenreader-server** (`~/code/elvenread/server`), which
already serves the ElevenLabs `/v1/text-to-speech` surface. The agent fetches speech
from it over HTTP, so no TTS engine is needed here and voices match the reader.

**Rust**, matching elvenreader-server: axum for the REST endpoints, `livekit-api`
to mint tokens, `livekit` for the agent's room participation. The cost is that
LiveKit's pipeline framework is Python-only, so turn detection, interruption
handling, and streaming TTS chunking are ours to write.

## Open questions

- Which VAD, STT, and LLM. Silero via ONNX and whisper.cpp via `whisper-rs` are the
  obvious Rust-native candidates; the LLM is an HTTP call either way.
- How VAD scores and speaking/listening mode changes reach the client. The SDK
  exposes `onVadScore` and `onModeChange`, so the agent has to emit both over a
  LiveKit data channel in whatever shape the SDK's decoder expects — needs capture
  from a live ElevenLabs session.
- The exact message shape of a client tool call, same capture.

## Background

The full replacement spec, including the parts already ruled out, lives in
`~/code/brandon-fryslie_happy/docs/plans/open-source-voice-replacement.md`.
