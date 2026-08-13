#!/bin/sh
#
# The runner's decision self-test.
#
# Every external tool the runner calls is replaced by a stub that returns
# canned output and records the call. The suite then asserts on *what the
# runner decided to do* -- the invocation log -- rather than on its exit code,
# because "it refused" and "it refused without touching anything" are different
# claims and only one of them matters.
#
# The refusals are tested harder than the successes. Choosing a disk is the one
# thing here that can destroy a machine, so the interesting question is not
# whether it works when there is exactly one empty disk, but whether it keeps
# its hands off everything else.
#
# Runs with nothing installed, nothing mounted and no privileges.
#
# Exit 0 if every case behaved, 1 otherwise.
set -eu

here=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
runner="${here}/../runner.sh"
[ -x "${runner}" ] || { echo "no runner at ${runner}" >&2; exit 1; }

pass=0
fail=0
CASE=""
WORK=""

# --- harness ---------------------------------------------------------------

# A PATH holding only stubs and the handful of real tools the runner needs.
# Isolating it is what makes "is there a container engine?" a question the test
# controls, rather than one answered by whatever this machine happens to have.
REAL_TOOLS="awk sed grep tr sort cat mkdir rm chmod cut head wc printf ln env sh basename dirname"

new_case() {
    CASE="$1"
    WORK=$(mktemp -d -t reaper-runner)
    mkdir -p "${WORK}/bin" "${WORK}/fix" "${WORK}/sysroot" "${WORK}/pool"
    : > "${WORK}/log"

    for t in ${REAL_TOOLS}; do
        p=$(command -v "${t}" 2>/dev/null) || continue
        ln -sf "${p}" "${WORK}/bin/${t}"
    done

    cat > "${WORK}/bin/_stub" <<'STUB'
#!/bin/sh
# One dispatcher, linked under every tool name. Records the call, then answers
# from the fixture directory.
# Parameter expansion, not basename: a stub that depends on a tool which may
# not be on the isolated PATH will fall through and report success, and a fake
# tool that silently succeeds makes the whole suite lie.
me=${0##*/}
printf '%s %s\n' "${me}" "$*" >> "${FIXLOG}"

fix() { [ -f "${FIX}/$1" ] && cat "${FIX}/$1" || true; }
rc()  { if [ -f "${FIX}/$1" ]; then exit "$(cat "${FIX}/$1")"; else exit "$2"; fi }

case "${me}" in
uname)
    printf '%s\n' "${FAKE_PLATFORM}" ;;
lsblk)
    # Per-device queries are answered from a fixture named for the device and
    # the column asked for, so each rule has its own input and can be broken
    # independently.
    cols=""; dev=""
    while [ $# -gt 0 ]; do
        case "$1" in
            /dev/*) dev=${1##*/} ;;
            -*)     : ;;
            *)      cols=$1 ;;
        esac
        shift
    done
    if [ -n "${dev}" ]; then fix "lsblk_${dev}_${cols}"; else fix lsblk_all; fi ;;
sysctl)
    case "$1 ${2:-}" in
        "-n kern.disks") fix kern_disks ;;
        *)               : ;;
    esac ;;
gpart)
    rc "gpart_${2:-none}.rc" 1 ;;
fstyp)
    arg=${1:-none}
    rc "fstyp_${arg##*/}.rc" 1 ;;
mount)
    fix mount ;;
zpool)
    case "$1" in
        list)
            case "$*" in
                *health*) printf 'ONLINE\n'; exit 0 ;;
            esac
            rc pool_exists.rc 1 ;;
        status) fix zpool_status ;;
        create) rc zpool_create.rc 0 ;;
        *) : ;;
    esac ;;
