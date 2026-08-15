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

sync:
  exclude: [/target/]              # optional; rsync patterns

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

Per-guest keys override the top-level defaults, key by key within the block:
override `build: {env: ...}` and the top level's `cmd` is still inherited. But
each key is replaced whole, and `env` is one key -- override one variable and
every top-level variable the guest did not restate is gone, not merged under
yours. Restate the whole `env` you mean.

## Container execution or host execution

`exec` must be stated -- at the top level, per guest, or per verb; a manifest
that never says is refused rather than guessed at. `exec: container` is the
better answer where it fits: your toolchain arrives as a digest-pinned image,
the guest template stays generic, and what you build with is exactly what you
declared.

`exec: host` runs your commands directly in the guest, with the toolchain
supplied by the template. Choose it when containers cannot give you what you
need -- a suite that must exercise the host operating system's own facilities is
the clear case, and a guest whose container engine cannot run native binaries is
another.

The tradeoff is real and worth naming: host execution moves your toolchain from
something you pin in a manifest to something the sysadmin bakes into a template.
You gain access to the host; you lose the guarantee that the toolchain is what
you said it was. Pick deliberately.

### And you may pick differently per verb

`exec` at the top level is a default. Either verb may override it:

```yaml
exec: container
build:
  image: registry/jdk@sha256:...
  cmd: ./gradlew shadowJar
run:
  exec: host                 # brings up a pod, so it needs the engine
  cmd: e2e/run.sh
```

This is not a nicety. A toolchain image carries a compiler and no
container-engine client, so a `run` that orchestrates containers cannot execute
inside one -- while a `run` that is simply `cargo test` very much wants to. Both
are ordinary, and neither is expressible if execution mode belongs to the guest.

A container-execution `run` that names no `image` of its own runs in
`build.image`, so a project whose two verbs share one toolchain writes the
digest once. A host-execution verb never inherits one.

### If your tests drive containers, containerize the driver too

A container-execution verb is given **no container-engine socket**, so it cannot
start sibling containers. That is deliberate, and it is why per-verb `exec`
exists. But it leads somewhere that is worth spelling out, because the first
tenant to meet it drew the wrong conclusion:

> "My battery brings up three containers, so `run` must be `exec: host`. Host
> execution forbids an image, so the toolchain has to come from the guest. No
> guest here carries a toolchain — so no guest can run my tests."

The last step does not follow. A host-execution `run` is a shell command on a
guest that **does** have a container engine, so it can orchestrate as much as it
likes. What it cannot do is supply node, or python, or a JDK.

So put those in a container too. Bring up your stack *and your test driver* as
containers from a `run.cmd` that uses nothing but the engine:

```yaml
exec: container
build:
  image: registry/toolchain@sha256:…   # compiles, bundles, produces artifacts
  cmd: make build
  cache: [deps]
run:
  exec: host                           # only needs the engine, no toolchain
  cmd: e2e/run.sh                      # which starts db, app, and the driver
```

Now the guest needs no toolchain at all, and every version your tests depend on
is digest-pinned by you rather than baked into a template by a sysadmin. This is
the shape `manifest/examples/yasss.reaper.yaml` demonstrates.

The alternative — a guest carrying node and python, registered as
host-execution — is legitimate and the guest contract allows it. But weigh it:
you move your toolchain from something you pin to something someone else
upgrades, and it tends to multiply into a guest per toolchain combination.
Bootstrapping a toolchain inside `build.cmd` is the third option, and an honest
staging post; pin what you download by checksum, because the digest guarantee
the schema enforces for `image` does not reach a `curl`.

## The loop

Four verbs, and the first is the only one that costs minutes.

```sh
reaper up            # clone a machine, build its pool, fetch your images
reaper sync          # your tree in, results back out
reaper build         # your build command
reaper run           # your run command
reaper down          # destroy it, results collected on the way
```

Every verb acts on all of this project's sessions, so a manifest naming two
guests tests both with one command. Name a session to act on just one.

### What `test` skips, and why

A step with nothing to do is skipped and says so, rather than failing:

| Skipped when | |
|---|---|
| no `build` block | a project whose test command needs no build step is ordinary |
| no `reset.datasets` | nothing to roll back |
| no `@pristine` yet | the **first** `test` on a session has nowhere to reset to; that run takes the snapshot, and every later `test` gets all four steps |

