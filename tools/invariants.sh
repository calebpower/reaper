#!/bin/sh
#
# Cross-cutting invariants, checked against the project's own source.
#
# This exists because of a pattern, not because of a bug. Three times now a
# defect has been fixed in one place and left standing in its sibling:
#
#   - a leaked process fixed in the Rust harness, left in the shell one
#   - `nftables` added to a template, `aardvark-dns` not
#   - `--manifest` added to sync/build/run, not to renew/down
#
# Per-feature tests cannot catch that, because each one is looking at the
# feature it was written for. These checks look across the whole tree and
# assert properties that must hold *everywhere* -- so a fix that lands in one
# place and not the others fails here rather than in production a week later.
#
# In the source-as-data spirit of the testing methodology's Tier 3: parse the
# project's own text and assert structural claims about it. Milliseconds, no
# build, no network.
#
# Exit 0 if every invariant holds, 1 otherwise.
set -eu

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "${root}"

# offenders <label> <why> -- reads candidate offenders on stdin.
#
# Anything printed is a violation. A check that finds nothing is silent, so a
# clean run says only how many invariants held.
#
# Note what this does NOT do: keep a running count in a variable. It is invoked
# on the right-hand side of a pipe, so it runs in a subshell and any counter it
# incremented would be discarded -- which is precisely the bug this project's
# sweeper shipped with, and which this file reproduced on its first run. The
# verdict is counted from the report instead, where it cannot be lost.
offenders() {
    label=$1
    why=$2
    found=$(cat)
    if [ -n "${found}" ]; then
        printf 'FAIL  %s\n' "${label}"
        printf '%s\n' "${why}" | sed 's/^/      /'
        printf '%s\n' "${found}" | sed 's/^/        /'
        printf '\n'
    else
        printf 'ok    %s\n' "${label}"
    fi
}

