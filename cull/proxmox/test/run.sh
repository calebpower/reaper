#!/bin/sh
#
# The sweeper's decision self-test.
#
# The sweeper is the backstop for everything else going wrong, so the question
# is not whether it destroys expired machines -- it is whether it can be
# provoked into destroying anything else. The refusals are tested harder than
# the successes, and every assertion is made against the log of calls it
# actually issued rather than its exit code.
#
# curl and date are stubbed, so nothing is contacted and "now" is fixed. jq is
# real, so the filter that decides which guests are even considered is the one
# that ships.
#
# Runs with no network, no credential and no privileges.
#
# Exit 0 if every case behaved, 1 otherwise.
set -eu

here=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
cull="${here}/../cull.sh"
[ -x "${cull}" ] || { echo "no sweeper at ${cull}" >&2; exit 1; }
command -v jq >/dev/null 2>&1 || { echo "these tests need jq" >&2; exit 1; }

pass=0
fail=0
CASE=""
WORK=""
NOW=1700000000

REAL_TOOLS="jq awk sed grep tr sort cat mkdir rm chmod cut head wc printf ln env sh basename dirname sleep"

new_case() {
    CASE="$1"
    WORK=$(mktemp -d -t reaper-cull)
    mkdir -p "${WORK}/bin" "${WORK}/fix"
    : > "${WORK}/log"

    for t in ${REAL_TOOLS}; do
        p=$(command -v "${t}" 2>/dev/null) || continue
        ln -sf "${p}" "${WORK}/bin/${t}"
    done

    printf 'PVE_TOKEN="PVEAPIToken=someone@realm!test=secret"\n' > "${WORK}/cred"
    chmod 600 "${WORK}/cred"

    cat > "${WORK}/bin/_stub" <<'STUB'
#!/bin/sh
me=${0##*/}
case "${me}" in
date)
    # A fixed "now", so expiry arithmetic is deterministic.
    printf '%s\n' "${FAKE_NOW}" ;;
logger)
    : ;;
sleep)
    : ;;
curl)
    method=GET
    url=""
    prev=""
    for a in "$@"; do
        [ "${prev}" = "-X" ] && method=${a}
        case "${a}" in https://*) url=${a} ;; esac
        prev=${a}
    done
    printf '%s %s\n' "${method}" "${url##*/api2/json}" >> "${FIXLOG}"

    case "${method} ${url}" in
        *"cluster/resources"*)
            if [ -f "${FIX}/resources.rc" ]; then exit "$(cat "${FIX}/resources.rc")"; fi
            cat "${FIX}/resources.json" ;;
        "GET "*"status/current")
            vmid=$(printf '%s' "${url}" | sed 's|.*/qemu/\([0-9]*\)/.*|\1|')
            if [ -f "${FIX}/status_${vmid}.json" ]; then
                cat "${FIX}/status_${vmid}.json"
            else
                printf '{"data":{"status":"stopped"}}\n'
            fi ;;
        "POST "*"status/stop")
            if [ -f "${FIX}/stop.rc" ]; then exit "$(cat "${FIX}/stop.rc")"; fi
            printf '{"data":null}\n' ;;
        "DELETE "*)
            if [ -f "${FIX}/delete.rc" ]; then exit "$(cat "${FIX}/delete.rc")"; fi
            printf '{"data":null}\n' ;;
        *)
            echo "STUB: unrouted ${method} ${url}" >&2
            exit 99 ;;
    esac ;;
*)
    echo "STUB: no behaviour defined for ${me}" >&2
    exit 99 ;;
esac
exit 0
STUB
    chmod +x "${WORK}/bin/_stub"
    for t in curl logger date sleep; do
        ln -sf "${WORK}/bin/_stub" "${WORK}/bin/${t}"
    done
}

# resources <<'JSON' ... JSON
resources() { cat > "${WORK}/fix/resources.json"; }
fixture_rc() { printf '%s\n' "$2" > "${WORK}/fix/$1"; }

run_cull() {
    ( PATH="${WORK}/bin" \
      FIX="${WORK}/fix" \
      FIXLOG="${WORK}/log" \
      FAKE_NOW="${NOW}" \
      PVE_HOST="somehost:8006" \
      PVE_POOL="a/pool" \
      VMID_MIN=9000 \
      VMID_MAX=9099 \
      CRED="${WORK}/cred" \
      PVE_INSECURE=1 \
      "${cull}" "$@" ) > "${WORK}/out" 2> "${WORK}/err"
}

ok()  { pass=$((pass + 1)); printf '  ok    %-50s %s\n' "${CASE}" "$1"; }
bad() { fail=$((fail + 1)); printf '  FAIL  %-50s %s\n' "${CASE}" "$1"
        sed 's/^/          | /' "${WORK}/err" | head -5; }

destroyed() {
    if grep -q "^DELETE /nodes/[a-z0-9]*/qemu/$1" "${WORK}/log"; then
        ok "destroyed $1"
    else
        bad "should have destroyed $1"
    fi
}

# The assertion that matters most. Not "it exited zero" -- it never issued the
# call at all.
untouched() {
    # Only the mutating verbs count: reading a guest's status is how it decides
    # to leave one alone, so a GET is not "touching" it.
    if grep -qE "^(DELETE|POST) /nodes/[a-z0-9]*/qemu/$1" "${WORK}/log"; then
        bad "must not have touched $1"
    else
        ok "never touched $1"
    fi
}