That last one has a consequence worth knowing: a run that *fails* takes no
pristine, so a session whose first run never succeeded keeps skipping the reset
until one does.

### What sync does, and what it deliberately does not

Forward, it is a delta copy of your tree with **deletions mirrored**: a session
still holding a file you removed is not the tree you are testing. `.git` goes
with it -- never needing a commit is the point, plenty of builds stamp a version
out of it, and after the first sync the deltas make it cheap. Anything you would
rather not send goes in `sync.exclude`, as rsync patterns; a leading slash
anchors one at the top of your tree.

Backward, it is `out/` and only `out/`, and it **never deletes**. The session is
authoritative for what it produced; it is not authoritative for what was in your
`out/` beforehand.

`out/` is excluded from the forward direction whatever else you write, because
mirroring deletions into the directory results arrive in would destroy them.

### Results arrive while a run is happening

Not at the end. A trace is fetched back every few seconds for as long as your
command runs, once more when it stops -- whether it passed or failed -- and once
more when you take the session down. A failure trace that exists only on a
machine scheduled for destruction is a failure trace nobody reads, and the runs
worth investigating are exactly the ones that end with somebody giving up.

### Where your command runs, and what it is told

Working directory is your synced tree. Three kinds of variable are set for you,
with the same names in both execution modes so your commands need not know
which one they got:

| Variable | What it is |
|---|---|
| `REAPER_WORK` | your tree |
| `REAPER_OUT` | where results are collected from |
| `REAPER_STATE` | your state -- the only thing `reset` rolls back |
| `REAPER_CONTROL` | where the reset trigger lives |
| `REAPER_CACHE_<NAME>` | one per name in `build.cache`, uppercased, `-` and `.` becoming `_` |

Put anything you want `reset` to undo under `REAPER_STATE`, and nothing else
there. A database's data directory belongs in it; your build output does not.

### Your command runs under `/bin/sh` — mind the exit status

This one has already caught somebody, and it fails in the worst direction.

`/bin/sh` is dash on at least one guest here, and **dash has no `pipefail`**. So
the obvious thing:

```yaml
cmd: make test-e2e | tee $REAPER_OUT/e2e.log      # WRONG
```

exits with **tee's** status. A failing suite is reported as a pass, `reaper
test` exits zero, and — if you declare a reset dataset — `@pristine` is then
taken *on the strength of that false pass*, so every later reset returns you to
a broken state.

Either avoid the pipe:

```yaml
cmd: make test-e2e > $REAPER_OUT/e2e.log 2>&1
```

or own the status explicitly, by pointing `cmd` at a script of your own with a
`#!/bin/bash` line and `set -euo pipefail`. reaper does not run your command
under bash for you: bash is not in the base system on every guest, and choosing
a shell you did not ask for would be a worse surprise than this one.

`&&` chains are safe, which is why reaper's own manifest happens to dodge this.

Your `cmd` is handed to a shell, so it may use those:

```yaml
cmd: CARGO_TARGET_DIR=$REAPER_CACHE_TARGET cargo build --locked
```

Values under `env` are **not** expanded -- they are passed through exactly as
written, and the framework never interpolates or interprets one. If you want
expansion, it belongs in the command.

**Put anything large in a cache, not on the guest's root disk.** A template's
boot disk is small — the Ubuntu one has under 4 GiB free — and it is not scratch
space. Managed language runtimes, dependency trees and browser bundles all
belong under `$REAPER_CACHE_*`, which lives on the session's own pool with tens
of gibibytes. This is load-bearing rather than an optimisation: a tenant that
lets `uv`, `npm` or Playwright write to the default location under `$HOME` will
run the root filesystem out of space.

Caches are declared once, under `build.cache`, and are given to every verb. A
profile with `warm_cache: false` gives none of them to any verb: not an unset
variable but no cache at all, because a cache reachable by a path you could
guess would quietly defeat the one thing determinism mode establishes.

## Digests, not tags

Every image reference is pinned by digest. The schema rejects tags outright.

