# reaper

An ephemeral test-VM harness for the **pre-push loop**.

`reaper` spins up a disposable virtual machine, syncs your *uncommitted* working
tree into it, runs your project's end-to-end battery there, rolls the guest's
state back between iterations via in-guest ZFS, and tears the machine down. A
fully independent sweep destroys any machine whose time-to-live has expired, so
no failure mode leaks VMs.

The purpose is the tight `edit -> test` cycle *before* commit and push. It is
deliberately **not** a CI platform. CI remains the independent re-proof after
you push, and nothing here replaces it.

> **Status: Phase 1.** The CLI can bring a session up, list it, renew it and
> take it down, against a provider behind a seam -- all verified offline against
> a stand-in hypervisor, none of it yet run against a real one. The runner and
> the guest templates are Phases 2 onward. See [`docs/STATUS.md`](docs/STATUS.md)
> for where the work actually stands, which is the document to trust over this
> one when they disagree.

## Vocabulary

Fixed, because the three components are easy to conflate:

| Term | Meaning |
|---|---|
| **reaper** | The framework, and the client CLI you run in your project |
| **runner** | The agent baked into a guest template, which executes your verbs |
| **cull** | The independent TTL sweep, deployed on its own machine |
| **tenant** | A project consuming the framework. Its integration surface is one manifest file |
| **guest** | A blessed VM template a tenant can run on |
| **provider** | The hypervisor plugin that creates and destroys machines |

## Architecture

Three components, strictly layered; each knows less than the one above it.

**The CLI** runs on your workstation, in your project's checkout. It reads
`.reaper.yaml`, talks to a provider, and talks to the runner over SSH. It owns
session lifecycle, working-tree sync (which must handle uncommitted trees --
committing to run your tests is not a thing this asks of you), heartbeat renewal
of the expiry tag, and pulling result artifacts back into your tree.

**The runner** lives inside the guest. It creates the storage pool on first
boot, lays out datasets, executes your manifest's verbs, takes the `@pristine`
snapshot after the first successful stack-up, and performs `reset` as *stop the
stack, roll back, let the next run restart it*.

**The cull** runs on a separate machine with a separate credential. It knows
only tags and time. It is the backstop, not the janitor for routine exits.

### Guest storage layout

```
tank/images   container image store        -- never rolled back
tank/cache    per-ecosystem build caches   -- never rolled back
tank/state    your project's state         -- rolled back freely
tank/work     synced tree + results out    -- never rolled back
```

## The three seams

`reaper` hosts any program with an end-to-end battery. It does not decline on
the grounds of what your program is, what it is written in, or what it runs on.
That resolves into three independent seams, which fail differently and are
therefore kept apart:

**Tenant.** No tenant name appears anywhere in framework code. The framework
never learns tenant vocabulary -- it does not know what a "stage", "journey",
"seed" or "shrinker" is, and your `run.cmd` is an opaque string handed to a
shell. A lint guard enforces this; good intentions do not.

**Guest OS.** Two different people make two different choices here, and
conflating them is exactly how an OS gets hardcoded. The *sysadmin* decides
which guests exist, in a site registry that lives outside this repository.
The *developer* declares which guests their project wants, in the manifest, and
may name several. Adding an operating system is a registry edit and a template
build, never a code change. See [`docs/guests.md`](docs/guests.md).

**Hypervisor.** Machine creation sits behind a provider trait. One
implementation ships. The door is left open a crack for a second, and no wider:
no dynamic loading, no ABI, no plugin boundary. A new provider is a new module
compiled in. See [`docs/providers.md`](docs/providers.md).

## Scope fence

What `reaper` is **not**, and what the answer is when a feature request implies
one of these -- a tenant-side change, or a plain "no":

- **Not a test framework.** Journeys, oracles, seeds and shrinkers are your
  code, invoked opaquely through `run.cmd`.
- **Not a scheduler.** No queueing, no multi-node placement, no fair sharing.
- **Not CI.** No pipeline integration, no build artifacts, no verdict publishing.
- **Not a secrets manager.** The scope is "where do the credentials live", and
  no further.

### Where this repository departs from `docs/reaper-plan.md`

The original plan ships verbatim as [`docs/reaper-plan.md`](docs/reaper-plan.md)
because it is a record. Three of its constraints have since been deliberately
lifted, and the plan text still carries the old wording. Rather than edit a
record, the overrides are named here:

| The plan says | What is true now |
|---|---|
| §4: "No support for guests other than the one blessed template" | Multiple guests are supported. Which ones exist is the sysadmin's registry, not the framework's business |
| §3: "No language toolchains in the template, ever" | Holds for container-execution templates, where toolchains arrive as pinned images. A host-execution template carries its toolchain by design |
| §5: "The PVE API is the only hypervisor interface" | True of the Proxmox provider specifically. The core talks to a provider trait |

## Which tests belong in a session

A session is expensive: it is a whole virtual machine, and starting one costs
minutes. It earns that cost for the tiers that need a real stack -- full-stack
containerized suites, simulated-user runs over accumulated history, and live
browser audits.

The cheap tiers -- unit tests, contract tests, browser-against-a-fake -- stay on
your workstation, where they belong. **Migrating them into sessions because
sessions exist is an anti-pattern**: it slows the fast loop and buys nothing,
since none of those tiers can see a defect that needs a real machine to surface.

[`docs/testing-methodology.md`](docs/testing-methodology.md) sets out the full
portfolio and what question each tier uniquely answers.

## Getting started

There is nothing to run yet beyond the manifest validator and the sweeper's
self-test; see [`docs/STATUS.md`](docs/STATUS.md). When there is, the shape will
be:

1. A sysadmin builds a guest template and registers it
   ([`docs/guests.md`](docs/guests.md), [`docs/site-config.md`](docs/site-config.md)).
2. You write a `.reaper.yaml` in your project
   ([`docs/tenants.md`](docs/tenants.md), `manifest/examples/`).
3. `reaper test` syncs, builds, resets and runs.

## License

MIT. See [`LICENSE`](LICENSE). Copyright (c) 2026 Caleb L. Power.
