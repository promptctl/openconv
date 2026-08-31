#!/usr/bin/env bash
#
# DISABLED. This script publishes an image built from a working tree.
#
# `git ls-files --cached --others` below tars the tree in whatever state it is on disk,
# untracked files included, and streams it over SSH to the gpu node to build there. The
# image that comes out records no commit, so it cannot be reproduced, bisected, rolled
# back to a known good, or audited — and nothing turns red when that happens. The build
# succeeds, the deploy succeeds, and the gap surfaces months later when someone asks what
# is running. `openconv:2026.08.24.2` was published this way and nothing can say what
# source produced it.
#
# Kept rather than deleted because its replacement has to be written against it. Three
# behaviours below are worth carrying over, and openconv-deploy-690.3 names them: the tag
# counter derived from what the registry has already published, the manifest check after
# the push, and the `:latest` alias a break-glass `nomad job run` lands on. Read it — the
# thing that must not happen is running it.
#
# The replacement is the openconv-deploy-690 epic: CI builds from a commit it fetched
# itself, stamps that commit into the image, and files the tag for Atlantis to deploy.
# See CLAUDE.md, "Never build an image from a working tree".

# [LAW:no-silent-failure] Refuse loudly, at the top, before any line can reach the network
# or the build host. [LAW:no-mode-explosion] No flag and no environment variable turns
# this off — an escape hatch is what gets reached for at 2am, which is precisely when the
# untraceable image gets published.
cat >&2 <<'REFUSED'
ERROR: scripts/build-image.sh is disabled and will not run.

It publishes an image built from your working tree. That image records no commit and
cannot be reproduced, rolled back, or audited — see CLAUDE.md, "Never build an image
from a working tree".

Build from a commit in CI instead; the openconv-deploy-690 epic tracks that work. This
file is kept only as the reference its replacement is written against.
REFUSED
exit 1

# ===========================================================================
# REFERENCE ONLY — nothing below this line executes.
#
# The original header, kept because it documents the interface the replacement owes its
# callers:
#
#   Builds the openconv image and publishes it to the homelab registry.
#
#     scripts/build-image.sh [tag]
#
#   Prints the published tag on stdout and nothing else, so a caller can read it:
#
#     tag=$(scripts/build-image.sh)
#
#   Everything a human wants to watch goes to stderr.
#
#   The build runs on a homelab node rather than here. The image is x86_64 Linux, this
#   machine is arm64 macOS, and the two heavy parts — a prebuilt multi-gigabyte libwebrtc
#   and whisper.cpp compiled from source — turn an emulated cross-build into hours. The
#   node is reached through `ops` because it holds the only key that opens it.
# ===========================================================================
set -euo pipefail

registry="${OPENCONV_REGISTRY:-192.168.7.208:5000}"
build_host="${OPENCONV_BUILD_HOST:-deploy@192.168.7.218}"
build_jump="${OPENCONV_BUILD_JUMP:-ops}"
build_dir="${OPENCONV_BUILD_DIR:-/home/deploy/openconv-build}"
image="openconv"

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# Runs a command on the build node. Its stdin is the caller's, which is what lets the
# source archive be streamed straight through both hops into `tar -x`.
on_build_host() {
  ssh "$build_jump" "ssh $build_host $(printf '%q' "$*")"
}

# ---------------------------------------------------------------------------
# The tag.
#
# `YYYY.MM.DD.N` is the homelab's format (home-infra docs/service-versions.md):
# lexicographically sortable, and the date says when a bad version was cut. N is derived
# from what is already published rather than from a counter kept somewhere, so two
# builds on the same day cannot collide by forgetting to bump a file.
# ---------------------------------------------------------------------------
resolve_tag() {
  local today published
  today="$(date +%Y.%m.%d)"

  # A missing repository is the first build, which is fine. Anything else — the registry
  # down, DNS gone, Tailscale off — must not read as "first build" and quietly restart
  # the numbering over tags that already exist.
  published="$(curl -sk --fail-with-body "https://registry.sanctuary.gdn/v2/$image/tags/list" || true)"
  if [ -z "$published" ]; then
    echo "ERROR: registry did not answer at https://registry.sanctuary.gdn — is Tailscale up?" >&2
    exit 1
  fi
  case "$published" in
    *NAME_UNKNOWN*) published="" ;;
    *'"tags"'*) ;;
    *)
      echo "ERROR: unexpected answer from the registry: $published" >&2
      exit 1
      ;;
  esac

  local highest
  highest="$(printf '%s' "$published" \
    | tr ',' '\n' \
    | grep -o "$today\.[0-9]\+" \
    | sed "s/^$today\.//" \
    | sort -n \
    | tail -n 1)"

  echo "$today.$(( ${highest:-0} + 1 ))"
}

tag="${1:-$(resolve_tag)}"
target="$registry/$image:$tag"

echo "building $target on $build_host" >&2

# ---------------------------------------------------------------------------
# The source.
#
# [LAW:one-source-of-truth] The file list comes from git, so the one place that says
# what is not part of this project stays .gitignore. `--others --exclude-standard` keeps
# a not-yet-committed Dockerfile buildable, which is the whole point during the change
# that introduces one.
# ---------------------------------------------------------------------------
on_build_host "rm -rf $build_dir && mkdir -p $build_dir"

git -C "$repo" ls-files --cached --others --exclude-standard -z \
  | tar -czf - -C "$repo" --null --files-from - \
  | on_build_host "tar -xzf - -C $build_dir"

# ---------------------------------------------------------------------------
# The build, and the publish.
#
# Tagged `latest` alongside the dated tag so a break-glass `nomad job run` with no
# `-var` — the default in every version-managed jobspec — lands on this build.
# ---------------------------------------------------------------------------
on_build_host "cd $build_dir && docker build --tag $target --tag $registry/$image:latest ." >&2
on_build_host "docker push $target" >&2
on_build_host "docker push $registry/$image:latest" >&2

# A push that reported success and left nothing behind is a deploy that fails minutes
# later on an image pull, far from here.
if ! curl -sk --fail "https://registry.sanctuary.gdn/v2/$image/manifests/$tag" \
     -H 'Accept: application/vnd.docker.distribution.manifest.v2+json' >/dev/null; then
  echo "ERROR: $target is not in the registry after a push that claimed to succeed" >&2
  exit 1
fi

echo "published $target" >&2
echo "$tag"