This is not fussiness. A tag is a moving target, and a test run that cannot say
which bytes it ran is a test run whose result does not mean anything tomorrow.
Digest pinning also means a registry outage costs you availability and never
correctness -- you may not be able to pull, but you can never pull the wrong
thing.

## What `reset` actually does

```sh
reaper reset                       # back to pristine
reaper snapshot before-the-change  # name a point
reaper reset --to before-the-change
```

1. Stop your containers.
2. Refuse if anything still has the dataset open.
3. `zfs rollback` each dataset you named under `reset.datasets`.
4. Exit. The next `run` restarts your stack.

Restarting rather than resuming is deliberate: a long-lived process holding
credentials, sessions or caches from *before* the rollback must never survive
it. If your stack replays migrations on boot, that replay is part of what you
are testing, and you want it.

**Step 2 is done here rather than left to ZFS**, and that is worth knowing
because the opposite was assumed while building this. ZFS does *not* refuse a
rollback on a dataset with open files: measured on a live guest, with a
descriptor verifiably open on a file inside the dataset, `zfs rollback`
succeeded, the file vanished, and the holder carried on reading an inode that
no longer had a name. So reset looks for holders itself and refuses, naming
them. A process holding a file that is *already* unlinked is the one exception:
a rollback cannot reach it, and counting it would let one leaked process veto
every reset for the life of the session.

### How long it takes

**Under three seconds**, and -- the part that matters -- the same three seconds
whether there is 100 KB of state or 800 MB. Measured on a live session:

| State rolled back | Wall time |
|---|---|
| 104 KB | 2.9s |
| 801 MB | 2.9s |

ZFS discards blocks rather than rewriting them, so the cost is in the two SSH
round trips and not in your data. That is the whole argument for this design
over rebuilding a stack per test.

### `@pristine`, and when it is taken

A session takes `@pristine` automatically after its **first successful run**,
and never after a failed one -- a pristine that returns you to a broken state
is worse than having none.

Be clear about what that captures: the state after the *whole* run, test
residue included, not the state right after your stack came up. Nothing here
can tell those apart, because your `run.cmd` is opaque and "the stack is up
now" is your vocabulary rather than the framework's. If you want a tighter
point, call `reaper snapshot <name>` at the moment you choose and reset to that
instead.

Rolling back to a snapshot discards snapshots taken after it, named ones
included. That is ZFS's behaviour rather than a choice made here.

## Resetting from inside your own stack

A driver container that runs journeys usually wants to reset *between* passes,
without knowing that ZFS exists or being able to run commands on the guest. So
the guest listens:

```sh
"$REAPER_CONTROL/reset"             # roll back; blocks until it is done
"$REAPER_CONTROL/reset" some-name   # or to a named point
"$REAPER_CONTROL/snapshot"          # mark *here* as pristine
"$REAPER_CONTROL/snapshot" mid      # or under a name of your own
```

`REAPER_CONTROL` is set for you, and under container execution it is mounted.
Mount it into your own containers if they need it.

`snapshot` is how you get a **tight** pristine. Call it the moment your stack has
finished coming up and before any test has touched anything, and that is the
point every later reset returns to -- rather than the end-of-run point reaper
takes for you if you never say. It keeps the first one it is given, so calling
it on every run is the intended use: a named point does not move under you.

Then drive the loop from it:

```sh
reaper test --to after-stack-up
```

`test` resets to `pristine` unless told otherwise. A name you pass that does not
exist is an error rather than a skip -- far more likely a typo than an
intention, and skipping it would run your tests against whatever state happened
to be lying about.

**The caller survives.** Stopping the container that asked for the reset would
look exactly like the reset having crashed, so it is spared -- identified by
its hostname, which a container engine sets to the container's id.

Two things to know. Only one request is served at a time, and the wrapper gives
up after five minutes rather than waiting forever on a guest that has stopped
listening. And anything that can reach that directory can trigger a rollback of
your state: inside a single-tenant disposable machine that is the point, but it
is worth knowing rather than discovering.

## What belongs in a session, and what does not

Sessions are for the tiers that need a real machine: a full containerized stack,
simulated users over accumulated history, a live browser audit.

Unit tests, contract tests and browser-against-a-fake belong on your
workstation. They are fast because they avoid all of this. Moving them into
sessions because sessions exist slows your loop and finds nothing new.
