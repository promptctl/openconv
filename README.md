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

## Where it runs

LiveKit is deployed in the homelab at `wss://livekit.sanctuary.gdn`, reachable over
Tailscale only. Both Happy clients dial that hostname directly — native passes it as
`serverUrl`, web as `livekitUrl` — so it is this project's published API and should
not change casually.

That hostname has no TLS certificate yet, so it fails the handshake today: Caddy's
Cloudflare API token is pinned to a WAN address the ISP has since changed, and the
DNS-01 challenge cannot write its record. Tracked as `home-misc-74p` in the homelab
repo. Until it clears, reach the SFU at `http://192.168.7.208:7880` over Tailscale —
same server, no Caddy in front.

Signaling goes through Caddy. Media does not, because WebRTC cannot cross an HTTP
reverse proxy: the SFU advertises the runner VM's own address and clients dial it
directly on 7881/tcp and 7882/udp, which the tailnet's `192.168.7.0/24` subnet route
makes reachable from every device.

The API key and secret live in Vault at `secret/livekit` — one path, read by both the
SFU that verifies room JWTs and the token endpoint that signs them. `room.auto_create`
is off, so a room exists only once `GET /v1/convai/conversation/token` has created it
and dispatched the agent; a client that skips that path fails to join rather than
landing in a room with nobody in it.

The job spec, firewall entries, and Vault scaffolding are in `~/code/home-infra`
(`jobs/livekit.nomad.hcl`). To check that the deployment is up and still accepts
these credentials:

```
LIVEKIT_API_KEY=... LIVEKIT_API_SECRET=... node scripts/livekit-smoke.mjs
```

It mints a `roomList` token and calls `ListRooms`, so a failure tells you whether
the SFU rejected the signature or was never reachable — two things that look the
same from inside the app.

## Backlog

Tracked in `lit` in this repo — twelve tickets under the `openconv` epic, ranked in
build order. `lit backlog` to see them.

## Background

The full replacement spec, including the parts already ruled out, lives in
`~/code/brandon-fryslie_happy/docs/plans/open-source-voice-replacement.md`.
