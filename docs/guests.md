# The guest contract

A **guest** is a VM template that `reaper` can clone into a session. This
document is what a template must provide for the runner to work. Anything
meeting this contract is a valid guest.

That is the whole point of writing it down. "reaper hosts any program" is only
true if an unsupported program is *a template nobody has built yet* rather than
a refusal. Adding an operating system is a template build plus a registry
entry -- never a change to framework code.

## Requirements

A template must provide:

**A second disk, unpartitioned**, which firstboot claims for the ZFS pool. It
must not be the root disk. The runner discovers it as *the block device that is
not the root device*; it never matches on a device name, because device naming
is one of the few things that genuinely differs between operating systems and
hardcoding it is how a framework accidentally supports exactly one.

**ZFS**, whether native to the operating system or installed. The runner uses
`zpool create`, `zfs create`, `zfs snapshot` and `zfs rollback`, and nothing
exotic. This command surface is identical across the platforms supported so far,
which is what makes the core mechanism portable for free.

**SSH**, with the session public key trusted. SSH is the transport; there is no
listening daemon in the guest beyond it.

**A discoverable IP.** The provider must be able to learn the machine's address
without help from DNS or mDNS -- a guest agent is the usual mechanism.

**An init mechanism** that starts the runner at boot and restarts it if it
dies. What that mechanism *is* -- an init system unit, an rc script, something
else -- is the template's business and lives behind the runner's platform seam.

**A container engine**, if and only if the template is intended for
`exec: container` tenants. Templates serving only `exec: host` tenants do not
need one.

## What a template must not have

**Language toolchains, on a container-execution template.** The point of that
mode is that toolchains arrive as digest-pinned images named by the tenant. A
compiler baked into the template is a compiler nobody declared and nobody can
pin, and it will be silently depended upon within a month.

Host-execution templates are the deliberate exception: their whole purpose is to
supply a toolchain the guest itself provides. That is a real cost -- the tenant
loses the ability to pin what it builds with -- and it is the reason
container execution is the default.

**Project state of any kind.** A template is not a fixture.

## Dataset layout

Firstboot creates a pool (`tank`) on the second disk and these datasets:

| Dataset | Contents | Rolled back? |
|---|---|---|
| `tank/images` | container image store | never |
| `tank/cache` | per-ecosystem build caches | never |
| `tank/state` | the tenant's own state | **yes**, freely |
| `tank/work` | synced tree, and results on the way out | never |

The split is the whole design. Rollback has to be cheap and total for `state`,
and must not touch the three things that make the next iteration fast or that
carry results outward.

Firstboot also caps the ZFS ARC (1--2 GB is the working figure) so the cache
does not compete with the workload under test for memory. A database and a
browser in the same machine will both lose that fight.

## Registering a guest

A template that meets this contract becomes available to tenants by being named
in the site registry. See [`site-config.md`](site-config.md). Tenants then name
it in `guests`.

The name is free-form and means nothing to the framework beyond "look this up in
the registry". It is deliberately not validated against a list of known
operating systems -- a closed list is the hardcoding this contract exists to
prevent.

## Platform differences, and where they live

Everything platform-specific lives in the runner's platform modules and nowhere
else. A lint guard fails the build if operating-system-specific identifiers
appear outside them.

The differences that have actually mattered so far:

| Concern | How it varies |
|---|---|
| ZFS tooling | Native on some systems, a package on others. The *commands* do not vary |
| Init | Unit files, rc scripts -- entirely different mechanisms |
| Second disk naming | Different device conventions; solved by discovery, not by matching |
| Container engine | Availability, privilege model, and whether it can run native binaries at all |

That last one is why `exec: host` exists. An engine that runs only foreign-format
images cannot execute a tenant's native binaries, and pretending otherwise would
produce a confusing failure deep inside a guest instead of a clear one at
validation time.

## Guests supported today

See [`STATUS.md`](STATUS.md). Template construction is Phase 2; this contract is
written ahead of it deliberately, so that the first two templates are built
*against* a contract rather than having one reverse-engineered out of them
afterwards. Expect this document to gain detail once real templates have been
built and the contract has met contact with reality.
