# Status

Where the work actually stands. When this document disagrees with any other,
trust this one -- the rest describe intended shapes, this describes the shape
that exists.

Last updated: 2026-08-13.

## Phases

| Phase | What it delivers | State |
|---|---|---|
| **0** | Repo bootstrap: docs, manifest schema, seam guards, sweeper | **Complete.** Sweeper imported, hardened, self-tested, and dry-run against the live API |
| 1 | Provider seam + session core (`up`/`list`/`renew`/`down`) | **Accepted live.** up, list, renew and down all run against the real cluster |
| 2 | Guest templates + the runner | **Complete.** Both guests rebuilt, registered and proven live |
| 3 | Sync, build, execution | **Accepted live.** sync, build and run against the real cluster, results flowing continuously |
| 4 | `reset` and the `@pristine` snapshot | **Accepted live.** Reset, named checkpoints, and an in-guest trigger a container can call |
| 5 | The loop as one verb | **Accepted live**, and now proven by a tenant that is not reaper |
| 6 | Hardening, `doctor`, third-tenant proof | Not started |

Phase detail is in [`reaper-plan.md`](reaper-plan.md) §6, amended by the
decisions below. Phase 2 is parallel-safe with Phase 1; nothing else is.

### What runs today

`tools/check.sh` runs everything that needs no hypervisor, token or network:

| Check | What it proves |
|---|---|
| `tools/lint-shell.sh` | Every shell script is shellcheck-clean, and the linter refuses to pass when shellcheck is missing |
| `tools/guards.sh` | No tenant, operating system or hypervisor has leaked out of its seam |
| `manifest/test/run.sh` | The schema accepts what it should and rejects what it claims to |

`cargo test --workspace` runs the Rust suites, including the CLI driven end to
end as a subprocess against a stand-in hypervisor, a stand-in ssh and a stand-in
rsync: up, list, renew and down, the concurrency cap, an unregistered guest, a
failed destroy, the heartbeat, the session's pool disk, the runner being
delivered and firstboot run before a session is called ready, and Phase 3's
sync, build and run -- including that a failing command still gets its results
out, and that a job reaches the guest as a delivered file rather than as an
argument.

It also runs **real rsync** between two local directories, over the same flags
reaper uses. Asserting that reaper passes `--delete` proves reaper passes
`--delete`; it says nothing about whether the results directory survives it.

`runner/test/run.sh` runs the runner's decision self-test: 97 assertions against
stubbed tools, asserting the invocation log rather than exit codes, because
"it refused" and "it refused without touching anything" are different claims.
Since Phase 3 it also asserts the runner's documented **exit codes** -- 2 for a
malformed call, 1 for a failure -- because a mutation that turned a refusal into
a log survived the first version of those tests: the shell then fell over one
line later trying to run an engine named by the empty string, which is
indistinguishable from a refusal if all you check is a boolean status.

### Measured against the real cluster

Phase 3, 2026-08-13, with reaper as its own tenant: a Rust workspace of five
crates, built and tested inside a session on `ubuntu-26.04`.

| | |
|---|---|
| `up`, cold, to a ready session | **1m56s**, including a 912 MB image pull |
| `sync`, whole tree including `.git` | 4s |
| `build`, empty caches | 49s (45s of it inside cargo) |
| `build`, warm caches | **3.4s** (0.17s inside cargo) |
| `build --profile nightly`, cold by design | 48s |
| `run` -- the entire battery, Rust and shell | 27s |
| `down` (final results pull, stop, destroy, disks) | 7s |
| Session cost on storage | 8 GiB boot clone + 64 GiB pool disk |

The warm loop -- sync, build, run -- is **35 seconds**, and that is the number
the original plan said to stop and reassess over if it did not clearly beat
running the suite locally. It does.

`up` at under two minutes also contradicts the 9m20s recorded during Phase 1,
and the honest position is that we do not know why. Same storage, same template,
same 8 GiB boot disk. It is not the image pull, which is inside the new figure
and not the old. Both numbers were measured rather than estimated; the earlier
one is left here rather than deleted, because a five-fold difference nobody can
explain is worth remembering the next time somebody wants to plan around it.

What has not changed: storage is plain LVM with no snapshots, so every clone is
a full copy. That is still why the pool disk is attached rather than carried --
only the boot disk is copied, and never the session's 64 GiB of storage.

### Guests registered

| Name | Template | Execution | Notes |
|---|---|---|---|
| `ubuntu-26.04` | 9001 | `container` | ZFS, podman, rsync, guest agent. No language toolchains. Rebuilt 2026-08-13 to add the three packages podman needs to work |
| `freebsd-15.1` | 9000 | `host` | ZFS in base, rsync, guest agent. No container engine, and none wanted -- the engine here runs only foreign-format images under emulation |