checks() {

# Files this project actually wrote. Build output and vendored code are not
# ours to hold to these rules. Untracked files ARE: a new script is untracked
# for exactly the window in which this gate is its only reviewer, and this
# battery itself passed its own gate while untracked and failed it one commit
# later. --others closes that window; --exclude-standard keeps build output
# out via .gitignore.
#
# This file is excluded from its own scans, and the reason has to cover the
# whole file: every rule below quotes the construct it forbids, in the pattern
# or in the message, so a textual scan cannot tell its rules from violations.
# The shell linter still covers it, and running it in the gate on both systems is
# what exercises its portability.
sources() {
    git ls-files --cached --others --exclude-standard \
        | grep -vE '^(docs/(reaper-plan|testing-methodology)\.md|tools/invariants\.sh)$'
}
shell_sources() { sources | grep -E '\.sh$'; }
rust_sources() { sources | grep -E '\.rs$'; }

printf '\n--- portability: constructs that differ silently between systems ---\n\n'

# Each of these has actually bitten this project, or is the same shape as one
# that did. What they share is the failure mode that matters: they do not
# error on the other platform, they *succeed* and mean something else.

shell_sources | xargs grep -n 'mktemp -d -t ' 2>/dev/null \
    | grep -vE ':[0-9]+:[[:space:]]*#' | offenders \
    "mktemp -t is a prefix on one system and a template on another" \
    "BSD appends X's; GNU requires them and fails without. Use a full
template: mktemp -d \"\${TMPDIR:-/tmp}/name.XXXXXXXX\"."

# `stat -f` is a format string on BSD and 'filesystem status' on Linux -- so
# the wrong spelling *succeeds* and prints something that is not what you
# asked for, and an || fallback never fires. Any file using one spelling must
# use both and judge the answer.
for f in $(shell_sources); do
    if grep -qE '(^|[^a-z])stat -f ' "${f}" 2>/dev/null && ! grep -q "stat -c" "${f}" 2>/dev/null; then
        printf '%s: uses stat -f with no stat -c fallback\n' "${f}"
    fi
    if grep -q 'stat -c' "${f}" 2>/dev/null && ! grep -qE '(^|[^a-z])stat -f ' "${f}" 2>/dev/null; then
        printf '%s: uses stat -c with no stat -f fallback\n' "${f}"
    fi
done | offenders \
    "stat's -f means different things on BSD and Linux, and the wrong one succeeds" \
    "Try both spellings and judge the output, not the exit status."

shell_sources | xargs grep -n -- '-printf' 2>/dev/null | offenders \
    "find -printf is GNU-only" \
    "BSD find has no -printf. Use -exec, or stat with both spellings."

shell_sources | xargs grep -n 'readlink -f' 2>/dev/null | offenders \
    "readlink -f is not portable" \
    "Absent on older BSDs. Use a cd/pwd subshell for canonical paths."

shell_sources | xargs grep -nE 'sed -i( |$)' 2>/dev/null | offenders \
    "sed -i takes a mandatory suffix on BSD and none on GNU" \
    "There is no spelling that works on both. Write to a temporary file
and move it."

shell_sources | xargs grep -n 'grep -P' 2>/dev/null | offenders \
    "grep -P is GNU-only" \
    "Use -E."

shell_sources | xargs grep -nE '^\s*echo -[en]' 2>/dev/null | offenders \
    "echo -e and echo -n are not portable" \
    "Use printf."

shell_sources | xargs grep -nE 'pipefail' 2>/dev/null \
    | grep -v 'docs/' | grep -vE '#' | offenders \
    "pipefail is not in POSIX sh" \
    "dash does not have it. Scripts here run under /bin/sh; if a pipeline's
status matters, capture it explicitly."

printf '\n--- resource discipline: nothing may be left running or lying about ---\n\n'

# A suite that starts something must have an exit path that stops it. A stop at
# the end of each case is not enough: a case that fails skips it.
for f in $(shell_sources | grep -E '/test/|test\.sh$'); do
    if grep -q 'mktemp -d' "${f}" 2>/dev/null && ! grep -q 'trap ' "${f}" 2>/dev/null; then
        printf '%s: makes temporary directories and has no trap to remove them\n' "${f}"
    fi
done | offenders \
    "a test harness that creates scratch directories must remove them" \
    "Add a trap on EXIT HUP INT TERM. This suite leaked 3,968 directories
before anyone looked."

for f in $(shell_sources | grep -E '/test/|test\.sh$'); do
    if grep -qE '(nohup|&\s*$)' "${f}" 2>/dev/null && ! grep -q 'trap ' "${f}" 2>/dev/null; then
        printf '%s: starts background processes and has no trap to stop them\n' "${f}"
    fi
done | offenders \
    "a test harness that starts processes must stop them" \
    "Same reason, and the same trap."

# The Rust equivalent: a harness owning a scratch directory needs a Drop.
for f in $(rust_sources | grep -E 'tests?'); do
    if grep -q 'temp_dir()' "${f}" 2>/dev/null && ! grep -q 'impl Drop' "${f}" 2>/dev/null; then
        printf '%s: takes a temporary directory and implements no Drop\n' "${f}"
    fi
done | offenders \
    "a Rust harness that takes a scratch directory must clean it up" \
    "Give it an impl Drop. Drop runs when a test panics; a call at the end
of the test does not."

printf '\n--- safety: refusals that must not quietly become permissions ---\n\n'

# Invocations only. A comment saying "not -f" and a test asserting its absence
# are the project agreeing with this rule, not breaking it.
sources | grep -v '^docs/' \
    | xargs grep -nE 'zpool create -f|zfs (destroy|rollback)[^|]*-f\b' 2>/dev/null \
    | grep -vE ':[0-9]+:[[:space:]]*#' \
    | grep -vE 'log_lacks|assert|refuted|must not' \
    | offenders \
    "no forcing past ZFS's own refusals" \
    "-f tells ZFS to ignore what it found, whatever it is. Clear the
residue deliberately and let it check again."

sources | grep -v '^docs/' | xargs grep -nE '#\[ignore\]|--skip ' 2>/dev/null \
    | offenders \
    "no test is skipped" \
    "A skipped test is a decision about what this project permanently stops
noticing."

# `|| true` is legitimate on cleanup. It is not legitimate on anything whose
# failure is the point.
shell_sources | xargs grep -nE '(zfs (rollback|snapshot|destroy)|zpool create|podman (rm|stop)) .*\|\| true' 2>/dev/null \
    | offenders \
    "no swallowing the failure of something destructive or load-bearing" \
    "|| true on cleanup is fine. On an operation whose success is the claim,
it converts a failure into a silent wrong answer."

printf '\n--- symmetry: a rule applied in one place must apply in its siblings ---\n\n'

# Every runner verb taking --project must validate it: these become paths.
# Named specifically. Asking whether the function mentions valid_name anywhere
# is not enough: cmd_exec validates cache names too, so dropping the check on
# the *project* left the mention behind and the test passed. A mutation caught
# that, which is the only reason this is written the harder way.
for v in workspace exec control; do
    if ! sed -n "/^cmd_${v}()/,/^}/p" runner/runner.sh \
        | grep -qE 'valid_name "\$\{?(2|[a-z_]*project)'; then
        printf 'cmd_%s takes --project and never validates it\n' "${v}"
    fi
done | offenders \
    "every verb that turns an argument into a path validates it" \
    "The manifest schema constrains these too, but this is the code that
builds a path and an argument vector, so it checks for itself."

# Every runner verb taking --dataset must resolve it through rollback_target,
# which is what refuses work, cache and images.
for v in snapshot snapshots rollback; do
    if ! sed -n "/^cmd_${v}()/,/^}/p" runner/runner.sh | grep -q 'rollback_target'; then
        printf 'cmd_%s takes --dataset and never resolves it through rollback_target\n' "${v}"
    fi
done | offenders \
    "every verb that names a dataset refuses the ones it must not touch" \
    "work carries results outward; cache and images are what make the next
iteration fast."

# Every runner verb taking a snapshot --name must validate it.
for v in snapshot rollback; do
    if ! sed -n "/^cmd_${v}()/,/^}/p" runner/runner.sh | grep -q 'valid_snapname'; then
        printf 'cmd_%s takes --name and never calls valid_snapname\n' "${v}"
    fi
done | offenders \
    "every verb that names a snapshot validates the name" \
    "It ends up in a ZFS argument."

# Every verb the usage header advertises must be dispatched, and vice versa.
{
    advertised=$(sed -n '/^# Usage:/,/^# Exit/p' runner/runner.sh \
        | sed -n 's/^#   runner\.sh \([a-z-]*\).*/\1/p' | sort -u)
    dispatched=$(sed -n '/^case "\${1:-}" in/,/^esac/p' runner/runner.sh \
        | sed -n 's/^    \([a-z-]*\)).*/\1/p' | grep -vE '^\*$|^$' | sort -u)
    for v in ${advertised}; do
        printf '%s\n' "${dispatched}" | grep -qx "${v}" || printf 'usage advertises %s, which is not dispatched\n' "${v}"
    done
    for v in ${dispatched}; do
        case "${v}" in -h|--help) continue ;; esac
        printf '%s\n' "${advertised}" | grep -qx "${v}" || printf '%s is dispatched but not in the usage header\n' "${v}"
    done
} | offenders \
    "the runner's usage and its dispatch agree" \
    "A verb documented and missing, or present and undocumented, is how
