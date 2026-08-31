# syntax=docker/dockerfile:1.7
#
# The deployable openconv: REST endpoints and the voice agent in one process, the way
# they run from a checkout.
#
# Built in CI, by the self-hosted Gitea act_runner, from a commit the runner fetched
# itself — never from a working tree. See CLAUDE.md for why that is not negotiable.
# .gitea/workflows/publish-image.yaml is the only thing that builds this file; it does so
# for master and for a manual dispatch, and publishes the result as YYYY.MM.DD.N and
# :latest.
#
# It must be built on x86_64 Linux: libwebrtc arrives as a prebuilt multi-gigabyte
# archive and whisper.cpp is compiled from source, so a cross-build under emulation is
# hours where a native build is minutes. The `gpu` node is NOT the builder — it is
# reserved for workloads that need the GPU. The runner host builds this.

# ---------------------------------------------------------------------------
# The model the agent hears with.
#
# Baked in rather than fetched at startup. The weights are the one thing between a
# started container and a container that can hear, and a cold start that downloads them
# is a cold start that fails whenever huggingface is having a bad day — at 2am, on a
# reschedule nobody asked for. Two hundred megabytes in a layer buys a start that
# depends on nothing outside the cluster.
#
# [LAW:one-source-of-truth] The URL and the truncated-download check live in
# fetch-whisper-model.sh and are run from there, not transcribed into a RUN line that
# would drift from it the first time the default model changes.
# ---------------------------------------------------------------------------
FROM debian:bookworm-slim AS model

RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl \
 && rm -rf /var/lib/apt/lists/*

COPY scripts/fetch-whisper-model.sh /fetch-whisper-model.sh
RUN OPENCONV_MODEL_DIR=/models /fetch-whisper-model.sh base.en

# ---------------------------------------------------------------------------
# The binary.
# ---------------------------------------------------------------------------
FROM rust:1.93-bookworm AS build

# cmake and clang are whisper-rs's: it compiles whisper.cpp with the former and
# generates its bindings with the latter, and neither is in the rust image.
RUN apt-get update && apt-get install -y --no-install-recommends \
      cmake clang libclang-dev pkg-config \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY . .

# The cache mounts make a rebuild after a one-line change a matter of seconds rather
# than re-downloading libwebrtc and recompiling whisper.cpp. They are the build host's
# and do not survive into any layer, so the binary is copied out of the mounted target
# directory inside the same RUN that produced it — after the mount is gone there is
# nothing left to copy.
#
# Release is not a preference here: the same sentence takes 121ms to transcribe in a
# release build and 41 seconds in a debug one, which is the difference between a
# conversation and a hang.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    <<'SHELL'
set -eu
cargo build --release -p openconv-server
cp target/release/openconv-server /openconv-server

# The ONNX Runtime the VAD dlopen's rather than links — see the `ort` entry in
# crates/openconv-agent/Cargo.toml for why it is loaded rather than linked. `load-dynamic`
# means ort's build script downloads nothing, so the library has to be fetched here.
#
# [LAW:one-source-of-truth] Which build, from where, and with what checksum are read out
# of the ort-sys crate cargo just resolved, rather than written down again in this file.
# A version pinned here would be a second opinion about which ONNX Runtime the compiled
# bindings expect, and the day it disagreed the failure would be undefined behaviour on
# the first call rather than a build error.
dist="$(find "$CARGO_HOME/registry/src" -path '*/ort-sys-*/dist.txt' | head -n 1)"
if [ -z "$dist" ]; then
  echo "ERROR: no ort-sys dist.txt in the cargo registry — cannot tell which ONNX" >&2
  echo "       Runtime build these bindings were compiled against." >&2
  exit 1
fi

# Only the version is taken from ort — every archive it distributes holds a static
# `libonnxruntime.a` and nothing else, which is the one form no dlopen can use. The
# shared build comes from Microsoft's release of that same version.
version="$(sed -n 's|.*/ms@\([0-9.]*\)/.*|\1|p' "$dist" | head -n 1)"
if [ -z "$version" ]; then
  echo "ERROR: could not read an ONNX Runtime version out of $dist" >&2
  exit 1
fi

release="onnxruntime-linux-x64-$version"
curl --fail --location --silent --show-error --output /tmp/onnxruntime.tgz \
  "https://github.com/microsoft/onnxruntime/releases/download/v$version/$release.tgz"
mkdir -p /tmp/onnxruntime && tar -xzf /tmp/onnxruntime.tgz -C /tmp/onnxruntime

# The real file rather than the two symlinks beside it, and from the directory the
# archive names after its own version — so an archive that turned out to hold some other
# build stops here instead of at a dlopen against mismatched bindings.
library="$(find "/tmp/onnxruntime/$release/lib" -type f -name 'libonnxruntime.so*' | head -n 1)"
if [ -z "$library" ]; then
  echo "ERROR: the ONNX Runtime $version release unpacked without a shared library" >&2
  exit 1
fi
cp "$library" /libonnxruntime.so
SHELL

# ---------------------------------------------------------------------------
# What actually runs.
# ---------------------------------------------------------------------------
FROM debian:bookworm-slim AS runtime

# ca-certificates: every outbound call — Anthropic, the SFU's room service, TTS — goes
# through rustls with native roots, and an image without them fails every one of them as
# an unreachable host.
# libstdc++6 and libgcc-s1: the C++ runtime that whisper.cpp, libwebrtc and ONNX Runtime
# are compiled against, and which the slim base ships neither of. These four are the
# whole of what `ldd` reports for the binary and for libonnxruntime.so.
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates libstdc++6 libgcc-s1 \
 && rm -rf /var/lib/apt/lists/*

COPY --from=model /models/ggml-base.en.bin /opt/openconv/models/ggml-base.en.bin
COPY --from=build /libonnxruntime.so /opt/openconv/lib/libonnxruntime.so
COPY --from=build /openconv-server /usr/local/bin/openconv-server

# The defaults that only make sense inside the image. Everything else — credentials, the
# SFU's address, where TTS lives — is the deployment's to say, and the process refuses
# to start when one of the required ones is missing.
ENV OPENCONV_WHISPER_MODEL=/opt/openconv/models/ggml-base.en.bin \
    OPENCONV_CONVERSATION_LOG=/var/lib/openconv/conversations.jsonl \
    ORT_DYLIB_PATH=/opt/openconv/lib/libonnxruntime.so

# A conversation is the fold of appended `started` and `finished` lines, so this
# directory is the service's only state. Declared as a volume so a container run without
# one still starts; the Nomad job mounts a host volume over it, which is what makes the
# usage record survive a redeploy.
VOLUME /var/lib/openconv

EXPOSE 8080

ENTRYPOINT ["/usr/local/bin/openconv-server"]
