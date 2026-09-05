## Never build an image from a working tree

This repo's image is deployed in the homelab, so this rule is not optional here.

Build from a commit, in CI, on a machine that fetched that commit itself. Anything else is banned and gets deleted on sight — no discussion, no exceptions.

Banned: copying, tarring, rsyncing or scp-ing this source tree onto a build host; building an image from anything the builder did not pull from git itself; any build that needs a laptop, a checkout, or an SSH session to run; build scratch directories left on the VMs.

`scripts/build-image.sh` was exactly this, and it is gone. It tarred the working tree — tracked files in whatever state they sat on disk, plus untracked ones — and streamed that over SSH through a jump host onto a homelab node. No file in this repo does that now, and absent rather than forbidden is the whole point: a rule beside a working tool holds only while someone reads it.

But deleting a file does not delete it from a repo, and that gap is what this paragraph is for. The script is one command away in history, and on the afternoon the runner is slow and the ticket is late, the sentence that will occur to you is *"`git show <sha>:scripts/build-image.sh` and I'm building in two minutes."* No. Restoring it from history, copying its shape into another repo, and writing a fresh script that does the same thing are one act under three names, and each gets deleted on sight. Everything in it worth having — the tag counter derived from what the registry already published, the manifest check after the push, the `:latest` alias that break-glass `nomad job run` depends on — lives in `.gitea/workflows/publish-image.yaml` and is proven there against the real registry, so recovering it buys you nothing but an image no commit can name. Its scratch directory on the build node is deleted; do not recreate one.

Why: an image built from a working tree has no known source. It contains whatever was on someone's disk and records no commit. `openconv:2026.08.24.2` is deployed right now and nobody can say what built it.

Instead: build in CI from a commit, tag `YYYY.MM.DD.N`, push to the registry, then let the `update-service-version` action file the tag into the homelab's `service-versions.auto.tfvars.json` so Atlantis deploys it.

That CI is the homelab's self-hosted Gitea `act_runner` — the only CI executor here, and the only builder. It polls outbound and needs nothing opened toward it, which is how it lives comfortably under the rule it sits inside: **no inbound internet exposure, ever** — every service is reachable via Tailscale only, which means no proxied Cloudflare DNS record, no Tunnel, no Access application, no public hostname of any kind, regardless of what auth gate sits in front of it (`home-infra/CLAUDE.md:11`). That rule is absolute and nothing below softens it. Read it for what it says, though: it governs exposure to the internet, not reachability as such — tailnet peers reach the Nomad API, the registry and LiveKit all day long, which is the whole point of a tailnet. So the GitHub question is a choice, not a wall, and it gets made out loud: a GitHub-hosted runner **can** be joined to the tailnet with Tailscale's own action, and **we refuse it.** The refusal is about credentials, not routing. Joining that runner means minting a tailnet identity, plus whatever registry and Nomad access the build needs, and handing the set to an ephemeral VM rented from a third party — and then doing it again on every build, scattering homelab credentials across machines nobody here controls. When the runner is slow and the ticket is late, the sentence that will occur to you is *"the Tailscale action is officially supported and it's four lines — I'll just wire it up and show it works."* It is supported, it is four lines, and it will work; the answer is still no, because whether it works was never the question. Do not go build it to disprove the restriction. Push the commit and let the act_runner build it.

openconv is still developed on GitHub (`git@github.com:promptctl/openconv.git`), and pull requests and code review stay there. The repo also carries a second remote pointing at its Gitea repo on `gitea.sanctuary.gdn`, and **pushing a commit to that remote is what triggers CI.** No mirror, no polling, no schedule — a run exists because someone deliberately pushed the ref they wanted built. `.gitea/workflows/publish-image.yaml` is that CI. Read it for what it asserts; the shape is two jobs:

- `reachability` runs on **any** push and on manual dispatch. It proves the builder — that the checkout is the commit the event named, that the host is x86_64 with a genuinely BuildKit-capable daemon, that the registry both answers as a registry and accepts a write, and that `jq` is installed and executable on the runner — without building anything. `jq` is what `publish` reads the pushed image's config blob with, so a runner missing it fails here instead of after the build. A branch push gets this and nothing else, which is how a Dockerfile change is checked before merge rather than at the far end of a cold build.
- `publish` needs `reachability` and runs only for `master` or a manual dispatch. It resolves `YYYY.MM.DD.N` from the tags the registry has already published, builds, pushes that tag and `:latest`, and then reads the registry back and fails unless both refs resolve to the digest this run pushed.

