# reaper — implementation plan

Ephemeral test-VM harness for pre-CI development loops. This document is the
handoff: it records what exists, what is decided, what remains open, and the
order of work. It is written to be executed by Claude Code with a human
(Cal) doing the small number of steps that require Proxmox UI access or
credentials.

Read the whole document before writing any code. The **Scope fence** and
**Ground rules** sections are constraints, not suggestions.

---

## 1. What this is

A framework that lets a developer (or Claude Code acting as one) spin up a
disposable Ubuntu VM on a Proxmox cluster, sync an uncommitted working tree
into it, build and run that project's e2e battery inside containers, roll the
system-under-test's state back between iterations via in-guest ZFS, and tear
the VM down — with a fully independent sweep ("cull") that destroys any VM
whose TTL tag has expired, so no failure mode leaks machines.

The purpose is the **pre-push loop**: hammer out bugs in a tight
edit→test cycle *before* commit/push/CI. It is explicitly not a CI platform.
CI (Bitbucket Pipelines) remains the independent re-proof after push.

Vocabulary, fixed: **reaper** = the framework and the client CLI.
**runner** = the agent baked into the guest template. **cull** = the TTL
sweep, deployed on its own VM. A project consuming the framework is a
**tenant**; its integration surface is one manifest file.

## 2. Environment facts (verified in prior work — do not rediscover)

- Proxmox VE node `bass`, API at `https://192.168.1.69:8006`, currently a
  self-signed cert (`--insecure`/`PVE_INSECURE=1` in use; a CA-issued cert is
  a planned improvement, see §10).
- Cal's account is a *tenant* on someone else's cluster: no node shell
  (Sys.Console renders a login prompt with no usable account), no user
  creation. All admin is via UI or API.
- Resource pool `cal`, with sub-pool **`cal/ephemeral`** created for this
  framework. Storage **`member-vms2`** is a member of the pool.
- `member-vms2` is **plain LVM** (~880 GB free of 1 TB): no hypervisor-level
  snapshots, no linked clones. Every VM clone is a full disk copy. This is
  why state reset lives *inside* the guest (ZFS) rather than at the
  hypervisor. Template disk size directly determines clone latency — keep it
  small.
- Other storages visible to the account: `member-vms` (lvmthin, ~86% full —
  do not use), `local` (dir; ISO/vztmpl/backup), `local-lvm` (lvmthin,
  ~77 GB free), `truenas-backup` (NFS, 2.7 TB free, content includes
  `images` and `import`).
- API tokens, both privilege-separated (they inherit nothing from the
  account):
  - `cal@pve!harness` — role `ClaudeEphemeral` on `/pool/cal/ephemeral`:
    VM.Allocate, VM.Clone, VM.Config.* (CPU/Memory/Disk/Network/Options/
    CDROM/Cloudinit/HWType), VM.PowerMgmt, VM.Audit, VM.Console,
    VM.GuestAgent.Audit, Datastore.AllocateSpace, Datastore.Audit,
    Pool.Audit. This is the token the reaper CLI/runner path uses.
  - `cal@pve!reaper` — role `ClaudeReaper` (VM.Audit, VM.PowerMgmt,
    VM.Allocate, Datastore.AllocateSpace, Pool.Audit) on
    `/pool/cal/ephemeral`. Held only by the cull VM. Claude Code must never
    handle this credential.
