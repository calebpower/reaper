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

**ZFS**, whether native to the operating system or installed. The runner uses
`zpool create`, `zfs create`, `zfs snapshot` and `zfs rollback`, and nothing
exotic. This command surface is identical across the platforms supported so far,
which is what makes the core mechanism portable for free.

**A POSIX shell**, and the ordinary tools around it. The runner is a shell
script the CLI delivers over SSH at session start; nothing is compiled for a
guest and nothing reaper wrote lives in the template.

**SSH**, with the session public key trusted **for root**. SSH is the transport:
there is no listening daemon in the guest beyond it, and the runner is invoked
rather than resident.

Root rather than an unprivileged user with escalation, deliberately. A session
is a whole disposable machine and its blast radius is the sandbox -- the same
reasoning that makes rootful containers acceptable inside one. Requiring
escalation instead would add a difference between guests that has nothing to do
with the work: on one platform `sudo` is a package, on another `su` wants a
password. Templates may still carry an ordinary user for console debugging.

**A discoverable address.** The provider must be able to learn it without DNS or
mDNS -- a guest agent is the usual mechanism.

**`rsync`**, which is how a working tree gets in and results get out.

**A container engine**, if and only if the template is intended for
`exec: container` tenants. Templates serving only `exec: host` tenants do not
need one, and firstboot skips the image-store configuration when it finds none.

Note that execution mode is a property of a *verb* rather than of a guest, so a
template may well serve a tenant that builds in a container and runs on the
host. The requirement is unchanged: an engine is needed if any verb aimed at
this guest asks for one.

And "provides a container engine" means one that can **start a container**, not
one that is installed. The distinction is not pedantic: an engine missing the
packet-filter tooling its network backend drives installs cleanly, reports
itself healthy, and pulls images without complaint, failing only when something
tries to run. That is how a template shipped here in exactly that state. The
runner now proves it after a pre-pull, at the cost of one container start.

## What a template does *not* provide

Three things it might seem to need, and does not.

**Not the data disk.** The template carries only its boot disk. The provider
attaches a fresh disk when it creates a session, and firstboot makes the pool on
it. On storage without snapshots -- where every clone is a byte-for-byte copy --
a data disk in the template would be copied in full on every single session,
empty or not. Attaching it instead means a clone copies the boot disk alone, and
the pool's size becomes a per-session decision rather than one frozen when the
template was built.

**Not an init mechanism for reaper.** Nothing reaper owns runs at boot. The CLI
connects, delivers the runner, and invokes it. A runner living in the template
would mean rebuilding two hand-made templates every time it changed, and version
skew between a template and the CLI driving it.

**Not installation media.** reaper never fetches an ISO and never uploads one.
Media is expected to be on the provider already; a missing one is a request to
whoever administers the cluster. The credential agrees with the design here --
the harness token can allocate space but not templates, so it could not upload
media even if the design wanted it to.

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

## How the pool disk is chosen

Firstboot has to pick a disk to destroy, so the rule is written to fail closed.
It is **not** "the disk that is not the root disk" -- on a system with a ZFS root
`mount -p /` reports a dataset rather than a device, and any rule phrased that
way needs a special case per platform.

The rule is:

> A candidate is a whole disk that is **unused**: no partition table, no
> filesystem signature, not mounted, and not a member of any pool. Exactly one
> candidate must exist.

Zero candidates is an error. Two or more is an error. Refusing on ambiguity
rather than guessing is the whole point, because the cost of guessing wrong is
somebody's data.

What varies by platform is only *how the list of disks is obtained*. Which disk
gets chosen never varies.

If the pool already exists and is healthy, firstboot does nothing and succeeds.

## Host keys, and what is being trusted

A freshly cloned machine has a host key nothing has ever seen, so strict
checking would reject every first connection. reaper accepts the key on first
use, against a **per-session** known-hosts file -- so a session starts with no
history and cannot inherit a stale key from an address that has been recycled.

State the trust plainly: this trusts the provider's report of the address. An
attacker who could forge that, on the network between workstation and cluster,
could impersonate a session. That is the same trust already placed in the
provider to create the machine at all, so it adds no new party -- but it is a
real assumption and it belongs written down rather than implied.

## Dataset layout

Firstboot creates a pool (`tank`) on the attached disk and these datasets:

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

## Inside those datasets

The runner makes two more layers, on demand rather than up front:

| Path | What |
|---|---|
| `tank/work/<project>` | one project's synced tree |
| `tank/work/<project>/out` | where that project's results are collected from |
| `tank/cache/<name>` | one directory per cache the tenant declared |

Directories rather than datasets, deliberately. A dataset per cache would buy
per-cache `zfs list` figures and nothing else, since `reset` never touches
`cache` whichever it is.

The results directory is made before the first job runs. Making it lazily would
mean the first attempt to fetch results failed on a directory that never
existed, and that failure reads as though results had been lost.

## Where a container sees them

Under container execution the runner mounts three things, at paths that are
fixed and documented rather than configurable:

| Inside | From |
|---|---|
| `/reaper/work` | `tank/work/<project>` |
| `/reaper/cache/<name>` | `tank/cache/<name>` |
| `/reaper/job.sh` | the rendered job, read-only |

Fixed because these paths reach a tenant's environment as `REAPER_WORK` and
`REAPER_CACHE_*`, and a site that moved them would quietly break every manifest
written against the documented ones. Read-only for the job because nothing
inside a container has any business rewriting what it was asked to run.

The runner is the only component that knows any of this. The CLI asks it where a
workspace is rather than working the path out for itself, so pool layout stays
one component's business.

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
| Enumerating disks | Entirely different tools; the *choice* among them does not vary |
| Capping the ARC | Different knob, different file to persist it in |
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