The FreeBSD template ships a C compiler because FreeBSD base includes one. It
ships no Rust, Node or JDK, so it cannot yet serve a host-execution tenant that
needs one; adding a toolchain is a deliberate, recorded act per the runbook.

### Building the second template was much cheaper than the first

Not because the first taught transferable fixes -- most of them did not
transfer -- but because the platform was checked rather than assumed. Four of
the six Ubuntu problems simply do not exist on FreeBSD: `rc.d/sshd` regenerates
host keys unconditionally, there is no cloud-init to destroy the network
configuration, `dhclient` identifies by MAC already, and sshd is a persistent
daemon rather than socket-activated so deleting keys does not sever the
connection in use.

Two FreeBSD-specific traps were found by looking instead: it carries **both**
`/etc/hostid` and `/etc/machine-id`, and it ships `PermitRootLogin no` where
Ubuntu ships `prohibit-password`.

The runner was exercised on real FreeBSD before the template was sealed,
including its refusal path: with no spare disk it declined to build a pool and
said why, and `info` reported `platform=FreeBSD` with no engine. Both platform
branches are now proven against real systems rather than fixtures.

### The sweeper proved itself in production, unplanned

A template-in-progress was left carrying the expiry tag it had been given as a
session. Its TTL passed while nobody was watching, and the **deployed** sweeper
collected it on its own cron -- no involvement from this project at all. That is
the backstop working exactly as designed, and it is the one thing that had not
been demonstrated in production rather than in a test.

It also cost an evening of template work, and the mistake was ours: **an expiry
tag must be cleared the moment a machine stops being a session.** The Ubuntu
template had this done; the FreeBSD one did not. Rebuilding it now uses a direct
clone that is never tagged at all, so the failure cannot recur -- an untagged
machine in the pool is reported by the sweeper and never destroyed, which is the
correct state for work in progress.

### FreeBSD works

Repaired and proven on 2026-08-13. `freebsd-15.1` is registered again, at
template **9004**.

| Proof | Result |
|---|---|
| `reaper up` | 1m34s to a reachable session |
| Firstboot | `tank` ONLINE on `vtbd1`, all four datasets |
| `reaper sync` | working tree in |
| `reaper run`, **host execution** | ran on the guest itself; this guest's whole reason for existing, exercised through a session for the first time |
| A second clone | distinct `hostid` *and* distinct host keys |

The repair itself was three lines: delete the six empty key files, let
`service sshd start` regenerate them, restore the truncated
`/boot/loader.conf`. It was done through the guest agent -- the machine had no
ssh, which was the fault -- after `VM.GuestAgent.Unrestricted` was added to the
token's pool grant. That privilege adds nothing meaningful: the token could
already create, destroy and re-disk every machine in the pool.

The broken template was 9002. It was destroyed once Cal authorised it, and the
working one took its name -- so the identifier changed and nothing else did. The
safety guard refused 9002 until it was named explicitly, which was correct: only
9001 had been authorised before, and inferring the rest from a precedent is how
a template gets destroyed that somebody still wanted.

### How it was diagnosed

**The cause is known exactly.** Clones of template 9002 have no ssh because the
template carries six **zero-byte** files in `/etc/ssh`, and FreeBSD's
`rc.d/sshd` regenerates a host key only when the file does not exist -- an empty
file exists. sshd then fails on an empty key, which is the
`failed precmd routine for sshd` recorded on every boot.

The empty files came from this project's own runbook: it said to stop the
machine hard at seal time, which is correct on Ubuntu and destructive on UFS.
On the sealed image, **every file written during the final session is zero
bytes and nothing else is** -- host keys, `/boot/loader.conf`, and the
`/etc/rc.local` that an earlier debugging attempt had added to fix this exact
symptom, which is why that attempt "did not help". The superblock's clean flag
is 0.

Two of the earlier conclusions were wrong, and both mattered:

- "the guest agent does not start either" -- it does. A clone boots to
  multi-user and reports an address; `rc` continues past a failed `sshd`. The
  address it reported first was often IPv6, which the CLI then could not route
  to. That was a genuine bug, found independently during Phase 3 and fixed.
- "an explicit early rc script did not help" -- it was never given the chance.

**How it was found**, since the method is reusable and beats what was tried
before: the clone's disk was attached to a running Linux guest and mounted
read-only, making the whole sealed image readable -- configuration, logs, file
sizes, superblock -- without booting it. No console, no extra privilege. Linux
cannot write UFS, so it is a reading instrument only.

