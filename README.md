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

- Which STT engine, and whether it holds real-time on CPU. `whisper-rs` is the
  Rust-native candidate.
- Whether the agent runs as one process per room or one process serving many, and
  how it gets dispatched into a room the token endpoint just created.
- How much of LiveKit's Python pipeline behaviour — barge-in, endpointing, TTS
  chunking — has to be rebuilt before conversations feel natural rather than
  walkie-talkie.

The control protocol is not among these. It ships as generated TypeScript in the
`@elevenlabs/types` package, so the message shapes are read, not guessed.

## Layout

`crates/openconv-protocol` holds the control-message types — two serde enums,
`ServerEvent` and `ClientEvent`, covering every message the SDK can send or receive
over the LiveKit data channel. Its tests pin each message to the JSON shape published
in `@elevenlabs/types`, because a wrong field name is otherwise invisible: it
round-trips perfectly and the client quietly ignores the message.

Those fixtures are transcribed by hand, so `scripts/check-against-published-types.mjs`
checks the transcription against the TypeScript itself:

```
node scripts/check-against-published-types.mjs \
  ~/code/brandon-fryslie_happy/node_modules/@elevenlabs/types/generated/types/asyncapi-types.ts
```

It lives outside `cargo test` because it needs the npm package, which CI does not
have. Run it whenever that package moves.

## Backlog

Tracked in `lit` in this repo — twelve tickets under the `openconv` epic, ranked in
build order. `lit backlog` to see them.

## Background

The full replacement spec, including the parts already ruled out, lives in
`~/code/brandon-fryslie_happy/docs/plans/open-source-voice-replacement.md`.
