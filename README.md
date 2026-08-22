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

> **Status: Phase 5.** A session can be brought up, listed, renewed and taken
> down; two guest templates exist and are proven; a working tree can be synced
> in, built, run, and its results collected back out; and tenant state rolls
> back in under three seconds. See [`docs/STATUS.md`](docs/STATUS.md) for
> where the work actually stands, which is the document to trust over this one
> when they disagree.

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
`.reaper.toml`, talks to a provider, and talks to the runner over SSH. It owns
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
- **Not a media manager.** reaper never fetches an installation image, and never
  uploads one. Media is expected to be on the provider already; a missing ISO is
  a request to whoever administers the cluster, exactly like a missing template.

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

## How to use it

Three audiences, in the order they appear: whoever administers the cluster,
whoever onboards a project, and whoever runs the loop all day.

### 1. Build the CLI

```sh
cargo build --release          # target/release/reaper
```

Tagging `v<version>` builds it for x86-64 and arm64 Linux and x86-64 and arm64
FreeBSD, and attaches the binaries to the release alongside a `hashes.txt` that
`sha256sum -c` reads directly. Each is built against the oldest system it is
meant to run on -- Debian 12's glibc, and FreeBSD 14 -- because both platforms
are forward compatible and neither is backward compatible, so building on the
newest thing available produces a binary that will not start on anything else. Each is a single self-contained file: the runner
is compiled in, so there is nothing to unpack and nothing to install beside it.
There is no Windows build, and that is a property of the program rather than of
the pipeline -- see the header of
[`.github/workflows/release.yml`](.github/workflows/release.yml).

Nothing is installed into a guest. The runner is compiled into the binary and
delivered over SSH on every operation, so upgrading reaper never means
rebuilding a template.

> Building on FreeBSD or another BSD? Build natively. There is no
> cross-compilation here, which is also why `rustls` is mandatory and
> `native-tls` is banned -- an OpenSSL linkage difference between build hosts is
> exactly the kind of portability failure that shows up late and confusingly.

### 2. Set up a site *(sysadmin, once)*

**Build a guest template** and register it. Follow
[`docs/runbooks/`](docs/runbooks/) exactly; both runbooks record defects found
the hard way, and both templates here shipped subtly broken at least once. The
guest contract is [`docs/guests.md`](docs/guests.md).

**Write the site registry** at `~/.config/reaper/config.toml`
(`$XDG_CONFIG_HOME/reaper/`, `/etc/reaper/`, or `REAPER_CONFIG` to be explicit):

```toml
provider = "proxmox"

[session]
default_ttl        = "2h"     # how long a session lives without a heartbeat
heartbeat_interval = "5m"     # must fit at least three times into the TTL
ready_grace        = "30m"    # the first expiry, covering a slow clone
max_concurrent     = 2        # across the provider, not per workstation
default_disk_gb    = 64
ssh_key            = "~/.config/reaper/session-key"

[guests."ubuntu-26.04"]       # the name tenants ask for
template = "9001"             # opaque; the provider reads it

[proxmox]
api          = "https://node.example:8006"
node         = "somenode"
pool         = "a/pool"
id_range     = [9000, 9099]
token_file   = "~/.config/reaper/token"
data_storage = "some-storage"  # where each session's blank pool disk is made
min_free_gb  = 10              # room to leave after a session takes its share
tls          = "ca-file"       # webpki | ca-file | insecure
ca_file      = "~/.config/reaper/node-ca.pem"
```

`data_storage` is not optional in practice: without it, the first `up` that
asks for a session disk is refused, and every session asks for one.

The credential is one line, `user@realm!name=secret`, in a file only you can
read. Full reference: [`docs/site-config.md`](docs/site-config.md).

**Deploy the sweeper** from [`cull/`](cull/) on a *different* machine with its
*own* credential. This is the backstop that destroys anything whose expiry has
passed, and it is what makes every other failure mode survivable. Run it from
cron; `--dry-run` first. Read [`cull/README.md`](cull/README.md) before you do:
it is short, and it is the only place the deployment itself is written down --
[`docs/site-config.md`](docs/site-config.md) says why that credential is
separate, not what the file looks like. Two things in it will otherwise cost
you an evening:

- **The credential is not the same shape as the CLI's.** The sweeper *sources*
  a file that sets `PVE_TOKEN` to the entire authorization header value; the
  CLI reads a file containing just `user@realm!name=secret`. They live on
  different machines and neither should learn about the other, which is exactly
  why nothing warns you when you write one and expect the other.
- **The cron interval and `sweep_within` are the same decision.** `doctor`
  reports the sweeper absent when something has been expired for longer than
  `sweep_within` ([`docs/site-config.md`](docs/site-config.md), default 15m),
  so a sweeper on a slower schedule than that is a healthy site that reports
  itself broken. Set the interval comfortably under it.

Nothing here deploys it, and nothing here should: adopting a new version means
a person copying it across and reading the diff first. A backstop that shared a
deployment path with the thing it backs up would fail at the same moment.

### 3. Onboard a project *(once per project)*

Write `.reaper.toml` at its root. That is the entire integration surface -- no
plugin, no callback, no framework edit:

```toml
schema = 1
project = "my-project"
guests = ["ubuntu-26.04"]     # what the sysadmin registered
exec = "container"            # or: "host"

[build]
image = "docker.io/library/rust@sha256:3382bd…"   # digest, never a tag
cmd = "cargo build --locked --tests"
cache = ["cargo", "target"]

[run]
cmd = "cargo test --workspace"
images = []                   # pre-pulled for you if you list any

[sync]
exclude = ["/target/"]        # rsync patterns; out/ is always excluded

[reset]
datasets = ["state"]

[resources]
cores = 4
ram_gb = 8
```

