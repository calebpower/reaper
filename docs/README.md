# Documentation index

Two kinds of document live here: **records**, imported verbatim and not edited,
and **contracts**, written for this repository and maintained with it.

## Records

| File | What it is |
|---|---|
| [`reaper-plan.md`](reaper-plan.md) | The original phased implementation plan, as handed over |
| [`testing-methodology.md`](testing-methodology.md) | How we test, and why each tier exists |

Both are copied byte-for-byte from their sources and are **not** edited when the
project moves past them. Where the project has departed from the plan, the
departure is recorded in the root [`README.md`](../README.md) and in
[`STATUS.md`](STATUS.md), not by rewriting the record.

Two things to know when reading them:

**`testing-methodology.md` has a dangling link.** Its §7 and §11 point at a
`simulated-user-testing.md` deep-dive. That file is superseded and deliberately
not shipped here; the living specification for that tier is a tenant's own
`journeys` implementation, not a document. The link is left as-is because
editing a record to tidy a link is how records stop being records.

**`testing-methodology.html` is not shipped.** It is a rendering of the
Markdown, which is normative. One source, not two.

## Contracts

| File | Answers |
|---|---|
| [`tenants.md`](tenants.md) | I have a project. How do I run it here? |
| [`guests.md`](guests.md) | I want to add an operating system. What must it provide? |
| [`providers.md`](providers.md) | I want to add a hypervisor. What must it implement? |
| [`site-config.md`](site-config.md) | I run the hypervisor. What do I configure, and where? |
| [`STATUS.md`](STATUS.md) | What actually works today, and what is still a plan? |

`STATUS.md` is the one to trust when documents disagree. Everything else
describes an intended shape; `STATUS.md` describes the shape that exists.

## A note on the methodology's scope

`testing-methodology.md` §2 is written as guidance for tenants, but it governs
**this codebase and anyone working on it** just as much:

- never weaken a test, check, assertion or lint to route around a defect;
- every narrowing carries a stated reason covering exactly what it narrows;
- every fix ships with a test that would have caught it, or an explicit
  statement of why it is untestable;
- a pre-existing failure is *proven* pre-existing before it is called that;
- new assertions are mutation-checked -- break the thing, watch the test fail --
  before they count as coverage.

The sweeper in `cull/` has already had one incident of exactly the §2 shape: a
swallowed error that made failures invisible. Its decision self-test exists
because of it.
