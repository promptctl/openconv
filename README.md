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
| `POST /livekit/webhook` | Not part of the ElevenLabs surface. LiveKit posts room lifecycle events here; `room_finished` is what gives a conversation its duration. |

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

`crates/openconv-server` serves the REST endpoints. The room name is the piece worth
knowing about: it is not derived from the conversation ID, it *is* the conversation
ID, because both consumers recover that ID by running `(conv_[a-zA-Z0-9]+)` over a
longer string and neither raises an error when the pattern does not match. A
`ConversationId` can only be built by generating one or by parsing one, so a room
whose name breaks that regex cannot be named.

```
LIVEKIT_API_KEY=... LIVEKIT_API_SECRET=... OPENCONV_API_KEY=... cargo run -p openconv-server
```

`LIVEKIT_URL`, `OPENCONV_BIND`, and `OPENCONV_CONVERSATION_LOG` have defaults; the
three above do not, and the process refuses to start without them.

It also serves `POST /livekit/webhook`, which is how conversations get their durations.
The end of a call is observed rather than reported — the SFU sees the room close even
when the agent crashed, and a conversation with no end reads to Happy as free usage.
That makes the conversation log an event log: `started` and `finished` are two appended
lines, and a conversation is the fold of them, so nothing is ever rewritten in place.

Two acceptance scripts check a running instance against what its callers actually do,
rather than against what this README claims:

```
OPENCONV_API_KEY=... LIVEKIT_API_KEY=... LIVEKIT_API_SECRET=... \
  node scripts/token-endpoint-acceptance.mjs  http://127.0.0.1:8080
  node scripts/conversations-acceptance.mjs   http://127.0.0.1:8080
```

They need a real LiveKit deployment because parts of the contract cannot be observed
without one — a rejected signature, a room that was never created, and a build with no
TLS backend compiled in all look identical to a passing unit test.

`crates/openconv-agent` is the participant on the other side of the room. It runs in
the same process as the endpoints, one task per conversation, spawned when the token is
minted — LiveKit's explicit dispatch is not an option, because it targets a worker
registered under an `agent_name` and that worker framework is Python and Node only. The
seam stays narrow anyway: an agent is a function of a URL, a token, and a conversation
id, so moving agents into their own process later changes how they receive those three
things and nothing else.

The ordering rule in `control.rs` is the part to read first. The ElevenLabs client
resolves its connect promise from a `{once: true}` listener on the first data message,
so if `conversation_initiation_metadata` is not first, `startSession()` never resolves —
no error, no timeout, just a user waiting in a room. That is why `announce()` consumes
the unannounced room and is the only way to obtain something that can publish.

Note for anyone building this on macOS: `.cargo/config.toml` passes `-ObjC`, and it is
load-bearing. libwebrtc implements part of itself as Objective-C categories that the
linker otherwise drops, and the process aborts the first time an agent joins a room.

Two further caveats before trusting any of this in production: openconv accepts
`room_finished` deliveries, but the LiveKit deployment is not yet configured to send
them. That is `webhook.urls` in `jobs/livekit.nomad.hcl` over in `home-infra`, and it
needs a reachable openconv to point at. Until it is set, every conversation reads as
in-progress and is billed for elapsed time capped at six hours.

The agent listens. It joins, announces, plays a tone so the track can be proven end to
end, and transcribes what the caller says into `user_transcript` events. It has nothing
to *say* yet — that arrives with the LLM and TTS tickets.

Hearing needs a model, which lives outside the repository:

```
scripts/fetch-whisper-model.sh          # ~/.cache/openconv/models/ggml-base.en.bin
```

**Run the agent in release.** This is not a preference. The same sentence takes 121 ms
to transcribe in a release build and 41 seconds in a debug one, because whisper.cpp
without optimisation is three hundred times slower than with it — the difference between
a conversation and a hang. The model also warms itself up at startup rather than lazily:
the first call through Metal compiles a shader library, and unpaid it lands on the first
thing the first caller ever says.

```
OPENCONV_API_KEY=... node scripts/agent-acceptance.mjs http://127.0.0.1:8080 wss://livekit.sanctuary.gdn
```

That one needs `npm install @livekit/rtc-node`. It joins a real room as the app would
and asserts what the app depends on: the agent is a connected participant, its first
control event is the announcement, a `vad_score` follows, and the published track
carries audible samples rather than silence.

```
OPENCONV_API_KEY=... node scripts/stt-acceptance.mjs http://127.0.0.1:8080 wss://livekit.sanctuary.gdn
```

That one speaks. It renders a sentence with the macOS `say` voice, publishes it as a
microphone in real time, and checks the words come back as a `user_transcript`. Real
synthesized speech over a real track rather than a recorded fixture, because the
resampling and the endpointing are exactly the parts a fixture would skip.

To try the model on its own, without a room — the fastest way to tell a speech problem
from a transport one:

```
say -o /tmp/s.wav --data-format=LEI16@16000 "hello can you hear me"
cargo run --release -p openconv-agent --example transcribe_wav -- /tmp/s.wav
```

## Where it runs

LiveKit is deployed in the homelab at `wss://livekit.sanctuary.gdn`, reachable over
Tailscale only. Both Happy clients dial that hostname directly — native passes it as
`serverUrl`, web as `livekitUrl` — so it is this project's published API and should
not change casually.

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
