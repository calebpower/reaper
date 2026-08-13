# Testing methodology

How we test, and why each layer exists. Written to be transferable: the
principles are general, and the yasss implementation is cited as a worked
example rather than as the definition.

The simulated-user tier has its own deep-dive in
[`simulated-user-testing.md`](simulated-user-testing.md); §7 below is the
summary and that document is the specification.

---

## 1. The organizing idea

Not a pyramid. A **portfolio of oracles**.

The usual framing — lots of unit tests, fewer integration tests, a handful of
end-to-end tests — describes cost, not capability. It says nothing about *which
defects each layer can see*, which is the only question that matters when you
are deciding whether a layer earns its place.

So every tier here is justified by a question no cheaper tier can answer:

| Tier | The question only it can answer |
|---|---|
| Pure unit | Is this calculation right, at its boundaries? |
| Cross-language vectors | Do two independent implementations agree? |
| Component conformance | Did the rendered output drift? |
| Source-as-data | Does the code's *structure* still hold its claims? |
| Server contract | Do the authorization and enum rules hold, in isolation? |
| Browser vs fake API | What does the app do when a request *fails*? |
| Full stack | Does it work against a real database, real mail, real charset? |
| Seeded fuzz | Does any ordinary-but-untried request crash it? |
| Concurrency | Does the invariant survive simultaneous writers? |
| Simulated users | What breaks only after history accumulates? |
| Live browser audit | Does the UI hold up over a world somebody else built? |
| Human evidence | Does this actually make sense to a newcomer? |

A tier that cannot answer a question the tier below it already answers is a
tier to delete.

---

## 2. Non-negotiables

These are process rules, and they matter more than any individual test.

**Never weaken a test, check, assertion or lint to route around a defect.** Not
by disabling a phase, adding an exclusion, lowering a threshold, marking
`skip`/`ignore`/`allow`, appending `|| true`, catching and swallowing, or
narrowing a suite's scope. Every one of those is a permanent decision about
what the project stops noticing.

**Every narrowing needs a stated reason, and the reason must cover exactly what
it narrows — no more.** An exclusion that suppresses four things behind a
justification for two is a bug in the justification. Narrowings get reported in
the human-facing summary, not buried in a code comment.

**Every fix ships with a test that would have caught it.** Where a change is
genuinely untestable in isolation — dead-code removal, a transposed log
argument, a version bump, configuration, comments — say so explicitly rather
than inventing a test that only asserts a constant.

**A pre-existing failure must be proven pre-existing.** Stash the working tree,
re-run, and name it in the report. Otherwise "that one always fails" becomes
the sentence that hides a regression.

**New assertions get mutation-checked.** After writing a test, break the thing
it covers and confirm it fails. A test never observed failing is a test whose
value is unmeasured. This has repeatedly caught assertions that were
accidentally tautological — matching an explanatory comment rather than the
code, or matching the test's own fixture.

**Fix the cause, not the symptom.** A quiet terminal is not the goal.

---

## 3. Tier 1 — Pure unit tests

No DOM, no network, no framework. Pure functions and plain data: arithmetic,
validation, serialization, parsers, layout math, date handling, format
helpers.

Two habits that pay disproportionately:

**Keep testable logic out of components.** Label strings, state vocabularies
and layout arithmetic live in plain modules, so they can be asserted without a
renderer and shared between the app and its tests. When a test needs a DOM to
check a string, that is usually a design smell, not a test problem.

**Test the boundary, not the middle.** Off-by-one, empty, one, many, the cap,
the cap plus one, the value that lands exactly on the interval.

### Cross-language golden vectors

Where two implementations must agree — a client and a server computing the same
credential derivation, code alphabet, or signature — neither one is the oracle.
A generated **vector file** is: fixed inputs, fixed expected outputs, committed,
and consumed by both sides' test suites.

This catches the failure mode where both implementations are self-consistent
and disagree with each other, which no amount of testing either side alone will
find.

A refinement worth stealing: **run the interop vectors a second time under a
hostile environment default.** In yasss the credential vectors run once
normally and once with a non-UTF-8 default charset (`charsetTest`), because
under a UTF-8 default the suite passes whether or not the encoding is actually
pinned — so the ordinary run cannot see the bug at all.

---

## 4. Tier 2 — Component conformance

Render a component; assert its output exactly.