What remains is the repair, and it is blocked on reaching a machine with no
ssh. See "What is blocked".

### The console cannot be read with an API token

`providers/proxmox/tools/console.mjs` reads a guest's serial console through the
API. It is finished and it cannot be used with the harness credential: **an API
token cannot authenticate a console.** The `termproxy` call succeeds -- it is an
ordinary API call and `VM.Console` covers it -- and then the terminal's own
ticket check rejects a name of the form `user@realm!tokenid`. The websocket
opens and is closed a few seconds later with no diagnosis. This is a known
Proxmox limitation, confirmed against their support forum after the behaviour
was reproduced.

The tool therefore takes `--user` and `--password-file` instead. A user holding
`VM.Console` on the pool and nothing else is enough, and is less privileged than
the harness token in every other respect. Until such a user exists, the tool
refuses at startup and explains why rather than failing at the websocket.

### FreeBSD, as previously recorded

Clones of the FreeBSD template do not boot usefully: without host keys they come
up with no ssh, and with host keys the last attempt did not start the guest
agent either. Six hypotheses were tested and refuted -- `sshd_enable`, malformed
`sshd_config`, entropy, missing `hostid`, the attached data disk, and an
explicit early rc script. `docs/runbooks/freebsd.md` records each with its
evidence.

The sharpest unexplained clue: key generation succeeds on a warm reboot and
fails on a cold boot.

It blocks nothing. Phase 3 is sync, build and execution, which needs the
container-execution guest -- Ubuntu -- and there are no host-execution tenants
yet. `freebsd-15.1` is unregistered so nothing can depend on it by accident.

The next step is not more remote inference: it is **watching the console during
a clone's first boot**, which is where every one of these failures is visible
and outside the machine none of them are. That should have been the second move
rather than the last.

### Phase 3, and what running it live found

Five findings, four fixed and one handed back. Every one of them was invisible
to a suite that never left this machine, which is the same lesson Phase 1
recorded and worth recording again rather than assuming it was learned.

| Found | Why nothing offline saw it |
|---|---|
| The engine can pull images but cannot start one -- no `nftables` | Nothing offline runs a container, and `podman info` does not either |
| Containers cannot resolve each other by name -- no `aardvark-dns` | As above, and it only *warns*, so nothing fails loudly enough to notice |
| An address a machine reports is not an address anything can reach | Both ends of an offline test are the same machine, and the stand-in has one address |
| Determinism mode broke every command that named a cache | The stand-in never expanded a variable |
| rsync carried uids across machines with no shared user database | Both ends of an offline test are the same machine |
| `manifest/test` ignored `CARGO_TARGET_DIR`, which a session always sets | Nothing here redirects the build output |
| `mktemp -d -t <name>` is a prefix on one system and a template on another | The suites had only ever run on one of them |

The first two are template defects and were fixed by **rebuilding template 9001**
on 2026-08-13, with Cal's explicit authorisation to replace it in place. See
below.

The second is worth stating as a rule rather than a bug. `warm_cache: false`
means *the cache is not warm*. It never meant "the tenant's command must be
written twice", which is what dropping the variables amounted to -- the
documented way to use a cache is to name its path in your command, and that
expanded to an empty string. A cold run now gets the same variables and the
same paths, pointing at a directory emptied first, and the claim that matters
is unchanged: nothing from the warm cache is reachable.

Two of the five were bugs in reaper's own test suites, found only because
reaper became its own tenant and tried to run them somewhere that was not this
workstation. That is the whole argument for dogfooding, arriving on the first
day of it.

### The first tenant that is not reaper

Onboarded 2026-08-14 from Arch on WSL2 — the first time the CLI has been driven
from Linux. Findings are recorded in `reaper_bugs.md` beside this repository.

**It needed no change to reaper.** One manifest and four tenant-side scripts;
no framework patch, no plugin. The seam held, which is the claim the three lint
guards exist to protect and the first time anything outside this repository has
tested it.

**And the cycle time came out the other way round.** Phase 5 could only measure
reaper against itself, where the loop loses because reaper's own battery needs no
machine. This tenant's battery **cannot run on the workstation at all** — a
7.6 GiB WSL2 exhausts itself on the stack plus a browser — and the machine it
replaces was a hand-kept VM that hard-reset under the same load.

| | |
|---|---|
| the tenant's battery, on the workstation | does not run |
| on the hand-kept VM it replaces | 8.7–10 min, when that VM survived the load |
| `reaper test`, warm | **9m04s**, of which the suite is 8.6 min |

So reaper's overhead on a warm session is **about 30 seconds on a nine-minute
suite** — a sync, a no-op build, and result collection. That is the comparison
the original plan asked for, and it is the tier a session is for.

