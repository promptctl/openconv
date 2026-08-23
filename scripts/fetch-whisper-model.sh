#!/usr/bin/env bash
# Fetches the speech-to-text model the agent hears with.
#
#   scripts/fetch-whisper-model.sh [model]
#
# Default is base.en, which transcribes at roughly 30x realtime on an M2 with Metal —
# fast enough that the caller is never waiting on it. tiny.en is quicker and noticeably
# worse; small.en is better and around three times slower.
#
# The weights live under the user's cache rather than in the repository, because they
# are a hundred-odd megabytes that no commit should carry, and rather than a temporary
# directory, because they should survive a reboot and be fetched once.
set -euo pipefail

model="${1:-base.en}"
dir="${OPENCONV_MODEL_DIR:-$HOME/.cache/openconv/models}"
target="$dir/ggml-$model.bin"
url="https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-$model.bin"

if [ -s "$target" ]; then
  echo "already have $target"
  exit 0
fi

mkdir -p "$dir"

# Downloaded to a temporary name and moved into place only once complete. An interrupted
# download left at the real path is worse than no download at all: it is a file that
# exists, passes every "is it there" check, and fails to load at startup.
tmp="$target.partial"
trap 'rm -f "$tmp"' EXIT

echo "fetching $model from huggingface..."
curl --fail --location --progress-bar --output "$tmp" "$url"

# A truncated or error-page download is smaller than any real model. Catch it here
# rather than as an unreadable-tensor panic on the first call.
size=$(wc -c < "$tmp" | tr -d ' ')
if [ "$size" -lt 10000000 ]; then
  echo "ERROR: downloaded only $size bytes from $url — that is not a model" >&2
  exit 1
fi

mv "$tmp" "$target"
trap - EXIT
echo "$target ($(du -h "$target" | cut -f1))"
echo
echo "openconv finds this path by default; override with OPENCONV_WHISPER_MODEL."