- The auth header is case-sensitive: `Authorization: PVEAPIToken=user@realm!id=secret`.
- VMID conventions: **9000–9099** = ephemeral range (the only IDs the cull
  will destroy); the cull VM itself is outside that range (8100, named
  `pve-reaper`, FreeBSD, in pool `cal` — *not* in `cal/ephemeral`, which is
  what keeps it out of the harness token's reach).
- Cull is deployed and verified: `/usr/local/sbin/pve-reap` on the FreeBSD
  VM, cron `*/5` under unprivileged user `reaper` with `lockf`, token at
  `/usr/local/etc/reaper/token` (0640 root:reaper), syslog via `logger -t
  pve-reap`. Contract: destroys only VMs in the pool AND in 9000–9099 AND
  bearing an expired `expires-<unix-epoch>` tag; untagged VMs are logged and
  left alone. A missing tag means a half-failed create and requires a human.
- Guest agent IP retrieval works (`VM.GuestAgent.Audit`); this PVE version
  uses the VM.GuestAgent.* privilege split (no VM.Monitor).
- Every VM create/clone MUST pass `pool=cal/ephemeral` explicitly — the
  token has VM.Allocate nowhere else, and omitting it produces a permission
  error that looks like a bug.
- First tenant: **yasss** (Java/Gradle; MariaDB + mailpit + app in one
  podman pod; drivers and Playwright run inside the pod; `e2e/run.sh` with
  independent `--only` stages and a `--jar` mode). GitHub mirror:
  `github.com/axonibyte/yasss`. Its journeys stage is a stateful
  property-based engine whose replay fidelity is currently degraded by
  database state accumulation (`RUN_TAG`/`attempt` workarounds; a stale
  promoted seed in `seeds.json`) — in-guest ZFS rollback is the fix.
- Client environment: Windows workstation with an Arch WSL instance;
  WireGuard tunnels terminate on Windows. The CLI must work from WSL and
  treat the PVE API + guest IPs as reachable over LAN/tunnel. Do not assume
  mDNS or DNS for guests; use IPs from the guest agent.

## 3. Architecture (decided)

Three components, strictly layered; each knows less than the one above it.

**reaper CLI** (workstation, in the tenant checkout). Reads `.reaper.yaml`.
Talks to the PVE API (harness token) and to the runner (SSH). Owns: session
lifecycle, working-tree sync (rsync over SSH; must handle uncommitted trees —
never require a git commit), heartbeat renewal of the `expires-` tag, and
pulling result artifacts back into the tree. Session-oriented UX: `up`,
`sync`, `build`, `reset`, `run`, `test` (= sync→build→reset→run), `renew`,
`list`, `down`. `list` presents sessions (project, age, TTL remaining, IP),
not raw VMs. A second `up` for the same project offers to reuse the live
session.

**runner** (inside the guest, from the template). A small agent (systemd
unit) that: creates the zpool on the second disk at first boot, lays out
datasets, executes manifest verbs in toolchain containers, takes the
`@pristine` snapshot after the first successful stack-up, performs
`reset` = stop pod/containers → `zfs rollback` of the manifest's reset
datasets → (the next run restarts the stack), and pushes traces/artifacts to
the workspace so the CLI's reverse-sync picks them up. Exposes its verbs over
SSH invocation (no listening daemon in v1 — SSH is the transport; the
"reset endpoint reachable from inside the pod" need is met by a
runner-provided socket/wrapper, see Phase 4).

**cull** (separate VM, separate token). Already running. Knows only tags and
time. Gets imported into the repo, renamed, and hardened; its deployment
remains outside anything Claude Code can modify.

Guest dataset layout (zpool `tank` on the second virtual disk):

    tank/images   podman graphroot (image store)      — never rolled back
    tank/cache    per-ecosystem build caches          — never rolled back
    tank/state    DB data dirs, app file storage      — rolled back freely
    tank/work     synced working tree + results out   — never rolled back

Guest: Ubuntu Server LTS (26.04; prefer 26.04.1 media once available), ext4
root on the boot disk, native zfsutils (no DKMS), podman 5.x **rootful**
(acceptable: the whole VM is disposable and the blast radius is the
sandbox), ZFS ARC capped (1–2 GB) so it doesn't fight the DB/browser
workload. No language toolchains in the template, ever — toolchains arrive
as container images named by the manifest.

Manifest (`.reaper.yaml`, schema v1) — the entire tenant integration:

    schema: 1
    project: <name>
    build:
      image: <registry>/<image>@sha256:...    # toolchain container
      cmd: <build command>
      cache: [gradle]                          # names under tank/cache
    run:
      cmd: <entry command>                     # env passthrough supported
      images: [<digest>, ...]                  # pre-pulled into tank/images
    reset:
      datasets: [state]                        # rolled back by `reset`
    resources: { cores: N, ram_gb: N }
    profiles:
      dev:     { ttl: 2h,  warm_cache: true }
      nightly: { ttl: 12h, warm_cache: false, env: {...} }

Digests, not tags. `warm_cache: false` (nightly/hunting profile) runs
without `tank/cache` mounted: determinism mode. An explicitly discouraged
`host_packages` escape hatch may exist in the schema, logged loudly when
used; implement last, if at all.

Lifecycle invariants:

- The `expires-` tag is a dead-man's switch: the CLI heartbeat renews it
  while a session is active; expiry means the operator vanished. Set the tag
  immediately after clone *before* first boot; a crash between clone and tag
  is exactly the "untagged VM" state the cull logs for a human.
- Normal teardown is `reaper down` (API destroy via harness token). The cull
  is the backstop, not the janitor for routine exits.
- TTL is measured from readiness, not from the create request (full-copy
  clones are slow on plain LVM).
- Results flow outward continuously (reverse rsync of `tank/work/<proj>/out`
  after every run, and on `down`). A failure trace must never exist only on
  a VM scheduled for destruction.
- `create` preconditions in the CLI: free-space floor on `member-vms2`
  (query at call time — other tenants share it), concurrency cap on live
  sessions, VMID allocated from 9000–9099 only.

## 4. Scope fence (what reaper is NOT)

No test framework: journeys, oracles, seeds, shrinkers are tenant code,
invoked opaquely through `run.cmd`. No scheduling, no multi-node placement,
no queueing. No CI integration. No secrets management beyond "where do the
two tokens live". No support for guests other than the one blessed template.
The moment a feature request implies any of these, the answer is a tenant
change or a "no". The README carries this fence.

## 5. Ground rules for the implementing agent

- Secrets: the harness token is provided via environment/file outside the
  repo (`~/.config/reaper/`); never committed, never logged, never echoed.
  The cull token is off-limits entirely — its VM is not a deploy target for
  Claude Code.
- Anything in pool `cal` but outside `cal/ephemeral`, and anything outside
  pool `cal`, does not exist as far as this codebase is concerned. Belt and
  suspenders on top of the token scoping: the CLI refuses VMIDs outside
  9000–9099 in every code path, mirroring the cull.
- The PVE API is the only hypervisor interface (no `qm`, no SSH-to-node —
  there is no node access). Clone/destroy are async: poll the returned task
  UPID to completion; treat timeouts as failures that leave the VM for the
  cull rather than retry-destroying blindly.
- Language: implementer's choice, but the CLI must run on Arch/WSL with
  minimal runtime deps, and the runner on stock Ubuntu; a single static
  binary (Go or Rust) or POSIX sh + a small helper are both acceptable.
  Match the discipline visible in the yasss repo: heavily-commented,
  deterministic, explicit about caveats.
- Every destructive operation (destroy, rollback) logs what and why before
  acting, and refuses when its preconditions aren't provable (e.g. rollback
  with containers still running is a stop-then-rollback, never a rollback
  under a live pod).
