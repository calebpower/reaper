# Template runbooks

One runbook per guest. Each builds a template by hand in the hypervisor's web
interface, because that is the only route available on an account with no node
shell.

| Runbook | Guest | Execution mode it serves |
|---|---|---|
| [`ubuntu.md`](ubuntu.md) | Ubuntu 26.04 LTS | `container` |
| [`freebsd.md`](freebsd.md) | FreeBSD 15.1-RELEASE | `host` |

## What every template is, and is not

A template is an operating system with ZFS, SSH, a guest agent, and nothing
else. In particular:

**No reaper code.** The runner is a shell script the CLI delivers over SSH at
session start. That is deliberate: a runner living in the template would mean
rebuilding two hand-made templates every time it changed.

**No data disk.** The template carries only its boot disk. The provider attaches
a fresh data disk when it creates a session, so a clone copies the boot disk
alone and the pool's size is a per-session decision. On storage without
snapshots — where every clone is a full copy — this is the difference between
copying eight gigabytes and copying eighty.

**No installation media.** reaper never fetches an ISO. Step one of each runbook
verifies the media is already there; if it is not, that is a request to whoever
administers the cluster.

**No project state.** A template is not a fixture.

## Before you start either one

You need the web interface and nothing else — no harness token, no node shell.

Decide two things first, because both runbooks ask for them:

1. **Which identifier.** Templates live in the same range reaper is configured
   to touch, so that reaper's own refusals cover them too. Pick one that is
   free.
2. **Which public key.** Sessions are reached over SSH as a single unprivileged
   user. Have the public half of that key to hand; you will paste it during the
   install.

## After you finish either one

Record what you actually built. A hand-made template is a pet, and the only
thing that makes it reproducible is a written account of what went into it:

- the exact package versions the runbook asks you to print;
- the identifier you used, so it can go in the site registry;
- anything you had to do differently from the runbook, which is a bug in the
  runbook.

Then register it. See [`../site-config.md`](../site-config.md).
