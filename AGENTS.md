# AGENTS

## Never build an image from a working tree

Build from a commit, in CI, on a machine that fetched that commit itself. Anything else is banned and gets deleted on sight — no discussion, no exceptions.

Banned: copying, tarring, rsyncing or scp-ing this source tree onto a build host; building an image from anything the builder did not pull from git itself; any build that needs a laptop, a checkout, or an SSH session to run; build scratch directories left on the homelab VMs.

Why: an image built from a working tree has no known source. It contains whatever was on someone's disk, records no commit, and cannot be reproduced, bisected, or audited.

Instead: build in CI from a commit, tag `YYYY.MM.DD.N`, push to the homelab registry, then file that tag into the homelab's `service-versions.auto.tfvars.json` so Atlantis deploys it.

The only builder is the homelab's self-hosted Gitea `act_runner`. The `gpu` node is not a build host — it is reserved for workloads that need the GPU.

See CLAUDE.md in this repo for the full rule — why that runner is the only executor, how CI is triggered, and what the workflow does today.

## Never guard against an absent LiveKit stats field

`scripts/lib/caller.mjs` reads every number out of `getRtcStats().toJson()` bare — no presence check, no `?? 0`, no null-carrying parse. That is correct, and it must stay that way.

Banned: adding a presence check, a `?? 0` default, or an absence-carrying return type to any of those fields on the grounds that a protobuf scalar disappears at its default value.

Why: `stats.proto` is `syntax proto2`, and the fields in question are declared `required` — explicit presence. protobuf-es either writes such a field, including when its value is `0`, or throws `required field not set`. Only *optional* fields vanish at their default, and this schema has none of the kind being worried about. A `?? 0` would be worse than dead code: a zero jitter beside a zero level is the exact shape of a flawless call, which is the one reading this parser exists to tell apart from an unmeasured one.

The automated reviewer filed this as a NaN/undefined crash three times in a single review of PR #9. It was wrong all three times. The test `counters that are legitimately zero render as zero, never NaN or undefined` pins the invariant — cite it, cite the proto2 `required` declaration, and resolve the thread rather than re-deriving the argument.

<!-- BEGIN LIT INTEGRATION -->
## lit Agent-Native Workflow

This repository uses `lit` for agent-native issue tracking.

Start by running `lit quickstart` to load the workflow instructions. It prints how tickets are found, created, updated, and closed here, so running it first means the rest of your work follows the conventions this repo expects. It's a quick, read-only command — no need to check in before running it.

<!-- END LIT INTEGRATION -->