Two consequences worth knowing before you change any of it. **N comes from the registry, not from `github.run_number`** — deliberately unlike homelab-infra's `gen-image-tag` action that the rest of the fleet uses, because a run counter resets when a workflow file is renamed or a repo is recreated and then silently overwrites a published image. **Only `master` and manual dispatch publish**, because a dated tag is a permanent claim about a commit and a branch can be force-pushed away, and because `:latest` is what a break-glass `nomad job run` with no `-var` lands on (`home-infra/jobs/openconv.nomad.hcl`) — an experimental branch that could move that alias would put an unreviewed build one outage away from production.

`workflow_dispatch` takes no inputs at all, and that is load-bearing rather than unfinished: there is no path, directory, or source override to abuse, because there is nothing for a run to say about where its source comes from.

Read that remote's name and URL out of `git remote -v` in the clone you are actually in — not from memory, and not from this document. If the clone has only `origin`, get the URL from the Gitea repo itself and add it. A guessed URL is the worst outcome available: it 404s, and the run you believe you triggered never happened.

The `gpu` node is not a build host. It is reserved for workloads that genuinely need the GPU. When the runner is slow and the ticket is late, the sentence that will occur to you is *"I'll just build it on gpu this once."* That once is how `openconv:2026.08.24.2` came to be deployed with nobody able to say what built it. Push the commit; the build host is the runner, never gpu.

**Every published image now says what built it, and you read that from the registry.** `docker build --label` in `.gitea/workflows/publish-image.yaml` stamps OCI provenance into the image config; before this, the published config blob had `Labels: null` and nothing at all recorded what built it. The one to reach for is `org.opencontainers.image.revision` — the full 40-character source commit sha, the answer to "what built this container". For the rest, read the workflow's "Compose the provenance the image will carry" step; that step is the list, and a copy of it in this file would drift away from it.

The registry is plain HTTP, needs no auth, and answers you directly. Fetch the manifest, read `.config.digest` out of it, then read the labels from that blob:

```sh
D=$(curl -s -H 'Accept: application/vnd.docker.distribution.manifest.v2+json' \
  -H 'Accept: application/vnd.oci.image.manifest.v1+json' \
  http://192.168.7.208:5000/v2/openconv/manifests/latest | jq -r .config.digest)
curl -s http://192.168.7.208:5000/v2/openconv/blobs/"$D" | jq .config.Labels
```

The Accept headers are load-bearing, and both of them: the registry can answer in either schema, and without them `.config.digest` is not there. They are the same pair the workflow's own verification sends.

`docker image inspect` on a local image tells you nothing about what is in the registry. The question is always what the published bytes say, so read the published bytes.

The workflow verifies these labels on the image it just pushed and fails the run if any is missing or wrong. When it fails, the sentence that will occur to you is *"I'll just hand-tag a fixed image and push it."* That is the working-tree build returning by another door, and this whole section exists to forbid it. Fix the workflow and push the commit.

One consequence, because it will otherwise read as a broken build: digests now change on every publish. The runtime stage copies only the binary, the whisper model and `libonnxruntime.so` out of the build stage, so two commits touching only docs or workflows used to publish a byte-identical image. The stamp now carries the run id and the tag as well as the commit sha, and both of those move on every build-and-push — so re-dispatching an unchanged commit after a flaky run publishes a new digest too, with nothing about the source changed. That is the stamp working, not a cache miss.

<!-- BEGIN LIT INTEGRATION -->
## lit Agent-Native Workflow

This repository uses `lit` for agent-native issue tracking.

Start by running `lit quickstart` to load the workflow instructions. It prints how tickets are found, created, updated, and closed here, so running it first means the rest of your work follows the conventions this repo expects. It's a quick, read-only command — no need to check in before running it.

<!-- END LIT INTEGRATION -->