zfs)
    case "$1" in
        list)
            for a in "$@"; do
                case "${a}" in
                    */*) grep -qx "${a}" "${FIX}/zfs_datasets" 2>/dev/null && exit 0 || exit 1 ;;
                esac
            done
            exit 1 ;;
        create) exit 0 ;;
        *) : ;;
    esac ;;
podman)
    printf 'podman stub\n' ;;
*)
    # An unstubbed tool must be loud. Answering "fine" to a call nobody
    # modelled is how a suite ends up asserting nothing at all.
    printf 'STUB: no behaviour defined for %s\n' "${me}" >&2
    exit 99 ;;
esac
exit 0
STUB
    chmod +x "${WORK}/bin/_stub"
    for t in uname lsblk sysctl gpart fstyp mount zpool zfs; do
        ln -sf "${WORK}/bin/_stub" "${WORK}/bin/${t}"
    done
}

with_engine() { ln -sf "${WORK}/bin/_stub" "${WORK}/bin/podman"; }

fixture() { cat > "${WORK}/fix/$1"; }
fixture_rc() { printf '%s\n' "$2" > "${WORK}/fix/$1"; }

run_runner() {
    run_rc=0
    ( PATH="${WORK}/bin" \
      FIX="${WORK}/fix" \
      FIXLOG="${WORK}/log" \
      FAKE_PLATFORM="${FAKE_PLATFORM:-Linux}" \
      REAPER_SYSROOT="${WORK}/sysroot" \
      REAPER_POOL_MOUNT="${WORK}/pool" \
      "${runner}" "$@" ) > "${WORK}/out" 2> "${WORK}/err" || run_rc=$?
    printf '%s\n' "${run_rc}" > "${WORK}/status"
    return "${run_rc}"
}

ok()   { pass=$((pass + 1)); printf '  ok    %-52s %s\n' "${CASE}" "$1"; }
bad()  { fail=$((fail + 1)); printf '  FAIL  %-52s %s\n' "${CASE}" "$1"
         sed 's/^/          err| /' "${WORK}/err" | head -6 ; }

# Written out rather than as A && B || C: in that form, C also runs when B
# fails, so a reporting helper that stumbled would record both a pass and a
# failure for one assertion.
log_has() {
    if grep -q "$1" "${WORK}/log"; then ok "logged: $1"; else bad "expected in log: $1"; fi
}
log_lacks() {
    if grep -q "$1" "${WORK}/log"; then bad "must not be in log: $1"; else ok "absent: $1"; fi
}
# The runner's documented contract: 0 success, 1 failure, 2 a malformed call.
# Asserting it is what separates "it refused" from "it fell over one line after
# it should have refused", which look identical from a boolean exit status.
exited() {
    got=$(cat "${WORK}/status")
    if [ "${got}" = "$1" ]; then ok "exited $1"; else bad "expected exit $1, got ${got}"; fi
}

errsays() {
    if grep -qi "$1" "${WORK}/err"; then ok "said: $1"; else bad "error should mention: $1"; fi
}
outsays() {
    if grep -q "$1" "${WORK}/out"; then ok "reported: $1"; else bad "output should have: $1"; fi
}
wrote() {
    if [ -f "${WORK}/sysroot/$1" ]; then ok "wrote $1"; else bad "should have written $1"; fi
}
not_wrote() {
    if [ -f "${WORK}/sysroot/$1" ]; then bad "should not have written $1"; else ok "left alone: $1"; fi
}
grep_file() { # grep_file <path> <pattern> <description>
    if grep -q "$2" "${WORK}/$1"; then ok "$3"; else bad "$3"; fi
}

# The claim that matters most: whatever else happened, no pool was made on any
# disk. Asserted against the log, not inferred from an exit code.
made_no_pool() { log_lacks 'zpool create'; }

# --- Linux fixtures --------------------------------------------------------

linux_disks() {  # linux_disks "<all>" then per-disk detail via linux_disk
    FAKE_PLATFORM=Linux
    fixture lsblk_all
}

linux_disk() { # linux_disk <name> <detail lines...>
    fixture "lsblk_$1"
}

# ===========================================================================

echo "one empty disk, and everything that follows from it"
new_case "linux: builds the pool on the only empty disk"
FAKE_PLATFORM=Linux
printf 'vda disk\nvda1 part\nvdb disk\n' > "${WORK}/fix/lsblk_all"
printf 'vda\nvda1\n' > "${WORK}/fix/lsblk_vda_NAME"
printf 'ext4\n'       > "${WORK}/fix/lsblk_vda_FSTYPE"
printf '/\n'          > "${WORK}/fix/lsblk_vda_MOUNTPOINT"
printf 'vdb\n'        > "${WORK}/fix/lsblk_vdb_NAME"
fixture_rc pool_exists.rc 1
with_engine
if run_runner firstboot; then :; else bad "firstboot should have succeeded"; fi
log_has 'zpool create .* vdb'
log_lacks 'zpool create .* vda'
log_has 'zfs create tank/images'
log_has 'zfs create tank/cache'
log_has 'zfs create tank/state'
log_has 'zfs create tank/work'
wrote 'etc/modprobe.d/zfs-reaper.conf'
wrote 'etc/containers/storage.conf'
grep_file 'sysroot/etc/containers/storage.conf' "graphroot = \"${WORK}/pool/images\"" \
    "image store points at the pool"

echo
echo "refusals -- the half that matters"

new_case "linux: refuses when there is no empty disk"
FAKE_PLATFORM=Linux
printf 'vda disk\n' > "${WORK}/fix/lsblk_all"
printf 'vda\n'  > "${WORK}/fix/lsblk_vda_NAME"
printf 'ext4\n' > "${WORK}/fix/lsblk_vda_FSTYPE"
fixture_rc pool_exists.rc 1
if run_runner firstboot; then bad "should have refused"; else ok "refused"; fi
errsays 'no unused disk'
made_no_pool

new_case "linux: refuses to guess between two empty disks"
FAKE_PLATFORM=Linux
printf 'vda disk\nvdb disk\nvdc disk\n' > "${WORK}/fix/lsblk_all"
printf 'vda\n'  > "${WORK}/fix/lsblk_vda_NAME"
printf 'ext4\n' > "${WORK}/fix/lsblk_vda_FSTYPE"
printf 'vdb\n'  > "${WORK}/fix/lsblk_vdb_NAME"
printf 'vdc\n'  > "${WORK}/fix/lsblk_vdc_NAME"
fixture_rc pool_exists.rc 1
if run_runner firstboot; then bad "should have refused"; else ok "refused"; fi
errsays 'more than one unused disk'
errsays 'refusing to guess'
made_no_pool

new_case "linux: a disk with partitions is not empty"
FAKE_PLATFORM=Linux
printf 'vdb disk\n' > "${WORK}/fix/lsblk_all"
printf 'vdb\nvdb1\n' > "${WORK}/fix/lsblk_vdb_NAME"
fixture_rc pool_exists.rc 1
if run_runner firstboot; then bad "should have refused"; else ok "refused"; fi
made_no_pool

new_case "linux: a disk carrying a filesystem is not empty"
FAKE_PLATFORM=Linux
printf 'vdb disk\n' > "${WORK}/fix/lsblk_all"
printf 'vdb\n' > "${WORK}/fix/lsblk_vdb_NAME"
printf 'xfs\n' > "${WORK}/fix/lsblk_vdb_FSTYPE"
fixture_rc pool_exists.rc 1
if run_runner firstboot; then bad "should have refused"; else ok "refused"; fi
made_no_pool

new_case "linux: a mounted disk is not empty"
FAKE_PLATFORM=Linux
printf 'vdb disk\n' > "${WORK}/fix/lsblk_all"
printf 'vdb\n' > "${WORK}/fix/lsblk_vdb_NAME"
printf '/mnt/somewhere\n' > "${WORK}/fix/lsblk_vdb_MOUNTPOINT"
fixture_rc pool_exists.rc 1
if run_runner firstboot; then bad "should have refused"; else ok "refused"; fi
made_no_pool

new_case "a disk already in a pool is never a candidate"
FAKE_PLATFORM=Linux
printf 'vdb disk\n' > "${WORK}/fix/lsblk_all"
printf 'vdb\n' > "${WORK}/fix/lsblk_vdb_NAME"
printf '  pool: other\n config:\n\tNAME STATE\n\t/dev/vdb ONLINE\n' > "${WORK}/fix/zpool_status"
fixture_rc pool_exists.rc 1
if run_runner firstboot; then bad "should have refused"; else ok "refused"; fi
errsays 'no unused disk'
made_no_pool

new_case "a partition of a disk in a pool disqualifies the whole disk"
FAKE_PLATFORM=Linux
printf 'vdb disk\n' > "${WORK}/fix/lsblk_all"
printf 'vdb\n' > "${WORK}/fix/lsblk_vdb_NAME"
printf '\t/dev/vdb1 ONLINE\n' > "${WORK}/fix/zpool_status"
fixture_rc pool_exists.rc 1
if run_runner firstboot; then bad "should have refused"; else ok "refused"; fi
made_no_pool

new_case "a whole disk whose name ends in a digit is still a candidate"
# Regression. Deriving a parent disk by stripping trailing digits turns
# nvme0n1 -- a whole disk -- into nvme0n, and this disk would never be found.
FAKE_PLATFORM=Linux
printf 'nvme0n1 disk\n' > "${WORK}/fix/lsblk_all"
printf 'nvme0n1\n' > "${WORK}/fix/lsblk_nvme0n1_NAME"
fixture_rc pool_exists.rc 1
if run_runner firstboot; then :; else bad "should have built the pool"; fi
log_has 'zpool create .* nvme0n1'

new_case "a pooled partition of a digit-suffixed disk still disqualifies it"
FAKE_PLATFORM=Linux
printf 'nvme0n1 disk\n' > "${WORK}/fix/lsblk_all"
printf 'nvme0n1\n' > "${WORK}/fix/lsblk_nvme0n1_NAME"
printf '\t/dev/nvme0n1p3 ONLINE\n' > "${WORK}/fix/zpool_status"
fixture_rc pool_exists.rc 1
if run_runner firstboot; then bad "should have refused"; else ok "refused"; fi
made_no_pool

new_case "a similarly-named disk is not disqualified by its neighbour"
# vdb being in a pool must not disqualify vdb2, which is a different disk.
FAKE_PLATFORM=Linux
printf 'vdb2 disk\n' > "${WORK}/fix/lsblk_all"
printf 'vdb2\n' > "${WORK}/fix/lsblk_vdb2_NAME"
printf '\t/dev/vdb ONLINE\n' > "${WORK}/fix/zpool_status"
fixture_rc pool_exists.rc 1
if run_runner firstboot; then :; else bad "should have built the pool"; fi
log_has 'zpool create .* vdb2'

echo
echo "idempotence"

new_case "an existing pool means there is nothing to do"
FAKE_PLATFORM=Linux
printf 'vdb disk\n' > "${WORK}/fix/lsblk_all"
printf 'vdb\n' > "${WORK}/fix/lsblk_vdb_NAME"
fixture_rc pool_exists.rc 0
if run_runner firstboot; then :; else bad "should have succeeded"; fi
made_no_pool
log_lacks 'zfs create'

echo
echo "the other platform"

new_case "freebsd: builds the pool, and skips the image store with no engine"
FAKE_PLATFORM=FreeBSD
printf 'da0 cd0 vtbd1 vtbd0\n' > "${WORK}/fix/kern_disks"
fixture_rc gpart_vtbd0.rc 0      # root disk: partitioned
fixture_rc gpart_da0.rc 0
fixture_rc gpart_vtbd1.rc 1      # the empty one
fixture_rc fstyp_vtbd1.rc 1
fixture_rc pool_exists.rc 1
if run_runner firstboot; then :; else bad "firstboot should have succeeded"; fi
log_has 'zpool create .* vtbd1'
log_lacks 'zpool create .* vtbd0'
log_has 'sysctl vfs.zfs.arc_max'
wrote 'etc/sysctl.conf'
# No engine on PATH, which is the ordinary case for a host-execution guest.
not_wrote 'etc/containers/storage.conf'

new_case "freebsd: an optical device is never a candidate"
FAKE_PLATFORM=FreeBSD
printf 'cd0\n' > "${WORK}/fix/kern_disks"
fixture_rc pool_exists.rc 1
if run_runner firstboot; then bad "should have refused"; else ok "refused"; fi
errsays 'no unused disk'
made_no_pool

new_case "freebsd: a disk with a filesystem but no partition table is not empty"
FAKE_PLATFORM=FreeBSD
printf 'vtbd1\n' > "${WORK}/fix/kern_disks"
fixture_rc gpart_vtbd1.rc 1      # no partition table...
fixture_rc fstyp_vtbd1.rc 0      # ...but a filesystem written straight to it
fixture_rc pool_exists.rc 1
if run_runner firstboot; then bad "should have refused"; else ok "refused"; fi
made_no_pool

echo
echo "reporting"

new_case "info reports what is actually there"
FAKE_PLATFORM=Linux
fixture_rc pool_exists.rc 0
printf 'tank/images\ntank/work\n' > "${WORK}/fix/zfs_datasets"
if run_runner info; then :; else bad "info should have succeeded"; fi
outsays 'pool_present=yes'
outsays 'dataset_images=present'
outsays 'dataset_state=absent'
outsays 'platform=Linux'

new_case "an unknown platform is refused rather than guessed at"
FAKE_PLATFORM=Plan9
fixture_rc pool_exists.rc 1
if run_runner firstboot; then bad "should have refused"; else ok "refused"; fi
errsays 'unsupported platform'
made_no_pool

echo
echo "workspaces, images and jobs"

# A job script the runner is asked to run. Its contents are irrelevant here --
# the CLI renders it and has its own suite for that -- but it must exist,
# because a runner that will run a job it cannot read is a runner that will run
# anything.
a_job() {
    # It records what it was handed. For host execution the job really runs, so
    # asserting on what it saw is a better claim than asserting on an
    # invocation -- it is the environment a tenant's command would actually get.
    cat > "${WORK}/job.sh" <<EOF
#!/bin/sh
{ pwd; env | grep '^REAPER_'; } > "${WORK}/job-saw"
EOF
    printf '%s\n' "${WORK}/job.sh"
}

# What the job observed, once it has run.
saw() {
    if [ ! -f "${WORK}/job-saw" ]; then
        bad "the job never ran"
    elif grep -q "$1" "${WORK}/job-saw"; then
        ok "the job saw: $1"
    else
        bad "the job should have seen: $1"
    fi
}

new_case "workspace makes the working tree and the results directory"
FAKE_PLATFORM=Linux
if run_runner workspace --project a-project; then :; else bad "workspace should have succeeded"; fi
if [ -d "${WORK}/pool/work/a-project/out" ]; then
    ok "made the results directory"
else
    bad "the results directory must exist before the first job, or the reverse channel has nothing to read"
fi
outsays 'out='

new_case "workspace refuses a project name that is not one"
FAKE_PLATFORM=Linux
if run_runner workspace --project '../../etc'; then bad "should have refused"; else ok "refused"; fi
errsays 'not a usable project name'
exited 2

new_case "exec runs a job in the named image, with the tree and the caches mounted"
FAKE_PLATFORM=Linux
with_engine
job=$(a_job)
run_runner workspace --project a-project
if run_runner exec --project a-project --job "${job}" \
     --image docker.io/library/example@sha256:deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef \
     --cache cargo --cache build-dir; then :; else bad "exec should have succeeded"; fi
log_has 'podman run --rm'
log_has 'run --rm -w /reaper/work'
log_has "${WORK}/pool/work/a-project:/reaper/work"
log_has "${WORK}/job.sh:/reaper/job.sh:ro"
log_has 'REAPER_WORK=/reaper/work'
log_has 'REAPER_OUT=/reaper/work/out'
log_has "${WORK}/pool/cache/cargo:/reaper/cache/cargo"
log_has 'REAPER_CACHE_CARGO=/reaper/cache/cargo'
# A manifest name may carry a hyphen; an environment variable may not.
log_has 'REAPER_CACHE_BUILD_DIR=/reaper/cache/build-dir'
log_has '/bin/sh /reaper/job.sh'
if [ -d "${WORK}/pool/cache/cargo" ]; then ok "made the cargo cache"; else bad "cache directory missing"; fi

new_case "a cold profile mounts no cache at all"
FAKE_PLATFORM=Linux
with_engine
job=$(a_job)
run_runner workspace --project a-project
if run_runner exec --project a-project --job "${job}" \
     --image docker.io/library/example@sha256:deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef; then :; else bad "exec should have succeeded"; fi
log_has 'podman run --rm'
# Not merely unset: a cold run must not even name a cache. Determinism mode is
# the control for "was the warm cache the reason this passed", and a cache
# reachable by a path the tenant could guess would defeat it.
log_lacks '/reaper/cache'
log_lacks 'REAPER_CACHE_'

new_case "host execution runs the job here, with no engine involved"
FAKE_PLATFORM=Linux
job=$(a_job)
run_runner workspace --project a-project
if run_runner exec --project a-project --job "${job}"; then :; else bad "exec should have succeeded"; fi
log_lacks 'podman'
# The working directory is the synced tree, without the job having to arrange
# it: a tenant's command should not need to know where it landed.
saw "^${WORK}/pool/work/a-project\$"
saw "REAPER_WORK=${WORK}/pool/work/a-project"
saw "REAPER_OUT=${WORK}/pool/work/a-project/out"

new_case "host execution still hands over the caches, by their host paths"
FAKE_PLATFORM=Linux
job=$(a_job)
run_runner workspace --project a-project
if run_runner exec --project a-project --job "${job}" --cache cargo; then :; else bad "exec should have succeeded"; fi
saw "REAPER_CACHE_CARGO=${WORK}/pool/cache/cargo"

new_case "exec refuses an image when there is no engine to run it"
FAKE_PLATFORM=Linux
job=$(a_job)
run_runner workspace --project a-project
if run_runner exec --project a-project --job "${job}" \
     --image docker.io/library/example@sha256:deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef; then
    bad "should have refused"
else
    ok "refused"
fi
errsays 'no container engine'
# And refused *before* invoking anything. Without this the assertion above
# passes just as well when the refusal only logs and the shell then fails
# trying to run an engine whose name is the empty string.
exited 1

new_case "exec refuses an image that is not pinned by digest"
FAKE_PLATFORM=Linux
with_engine
job=$(a_job)
run_runner workspace --project a-project
if run_runner exec --project a-project --job "${job}" --image docker.io/library/example:latest; then
    bad "should have refused"
else
    ok "refused"
fi
errsays 'not a digest-pinned image'
exited 2
log_lacks 'podman run'

new_case "exec refuses a job script it cannot read"
FAKE_PLATFORM=Linux
run_runner workspace --project a-project
if run_runner exec --project a-project --job "${WORK}/absent.sh"; then bad "should have refused"; else ok "refused"; fi
errsays 'no job script'
exited 2

new_case "exec refuses before a tree has ever been synced"
FAKE_PLATFORM=Linux
job=$(a_job)
if run_runner exec --project never-synced --job "${job}"; then bad "should have refused"; else ok "refused"; fi
errsays 'sync first'
exited 1

new_case "pull fetches each digest, and refuses anything unpinned"
FAKE_PLATFORM=Linux
with_engine
if run_runner pull \
     docker.io/library/a@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa \
     docker.io/library/b@sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb; then
    :
else
    bad "pull should have succeeded"
fi
log_has 'podman pull docker.io/library/a@sha256:aaa'
log_has 'podman pull docker.io/library/b@sha256:bbb'

new_case "pull refuses a moving tag rather than fetching it"
FAKE_PLATFORM=Linux
with_engine
if run_runner pull docker.io/library/a:latest; then bad "should have refused"; else ok "refused"; fi
errsays 'not a digest-pinned image'
exited 2
log_lacks 'podman pull'

new_case "pull on a guest with no engine says so and does not fail"
FAKE_PLATFORM=FreeBSD
if run_runner pull docker.io/library/a@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa; then
    ok "did not fail"
else
    bad "a pre-pull is an optimisation; failing it would cost a usable session"
fi
errsays 'no container engine'

echo
printf '%s passed, %s failed\n' "${pass}" "${fail}"
[ "${fail}" -eq 0 ]