- No feature is done without the test named in its phase's acceptance
  criteria.
- The testing methodology's §2 non-negotiables apply to *this codebase and
  this agent*, not just to tenants: never weaken a test, check, or lint to
  route around a defect (no skip/ignore/`|| true`/swallowed errors/narrowed
  scope without a stated reason covering exactly what it narrows); every
  fix ships with a test that would have caught it, or an explicit statement
  of why it is untestable; a pre-existing failure is proven pre-existing
  (stash, re-run, name it) before being called pre-existing; new assertions
  are mutation-checked — break the thing, watch the test fail — before they
  count as coverage.

## 6. Phases

Each phase ends with acceptance criteria. Do not start a phase before the
prior one's criteria pass, except where marked parallel-safe.

### Phase 0 — Repo bootstrap *(Claude Code)*
Init `reaper` repo: README (purpose, vocabulary, tenant/landlord split,
scope fence), `LICENSE`, `docs/` containing this plan and
`testing-methodology.md` (note: that document's internal link to a
`simulated-user-testing.md` deep-dive refers to a superseded file that is
deliberately not shipped — the living specification for that tier is the
yasss repo's `e2e/journeys/` implementation), `manifest/` with schema v1
(JSON Schema or equivalent) + a worked yasss example, `cull/` importing the
deployed `pve-reap` script verbatim as the starting point, then renamed
`cull` and hardened: fix the pipeline-head error-masking (API failure must
exit non-zero, not be swallowed by the `| jq | while` pipeline), keep the
TLS-after-sourcing fix, add a `--dry-run` flag, and a shellcheck pass.
Add a **decision self-test** in the methodology's oracle-self-test spirit:
with the API mocked by canned `cluster/resources` payloads, assert cull
destroys an expired in-range VM, leaves a future-tagged VM, logs-and-leaves
an untagged VM, and refuses an out-of-range VMID — runnable with nothing
live. (The cull has already had one §2-shaped incident — a swallowed curl
failure that made errors invisible; this test is what keeps the backstop a
backstop.)
**Accept:** shellcheck clean; decision self-test passes; `cull --dry-run`
against the live API lists the pool correctly from a dev machine; schema
validates the yasss example.

### Phase 1 — PVE client library + session core *(Claude Code)*
The API client (token auth, task polling, clone/config/tag/start/stop/
destroy/guest-IP) and session bookkeeping (local state: session name ↔
VMID ↔ IP ↔ project). CLI verbs `up` (clone stub template, tag, wait,
report IP), `list`, `renew`, `down`. Heartbeat as a background process of
the CLI with a documented cadence comfortably inside the TTL.
**Accept:** integration test against the real pool using a trivial stub VM:
up → tagged correctly → list shows it → renew moves the tag → down destroys
it; kill -9 the heartbeat, confirm the cull collects the VM after expiry
(this doubles as the cull's end-to-end re-verification); a create attempted
with VMID 8100 or without `pool=` is refused by the CLI before any API call.

### Phase 2 — Template build *(human, with Claude Code writing the runbook
and cloud-init/firstboot payloads; parallel-safe with Phase 1)*
No node shell → no `qm importdisk`; the template is built through the UI.
Preferred route: upload/download the Ubuntu 26.04 server ISO to `local`,
create VM (VMID in 9000–9099, pool `cal/ephemeral`, storage `member-vms2`,
boot disk small — target ≤8 GB — plus a second disk for the zpool, serial
console, qemu-guest-agent enabled, VirtIO throughout), install with ext4
root, install `zfsutils-linux podman git openssh-server` + the runner
payload, set `cipassword`-equivalent console access for 1am debugging,
clean (machine-id, SSH host keys), shut down, convert to template, tick
Protection, and back it up (Datacenter → Backup or a manual backup to
`truenas-backup`). Investigate the `import` content type on `truenas-backup`
first: if the UI's import wizard accepts a cloud image qcow2 there, the ISO
route collapses to minutes. Claude Code writes: the firstboot script
(zpool create on the second disk by stable device path, dataset layout,
ARC cap, podman graphroot at `tank/images`, SSH key trust for the CLI),
and the step-by-step UI runbook with expected screens.
**Accept:** a hand-made clone of the template boots, firstboot builds the
pool and datasets, guest agent reports an IP through the API, SSH works
with the session key, `podman info` and `zfs list` are sane, and a second
clone proves the template wasn't dirtied by the first boot. Record the
observed full-clone wall time in the README (sets expectations; if it is
intolerable, see §10 storage fallback before proceeding).

### Phase 3 — Sync + build + toolchain containers *(Claude Code)*
`reaper sync` (rsync working tree → `tank/work/<project>`, delta, deletes
mirrored, excludes configurable; reverse channel for `out/`). `reaper build`
(run `build.cmd` in `build.image` with `tank/work` mounted and `tank/cache/
<names>` mounted per manifest+profile). Digest pre-pull into `tank/images`
on first `up` of a session (cache-hit thereafter).
**Accept:** on a live session, yasss builds its shadow jar inside
`temurin:17-jdk@sha256:...` with a warm Gradle cache; second build is
measurably incremental; `warm_cache: false` profile builds cold; sync of a
dirty uncommitted tree round-trips a file created on the guest side back
into `out/`, including a multi-megabyte binary (the human-evidence tier
emits per-step screenshots — the reverse channel must carry bulky binary
artifacts, not just JSON traces).

### Phase 4 — Reset verb + pristine snapshot *(Claude Code)*
Runner: `reset` = stop the project's containers/pod → `zfs rollback -r`
each manifest reset dataset to `@pristine` → exit; the next `run` restarts
the stack (matches yasss's restart-replays-migrations semantics — a JVM
holding pre-rollback signers/sessions must never survive a rollback).
`@pristine` is taken automatically after the first successful `run`
completes its stack-up. A rule that must hold forever: **the runner never
constructs or seeds project state** — pristine is whatever the tenant's own
stack-up produced, hostility included. The methodology's Tier 6 starts the
database in a hostile configuration (e.g. `latin1`) precisely so the schema
migration is load-bearing, and builds state through the real API and mail
flow ("no backdoors"); a runner convenience that pre-seeds a friendly
database would silently defeat both. Rollback-to-pristine is legitimate
exactly because the snapshot was earned through the real path once. Provide the in-pod reset trigger: a unix socket (or
tiny loopback listener) on the guest that the driver container can hit,
wrapped so tenants like yasss's journey engine can call reset between
`runJourney` passes without knowing ZFS exists. Optional but cheap:
`reset --to <name>` and `snapshot <name>` for the checkpoint-and-replay
shrink pattern.
**Accept:** with the yasss stack up: write rows, `reset`, prove the DB is
back to pristine and the app restarted clean; `tank/work`, `tank/cache`,
`tank/images` provably untouched by reset; reset refused/converted to
stop-first when containers are live; two consecutive resets are idempotent.

### Phase 5 — First tenant end-to-end *(Claude Code, with yasss-side changes
as separate commits in the yasss repo)*
Write yasss's real `.reaper.yaml` (digest-pin the images `run.sh` names —
mariadb, temurin JRE, node:22-slim, playwright — and the temurin JDK builder).
yasss-side task, kept out of the framework: containerize `run.sh`'s build
stage (host Gradle → toolchain container; retain a host/CI path so
Pipelines, where the pipeline image is the toolchain, stays redundant-free).
Then the loop: `reaper test` = sync → build → reset → run
(`--only ${STAGES:-journeys}` as dev default), traces landing in the repo.
**Accept:** the full battery (`reaper test --all` → every run.sh stage)
passes green in a session; a deliberately introduced bug (e.g. re-break the
ungrouped `?volunteer=` join) is caught by the journeys stage, its shrunk
trace appears in the working tree, and — the payoff test — replaying the
promoted seed against a fresh `reset` reproduces it *identically twice*,
demonstrating the state-accumulation problem the `_staleNote` in
`seeds.json` documents is gone. Measure and record the warm-loop cycle time
(sync+build+reset+journeys); if it does not clearly beat running `run.sh`
locally, stop and reassess before polishing.

