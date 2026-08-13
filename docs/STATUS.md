# Status

Where the work actually stands. When this document disagrees with any other,
trust this one -- the rest describe intended shapes, this describes the shape
that exists.

Last updated: 2026-08-12.

## Phases

| Phase | What it delivers | State |
|---|---|---|
| **0** | Repo bootstrap: docs, manifest schema, seam guards, sweeper | **In progress** |
| 1 | Provider seam + session core (`up`/`list`/`renew`/`down`) | Not started |
| 2 | Guest templates + the runner | Not started |
| 3 | Sync, build, execution | Not started |
| 4 | `reset` and the `@pristine` snapshot | Not started |
| 5 | Tenant onboarding | Not started |
| 6 | Hardening, `doctor`, third-tenant proof | Not started |

Phase detail is in [`reaper-plan.md`](reaper-plan.md) §6, amended by the
decisions below. Phase 2 is parallel-safe with Phase 1; nothing else is.

## Decisions taken

| Question | Answer | When |
|---|---|---|
| CLI implementation language | Rust. The runner too, for portability across guests | Phase 0 |
| Guest support | Multiple guests from the start, registry-driven and sysadmin-owned | Phase 0 |
| First two guests | Ubuntu 26.04 LTS and FreeBSD 15.1-RELEASE | Phase 0 |
| Hypervisor coupling | A provider seam; Proxmox implemented, CBSD documented only | Phase 0 |
| Worked examples | Two, deliberately dissimilar, shipped in Phase 0 rather than deferred | Phase 0 |
| TLS stack | `rustls` only, never `native-tls` | Phase 0 |

### Overrides of `reaper-plan.md`

The plan ships verbatim as a record. Three of its constraints have been lifted;
the plan text still carries the old wording. Also listed in the root README.

| The plan says | What is true now |
|---|---|
| §4: "No support for guests other than the one blessed template" | Multiple guests; which exist is a registry entry |
| §3: "No language toolchains in the template, ever" | Holds for container-execution templates; host-execution templates carry a toolchain by design |
| §5: "The PVE API is the only hypervisor interface" | True of the Proxmox provider; the core talks to a trait |

## Still open

Each is deliberately deferred to the phase that first needs it, per
[`reaper-plan.md`](reaper-plan.md) §8.

| Question | Decide in |
|---|---|
| Heartbeat cadence and per-profile TTL defaults (strawman: renew every 10 min to now+2h for dev) | Phase 1 |
| Whether `resources.cores`/`ram_gb` apply at clone time or are fixed per template | Phase 1 or 2 |
| Registry strategy for image pulls: direct, or a pull-through cache on the LAN | Phase 3 |
| `reset --to <name>` and `snapshot <name>` in v1, or deferred | Phase 4 |
| Storage fallback if full-copy clone latency proves intolerable | Phase 2 |

## Environment facts

Verified 2026-08-12. These correct and extend [`reaper-plan.md`](reaper-plan.md)
§2, which describes a client environment that is not where development is
happening.

- **The development host is FreeBSD 15.0-RELEASE-p1**, not the Windows/WSL
  workstation §2 describes. WSL remains a target. The dev host is also the
  natural build host for a FreeBSD guest's runner.
- **No cross-compilation from the dev host.** `rustc`/`cargo` 1.97.1 are
  source-tarball builds with no `rustup`; the only installed rustlib targets are
  `x86_64-unknown-freebsd` and `wasm32-unknown-unknown`, and there is no cross
  linker. **Build natively per host and per guest.** This is why `rustls` is
  mandatory and `native-tls` is banned: an OpenSSL linkage difference between
  build hosts is exactly the kind of portability failure that would show up
  late and confusingly.
- **`podman` on FreeBSD is rootful-only** and runs Linux images under the
  Linux emulation layer, so it cannot execute FreeBSD-native binaries. This is
  the concrete reason `exec: host` exists.
- Present: `shellcheck` 0.11.0 (package `hs-ShellCheck` -- note the
  capitalisation, `pkg search -q shellcheck` finds nothing and `-i` is needed),
  `node` v24.18.0, `jq`, `rsync`, `yq`, `podman`.