### Three things that tenant found

**A false pass, waiting to happen.** A job runs under `/bin/sh`, which is dash
on Ubuntu, and dash has no `pipefail` — so `make test | tee $REAPER_OUT/log`
exits with *tee's* status. A failing suite reads as a pass, and on a tenant that
declares a reset dataset `@pristine` is then taken on the strength of it, so
every later reset returns to a broken state. Now documented in
`docs/tenants.md`, with the reason reaper does not silently switch to bash:
bash is not in the base system on every guest.

**The guest is missing ordinary build plumbing**, and "no language toolchain"
does not imply "no make". Recorded below and in `docs/guests.md`.

**A wrong conclusion worth recording**, because the next tenant may draw it too:
that no registered guest can run a containerized test stack. It does not follow.
A container-execution verb gets no engine socket, so a tenant orchestrating
containers must run that verb on the host — but `ubuntu-26.04` under
`exec: host` has working podman and can orchestrate anything. What it lacks is a
*toolchain*, and the answer is to containerize the test driver as well, which is
the shape `manifest/examples/yasss.reaper.toml` demonstrates. **No third guest is
being built**; `docs/tenants.md` now spells the pattern out.

### `ubuntu-26.04` as a tenant finds it

Probed 2026-08-14 on template 9001. A tenant cannot see inside a template until
a session exists, so this is written down rather than discovered a clone at a
time.

| | |
|---|---|
| Present | bash, curl, wget, tar, xz, rsync, zfs, podman 5.7.0, pgrep/pkill, ss, python3 3.14.4 |
| **Absent** | **make**, git, gcc, unzip, node, npm, jq, lsof, fuser |
| Resources | 4 cores, 15.5 GiB RAM, `/dev/shm` 7.6 GiB |
| Root disk | 7.8 GiB, **~3.3 GiB free** — not scratch space; put caches on the pool |
| Pool | `tank`, 62 GiB |
| Egress | reachable: docker.io, archive.ubuntu.com, nodejs.org, npmjs, astral.sh, playwright |

### Phase 5: the loop, and an honest cycle time

`reaper test` is sync -> build -> reset -> run. Proven live against reaper
itself and against a fixture tenant, three loops in sequence on one session:

| Loop | What ran | |
|---|---|---|
| 1st, fresh session | sync, build, run -- reset skipped, nothing to reset to | 2m05s |
| 2nd | the same; the run succeeded, so it took `@pristine` | 55s |
| 3rd | **all four steps**, rolled back in 2s | 57s |

The first run failed, which was not staged: it hit a real bug (below). That made
the design's own rule visible without arranging it -- **a failed run takes no
pristine**, so the loop kept skipping the reset until a run succeeded.

### The payoff, proven both ways

> The same seed, replayed against two consecutive resets, produces an identical
> action sequence and an identical outcome.

A fixture tenant whose action sequence depends on accumulated state as well as
on its seed -- the state-accumulation problem the methodology describes, where a
journey engine tags data per-run to avoid colliding with its own history.

| | Same seed, twice |
|---|---|
| **Without** a reset | `step 1 -> 749 …` then `step 1 -> 780 …` -- **different** |
| **With** a reset | `step 1 -> 749 …` both times -- **identical** |

The negative half was run first and deliberately: a fixture that cannot detect
accumulation would have proved nothing at all.

### The cycle time, and what it does not show

| | |
|---|---|
| reaper's battery, locally on the workstation | **27.5s** |
| `reaper test` for reaper, warm | **57s** |
| `reaper test` for the fixture tenant, warm | **9.5s** |

So for reaper as its own tenant the loop is **twice as slow as running it
locally**, and the original plan says to stop and reassess if it does not
clearly beat local. Reassessed, and recorded rather than dropped for being
unflattering: this is the correct answer for this tenant, not a failure of the
design. reaper's battery needs no database, no browser and no pod. It is exactly
the kind of suite `docs/tenants.md` says belongs on a workstation, and moving it
into a session because sessions exist is the anti-pattern that document warns
about.

What the third row shows is that the loop's own overhead is small -- under ten
seconds including a rollback. The 57s is reaper's battery being run twice over,
once locally to build and once in the session. A tenant whose battery genuinely
needs a real machine is where the comparison becomes meaningful, and this
repository does not contain one.

### What dogfooding caught that nothing else had

Running reaper's own suite inside a Linux session failed, on a check that has
always passed on the FreeBSD workstation. `stat -f` means "format" on BSD and
**"filesystem status" on Linux** -- so the BSD spelling *succeeds* there and
prints something that is not a mode, and the `||` fallback never fires. The test
now tries both spellings and judges the answer rather than the exit status.

