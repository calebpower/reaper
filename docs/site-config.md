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

## The file

`config.toml`, in one of the directories above. `REAPER_CONFIG` overrides the
search entirely when you want to be explicit.

```toml
provider = "proxmox"

[session]
default_ttl        = "2h"    # how long a session lives without a heartbeat
heartbeat_interval = "5m"    # how often the CLI renews it
ready_grace        = "30m"   # the first expiry, covering creation
max_concurrent     = 2       # counted across the provider, not per workstation
default_disk_gb    = 64      # size of each session's storage pool
rsync_command      = "rsync"  # the binary that moves the tree and the results
results_interval   = "5s"    # how often results are pulled while a command runs
ssh_user           = "root"  # who reaper connects as; see docs/guests.md
ssh_key            = "~/.config/reaper/session-key"  # the key every template trusts
ssh_command        = "ssh"   # or a wrapper
ssh_connect_timeout = "15s"

# The guest registry. Names are free-form and mean whatever you say they mean;
# a tenant asks for one by name and the framework looks it up here. Quote keys
# containing dots, as TOML requires.
[guests."ubuntu-26.04"]
template = "9001"

[guests."freebsd-15.1"]
template = "9004"

# The selected provider's own table. reaper carries this through without
# reading it; what the keys mean is the provider's business.
[proxmox]
api        = "https://node.example:8006"
node       = "somenode"
pool       = "a/pool"
id_range   = [9000, 9099]
token_file = "~/.config/reaper/token"
data_storage = "some-storage"  # where each session's blank pool disk is made
min_free_gb  = 10              # room to leave on a storage after a session takes its share
task_timeout = "10m"           # how long to wait on a clone or a destroy
sweep_within = "15m"           # doctor: how long past expiry before the sweeper is presumed absent
request_timeout = "30s"        # a single API call
data_bus     = "virtio1"       # which slot it hangs on; templates boot from virtio0
tls        = "ca-file"       # webpki | ca-file | insecure
ca_file    = "~/.config/reaper/node-ca.pem"
```

**`max_concurrent` counts sessions on the provider**, not sessions in your own
session file. The things a cap protects -- identifiers and storage -- belong to
the cluster, and a limit that only saw your own sessions would stop being a
limit the moment a second person shared the hardware. So a refusal may name
sessions that are not yours, and says so when it does.

**`min_free_gb` is the room left behind.** Before cloning, reaper prices the
session -- the template's disks, which are copied whole on storage without
snapshots, plus the blank pool disk -- and refuses if that would leave a storage
with less than this. A clone that fills a shared storage takes down everything
else living on it, and the failure otherwise arrives minutes in with a
half-copied disk to clean up. A storage that cannot be queried is not treated as
full: not knowing is not the same as knowing there is no room.

Every value is checked when it loads, and unacceptable combinations are refused
rather than assumed: a provider with no table, an empty registry, a guest with
no template, a heartbeat that does not fit at least three times into the TTL.
That last one is a margin rather than a formality -- it is what lets two
renewals fail before a machine is lost.

`ssh_command` and `rsync_command` exist so a site can point at a wrapper, and
because it is what lets the whole path be exercised with no network at all.
`results_interval` is how often a run's output is fetched back *while it is
still running*; the shorter it is, the less of a trace can be lost when a
session is taken down mid-run, and the more often a small transfer happens.

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

### The token file

One file, holding the whole credential on one line:

```
user@realm!name=secret
```

Not an identifier in configuration beside a secret in a file: one secret, one
place, nothing to keep in step. reaper refuses to read it if anyone but you can
-- the same rule ssh applies to a private key, and for the same reason.

```sh
chmod 600 ~/.config/reaper/token
```

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

## Transport security

`tls` takes three values, and there is no default -- the choice is too
consequential to make on somebody's behalf.

| Value | Meaning |
|---|---|
| `webpki` | Ordinary public trust roots. Right when the node has a publicly-issued certificate |
| `ca-file` | Trust one specific authority. Right for a node whose certificate comes from an internal CA, which is most of them |
| `insecure` | No verification at all. Warns on every invocation |

`insecure` is honest rather than forbidden, because the alternative is people
disabling checks in ways nobody can see. It prints a warning every time reaper
runs, and that warning is the only thing keeping it temporary.

To move off it you need the certificate authority that issued the node's
certificate -- not the node's own certificate, which is a leaf and cannot serve
as a trust anchor. A hypervisor typically does not publish its CA over the API,
so the practical route is to open the web interface in a browser, inspect the
certificate, and export the *issuer*. Save it as PEM, point `ca_file` at it, and
change one line.

Also worth knowing: plain HTTP is refused outright unless the host is loopback.
A credential travelling in a header over a plaintext link to another machine is
a credential you have given away.

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
