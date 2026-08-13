# The sweeper

An independent backstop that destroys ephemeral machines whose expiry has
passed. It is the reason a crashed CLI, a closed laptop or a killed terminal
cannot leak a machine.

Organised by provider, because a sweeper is provider-specific: it talks to a
hypervisor's API directly, and deliberately shares no code with reaper. A
backstop that imported the thing it is backing up would fail in the same ways at
the same times.

| Provider | Sweeper |
|---|---|
| Proxmox | [`proxmox/cull.sh`](proxmox/cull.sh) |

## What it will and will not do

It destroys a machine only when **all** of the following hold:

- it is a member of the configured pool;
- its identifier is inside the configured range;
- it carries a tag `expires-<unix-epoch>` that is now in the past.

Anything in the pool **without** a valid expiry tag is reported and left alone.
That is deliberate and worth understanding: a missing tag means a create failed
part-way through, and deleting on an unknown state is the wrong answer to
uncertainty. Those reports should be rare, and each one is a bug worth chasing.

The identifier range is a second opinion. The credential is already scoped to
the pool, so the range check only matters when pool membership itself was set up
wrongly — which is exactly the case where you want another guard.

## Deployment stays manual, and outside this repository

The sweeper runs on its own machine, under its own unprivileged user, with its
own credential — one that is deliberately more powerful than the harness
credential in the one respect that matters, and is therefore never present on a
workstation.

**Nothing automated deploys this, and no tooling that works on this repository
should ever hold that credential.** Adopting a new version means a person
copying it across and reading the diff first. The whole value of a backstop is
that it fails independently of the thing it is backing up.

Note the credential file is not the same shape as the CLI's. The sweeper sources
a file that sets `PVE_TOKEN` to the entire authorization header value; the CLI
reads a file containing just `user@realm!name=secret`. They live on different
machines and neither should learn about the other, but the difference will
surprise someone who has seen only one.

## Testing it

```sh
./proxmox/test/run.sh          # decisions, fully mocked, nothing contacted
./proxmox/cull.sh --dry-run    # against a real API; reports, destroys nothing
```

The self-test stubs `curl` and `date` but uses the real `jq`, so the filter that
decides which guests are even considered is the one that ships. Assertions are
made against the log of calls actually issued rather than exit codes, because
"it refused" and "it refused without touching anything" are different claims.