That is the third portability bug in this project's own suites found only by
running them somewhere other than where they were written, after `mktemp -t` and
`CARGO_TARGET_DIR`. It is the argument for dogfooding stated as a fact rather
than a hope.

### Phase 4, and the number that justifies the whole design

`reset` rolls tenant state back in **2.9 seconds** -- and the same 2.9 seconds
whether there is 104 KB of state or 801 MB, because ZFS discards blocks rather
than rewriting them. The cost is two SSH round trips, not the data. That is the
argument for this design over rebuilding a stack per test, and it is now
measured rather than asserted.

Proven live, with a tenant that accumulates state:

| | |
|---|---|
| `@pristine` | taken automatically after the first successful run |
| Rollback | rows written after the snapshot gone, earlier ones back, exactly |
| `tank/work`, `tank/cache` | byte-identical across a reset |
| `tank/images` | image still present, no re-pull; only the engine's own bookkeeping moves |
| Idempotence | two consecutive resets leave the same state |
| Named checkpoints | `snapshot mid` -> change -> `reset --to mid` returns to `mid` |
| Stop-first | a live container is stopped (SIGTERM, SIGKILL after 10s) and the rollback proceeds |
| The in-guest trigger | a container called `$REAPER_CONTROL/reset`, the reset happened, **and the container survived to keep working** |

### Three things Phase 4 got wrong before it got them right

**A safety promise that was not kept.** `docs/tenants.md` said reset "never
rolls the filesystem out from under a running process", on the assumption that
ZFS refuses a rollback on a busy dataset. It does not. Measured on a live guest
with a descriptor verifiably open inside the dataset -- `/proc/<pid>/fd/6 ->
/tank/state/db/held` -- `zfs rollback -r` succeeded, the file vanished, and the
holder carried on reading an inode with no name. The runner now looks for
holders itself and refuses, naming them; a process holding an *already unlinked*
file is the one exception, since a rollback cannot reach it and counting it
would let one leaked process veto every reset for the session's life.

**A root-executed script inside a container-writable mount.** The control loop's
copy of the runner was placed in the directory bind-mounted into containers. Any
toolchain image could have replaced it and had root on the guest -- and from
there the results channel runs back to the workstation. Caught by the automated
security review of the commit, and the galling part is that the job script two
functions away is mounted read-only with a comment saying exactly why. The
control directory is now split: what the host executes lives outside anything
mounted, mode 0700, and containers see a writable queue and a read-only wrapper.
The same review caught the caller id, which arrives from inside a container and
is used as a shell pattern -- `*` would have spared every container and left the
rollback running against a live stack.

**A disk that was never used, and was not blank.** The first session after some
volumes were destroyed failed with "corrupt primary EFI label" on a freshly
created 64 GiB disk. The storage recycles space without zeroing it, so a *backup*
partition-table header survived in the final sector of a volume deleted long ago
-- invisible to every check the runner makes, and fatal to `zpool create`. The
answer was not `-f`: the runner now zeroes the first and last mebibyte of a disk
it has already accepted as unused, and lets ZFS check again with its veto
intact.

### Phase 4 decisions

| Question | Answer |
|---|---|
| When `@pristine` is taken | After the first successful run, never after a failed one. It captures post-run state, which is said plainly at the time, and `reaper snapshot` names an earlier point |
| Where state lives | `tank/state`, reachable as `REAPER_STATE` in both execution modes. It was created by firstboot and exposed to nothing at all until now |
| The in-guest trigger | Request files and rename, not a socket or FIFO -- neither template ships netcat, and a FIFO handshake in shell is delicate about who opens what and when |
| Named checkpoints | Shipped. `snapshot <name>` and `reset --to <name>`, which is what makes checkpoint-and-replay shrinking possible |
| What may be rolled back | `state`, and only `state` -- enforced in the schema and again in the runner |

### Phase 3 decisions

| Question | Answer |
|---|---|
| Execution mode | A property of a **verb**, not of a guest. A toolchain image carries no engine client, so a run that orchestrates containers cannot execute inside the image that built it -- while a run that is `cargo test` very much wants to |
| Image inheritance | A container-execution `run` with no image of its own uses `build.image`, resolved before validation so the schema still sees a complete guest |
| Where paths live | The runner. The CLI asks it where a workspace is rather than computing one, so pool layout is one component's business |
| Job delivery | Rendered in Rust, quoted there, delivered over stdin. Never an argument -- an apostrophe in a command has already cost this project a `rm -f` on the workstation |
| Results | Continuous while a command runs, again when it stops whatever it did, and once more on `down`. A trace that exists only on a machine scheduled for destruction is a trace nobody reads |
| Reverse sync | Never `--delete`. The guest is authoritative for what it produced, not for what was in the operator's results directory beforehand |
| `.git` | Synced by default. Never needing a commit is the point, and deltas make it cheap after the first |
| Pre-fetch failure | Warns, never fatal. The engine would fetch on demand anyway, and a registry blip must not cost a machine that took two minutes to clone |