Deliberately **whole-string equality on class attributes**, exact element
counts, exact text content. This is unusual — most advice says assert behavior,
not markup — and it is right here for one reason: the rendered structure *is*
the contract for a design system. `is-outlined is-light` and `is-light` are
different components on screen, and a containment check accepts either.

The consequence has to be accepted honestly: these tests make the markup
expensive to change, which is the point, but it means **you add a sibling
component rather than generalizing an existing one**. Growing a well-covered
component a set of props to serve a second use case puts every one of its
assertions up for renegotiation. A sibling that shares the *logic* modules
costs a little duplication in markup and keeps the conformance suite meaningful.

A related discipline: when a shared component must gain an option, gain it in a
form that renders **nothing at all** when unused, so the existing component's
output is byte-identical. Then "the old suite passes untouched" is the
acceptance gate on the change.

---

## 5. Tier 3 — Source-as-data structural tests

The least conventional tier, the cheapest to run, and the one that catches
**rot**: code that is still correct about a product that changed.

The technique: parse the project's own source as data and assert structural
claims about it. No browser, no framework, no runtime.

Examples of claims this tier holds:

- every declared item has corresponding content, and no content is orphaned
  (a renamed key silently loses its copy otherwise);
- every selector referenced anywhere names a hook the application actually
  produces — attribute *and* value, since a numeric value like `"0"` matches
  almost any file on its own;
- a step that points at a form field must also open the dialog that field lives
  in — i.e. *the thing being described must be on screen*;
- a control named in user-facing copy must exist verbatim somewhere in the
  application;
- every emphasized span in copy is classified as either a control name or
  prose, so a new one has to be classified by whoever wrote it instead of
  quietly landing in whichever bucket a heuristic guessed;
- when the surface changes under the user, the copy acknowledges it.

That last group turns a subjective question — "is this documentation
coherent?" — into a set of mechanical ones. It will not tell you whether a
sentence *lands*. It will tell you that the sentence describes a button that no
longer exists, or a screen the reader is not on, and in practice that is most
of what goes wrong.

Two design rules make this tier durable:

- **Assert against the source, not against a re-export of it.** A check derived
  from the same structure it is checking will agree with itself no matter what.
  Where a test needs a mapping the app also has, duplicate it deliberately in
  the test and let the duplication be the check.
- **Exclude the content being checked from the corpus being searched**, or copy
  can satisfy a check by quoting itself.

---

## 6. Tier 4 — Server contract tests

Fast, in-process, no database. The targets are the rules that are pure
functions in disguise:

- **The authorization matrix.** Every role × every resource state × every
  operation, as a table. When authorization is refactored — for example when a
  second entity type gains the same ownership semantics — *the existing rows
  passing unchanged* is the gate on the refactor. This is what stops the rule
  existing in two copies that can drift.
- **Enum and boundary decoding.** Ordinals that clamp rather than throw, string
  bounds, id parsing.
- **Visibility rules.** Six-way result-visibility settings × viewer identity ×
  before/after a deadline, asserted as a pure function, so the expensive tiers
  do not have to enumerate 36 cases through HTTP.

---

## 7. Tier 5 — Browser against a fake API

Real browser, real application build, **fake server in-process**.

This tier is fast, deterministic, parallelisable, and runs the cross-engine
matrix (Chromium, Firefox, WebKit, mobile viewport). It is where the bulk of
UI behavior is asserted.

Its unique capability is **transport error injection**. Route interception in
the browser lets any request be made to fail on demand:

```
route → 500 { status: "error", info: "boom" }
```

Failure paths are written deliberately — toast, return false, and only *then*
update the local model, so a failed save leaves the screen showing the truth
rather than an optimistic lie — and without injection none of that code is ever
exercised. A real server will not produce a 500 when you ask it to; a fake one
will produce exactly the one you asked for.

Injection belongs on the fake tier, not the live one, for a reason worth
stating: the live suite's watchdog **fails any test that observes a 5xx**, and
that watchdog is what makes the live suite worth running. Injecting faults
there would mean disabling it, which is precisely the kind of narrowing §2
forbids.

The fake server must import the **real** implementations of anything
normalizing or validating (code normalization, id parsing) rather than
re-implementing them, or the fake drifts into testing itself.

---

## 8. Tier 6 — Full stack, containerized, staged

A real database, a real mail catcher, the real application artifact, in
containers, driven by one script with named stages that can be run
individually:

```
fuzz  accounts  sessions  reminders  text  <feature>  concurrency
regressions  journeys  browser  health
```

Properties worth copying:

