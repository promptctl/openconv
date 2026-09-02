# syntax=docker/dockerfile:1.7
#
# The deployable openconv: REST endpoints and the voice agent in one process, the way
# they run from a checkout.
#
# Built in CI, by the self-hosted Gitea act_runner, from a commit the runner fetched
# itself — never from a working tree. See CLAUDE.md for why that is not negotiable.
# .gitea/workflows/publish-image.yaml is the only thing that builds this file, and it owns
# which refs get published and how they are tagged.
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
      cmake clang libclang-dev pkg-config curl ca-certificates gnupg \
 && rm -rf /var/lib/apt/lists/*

# CUDA, because crates/openconv-agent/Cargo.toml compiles whisper-rs with the `cuda`
# feature on Linux and that feature builds whisper.cpp with nvcc. No GPU is needed to
# *compile* CUDA, which is what lets the runner — which has no card — build this.
#
# Toolkit only, not the driver: the driver belongs to the host, and the container is
# handed it at runtime by the nvidia container runtime. Installing one here would be a
# second copy of a thing the host already owns, at the version this file happened to
# name. [LAW:one-source-of-truth]
#
# 12.8 matches the deployment's driver (570.195.03, "CUDA Version: 12.8"). CUDA's minor
# version compatibility means a 12.x runtime works against any 12.x-capable driver, so
# this tracks the card rather than pinning to it exactly.
ARG CUDA_RELEASE=12-8
RUN <<'SHELL'
set -eu
curl --fail --location --silent --show-error \
  --output /tmp/cuda-keyring.deb \
  https://developer.download.nvidia.com/compute/cuda/repos/debian12/x86_64/cuda-keyring_1.1-1_all.deb
dpkg -i /tmp/cuda-keyring.deb
rm /tmp/cuda-keyring.deb
apt-get update
# nvcc to compile it, cudart/cublas headers and import libraries to link against. The
# matching runtime .so files are copied into the final stage below.
apt-get install -y --no-install-recommends \
  "cuda-nvcc-${CUDA_RELEASE}" \
  "cuda-cudart-dev-${CUDA_RELEASE}" \
  "libcublas-dev-${CUDA_RELEASE}"
rm -rf /var/lib/apt/lists/*
SHELL

ENV PATH=/usr/local/cuda/bin:$PATH \
    CUDA_PATH=/usr/local/cuda
# Build device code for exactly the card this deploys to — an RTX 2070, Turing, compute
# capability 7.5. The default is every architecture nvcc knows, which is many minutes of
# compilation and a fat binary, all but one slice of it for cards this cluster does not
# have. A different card means changing this number and rebuilding; a wrong number fails
# loudly at load rather than silently on the CPU, which the transcriber now refuses.
ENV CUDAARCHS=75

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

# The CUDA libraries the binary now links against, gathered for the runtime stage. Only
# these three: whisper.cpp's CUDA backend calls into the runtime and cuBLAS, and the rest
# of the toolkit is compiler and headers that nothing at runtime reads. libcuda.so itself
# is deliberately NOT here — that one is the driver, and the nvidia container runtime
# injects the host's copy. Shipping ours would override the host's and mismatch the kernel
# module.
#
# Resolved through the symlinks with `readlink -f`, matching what is done for ONNX
# Runtime above, and named without their minor version so the copy does not have to be
# re-pinned every toolkit bump.
mkdir -p /cuda-runtime
for soname in libcudart.so.12 libcublas.so.12 libcublasLt.so.12; do
  found="$(readlink -f "/usr/local/cuda/lib64/$soname" || true)"
  if [ -z "$found" ] || [ ! -f "$found" ]; then
    echo "ERROR: $soname is not in the CUDA toolkit this stage installed." >&2
    echo "       The runtime stage would build fine and the binary would fail to" >&2
    echo "       start, so this stops here instead." >&2
    exit 1
  fi
  cp "$found" "/cuda-runtime/$soname"
done
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

# Into the loader's own search path rather than behind LD_LIBRARY_PATH: these are linked,
# not dlopen'd, so the binary names them in DT_NEEDED and the loader has to find them
# before main runs. An environment variable is something a `docker run --env` or an
# entrypoint wrapper can drop, and dropping it fails the container at exec with a message
# about a shared object rather than about openconv. [LAW:single-enforcer]
COPY --from=build /cuda-runtime/ /usr/local/lib/
RUN ldconfig

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