### Phase 6 — Hardening + docs *(Claude Code)*
Free-space floor + concurrency cap enforced in `up`. Structured session log.
`reaper doctor` (token reachability, pool visibility, template presence,
cull liveness via recent syslog-visible activity — or simply "an expired
canary VM disappears"). Failure-mode table in the README (heartbeat died,
clone task timeout, rollback refused, registry unreachable). Manifest schema
docs + a second worked example for a non-JVM tenant (Rust or Elixir shape,
even if hypothetical) to prove language-agnosticism on paper. README tenant
guidance states which tiers belong here: the sandbox exists for the tiers
that need a real stack (full-stack, simulated users, live browser audit);
the cheap tiers — unit, contract, browser-vs-fake — stay on the
workstation, and migrating them into sessions because sessions exist is an
anti-pattern that slows the fast loop for nothing. Nightly
profile documented (long TTL, cold cache, env passthrough for
JOURNEY_ITERATIONS-style knobs).
**Accept:** `doctor` distinguishes each induced failure; README failure
table matches observed behavior; a new tenant can be described end-to-end
without editing framework code.

## 7. Human task list (everything Claude Code cannot do)

1. Phase 2 template build in the PVE UI per the runbook (ISO or import
   route), Protection flag, backup job.
2. Provide the harness token to the CLI environment on the workstation
   (`~/.config/reaper/`), and an SSH keypair decision for sessions (bake the
   public key into the template via the runbook).