**Start the environment hostile.** The database starts in `latin1`, not the
modern `utf8mb4` default, because the schema's charset migration is only
load-bearing on a server that does not already do the right thing. Booting in
the friendly configuration makes the migration and its assertions pass whether
or not they work. Push astral-plane text through the newest text column in
every feature stage.

**Assert idempotence of anything that re-runs.** Migrations execute on every
boot, so a stage asserts a second boot rebuilds nothing and backfills are no-ops
the second time.

**No backdoors.** State is built through the real API and the real mail flow —
registration, emailed verification link, sign-in — because a seeding shortcut
tests a path production does not have.

---

## 9. Tier 7 — Seeded fuzzing

Cheap, general, and aimed at a specific empirical observation: **most defects
found in practice were unguarded dereferences surfacing as 500s** — a UUID
belonging to a different parent, a null actor on an anonymous request, an empty
collection, a validator that was never constructed. Not exotic inputs. Ordinary
requests nobody tried.

So the oracle is not "is the answer correct" but the far cheaper and more
general:

1. no 5xx, ever;
2. every response is a well-formed envelope;
3. the server is still alive afterwards.

The input corpus targets seams that actually broke: type confusion at the
deserializer, unparseable ids, boundary values on unsigned columns, oversized
strings, and text that is only a problem if something concatenates it into SQL
or HTML.

**Determinism is the whole game.** The seed is printed on every run and
replayable by environment variable. A fuzzer you cannot replay is a fuzzer you
cannot act on. That corpus is shared with the higher tiers so the same hostile
values reach the API directly and through the UI.

---

## 10. Tier 8 — Concurrency harnesses

Targeted, scenario-driven, N-way simultaneous requests against one resource.

The critical design decision is **what to assert**. Counting successful
responses is the obvious oracle and the wrong one: it passes a system that
answers correctly and stores wrongly. Assert the **resource's own state read
back afterwards** — the number the user will see and the number the rule is
about.

Scenarios cover both the race and its non-racing sibling, because in practice
the same capacity rule was enforced in one endpoint and not in another, and the
second needed no concurrency at all to violate.

---

## 11. Tier 9 — Simulated users (the seed-driven tier)

Full specification in [`simulated-user-testing.md`](simulated-user-testing.md).
The summary:

Several **actors**, each with a real account and a real session, take turns
doing whatever they are currently able to do, for hundreds of actions. A
**shadow model** records what should be true; **invariants** diff it against the
server as the run proceeds.

The architecture is six parts:

| Part | Role |
|---|---|
| Generator | Seeded PRNG choosing a weighted action from those currently applicable |
| Actors | Independent identities with their own rotating credentials |
| Shadow model | A deliberately *partial* second copy of the truth |
| Checker | Invariants diffing model against server, periodically and at the end |
| Nemesis | Adversarial action classes (below) |
| Shrinker | Reduces a failing run to something a person can read |

**Quasi-nondeterministic** is the right description: the action sequence looks
random and is fully reproducible from a seed. Anything that must vary between
runs — a tag distinguishing this process's data from an earlier run's — is
drawn *outside* the seeded stream, so replay reproduces the action sequence
exactly.

### Error injection, as action classes

The nemesis is not a separate mode; it is a set of actions in the same weighted
pool, so faults land at arbitrary depths in an accumulated history rather than
against a clean fixture:

| Class | What it simulates |
|---|---|
| Stale credential | A second tab holding a retired session ticket |
| Act-on-deleted | A page rendered before someone else deleted its subject |
| Hostile input | The shared fuzz corpus, aimed at a write endpoint |
| Double submit | Save pressed twice before the first answer returns |
| Typo-then-correct | A value corrected, checked later from another actor's view |
| Abandonment | Half-finished work left behind |

Each asserts the *coherent* outcome rather than a specific one. A stale ticket
must produce a refusal, not a crash, and must not disturb the live session. Two
simultaneous submits may both legitimately succeed — what is asserted is that
the model and the server agree afterwards about how many there are.

### Cheap oracles on every call

Independent of the invariants, every response passes through a wrapper
asserting no 5xx, well-formed JSON, and a status field. Nothing can escape the
cheap checks, and it costs nothing.

### Shrinking

A seed tells you a bug exists; a trace tells you what it is. On failure:

1. **Bisect the length** — find the shortest prefix of the same seed that still
   fails;
2. **Remove action kinds** — drop each kind in turn, keep the removal if it
   still fails.

