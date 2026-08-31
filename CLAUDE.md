## Never build an image from a working tree

This repo's image is deployed in the homelab, so this rule is not optional here.

Build from a commit, in CI, on a machine that fetched that commit itself. Anything else is banned and gets deleted on sight — no discussion, no exceptions.

Banned: copying, tarring, rsyncing or scp-ing this source tree onto a build host; building an image from anything the builder did not pull from git itself; any build that needs a laptop, a checkout, or an SSH session to run; build scratch directories left on the VMs.

`scripts/build-image.sh` is exactly this and is being replaced — it tars the working tree, including untracked files, and ships it over SSH to the gpu node. Do not copy its shape into another repo, and do not reach for it as a shortcut. Its scratch directory on the gpu node has been deleted; do not recreate one.

Three things in it are worth keeping when it is replaced: the tag counter derived from what the registry already published, the manifest check after the push, and the `:latest` alias that break-glass `nomad job run` depends on.

Why: an image built from a working tree has no known source. It contains whatever was on someone's disk and records no commit. `openconv:2026.08.24.2` is deployed right now and nobody can say what built it.

Instead: build in CI from a commit, tag `YYYY.MM.DD.N`, push to the registry, then let the `update-service-version` action file the tag into the homelab's `service-versions.auto.tfvars.json` so Atlantis deploys it.

That CI is the homelab's self-hosted Gitea `act_runner` — the only CI executor here. It polls outbound and exposes no inbound port, which is precisely why it is the builder: this network accepts no inbound connections, ever, stated authoritatively in `home-infra/CLAUDE.md:11`. A GitHub-hosted runner joining the network, and a GitHub Actions job driving anything inside it, are both forbidden. Do not go looking for the arrangement that makes one of them work; there isn't one.

openconv is still developed on GitHub (`git@github.com:promptctl/openconv.git`), and pull requests and code review stay there. The repo also carries a second remote pointing at its Gitea repo on `gitea.sanctuary.gdn`, and **pushing a commit to that remote is what triggers a build.** No mirror, no polling, no schedule — a build exists because someone deliberately pushed the ref they wanted built. `.gitea/workflows/ci-builder.yaml` is the workflow that runs, on any push (no branch filter) and on manual dispatch.

Read that remote's name and URL out of `git remote -v` in the clone you are actually in — not from memory, and not from this document. If the clone has only `origin`, get the URL from the Gitea repo itself and add it. A guessed URL is the worst outcome available: it 404s, and the build you believe you triggered never ran.

The `gpu` node is not a build host. It is reserved for workloads that genuinely need the GPU. When the runner is slow and the ticket is late, the sentence that will occur to you is *"I'll just build it on gpu this once."* That once is how `openconv:2026.08.24.2` came to be deployed with nobody able to say what built it. Push the commit; let the runner build it.

<!-- BEGIN LIT INTEGRATION -->
## lit Agent-Native Workflow

This repository uses `lit` for agent-native issue tracking.

Start by running `lit quickstart` to load the workflow instructions. It prints how tickets are found, created, updated, and closed here, so running it first means the rest of your work follows the conventions this repo expects. It's a quick, read-only command — no need to check in before running it.

<!-- END LIT INTEGRATION -->