3. Approve the yasss-side `run.sh` build-stage refactor.
4. When touching the cull VM next: adopt the hardened `cull` from the repo
   (rename from `pve-reap`), keeping deployment manual and outside agent
   reach.
5. Optional, recommended: issue `bass` a cert from the in-house CA and flip
   both cull and CLI from `insecure` to CA verification.

## 8. Open decisions (decide during the phase that first needs them)

- CLI implementation language (Phase 1; single static binary preferred).
- Registry strategy for digest pulls: direct docker.io/mcr + baked warm
  layer in the template, vs a pull-through cache on the LAN (Phase 3; start
  direct, add the cache when a registry outage first stings — but note
  digest pinning already prevents wrong bytes, only availability is at
  stake).
- Whether `resources.cores/ram_gb` are applied at clone time via the API
  (they can be: VM.Config.CPU/Memory are granted) or fixed by the template
  in v1 (Phase 1/2).
- Exact heartbeat cadence and default TTLs per profile (Phase 1; strawman:
  renew every 10 min to now+2h for dev).
- `reset --to <checkpoint>` in v1 or deferred (Phase 4; it is cheap once
  rollback works).
- Storage fallback if full-clone latency is intolerable (see §10).

## 9. Risks, stated plainly

Full-copy clones on plain LVM are the big unknown: session start is minutes,
not seconds — acceptable for session-per-afternoon, and the in-session loop
never pays it, but Phase 2 must measure it honestly. Thin alternatives exist
(`local-lvm` has ~77 GB free; `truenas-backup` qcow2-over-NFS trades IOPS
for linked clones) and the design deliberately confines the choice to "which
storage the template lives on", so it is reversible. Warm caches trade
hermeticity for speed by design; the nightly profile is the control. The
template is a pet-shaped artifact built by hand in v1 — the runbook plus the
backup is the mitigation, and a Packer-style automated rebuild is a welcome
v2 item, not a v1 requirement. Ubuntu 26.04 is a young LTS; pin to .1 media
and record exact package versions in the template's README.

## 10. v2 horizon (explicitly deferred)

CA-issued TLS everywhere (drop `insecure`). Automated template rebuilds.
Pull-through registry cache. Multi-VM topologies per session. A broker that
holds the harness token off-workstation (only needed if the trust model for
the agent changes). LXC profile for sub-second sandboxes on projects that
don't need a kernel boundary.