nothing_destroyed() {
    if grep -q '^DELETE' "${WORK}/log"; then
        bad "nothing should have been destroyed"
    else
        ok "destroyed nothing"
    fi
}

says() {
    if grep -qi "$1" "${WORK}/err" || grep -qi "$1" "${WORK}/out"; then
        ok "said: $1"
    else
        bad "should have said: $1"
    fi
}

expired="expires-1699999999"     # one second before NOW
future="expires-1700009999"

echo "the ordinary case"
new_case "an expired guest in range and in the pool is destroyed"
resources <<JSON
{"data":[{"vmid":9001,"node":"n1","status":"stopped","pool":"a/pool","tags":"${expired}"}]}
JSON
if run_cull; then ok "exited 0"; else bad "should have succeeded"; fi
destroyed 9001
says "reaped 1, skipped 0"

echo
echo "everything it must not touch"

new_case "a guest whose expiry has not passed is left alone"
resources <<JSON
{"data":[{"vmid":9002,"node":"n1","status":"running","pool":"a/pool","tags":"${future}"}]}
JSON
run_cull || bad "should have succeeded"
untouched 9002
nothing_destroyed

new_case "an untagged guest is reported and left alone"
resources <<JSON
{"data":[{"vmid":9003,"node":"n1","status":"running","pool":"a/pool","tags":""}]}
JSON
run_cull || bad "should have succeeded"
untouched 9003
nothing_destroyed
says "no valid expires- tag"
says "reaped 0, skipped 1"

new_case "an expired guest outside the range is refused"
# The credential is already scoped to the pool; this is the second opinion, and
# the case where pool membership itself was set up wrongly.
resources <<JSON
{"data":[{"vmid":8100,"node":"n1","status":"stopped","pool":"a/pool","tags":"${expired}"}]}
JSON
run_cull || bad "should have succeeded"
untouched 8100
nothing_destroyed
says "outside"

new_case "an expired guest in another pool is never considered"
resources <<JSON
{"data":[{"vmid":9004,"node":"n1","status":"stopped","pool":"someone/else","tags":"${expired}"}]}
JSON
run_cull || bad "should have succeeded"
untouched 9004
nothing_destroyed

new_case "a tag that is not an expiry is not read as one"
resources <<JSON
{"data":[{"vmid":9005,"node":"n1","status":"stopped","pool":"a/pool","tags":"expires-soon;ephemeral"}]}
JSON
run_cull || bad "should have succeeded"
untouched 9005
nothing_destroyed

echo
echo "when the API is not answering"

new_case "an unreachable API fails loudly instead of looking idle"
# The defect this test exists for: as one pipeline, a failed call left the exit
# status to the while loop, so an outage was indistinguishable from an empty
# cluster and the sweeper reported success having done nothing.
fixture_rc resources.rc 22
if run_cull; then bad "should have failed"; else ok "exited non-zero"; fi
nothing_destroyed
says "unreachable"

new_case "a reply that is not the guest list fails loudly"
resources <<'JSON'
{"unexpected":"shape"}
JSON
if run_cull; then bad "should have failed"; else ok "exited non-zero"; fi
nothing_destroyed

echo
echo "stopping before destroying"

new_case "a running guest is stopped first"
resources <<JSON
{"data":[{"vmid":9006,"node":"n1","status":"running","pool":"a/pool","tags":"${expired}"}]}
JSON
run_cull || bad "should have succeeded"
if grep -q '^POST /nodes/n1/qemu/9006/status/stop' "${WORK}/log"; then
    ok "asked it to stop"
else
    bad "should have asked it to stop"
fi
destroyed 9006

new_case "a guest that will not stop is not destroyed"
resources <<JSON
{"data":[{"vmid":9007,"node":"n1","status":"running","pool":"a/pool","tags":"${expired}"}]}
JSON
fixture_rc stop.rc 22
run_cull || bad "should have succeeded"
nothing_destroyed
says "stop request failed"

echo
echo "dry run"

new_case "a dry run reports what it would do and does none of it"
resources <<JSON
{"data":[{"vmid":9008,"node":"n1","status":"stopped","pool":"a/pool","tags":"${expired}"}]}
JSON
run_cull --dry-run || bad "should have succeeded"
nothing_destroyed
untouched 9008
says "dry run"

echo
echo "several at once"

new_case "a mixed cluster is sorted correctly"
resources <<JSON
{"data":[
 {"vmid":9010,"node":"n1","status":"stopped","pool":"a/pool","tags":"${expired}"},
 {"vmid":9011,"node":"n1","status":"stopped","pool":"a/pool","tags":"${future}"},
 {"vmid":9012,"node":"n1","status":"stopped","pool":"a/pool","tags":""},
 {"vmid":8100,"node":"n1","status":"stopped","pool":"a/pool","tags":"${expired}"},
 {"vmid":9013,"node":"n1","status":"stopped","pool":"someone/else","tags":"${expired}"}
]}
JSON
run_cull || bad "should have succeeded"
destroyed 9010
untouched 9011
untouched 9012
untouched 8100
untouched 9013
says "reaped 1, skipped 2"

echo
printf '%s passed, %s failed\n' "${pass}" "${fail}"
[ "${fail}" -eq 0 ]
