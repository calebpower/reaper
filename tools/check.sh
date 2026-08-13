#!/bin/sh
#
# Everything that can be checked without a hypervisor, a token or a network.
#
# This is the gate a commit is expected to pass. It deliberately runs every
# check rather than stopping at the first failure, because knowing that three
# things broke is worth more than knowing that one did.
#
# Exit 0 if everything passed, 1 otherwise.
set -eu

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "${root}"

failed=''

run() { # run <label> <command...>
    label=$1
    shift
    printf '\n=== %s ===\n' "${label}"
    if "$@"; then
        :
    else
        failed="${failed} ${label}"
    fi
}

run "shell lint"       ./tools/lint-shell.sh
run "seam guards"      ./tools/guards.sh
run "manifest schema"  ./manifest/test/run.sh

# The sweeper is provider-specific and arrives with its provider. Absent is
# fine; present but unrunnable is not.
for t in cull/*/test/run.sh; do
    [ -x "${t}" ] || continue
    run "sweeper: ${t}" "${t}"
done

printf '\n'
if [ -n "${failed}" ]; then
    echo "FAILED:${failed}"
    exit 1
fi
echo "all checks passed"