- Absent: `npm`/`npx`, `pip` (system Python has `venv` only), `rustup`.
  `cargo-spellcheck` is installed but is a different tool from `shellcheck` and
  is not used by this project.
- The Proxmox API is reachable from the dev host (verified: HTTP 401 without a
  token, which is the API answering). This was **not** true at the start of
  Phase 0 and required a routing change.

## Corrections carried forward

Findings from reading the reference tenants. Recorded rather than silently
absorbed, because each one changes an acceptance criterion.

### 1. Phase 5's payoff test conflates two different problems

[`reaper-plan.md`](reaper-plan.md) §6 Phase 5 says that replaying a promoted
seed proves "the state-accumulation problem the `_staleNote` in `seeds.json`
documents is gone".

Those are two different problems. The `_staleNote` documents **seed-stream
drift**: an action began drawing one extra random number, and every seed
consequently walked a different sequence from then on. Rollback cannot fix
that, and neither can anything else in this framework -- it is a property of
the tenant's own generator, and the note itself draws the right conclusion
("a regression that must stay caught belongs in a written test, not in a seed").

What `reset` genuinely fixes is the **state accumulation** that forces a
journeys engine to tag its data per-run and per-attempt to avoid colliding with
its own history.

So the acceptance criterion is restated:

> The same seed, replayed against two consecutive `reset`s, produces an
> identical action sequence and an identical outcome.

That is a claim `reset` can actually carry, and it is worth carrying.

### 2. `testing-methodology.md` links to a document that is not here

Its §7 and §11 point at `simulated-user-testing.md`. That file is superseded and
deliberately not shipped; the living specification is a tenant's own `journeys`
implementation. The link is left intact because the document is a record. Noted
in [`README.md`](README.md).

## Reference tenants

Neither is a dependency, and neither is named anywhere in framework code -- a
lint guard enforces that. They exist as worked examples, chosen to be
dissimilar, because a schema validated against one project silently becomes that
project's schema.

| | JVM web service | Rust CLI |
|---|---|---|
| Stack | database + mail catcher + app in one pod | none: a process and temp trees |
| Drivers | browser automation inside the pod | the language's own test runner |
| State to roll back | database data dir, app file storage | embedded-SQLite cache, fixture trees |
| Build cache | one ecosystem's | a different ecosystem's |
| Pre-pulled images | four | **none** |
| Guest / exec | Linux, `container` | **BSD, `host`** |

The second one is the more valuable, because it breaks assumptions the first
would let stand: that state means a database container, that a run needs
pre-pulled images at all, that a tenant has a pod, that the guest is Linux, and
that execution is containerized. One of its suites is gated on the host
operating system and had nowhere to run under a single-guest design.

## Known risks

- **Full-copy clones.** On storage without snapshots or linked clones, every
  session start is a whole disk copy -- minutes, not seconds. The in-session
  loop never pays it again, but Phase 2 must measure it honestly, and if it is
  intolerable the fallback is confined to which storage a template lives on.
- **Two templates double the storage and the maintenance.** This sharpens the
  storage decision above and makes automated template rebuilds more valuable,
  not less.
- **Templates are hand-built pets in v1.** Runbooks plus backups are the
  mitigation; automated rebuilds are a v2 item.
- **FreeBSD 15.1-RELEASE may not exist yet.** The dev host runs 15.0-RELEASE-p1.
  Phase 2 confirms the media before committing to it and falls back to 15.0
  explicitly rather than silently.
- **Ubuntu 26.04 is a young LTS.** Pin to point-release media, record exact
  package versions in the template's notes.
- **The provider seam has exactly one implementation**, so it is a hypothesis
  rather than a proof. The lint guard tests that hypervisor vocabulary has not
  leaked across the boundary; it cannot test that the boundary is in the right
  place. Only a second implementation would, and building one speculatively
  costs more than it proves.

## Deferred to v2

CA-issued TLS everywhere (dropping the insecure-transport flag), automated
template rebuilds, a pull-through registry cache, multi-VM topologies per
session, a broker holding the harness credential off-workstation, a
container/jail profile for sub-second sandboxes on projects that do not need a
kernel boundary, and the CBSD provider.

Multi-guest support is **not** on this list. It moved into v1.
