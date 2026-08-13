#!/bin/sh
#
# Schema test suite.
#
# Every fixture under valid/ must be accepted and every fixture under invalid/
# must be rejected, and the worked examples must be accepted. The invalid set is
# the half that matters: a schema whose rejections have never been observed is a
# schema nobody has checked, and it is the same mistake as an invariant that
# never fires.
#
# Exit 0 if every fixture behaved, 1 otherwise.
set -eu

here=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
root=$(CDPATH='' cd -- "${here}/../.." && pwd)

validate="${root}/target/debug/reaper-manifest-validate"
if [ ! -x "${validate}" ]; then
    echo "building the validator first" >&2
    ( cd "${root}" && cargo build --quiet --manifest-path manifest/validate/Cargo.toml ) \
        || { echo "cannot build the validator" >&2; exit 1; }
fi

pass=0
fail=0

# expect <wanted-exit> <label> <file>
expect() {
    wanted=$1
    label=$2
    file=$3

    got=0
    output=$("${validate}" "${file}" 2>&1) || got=$?

    if [ "${got}" -eq "${wanted}" ]; then
        pass=$((pass + 1))
        printf '  ok    %-28s %s\n' "${label}" "$(basename "${file}")"
    else
        fail=$((fail + 1))
        printf '  FAIL  %-28s %s (wanted exit %s, got %s)\n' \
            "${label}" "$(basename "${file}")" "${wanted}" "${got}"
        printf '%s\n' "${output}" | sed 's/^/          /'
    fi
}

echo "worked examples -- must be accepted"
for f in "${root}"/manifest/examples/*.yaml; do
    expect 0 "accepted" "${f}"
done

echo "valid fixtures -- must be accepted"
for f in "${here}"/valid/*.yaml; do
    expect 0 "accepted" "${f}"
done

echo "invalid fixtures -- must be rejected"
for f in "${here}"/invalid/*.yaml; do
    expect 1 "rejected" "${f}"
done

echo
echo "${pass} passed, ${fail} failed"
[ "${fail}" -eq 0 ]
