# The tenant contract

You have a project with an end-to-end battery. This is what it takes to run it
in a `reaper` session, and -- just as important -- what the framework will never
do for you.

## The integration surface is one file

`.reaper.yaml` at your project root. That is the whole of it. There is no
plugin to write, no interface to implement, no callback to register. If running
your project here requires editing framework code, the framework has a bug.

See `manifest/schema/v1.json` for the normative schema and
`manifest/examples/` for worked examples.

```yaml
schema: 1
project: my-project

guests: [some-guest-name]        # what the sysadmin registered
exec: container                  # or: host

build:
  image: registry/toolchain@sha256:...   # container exec only
  cmd: make build
  cache: [whatever-you-call-it]

run:
  cmd: make e2e
  images: [registry/thing@sha256:...]    # optional; pre-pulled for you

reset:
  datasets: [state]

resources: { cores: 4, ram_gb: 8 }
```

## What the framework promises

**Your uncommitted tree, as it is.** Sync is a delta copy of your working tree,
deletions mirrored. You never commit to run your tests.

**A machine that is yours alone**, destroyed when you are done, and destroyed
anyway if you vanish.

**`reset` returns your state to pristine.** Whatever your stack built the first
time it came up is what you get back, every time, in seconds rather than by
rebuilding it.

**Results come back out.** Traces, artifacts and screenshots written under
`out/` in the guest land in your working tree, continuously -- not only at the
end. A failure trace must never exist only on a machine scheduled for
destruction.

## What the framework will never do

**Construct or seed your state.** This is the load-bearing one. `@pristine` is
whatever *your* stack-up produced, hostility included. If your database starts
in a deliberately awkward charset so that your migration is load-bearing, that
is what gets snapshotted, and that is what rollback returns you to. A
convenience that pre-seeded a friendly database would silently defeat both the
migration test and the "no backdoors" rule -- rollback-to-pristine is legitimate
*exactly because* the snapshot was earned through the real path once.

**Learn your vocabulary.** `run.cmd` is an opaque string. The framework does not
know what a stage is, what a journey is, what a seed is, or what `--only` means
to you. It will not grow a flag for your test selector.

**Special-case your project.** No tenant name appears in framework code, and a
lint guard fails the build if one does.

**Decide what to test.** Which tiers you run, and in what order, is yours.

## Choosing your guests

`guests` names entries from the sysadmin's registry (see
[`site-config.md`](site-config.md)). Name one, or several -- a project that must
work on more than one operating system says so, and gets a session per guest.

Two forms. Shorthand, when every guest runs the same way:

```yaml
guests: [ubuntu-lts]
exec: container
build: { image: ..., cmd: ..., cache: [...] }
run:   { cmd: ... }
```

Expanded, when they differ:

```yaml
guests:
  - name: some-bsd
    exec: host                  # no container indirection
  - name: ubuntu-lts
    exec: container
    build: { image: ... }       # this guest needs a toolchain image
build: { cmd: make test, cache: [obj] }   # defaults both guests inherit
run:   { cmd: make e2e }
```

Per-guest keys override the top-level defaults. Anything you do not override is
inherited.

## Container execution or host execution

`exec: container` is the default and the better answer where it fits. Your
toolchain arrives as a digest-pinned image, the guest template stays generic,
and what you build with is exactly what you declared.

`exec: host` runs your commands directly in the guest, with the toolchain
supplied by the template. Choose it when containers cannot give you what you
need -- a suite that must exercise the host operating system's own facilities is
the clear case, and a guest whose container engine cannot run native binaries is
another.

The tradeoff is real and worth naming: host execution moves your toolchain from
something you pin in a manifest to something the sysadmin bakes into a template.
You gain access to the host; you lose the guarantee that the toolchain is what
you said it was. Pick deliberately.

## Digests, not tags

Every image reference is pinned by digest. The schema rejects tags outright.

This is not fussiness. A tag is a moving target, and a test run that cannot say
which bytes it ran is a test run whose result does not mean anything tomorrow.
Digest pinning also means a registry outage costs you availability and never
correctness -- you may not be able to pull, but you can never pull the wrong
thing.

## What `reset` actually does

1. Stop your stack.
2. `zfs rollback` each dataset you named under `reset.datasets`.
3. Exit. The next `run` restarts your stack.

Restarting rather than resuming is deliberate: a long-lived process holding
credentials, sessions or caches from *before* the rollback must never survive
it. If your stack replays migrations on boot, that replay is part of what you
are testing, and you want it.

`reset` refuses to roll back underneath a live stack. It stops first, or it
fails -- it never rolls the filesystem out from under a running process.

## What belongs in a session, and what does not

Sessions are for the tiers that need a real machine: a full containerized stack,
simulated users over accumulated history, a live browser audit.

Unit tests, contract tests and browser-against-a-fake belong on your
workstation. They are fast because they avoid all of this. Moving them into
sessions because sessions exist slows your loop and finds nothing new.
