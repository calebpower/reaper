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

**`testing-methodology.md` once had a dangling link, and no longer does.** Its
§7 and §11 pointed at a `simulated-user-testing.md` deep-dive that is superseded
and not shipped here. Two tenants in a row followed it and concluded a file was
missing from their checkout, which is a reader being misled rather than a record
being preserved -- so the citations now point at §11 of that same document,
which is the specification for the tier. What a tenant's own `journeys`
implementation is remains the living *example*, as it always was.

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

## Development prerequisites

Only two things are needed to run everything in `tools/check.sh`:

**A Rust toolchain**, for the manifest validator. Build natively on whatever
machine you are on; do not cross-compile. The project deliberately avoids
`native-tls` in favour of `rustls` so that a build host's OpenSSL differences
never become a portability problem.

**`shellcheck`**, for the shell scripts. `tools/lint-shell.sh` prefers one on
`PATH` and otherwise runs a digest-pinned container image; if it can find
neither it fails rather than reporting a clean tree.

One packaging trap worth recording, because it cost an hour: on FreeBSD the
port is `hs-ShellCheck`, and `pkg search -q shellcheck` finds nothing at all
because the search is case-sensitive. Use `pkg search -i shellcheck`, or install
`hs-ShellCheck` by name. It is a different tool from `cargo-spellcheck`, which
checks spelling in Rust doc comments and is not used here.
