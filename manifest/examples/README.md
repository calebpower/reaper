# Worked examples

Two manifests for two deliberately dissimilar projects.

They are here to keep the schema honest. A schema validated against a single
project quietly becomes *that project's* schema, and it calcifies long before
anyone notices -- so the second example exists specifically to break assumptions
the first one would let stand.

## Read this before copying anything

**These are illustrative, not verified.** No live session has run either of
them. They are shapes -- what a manifest for this kind of project looks like --
and they will be corrected once a real session has proved or disproved them.
Until then, treat any specific command or image here as a plausible guess.

One caveat is known and called out in the file itself: the JVM example pins a
plain JDK as its builder image, but that project's build also runs a frontend
bundler, so a working builder image would need a JavaScript runtime too. It is
left as a plain JDK because inventing a composite image nobody has built would
be a worse kind of wrong -- an example that looks verified and is not.

## What each one demonstrates

|  | `yasss.reaper.yaml` | `bgone.reaper.yaml` |
|---|---|---|
| Shape | JVM web service | Rust command-line tool |
| Stack | database + mail catcher + app in one pod | none: a process and temp trees |
| State rolled back | database data dir, file storage | embedded-SQLite cache, fixture trees |
| Build cache | one ecosystem's | two of its own choosing |
| Images pre-pulled | five | **none at all** |
| Guests | one | **two, with different execution modes** |
| Manifest form | shorthand | expanded, with per-guest overrides |

The second column is the one that earns its place. Written against the first
project alone, the schema would have made `run.images` required, would have
assumed every tenant has a pod and a database, and would have had no reason to
support more than one guest or more than one execution mode. Each of those
would have been a wrong constraint that looked entirely reasonable at the time.

## Image digests

Every reference is pinned by digest, and the schema rejects tags outright --
including the `repo:tag@sha256:...` form, where the tag is unverified, can drift
away from the digest, and reads as though something had checked it.

That does cost readability, so the provenance is recorded here instead. These
were resolved from the registries on **2026-08-12**:

| Tag resolved | Used by |
|---|---|
| `docker.io/library/eclipse-temurin:17-jdk` | JVM example, builder |
| `docker.io/library/eclipse-temurin:17-jre` | JVM example, application base |
| `docker.io/library/mariadb:11` | JVM example |
| `docker.io/axllent/mailpit:latest` | JVM example |
| `docker.io/library/node:22-slim` | JVM example, driver |
| `mcr.microsoft.com/playwright:v1.62.1-noble` | JVM example, browsers |
| `docker.io/library/rust:1.97` | Rust example, Linux guest |

A digest resolved from a moving tag is exactly the case pinning exists for:
`mailpit:latest` means something different next week, and the pinned digest does
not.

## The schema, and its tests

`../schema/v1.json` is normative. `../test/` holds the fixtures that prove it
rejects what it claims to reject; run `../test/run.sh`.

The invalid fixtures are the half that matters. A schema whose rejections have
never been observed is a schema nobody has checked -- the same mistake as an
invariant that never fires.
