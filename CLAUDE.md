## Never build an image from a working tree

This repo's image is deployed in the homelab, so this rule is not optional here.

Build from a commit, in CI, on a machine that fetched that commit itself. Anything else is banned and gets deleted on sight — no discussion, no exceptions.

Banned: copying, tarring, rsyncing or scp-ing this source tree onto a build host; building an image from anything the builder did not pull from git itself; any build that needs a laptop, a checkout, or an SSH session to run; build scratch directories left on the VMs.

`scripts/build-image.sh` is exactly this and is being replaced — it tars the working tree, including untracked files, and ships it over SSH to the gpu node. Do not copy its shape into another repo, and do not reach for it as a shortcut. Its scratch directory on the gpu node has been deleted; do not recreate one.

Three things in it are worth keeping when it is replaced: the tag counter derived from what the registry already published, the manifest check after the push, and the `:latest` alias that break-glass `nomad job run` depends on.

Why: an image built from a working tree has no known source. It contains whatever was on someone's disk and records no commit. `openconv:2026.08.24.2` is deployed right now and nobody can say what built it.

Instead: build in CI from a commit, tag `YYYY.MM.DD.N`, push to the registry, then let the `update-service-version` action file the tag into the homelab's `service-versions.auto.tfvars.json` so Atlantis deploys it.

<!-- BEGIN LIT INTEGRATION -->
## lit Agent-Native Workflow

This repository uses `lit` for agent-native issue tracking.

Start by running `lit quickstart` to load the workflow instructions. It prints how tickets are found, created, updated, and closed here, so running it first means the rest of your work follows the conventions this repo expects. It's a quick, read-only command — no need to check in before running it.

<!-- END LIT INTEGRATION -->