### Rebuilding a template through the API alone

The Ubuntu template was rebuilt on 2026-08-13 without touching the web
interface, which settles a question the Phase 1 plan left open: the harness
token can do it. `VM.Allocate` and `VM.Config.Options` come with the pool grant,
and `POST /nodes/{node}/qemu/{id}/template` accepts them by inheritance -- the
permissions endpoint reports `/vms/9001` with an empty *direct* set, so this had
to be established by doing it rather than by reading.

The sequence, which is the one to repeat:

1. Clone the template to a spare identifier. Reads the original, writes nothing
   to it. **Leave the clone untagged** -- an untagged machine in the pool is
   reported by the sweeper and never destroyed, which is the correct state for
   work in progress. A tagged one gets collected while you are asleep, which has
   happened here once already.
2. Clear the protection the clone inherited, start it, do the work.
3. `podman rmi -af` before sealing. A test image left behind is baked into every
   clone for the life of the template; the one here was 1.7 GB on an 8 GB disk.
4. Runbook step 6d, then a **hard** stop from the hypervisor.
5. Convert, then prove it with two clones before believing it -- distinct
   machine-ids and distinct host keys, or the template is carrying an identity.
6. Only then replace the original.

Doing it in that order meant the original template still existed at every point
up to the last one, which is the only reason replacing it in place was a
reasonable thing to agree to.

### An extra privilege PVE 9 requires

The original plan's list of token privileges predates PVE 9 and is incomplete.
Cloning a machine with a network interface now also needs **`SDN.Use`**, granted
here at `/sdn/zones` with propagation.

That path is broader than the ideal `/sdn/zones/localnetwork/vmbr0`, and it was
used because the narrower path could not be selected and this cluster has **no
SDN zones configured** -- so the two are presently the same set of networks. If
anyone configures SDN later, this grant silently widens to include those zones
and should be narrowed then.

### What live testing found that offline testing could not

Six defects, all of which would have surfaced as sessions that silently cannot
be reached or destroyed. Three of them shared a root cause worth naming: the
stand-in API was **kinder than the real one**, accepting operations the real
API refuses. It now models those refusals.

| Found | Why offline missed it |
|---|---|
| Clones inherit the template's protection flag, so sessions could never be destroyed | Stand-in ignored protection |
| A running machine cannot be deleted; destroy must stop first | Stand-in ignored run state |
| The heartbeat died with its terminal -- no `setsid` | No test harness signals the process group |
| `cloud-init clean` destroys networking on an ISO install | Not modelled at all |
| `ConditionFirstBoot` never fires for host-key generation | Not modelled at all |
| `DefaultDependencies=no` needed, or systemd deletes ssh.socket's start job | Not modelled at all |

### Phase 2 decisions

| Question | Answer |
|---|---|
| Runner | POSIX sh, delivered over SSH and invoked. Nothing reaper wrote lives in a template, so upgrading it never means rebuilding one |
| Data disk | Attached per session by the provider. Where cloning is a full copy, a template disk would be copied whole every time |
| Template build | ISO install, documented step by step in `docs/runbooks/` |
| FreeBSD root | UFS, so a session has exactly one pool |
| SSH user | root, with the key trusted for root. A session's blast radius is the sandbox, and escalation would differ per guest |
| Media | Never reaper's. Expected present on the provider; a missing ISO is a request to the cluster's administrator |

Every assertion in those suites has been mutation-checked: broken deliberately,
and observed failing, before being counted as coverage.

### Phase 1 decisions

| Question | Answer |
|---|---|
| Heartbeat cadence | 5 minutes by default, and configuration refuses any value that does not fit three times into the TTL |
| Resources | Applied at clone time, not baked into templates -- a guest is an operating system, not an operating system crossed with every size a tenant might want |
| TLS | Three modes, no default. `insecure` warns on every invocation; plain HTTP is refused unless the host is loopback |
| Credential | One file holding `user@realm!name=secret`, refused if anyone but the owner can read it |
| TTL from readiness | Creation tags a short `ready_grace`; the heartbeat switches to the real TTL once the machine answers. No untagged window at any point |

### First contact with the real cluster

