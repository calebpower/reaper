#!/bin/sh
#
# Runs shellcheck over every shell script in the tree.
#
# Prefers a shellcheck on PATH and falls back to a digest-pinned container
# image. Same tool either way, so this is portability across the machines this
# project is developed and run on -- not a narrowing, and not a way to pass
# when the tool is missing. If neither route is available this exits non-zero
# and says so; it never reports success by default.
#
# Exit 0 if every script is clean, 1 if shellcheck complained, 2 if shellcheck
# could not be run at all.
set -eu

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "${root}"

# Pinned by digest for the same reason tenant manifests are: a run that cannot
# say which bytes it ran is a run whose verdict stops meaning anything. Resolved
# from koalaman/shellcheck:v0.11.0 on 2026-08-12.
IMAGE='docker.io/koalaman/shellcheck@sha256:61862eba1fcf09a484ebcc6feea46f1782532571a34ed51fedf90dd25f925a8d'

# Every tracked file that is a shell script, by extension or by shebang. Going
# by shebang as well as extension matters: an extensionless hook or wrapper is
# still a shell script, and skipping it because it is not named *.sh is exactly
# the kind of silent gap this is meant to close.
scripts=$(
    for f in $(git ls-files); do
        [ -f "${f}" ] || continue
        case "${f}" in
            *.sh) printf '%s\n' "${f}"; continue ;;
        esac
        case "$(head -c 32 "${f}" 2>/dev/null)" in
            '#!'*/sh|'#!'*/sh\ *|'#!'*/bash|'#!'*/bash\ *|'#!'*env\ sh*|'#!'*env\ bash*)
                printf '%s\n' "${f}" ;;
        esac
    done
)

if [ -z "${scripts}" ]; then
    echo "no shell scripts found -- nothing to check, which is suspicious" >&2
    exit 2
fi

count=$(printf '%s\n' "${scripts}" | wc -l | tr -d ' ')

if command -v shellcheck >/dev/null 2>&1; then
    echo "shellcheck $(shellcheck --version | awk '/^version:/{print $2}') from PATH, ${count} script(s)"
    # shellcheck disable=SC2086
    # Deliberate: the file list is newline-separated and this project has no
    # paths containing whitespace. Quoting it would pass one argument naming a
    # file that does not exist.
    shellcheck ${scripts} || exit 1
    echo "clean"
    exit 0
fi

for engine in podman docker; do
    if command -v "${engine}" >/dev/null 2>&1; then
        echo "shellcheck via ${engine} (${IMAGE##*@}), ${count} script(s)"
        # shellcheck disable=SC2086
        # Same reason as above.
        "${engine}" run --rm -v "${root}:/mnt:ro" -w /mnt "${IMAGE}" ${scripts} || exit 1
        echo "clean"
        exit 0
    fi
done

echo "no shellcheck on PATH and no container engine to run the pinned image." >&2
echo "Install it (FreeBSD: pkg install hs-ShellCheck -- note the capitals, a" >&2
echo "lowercase search finds nothing) rather than skipping the check." >&2
exit 2
