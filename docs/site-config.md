# Site configuration

You run the hypervisor. This is what you configure, and where.

Nothing here lives in the repository, and nothing here lives in a tenant's
manifest. Site configuration is the third thing, owned by the person who runs
the infrastructure, and it is what lets a tenant say "run me on
`some-guest-name`" without either of you editing framework code.

## Where it lives

```
~/.config/reaper/          per-user
/etc/reaper/               system-wide
```

Per-user takes precedence. Credentials live here and **never** in a repository;
`.gitignore` carries entries for the obvious mistakes, but the real protection
is that this directory is not inside a checkout.

## What it holds

Three things:

**Which provider to use, and how to reach it.** Endpoint, credential location,
TLS policy. See [`providers.md`](providers.md).

**The guest registry** -- the list of templates that exist here, each mapping a
free-form name to whatever the provider needs in order to clone it. This is the
list a tenant's `guests` entries are resolved against. Adding an operating
system is an entry here plus a template build; it is never a code change.

Choose names that describe what a tenant is asking for rather than what you
happen to have built this quarter. A tenant that names a version-pinned guest
has to be edited when you rebuild it; a tenant that names the *role* does not.

**Operational limits** -- concurrency caps, free-space floors, default TTLs.
These protect shared storage from a runaway loop, and they belong to you rather
than to any tenant, because the tenant cannot see what else is running.

## Credentials

Two credentials, deliberately separate, and the separation is the design:

**The harness credential** is held by the CLI. It can create, configure, tag,
start, stop and destroy machines within its allotted scope, and it can do
nothing outside that scope. This is the one a developer's workstation holds, and
the one an agent working on this codebase may be given.

**The sweeper credential** is held only by the sweeper's own machine. It is
never present on a workstation, never in a checkout, and never handled by
tooling that works on this repository. The sweeper is the backstop for
everything else going wrong, including a compromised or confused harness
credential, and a backstop reachable from the thing it is backing up is not one.

Both should be scoped to the minimum the contract in [`providers.md`](providers.md)
requires -- never to an account's full authority. If your hypervisor supports
privilege-separated tokens that inherit nothing from the account that created
them, use them.

## The expiry contract

Every ephemeral machine carries an expiry. The CLI renews it while a session is
alive; the sweeper destroys machines whose expiry has passed. That is the whole
dead-man's switch: expiry means *the operator vanished*, not *the tests took
too long*.

Two consequences worth setting up for:

**TTL is measured from readiness, not from the create request.** On storage
where cloning is a full copy, creation is slow, and a TTL that started ticking
at request time would collect machines that never got used.

**A machine with no expiry tag is not the sweeper's to destroy.** It is logged
and left for a human, because an untagged machine means creation half-failed and
guessing is the wrong instinct. Watch your sweeper's log for these; they should
be rare, and each one is a bug.

## Verifying it

Once the CLI exists, `reaper doctor` checks this configuration end to end:
provider reachable, every registered guest's template actually present,
registry internally coherent, sweeper alive. Until then, see
[`STATUS.md`](STATUS.md) for what can be checked by hand.