The harness token works: `cal@pve!harness`, PVE 9.1.4, verified with a live
`GET /version`. `~/.config/reaper/config.toml` points at the real endpoint with
`tls = "insecure"`, since the node's certificate is issued by an internal CA
that is not available -- reaper warns on every invocation, and switching to
`ca-file` is a one-line change once it is.

The sweeper's live `--dry-run` reaches the API, parses the reply and reports
`0/0`. Be precise about what that proves: reachability, credential and JSON
handling. It does **not** prove the pool filter selects correctly, because there
is nothing yet to select. The mocked suite covers the filter; the live proof
arrives with the first machine.

The pool is empty -- zero VMs in `cal/ephemeral` -- so anything that appears
there is something reaper put there.

### What is blocked

**The FreeBSD template.** Not blocked by anything reaper owns. See above.

**The sweeper's live dry run**, from this workstation. Its credential file lives
on the sweeper's own VM, which is deliberately not a deploy target for anything
here, so `cull.sh --dry-run` can only be run there. The cluster was verified
clean through the API instead.

## Decisions taken

| Question | Answer | When |
|---|---|---|
| CLI implementation language | Rust. The runner too, for portability across guests | Phase 0 |
| Guest support | Multiple guests from the start, registry-driven and sysadmin-owned | Phase 0 |
| First two guests | Ubuntu 26.04 LTS and FreeBSD 15.1-RELEASE | Phase 0 |
| Hypervisor coupling | A provider seam; Proxmox implemented, CBSD documented only | Phase 0 |
| Worked examples | Two, deliberately dissimilar, shipped in Phase 0 rather than deferred | Phase 0 |
| TLS stack | `rustls` only, never `native-tls` | Phase 0 |

### Overrides of `reaper-plan.md`

The plan ships verbatim as a record. Three of its constraints have been lifted;
the plan text still carries the old wording. Also listed in the root README.

| The plan says | What is true now |
|---|---|
| §4: "No support for guests other than the one blessed template" | Multiple guests; which exist is a registry entry |
| §3: "No language toolchains in the template, ever" | Holds for container-execution templates; host-execution templates carry a toolchain by design |
| §5: "The PVE API is the only hypervisor interface" | True of the Proxmox provider; the core talks to a trait |

## Still open

Each is deliberately deferred to the phase that first needs it, per
[`reaper-plan.md`](reaper-plan.md) §8.

| Question | Decide in |
|---|---|
| Heartbeat cadence and per-profile TTL defaults (strawman: renew every 10 min to now+2h for dev) | Phase 1 |
| Whether `resources.cores`/`ram_gb` apply at clone time or are fixed per template | Phase 1 or 2 |
| ~~`reset --to <name>` and `snapshot <name>` in v1~~ | **Shipped in Phase 4.** Cheap once rollback works, and what makes checkpoint-and-replay shrinking possible |
| ~~Registry strategy for image pulls~~ | **Decided in Phase 3: direct.** A 912 MB image pulled inside a two-minute `up`, so availability is the only thing a LAN cache would buy and digest pinning already rules out getting the wrong bytes. Revisit when a registry outage first stings |
| Storage fallback if full-copy clone latency proves intolerable | **Not needed.** `up` measured at 1m56s; see the numbers above, and the unexplained gap from Phase 1's 9m20s |

## Environment facts

Verified 2026-08-12. These correct and extend [`reaper-plan.md`](reaper-plan.md)
§2, which describes a client environment that is not where development is
happening.

- **The development host is FreeBSD 15.0-RELEASE-p1**, not the Windows/WSL
  workstation §2 describes. WSL remains a target. The dev host is also the
  natural build host for a FreeBSD guest's runner.
- **No cross-compilation from the dev host.** `rustc`/`cargo` 1.97.1 are
  source-tarball builds with no `rustup`; the only installed rustlib targets are
  `x86_64-unknown-freebsd` and `wasm32-unknown-unknown`, and there is no cross
  linker. **Build natively per host and per guest.** This is why `rustls` is
  mandatory and `native-tls` is banned: an OpenSSL linkage difference between
  build hosts is exactly the kind of portability failure that would show up
  late and confusingly.
- **`podman` on FreeBSD is rootful-only** and runs Linux images under the
  Linux emulation layer, so it cannot execute FreeBSD-native binaries. This is
  the concrete reason `exec: host` exists.
- Present: `shellcheck` 0.11.0 (package `hs-ShellCheck` -- note the
  capitalisation, `pkg search -q shellcheck` finds nothing and `-i` is needed),
  `node` v24.18.0, `jq`, `rsync`, `yq`, `podman`.