somebody comes to rely on something that is not there."

# Every CLI verb that acts on a project's sessions takes --manifest. This is
# the exact asymmetry that made `reaper down --manifest x` a silent no-op.
{
    for v in Sync Build Run Test Reset Snapshot Renew Down; do
        sed -n "/^    [A-Za-z\/ ]*$/d;/    ${v} {/,/^    },/p" cli/src/main.rs \
            | grep -q 'manifest: Option<PathBuf>' \
            || printf '%s does not take --manifest\n' "${v}"
    done
} | offenders \
    "every verb acting on a project's sessions accepts --manifest" \
    "It must mean the same thing everywhere. When down and renew lacked it,
'reaper down --manifest x' was rejected outright -- which in a script reads
as a session taken down when it was not."

printf '\n--- the gate: what reviews the reviewers ---\n\n'

# A plain ls-files lists only what is committed, so a new script is reviewed
# by nothing until after its first commit -- this battery passed its own gate
# that way and failed it one commit later, with an identical tree. Every
# file-list a gate script builds must therefore include untracked files.
# The pattern is written so it cannot match itself; the flag filter means an
# invocation carrying the flags on the same line is the only accepted form.
for f in tools/*.sh; do
    grep -n 'git ls-file[s]' "${f}" 2>/dev/null \
        | grep -v -- '--others --exclude-standard' \
        | sed "s|^|${f}:|"
done | offenders \
    "the gate reviews untracked files too" \
    "A file is untracked for exactly the window in which the gate is its
only reviewer. --others --exclude-standard closes that window while
.gitignore keeps build output out."

printf '\n--- documentation that claims something the code must actually do ---\n\n'

# Every [session] key documented in site-config.md must be parsed, and every
# key parsed must be documented. max_concurrent was described as a site
# setting while behaving as a local one; this is the shape of that mistake.
{
    documented=$(awk '/^\[session\]/{f=1;next} f && /^\[|^```/{exit} f' docs/site-config.md \
        | sed -n 's/^\([a-z_]*\) *=.*/\1/p' | sort -u)
    parsed=$(sed -n '/^struct RawSession {/,/^}/p' core/src/config.rs \
        | sed -n 's/^    \([a-z_]*\): Option<.*/\1/p' | sort -u)
    for k in ${documented}; do
        printf '%s\n' "${parsed}" | grep -qx "${k}" || printf 'site-config.md documents session.%s, which nothing parses\n' "${k}"
    done
    for k in ${parsed}; do
        printf '%s\n' "${documented}" | grep -qx "${k}" || printf 'session.%s is parsed but undocumented in site-config.md\n' "${k}"
    done
} | offenders \
    "every session setting is both parsed and documented" \
    "A setting documented and unread is a lie; one read and undocumented is