Check it before you need it:

```sh
reaper-manifest-validate .reaper.toml
```

Three things to get right, because they are where projects actually stumble:

- **State must live under `$REAPER_STATE`.** `reset` rolls back that dataset and
  nothing else. A database whose data directory sits inside its container has no
  state reaper can roll back, and reset will appear to do nothing.
- **`exec` is per verb.** A build often wants a pinned toolchain image while the
  run needs the guest's own container engine -- a toolchain image has no engine
  client in it. Say `run: { exec: host }` when that is the shape.
- **Images are digest-pinned.** Tags are refused outright, including
  `repo:tag@sha256:…`, where the tag is unverified and can drift away from the
  digest while looking checked.

Worked examples: [`manifest/examples/`](manifest/examples/). Full contract:
[`docs/tenants.md`](docs/tenants.md).

### 4. The loop *(all day)*

```sh
reaper up                    # a machine of your own, images already fetched
reaper test                  # sync -> build -> reset -> run
reaper down                  # gone, and gone anyway if you vanish
```

Each step is also a verb of its own, for when you want one without the others:

| | |
|---|---|
| `reaper sync` | your uncommitted tree in, results back out |
| `reaper build` | your build command, in your pinned toolchain |
| `reaper run` | your run command; traces arrive *while* it runs |
| `reaper reset [--to NAME]` | state back to a known point, in seconds |
| `reaper test --to NAME` | the loop, resetting to a named point rather than pristine |
| `reaper snapshot NAME` | name a point to come back to |
| `reaper list` | what is up, how long it has left, whether its heartbeat is alive |
| `reaper renew [--ttl 4h]` | more time |

Useful flags: `--profile nightly` picks a profile (its TTL, its environment, and
whether caches are warm); `--manifest path` points at a project other than the
current directory; most verbs take a session name to act on just one.

**Results come back while a run is happening**, not at the end -- every few
seconds into `out/` in your tree, again when the command stops whether it passed
or failed, and once more on `down`. A failure trace should never exist only on a
machine scheduled for destruction.

**The first `test` on a session skips the reset**, because there is nothing to
reset to yet; that run takes `@pristine` and every later `test` gets all four
steps. A run that *fails* takes no snapshot, so a session whose first run never
succeeded keeps skipping until one does.

**Resetting from inside your own stack**, for a driver that wants a clean slate
between passes without knowing ZFS exists:

```sh
"$REAPER_CONTROL/reset"      # roll back; blocks until it is done
"$REAPER_CONTROL/snapshot"   # or mark *here*, once your stack is up
```

The container that asks is spared when the others are stopped. `snapshot` is how
you get a pristine taken at stack-up rather than at the end of a run.

### 5. Check the site before blaming your code

```sh
reaper doctor            # config, key, provider, templates, storage, records
reaper doctor --canary   # additionally prove the sweeper with a real machine
```

Every check runs and reports -- knowing three things broke beats knowing one
did. Exit 0 is healthy (warnings permitted), 1 means something failed, 2
means doctor itself could not run. The warnings are honest: an unexercised
sweeper reads WARN "no evidence either way", never a false ok.

### 6. When it goes wrong

| What you see | What it means |
|---|---|
| `no guest named "x" is registered here` | A typo, or a template nobody has built. Costs nothing -- it is refused before anything is created |
| `waiting -- <v6 address>: No route to host` | Normal. A dual-stacked guest reports IPv6 before its DHCP lease; reaper keeps trying until something answers |
| `created, but nothing answered on it within 30m` | The machine exists and carries an expiry, so nothing is leaked. `reaper list` shows it; `reaper down` removes it |
| `<pid> DEAD` in `reaper list` | The heartbeat stopped, so the expiry stopped moving. Nothing is leaked -- the sweeper will collect it -- but you are on a countdown nobody is winding |
| `refusing to roll … back: process(es) N still have files open` | Something is still using the dataset. Rolling back under it would leave it reading data that no longer exists |
| `there is no tank/state@pristine to roll back to` | No run has succeeded on this session yet |
| `… has N free and this session needs M, leaving less than the … floor` | The storage would be too full. Take a session down, or lower `min_free_gb` if you mean to run it close |
| `N session(s) are already up on this provider … not yours` | The cap counts the whole cluster, so somebody else's sessions count against it |
| `could not pre-fetch images` | A registry blip. The session is up and usable; the first build fetches them itself |
| a suite that failed but `reaper test` exited 0 | Your `cmd` pipes, and `/bin/sh` is dash, which has no `pipefail`. See `docs/tenants.md` — this one also poisons `@pristine` |
| `there is no "x" to reset to` from `test --to x` | A name that does not exist is an error, not a skip. Likely a typo |
| `WARNING: TLS certificate verification is disabled` | Exactly what it says. Export the node's CA and switch `tls` to `ca-file` |

If a session is unreachable and you want to know why, its console is readable
through the API -- see
[`providers/proxmox/tools/console.mjs`](providers/proxmox/tools/console.mjs).
Note the limitation it documents: an API token cannot open a console, so it
needs a user login.

### 7. This repository is its own first tenant

The `.reaper.toml` at the root is real, not an example. `reaper test` here runs
reaper's whole battery -- Rust suites, shell suites and seam guards -- inside a
session. It has already caught three portability bugs that a workstation-only
run could not, and that is the reason it exists.

## License

MIT. See [`LICENSE`](LICENSE). Copyright (c) 2026 Caleb L. Power.