This answers "is this action actually involved, or merely present". Because
shrink re-runs land on a stack that already holds the first run's data, a
successful reproduction is a strong signal and a failure to reproduce is a weak
one — stated plainly in the output rather than papered over.

### Self-testing the oracle

**An invariant that never fires is indistinguishable from a passing suite**, and
it is exactly the mistake that leaves everyone believing they are covered.

So the invariants are fed the responses a *broken* server would send — modeled
on the real defects that motivated them — and each must complain. This runs
without any stack at all, in a second, and it is the check that makes the
expensive stage mean anything. It is mutation testing pointed at the oracle
instead of the code.

The suite runs a small committed set of fixed seeds by default, so cost is
known; long hunting runs are opt-in; and **any seed that ever found a defect is
promoted into the fixed set permanently**.

---

## 12. Tier 10 — Live browser audit over an accumulated world

The browser suite from Tier 5, pointed at the real stack, run as an actor who
has *accumulated* something — hundreds of prior actions' worth of world.

It sees two things nothing else can: behavior that only appears once a user's
own data is deep (a listing that returns a duplicate, a paging control that only
exists past a threshold), and whether the deployed configuration is actually
being read, which a fake answering generically cannot distinguish.

### Two oracles for one claim

Where a claim really matters, assert it from both sides.

The tutorial's containment claim — *nothing the practice mode does reaches the
server* — is checked by counting the browser's requests, **and** afterwards by
asking the database whether anything arrived. The first proves the page made no
call; the second catches a leak by a route nobody thought to watch. Neither is
sufficient alone, which is exactly why there are two.

---

## 13. Tier 11 — Human evidence

Some questions cannot be automated and should not be faked.

"Would a first-timer understand this?" is a judgment call. What automation can
do is **produce the evidence** rather than the verdict: a per-step transcript
and screenshot, emitted to a directory, so a person can answer from evidence
instead of memory.

Alongside that, a zero-dependency reader that prints the user-facing content as
continuous prose, annotated with the structural transitions a reader cannot
infer from the words — where the page changes, where a dialog opens. Reading a
flow cold for five minutes finds more than any checker written for the purpose.

**Color is computed, not eyeballed, and not pixel-diffed.** Resolve the painted
colors through the browser's own computed styles, convert to relative
luminance, and assert the distinctions survive with hue discarded — in the same
units the accessibility guidelines use. That answers "is this conveyed by color
alone" as a number, and names the element and the pair that failed. A pixel diff
would say "these images differ by 3%" and need a baseline regenerated on every
legitimate design change and every font substitution between machines.

---

## 14. Cross-cutting practices

**Shared corpora.** One hostile-input list, one set of label constants, consumed
by every tier that needs them. A value that broke the API should reach the UI
too, without anybody re-typing it.

**Two oracles for claims that matter.** See §12.

**Determinism and replay everywhere randomness appears.** Print the seed; accept
it back through the environment; keep anything that must vary out of the seeded
stream.

**Assert the absence of things, with time allowed to pass.** Proving nothing
happened needs a wait; and assert the requests *before* the success indicator,
so a leak presents as "it made this call" rather than as "the toast never
appeared" thirty seconds later, several inferences from the cause.

**Name what a test does not prove.** Where a suite's name overstates it — a walk
through a flow that touches nothing proves *passivity*, not *containment* —
write that down in the file, and point at the test that does carry the stronger
claim.

**Match effort to change.** A comment does not warrant the containerized stack;
a schema change does, and it is not optional. Say which suites were skipped and
why.

---

## 15. Adopting this on a new project

In order of return on effort:

1. **Pure unit tests** on logic deliberately kept out of components.
2. **Seeded fuzz** with the no-5xx / well-formed / still-alive oracle. Highest
   defect-per-line ratio of anything here; a weekend to build.
3. **Browser-vs-fake with transport error injection** — the failure paths are
   usually the least-exercised code you have.
4. **Full stack, staged, started hostile.**
5. **Source-as-data structural checks** once the project has structures that can
   rot: routes, permissions, copy decks, generated clients.
6. **Concurrency harnesses** for each rule with a capacity or uniqueness
   constraint.
7. **Simulated users**, last, because it is the most machinery — and write the
   oracle self-test *first*, or you will not know whether it works.

The acceptance test for the whole exercise: **take defects you have already
found and fixed by hand, revert the fixes, and confirm the harness rediscovers
them.** A methodology that cannot rediscover your known bugs is not yet
measuring anything.