a trap."

# Same for each provider's own table. Providers are discovered, not named:
# name one here and the provider seam guard fails this file, correctly. The
# doc section is expected to carry the provider directory's name, so a second
# provider is checked the day its directory appears.
{
    for cfg in providers/*/src/config.rs; do
        [ -f "${cfg}" ] || continue
        prov=${cfg#providers/}; prov=${prov%%/*}
        documented=$(awk -v s="[${prov}]" 'index($0,s)==1{f=1;next} f && /^\[|^```/{exit} f' docs/site-config.md \
            | sed -n 's/^\([a-z_]*\) *=.*/\1/p' | sort -u)
        if [ -z "${documented}" ]; then
            printf 'site-config.md has no [%s] section, but providers/%s parses a config\n' "${prov}" "${prov}"
            continue
        fi
        parsed=$(sed -n '/^struct Raw {/,/^}/p' "${cfg}" \
            | sed -n 's/^    \([a-z_]*\): .*/\1/p' | sort -u)
        for k in ${documented}; do
            printf '%s\n' "${parsed}" | grep -qx "${k}" || printf 'site-config.md documents a %s key %s that nothing parses\n' "${prov}" "${k}"
        done
        for k in ${parsed}; do
            printf '%s\n' "${documented}" | grep -qx "${k}" || printf '%s key %s is parsed but undocumented\n' "${prov}" "${k}"
        done
    done
} | offenders \
    "every provider setting is both parsed and documented" \
    "Same reason. data_storage was effectively required and absent from the
example for weeks."

}

report=$(checks)
printf '%s\n' "${report}"

held=$(printf '%s\n' "${report}" | grep -c '^ok  ' || true)
failed=$(printf '%s\n' "${report}" | grep -c '^FAIL' || true)

printf '\n'
if [ "${failed}" -eq 0 ]; then
    printf '%s invariants hold\n' "${held}"
else
    printf '%s of %s invariants failed\n' "${failed}" "$((held + failed))"
fi
[ "${failed}" -eq 0 ]
