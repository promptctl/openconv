# AGENTS

## Never build an image from a working tree

Build from a commit, in CI, on a machine that fetched that commit itself. Anything else is banned and gets deleted on sight — no discussion, no exceptions.

Banned: copying, tarring, rsyncing or scp-ing this source tree onto a build host; building an image from anything the builder did not pull from git itself; any build that needs a laptop, a checkout, or an SSH session to run; build scratch directories left on the homelab VMs.

Why: an image built from a working tree has no known source. It contains whatever was on someone's disk, records no commit, and cannot be reproduced, bisected, or audited.

Instead: build in CI from a commit, tag `YYYY.MM.DD.N`, push to the homelab registry, then file that tag into the homelab's `service-versions.auto.tfvars.json` so Atlantis deploys it.

See CLAUDE.md in this repo for the full rule.

<!-- BEGIN LIT INTEGRATION -->
## lit Agent-Native Workflow

This repository uses `lit` for agent-native issue tracking.

Start by running `lit quickstart` to load the workflow instructions. It prints how tickets are found, created, updated, and closed here, so running it first means the rest of your work follows the conventions this repo expects. It's a quick, read-only command — no need to check in before running it.

<!-- END LIT INTEGRATION -->