- Absent: `npm`/`npx`, `pip` (system Python has `venv` only), `rustup`.
  `cargo-spellcheck` is installed but is a different tool from `shellcheck` and
  is not used by this project.
- The Proxmox API is reachable from the dev host (verified: HTTP 401 without a
  token, which is the API answering). This was **not** true at the start of
  Phase 0 and required a routing change.

## Corrections carried forward

Findings from reading the reference tenants. Recorded rather than silently
absorbed, because each one changes an acceptance criterion.

### 1. Phase 5's payoff test conflates two different problems

[`reaper-plan.md`](reaper-plan.md) §6 Phase 5 says that replaying a promoted
seed proves "the state-accumulation problem the `_staleNote` in `seeds.json`
documents is gone".

Those are two different problems. The `_staleNote` documents **seed-stream
drift**: an action began drawing one extra random number, and every seed
consequently walked a different sequence from then on. Rollback cannot fix
that, and neither can anything else in this framework -- it is a property of
the tenant's own generator, and the note itself draws the right conclusion
("a regression that must stay caught belongs in a written test, not in a seed").

What `reset` genuinely fixes is the **state accumulation** that forces a
journeys engine to tag its data per-run and per-attempt to avoid colliding with
its own history.

So the acceptance criterion is restated:

> The same seed, replayed against two consecutive `reset`s, produces an
> identical action sequence and an identical outcome.

That is a claim `reset` can actually carry, and it is worth carrying.

### 2. `testing-methodology.md` links to a document that is not here

Its §7 and §11 point at `simulated-user-testing.md`. That file is superseded and
deliberately not shipped; the living specification is a tenant's own `journeys`
implementation. The link is left intact because the document is a record. Noted
in [`README.md`](README.md).

## Reference tenants

Neither is a dependency, and neither is named anywhere in framework code -- a
lint guard enforces that. They exist as worked examples, chosen to be
dissimilar, because a schema validated against one project silently becomes that
project's schema.

| | JVM web service | Rust CLI |
|---|---|---|
| Stack | database + mail catcher + app in one pod | none: a process and temp trees |
| Drivers | browser automation inside the pod | the language's own test runner |
| State to roll back | database data dir, app file storage | embedded-SQLite cache, fixture trees |
| Build cache | one ecosystem's | a different ecosystem's |
| Pre-pulled images | four | **none** |
| Guest / exec | Linux, `container` | **BSD, `host`** |

The second one is the more valuable, because it breaks assumptions the first
would let stand: that state means a database container, that a run needs
pre-pulled images at all, that a tenant has a pod, that the guest is Linux, and
that execution is containerized. One of its suites is gated on the host
operating system and had nowhere to run under a single-guest design.

## Known risks

- **Full-copy clones.** On storage without snapshots or linked clones, every
  session start is a whole disk copy -- minutes, not seconds. The in-session
  loop never pays it again, but Phase 2 must measure it honestly, and if it is
  intolerable the fallback is confined to which storage a template lives on.
- **Two templates double the storage and the maintenance.** This sharpens the
  storage decision above and makes automated template rebuilds more valuable,
  not less.
- **Templates are hand-built pets in v1.** Runbooks plus backups are the
  mitigation; automated rebuilds are a v2 item.
- **FreeBSD 15.1-RELEASE may not exist yet.** The dev host runs 15.0-RELEASE-p1.
  Phase 2 confirms the media before committing to it and falls back to 15.0
  explicitly rather than silently.
- **Ubuntu 26.04 is a young LTS.** Pin to point-release media, record exact
  package versions in the template's notes.
- **A migrated session cannot be operated on.** Listing is cluster-wide, but
  every mutation addresses the configured node -- so a session VM that has been
  live-migrated elsewhere still shows in `reaper list` while stop, destroy and
  address queries fail against the node it left. Sessions are ephemeral and
  nothing reaper does migrates them, so this takes deliberate operator
  interference to hit; if it ever matters, the fix is resolving each machine's
  node from the cluster listing instead of the config. Recorded, not fixed.
- **The provider seam has exactly one implementation**, so it is a hypothesis
  rather than a proof. The lint guard tests that hypervisor vocabulary has not
  leaked across the boundary; it cannot test that the boundary is in the right
  place. Only a second implementation would, and building one speculatively
  costs more than it proves.

## Deferred to v2

CA-issued TLS everywhere (dropping the insecure-transport flag), automated
template rebuilds, a pull-through registry cache, multi-VM topologies per
session, a broker holding the harness credential off-workstation, a
container/jail profile for sub-second sandboxes on projects that do not need a
kernel boundary, and the CBSD provider.

Multi-guest support is **not** on this list. It moved into v1.
