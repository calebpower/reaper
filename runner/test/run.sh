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

# One parent for every case's scratch directory, so cleanup is a single removal
# and so a leak check can scope itself to *this* run rather than to whatever
# else is in /tmp.
RUNDIR=$(mktemp -d "${TMPDIR:-/tmp}/reaper-suite.XXXXXXXX")

# Stop anything this run started and take its directories with it, on every exit
# path including a failure or a signal.
#
# This suite leaked a control loop per container case and a directory per case,
# for as long as the trigger existed -- 379 spinning shells and four thousand
# directories by the time anyone looked. The Rust harness had the same defect
# and was given the same treatment; the lesson is that a suite which starts
# processes needs an exit path that stops them, not a stop call at the end of
# each case that a failure can skip.
cleanup() {
    pkill -f "${RUNDIR}" 2>/dev/null || true
    rm -rf "${RUNDIR}"
}
trap cleanup EXIT HUP INT TERM

# --- harness ---------------------------------------------------------------

# A PATH holding only stubs and the handful of real tools the runner needs.
# Isolating it is what makes "is there a container engine?" a question the test
# controls, rather than one answered by whatever this machine happens to have.
# The isolation exists so the suite controls the *decisions* -- which disk,
# which engine, which snapshot exists -- not so it second-guesses `cp`. These
# are ordinary file utilities whose behaviour nothing here asserts.
REAL_TOOLS="awk sed grep tr sort cat mkdir rm chmod cut head wc printf ln env sh
            basename dirname cp mv ls sleep nohup hostname date touch tail
            readlink"

new_case() {
    CASE="$1"
    # Not `mktemp -d -t <name>`. That is portable-looking and is not: on one
    # system the argument is a prefix, on another it is a template that must
    # end in X's, and there it fails outright. A full template works on both,
    # and this suite has to run on the systems it tests.
    WORK=$(mktemp -d "${RUNDIR}/case.XXXXXXXX")
    mkdir -p "${WORK}/bin" "${WORK}/fix" "${WORK}/sysroot" "${WORK}/pool" "${WORK}/proc"
    : > "${WORK}/log"

    for t in ${REAL_TOOLS}; do
        p=$(command -v "${t}" 2>/dev/null) || continue
        ln -sf "${p}" "${WORK}/bin/${t}"
    done

    # A plausible disk size by default, so every firstboot case need not
    # restate one. 64 GiB, which is the site default for a session's pool.
    printf '68719476736\n' > "${WORK}/fix/disk_bytes"
    # An empty directory nothing is holding open, so a rollback case has to opt
    # in to being blocked rather than tripping over the suite's own files.
    mkdir -p "${WORK}/idle"
    printf '%s\n' "${WORK}/idle" > "${WORK}/fix/zfs_mountpoint"
    printf 'somedisk 512 68719476736 134217728\n' > "${WORK}/fix/diskinfo"

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
dd)
    # Recorded and refused. A suite that really ran dd against a device would
    # be one keystroke from destroying the machine running it.
    exit 0 ;;
blockdev)
    fix disk_bytes ;;
diskinfo)
    fix diskinfo ;;
fstat)
    fix fstat ;;
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
        snapshot|rollback) exit 0 ;;
        get)
            if [ -f "${FIX}/zfs_get_rc" ]; then exit "$(cat "${FIX}/zfs_get_rc")"; fi
            fix zfs_mountpoint ;;
        list)
            # `zfs list -t snapshot` asks whether a named snapshot exists; the
            # fixture decides, so a test can model both answers.
            for a in "$@"; do
                case "${a}" in
                    snapshot) for b in "$@"; do
                                  case "${b}" in
                                      *@*) grep -qx "${b}" "${FIX}/zfs_snapshots" 2>/dev/null && exit 0 || exit 1 ;;
                                  esac
                              done
                              # No @ anywhere means a listing rather than an
                              # existence check: `-r <dataset>`.
                              fix zfs_snapshots; exit 0 ;;
                esac
            done
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
    # `ps -q` has to answer with plausible ids, because the reset path iterates
    # over them. Anything else just records the call.
    if [ "$1" = ps ]; then fix running_containers; else printf 'podman stub\n'; fi ;;
*)
    # An unstubbed tool must be loud. Answering "fine" to a call nobody
    # modelled is how a suite ends up asserting nothing at all.
    printf 'STUB: no behaviour defined for %s\n' "${me}" >&2
    exit 99 ;;
esac
exit 0
STUB
    chmod +x "${WORK}/bin/_stub"
    for t in uname lsblk sysctl gpart fstyp mount zpool zfs blockdev diskinfo dd fstat; do
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
      REAPER_PROC="${WORK}/proc" \
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
# Residue is cleared at both ends before the pool is made. A provider hands out
# recycled space, so a backup partition-table header can outlive the volume it
# belonged to and nothing but zpool will see it.
log_has 'dd if=/dev/zero of=/dev/vdb bs=1048576 count=1 conv=notrunc'
log_has 'dd if=/dev/zero of=/dev/vdb bs=1048576 count=1 seek=65535 conv=notrunc'
# And zpool keeps its veto: no -f anywhere.
log_lacks 'zpool create -f'
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
# A tenant that declares something to roll back gets a trigger at `up`, so the
# channel exists by the time anything is executed.
run_runner control --project a-project start
if run_runner exec --project a-project --job "${job}" \
     --image docker.io/library/example@sha256:deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef \
     --cache cargo --cache build-dir; then :; else bad "exec should have succeeded"; fi
log_has 'podman run --rm'
log_has 'run --rm -w /reaper/work'
log_has "${WORK}/pool/work/a-project:/reaper/work"
log_has "${WORK}/job.sh:/reaper/job.sh:ro"
log_has 'REAPER_WORK=/reaper/work'
log_has 'REAPER_OUT=/reaper/work/out'
# State is the dataset reset rolls back, and until Phase 4 it was created by
# firstboot and then reachable by nothing at all.
log_has "${WORK}/pool/state:/reaper/state"
log_has 'REAPER_STATE=/reaper/state'
# Split deliberately: the writable half a container can reach, and the
# read-only wrapper. Nothing the host executes is inside either.
log_has "${WORK}/pool/control/a-project/io:/reaper/control/io"
log_has "${WORK}/pool/control/a-project/reset:/reaper/control/reset:ro"
log_has "${WORK}/pool/control/a-project/snapshot:/reaper/control/snapshot:ro"
log_has 'REAPER_CONTROL=/reaper/control'
log_has "${WORK}/pool/cache/cargo:/reaper/cache/cargo"
log_has 'REAPER_CACHE_CARGO=/reaper/cache/cargo'
# A manifest name may carry a hyphen; an environment variable may not.
log_has 'REAPER_CACHE_BUILD_DIR=/reaper/cache/build-dir'
log_has '/bin/sh /reaper/job.sh'
if [ -d "${WORK}/pool/cache/cargo" ]; then ok "made the cargo cache"; else bad "cache directory missing"; fi
run_runner control --project a-project stop

new_case "with no trigger set up, exec mounts none and starts none"
FAKE_PLATFORM=Linux
with_engine
job=$(a_job)
run_runner workspace --project a-project
# No `control start`, which is what a tenant declaring no reset datasets gets.
if run_runner exec --project a-project --job "${job}" \
     --image docker.io/library/example@sha256:deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef; then :; else bad "exec should have succeeded"; fi
log_has 'podman run --rm'
# Starting a daemon is a decision. exec used to make it here, as a side effect
# of wanting a file to mount -- for every tenant, including those the CLI
# deliberately gives no trigger.
log_lacks '/reaper/control'
log_lacks 'REAPER_CONTROL'
if [ -f "${WORK}/pool/control/a-project/loop.pid" ]; then
    bad "exec started a control loop nobody asked for"
else
    ok "no control loop was started"
fi

new_case "a cold run gets an empty cache, not a missing one"
FAKE_PLATFORM=Linux
with_engine
job=$(a_job)
run_runner workspace --project a-project
# Something left behind by an earlier cold run, which must not survive.
mkdir -p "${WORK}/pool/cold/cargo"
: > "${WORK}/pool/cold/cargo/stale"
if run_runner exec --project a-project --job "${job}" --cold \
     --image docker.io/library/example@sha256:deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef \
     --cache cargo; then :; else bad "exec should have succeeded"; fi
log_has 'podman run --rm'
# The load-bearing claim: nothing from the warm cache is reachable. That is what
# makes determinism mode a control for "was the warm cache the reason this
# passed".
log_lacks "${WORK}/pool/cache"
# But the tenant still gets the variable and the same path inside, because a
# command that names a cache path is the documented way to use one.
log_has 'REAPER_CACHE_CARGO=/reaper/cache/cargo'
log_has "${WORK}/pool/cold/cargo:/reaper/cache/cargo"
if [ -e "${WORK}/pool/cold/cargo/stale" ]; then
    bad "a cold run must not inherit the last cold run's output"
else
    ok "the cold cache was emptied first"
fi

new_case "host execution runs the job here, with no engine involved"
FAKE_PLATFORM=Linux
job=$(a_job)
run_runner workspace --project a-project
run_runner control --project a-project start
if run_runner exec --project a-project --job "${job}"; then :; else bad "exec should have succeeded"; fi
log_lacks 'podman'
# The working directory is the synced tree, without the job having to arrange
# it: a tenant's command should not need to know where it landed.
saw "^${WORK}/pool/work/a-project\$"
saw "REAPER_WORK=${WORK}/pool/work/a-project"
saw "REAPER_OUT=${WORK}/pool/work/a-project/out"
# Same names in both modes, so a tenant's command need not know which it got.
saw "REAPER_STATE=${WORK}/pool/state"
saw "REAPER_CONTROL=${WORK}/pool/control/a-project"
run_runner control --project a-project stop

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
echo "snapshots, and rolling back to one"

new_case "snapshot takes one, by its full dataset path"
FAKE_PLATFORM=Linux
: > "${WORK}/fix/zfs_snapshots"
if run_runner snapshot --dataset state --name pristine; then :; else bad "snapshot should have succeeded"; fi
log_has 'zfs snapshot tank/state@pristine'
outsays 'snapshot=tank/state@pristine'

new_case "snapshots lists the points that exist, by name alone"
FAKE_PLATFORM=Linux
printf 'tank/state@pristine\ntank/state@mid\n' > "${WORK}/fix/zfs_snapshots"
if run_runner snapshots --dataset state; then :; else bad "snapshots should have succeeded"; fi
outsays '^pristine$'
outsays '^mid$'
# The full path is this script's business; a caller asked which points exist.
if grep -q 'tank/state@' "${WORK}/out"; then bad "leaked the dataset path"; else ok "names only"; fi

new_case "snapshots on a dataset with none says nothing, and succeeds"
FAKE_PLATFORM=Linux
: > "${WORK}/fix/zfs_snapshots"
if run_runner snapshots --dataset state; then ok "succeeded"; else bad "an empty list is not a failure"; fi
if [ -s "${WORK}/out" ]; then bad "should have printed nothing"; else ok "printed nothing"; fi

new_case "snapshots refuses a dataset it does not know"
FAKE_PLATFORM=Linux
if run_runner snapshots --dataset work; then bad "should have refused"; else ok "refused"; fi
exited 2

new_case "--if-absent keeps the snapshot that is already there"
FAKE_PLATFORM=Linux
printf 'tank/state@pristine\n' > "${WORK}/fix/zfs_snapshots"
if run_runner snapshot --dataset state --name pristine --if-absent; then :; else bad "should have succeeded"; fi
# The point of a named point is that it does not move under you.
log_lacks 'zfs snapshot'
errsays 'already exists'

new_case "without --if-absent, an existing snapshot is refused rather than replaced"
FAKE_PLATFORM=Linux
printf 'tank/state@pristine\n' > "${WORK}/fix/zfs_snapshots"
if run_runner snapshot --dataset state --name pristine; then bad "should have refused"; else ok "refused"; fi
exited 1
log_lacks 'zfs snapshot'

new_case "only state may be snapshotted or rolled back"
FAKE_PLATFORM=Linux
: > "${WORK}/fix/zfs_snapshots"
for ds in work cache images tank; do
    if run_runner snapshot --dataset "${ds}" --name pristine; then
        bad "should have refused ${ds}"
    else
        ok "refused ${ds}"
    fi
done
# work carries results outward; cache and images are what make the next
# iteration fast. Rolling either back would destroy the reason they are split.
log_lacks 'zfs snapshot'
exited 2

new_case "rollback rolls back, after stopping what is running"
FAKE_PLATFORM=Linux
with_engine
printf 'tank/state@pristine\n' > "${WORK}/fix/zfs_snapshots"
printf 'aaa111\nbbb222\n' > "${WORK}/fix/running_containers"
if run_runner rollback --dataset state --name pristine; then :; else bad "rollback should have succeeded"; fi
log_has 'podman stop aaa111'
log_has 'podman stop bbb222'
log_has 'zfs rollback -r tank/state@pristine'
outsays 'rolled_back=tank/state@pristine'

new_case "the container that asked for the reset is not the one stopped"
FAKE_PLATFORM=Linux
with_engine
printf 'tank/state@pristine\n' > "${WORK}/fix/zfs_snapshots"
printf 'aaa111\nbbb222\n' > "${WORK}/fix/running_containers"
if run_runner rollback --dataset state --name pristine --except-container bbb222; then :; else bad "should have succeeded"; fi
log_has 'podman stop aaa111'
# Stopping the caller would look exactly like the reset having crashed.
log_lacks 'podman stop bbb222'
log_has 'zfs rollback'

new_case "an unreadable mountpoint refuses with a sentence, not a silent death"
FAKE_PLATFORM=Linux
with_engine
printf 'tank/state@pristine\n' > "${WORK}/fix/zfs_snapshots"
: > "${WORK}/fix/running_containers"
# zfs get fails. Under set -e an unguarded substitution here used to kill the
# runner with no message and no exit-2 contract -- and, worse to think about,
# it died before the open-files check it exists to feed.
printf '1\n' > "${WORK}/fix/zfs_get_rc"
if run_runner rollback --dataset state --name pristine; then
    bad "should have refused when the mountpoint cannot be read"
else
    ok "refused"
fi
errsays 'cannot read'
errsays 'not rolling back'
log_lacks 'zfs rollback'

new_case "a live process on the dataset stops the rollback, whatever ZFS would allow"
FAKE_PLATFORM=Linux
with_engine
printf 'tank/state@pristine\n' > "${WORK}/fix/zfs_snapshots"
: > "${WORK}/fix/running_containers"
# The mountpoint is a scratch directory, and process 4242 holds a descriptor
# onto a file inside it -- exactly the shape /proc gives for a tenant that
# daemonised something.
mkdir -p "${WORK}/held" "${WORK}/proc/4242/fd"
printf '%s\n' "${WORK}/held" > "${WORK}/fix/zfs_mountpoint"
: > "${WORK}/held/file"
ln -sf "${WORK}/held/file" "${WORK}/proc/4242/fd/6"
# And one holding a file that is already unlinked, which must NOT block: it is
# reading an inode a rollback cannot reach, and counting it would let a single
# leaked process veto every reset for the life of the session.
mkdir -p "${WORK}/proc/4243/fd"
ln -sf "${WORK}/held/gone (deleted)" "${WORK}/proc/4243/fd/6"
if run_runner rollback --dataset state --name pristine; then
    bad "should have refused while a process held the dataset"
else
    ok "refused"
fi
errsays 'have files open'
# Named, so an operator can go and look. And the deleted-file holder is not
# among them.
errsays '4242'
if grep -q '4243' "${WORK}/err"; then bad "a deleted-file holder must not block"; else ok "deleted-file holder ignored"; fi
# ZFS itself would have gone ahead: this was tested on a live guest, and the
# holder was left reading data that no longer existed. So the refusal has to
# happen before the call, not be hoped for from it.
log_lacks 'zfs rollback'

new_case "a process merely sitting in the dataset blocks it too"
FAKE_PLATFORM=Linux
with_engine
printf 'tank/state@pristine\n' > "${WORK}/fix/zfs_snapshots"
: > "${WORK}/fix/running_containers"
# No open descriptor at all -- just a working directory inside the dataset,
# which is every bit as much a live view of it.
mkdir -p "${WORK}/held" "${WORK}/proc/4244"
printf '%s\n' "${WORK}/held" > "${WORK}/fix/zfs_mountpoint"
ln -sf "${WORK}/held" "${WORK}/proc/4244/cwd"
if run_runner rollback --dataset state --name pristine; then bad "should have refused"; else ok "refused"; fi
errsays '4244'
log_lacks 'zfs rollback'

new_case "a process holding only an unlinked file does not block a rollback"
FAKE_PLATFORM=Linux
with_engine
printf 'tank/state@pristine\n' > "${WORK}/fix/zfs_snapshots"
: > "${WORK}/fix/running_containers"
mkdir -p "${WORK}/held" "${WORK}/proc/4243/fd"
printf '%s\n' "${WORK}/held" > "${WORK}/fix/zfs_mountpoint"
ln -sf "${WORK}/held/gone (deleted)" "${WORK}/proc/4243/fd/6"
if run_runner rollback --dataset state --name pristine; then ok "rolled back"; else bad "should not have been blocked"; fi
log_has 'zfs rollback -r tank/state@pristine'

new_case "FreeBSD: a live holder stops the rollback, and is named"
FAKE_PLATFORM=FreeBSD
printf 'tank/state@pristine\n' > "${WORK}/fix/zfs_snapshots"
mkdir -p "${WORK}/held"
printf '%s\n' "${WORK}/held" > "${WORK}/fix/zfs_mountpoint"
# The observed shape, live on 15.1-RELEASE-p2: MOUNT ($5) is the mountpoint
# for a linked file; the header row is skipped by NR > 1.
printf 'USER CMD PID FD MOUNT INUM MODE SZ|DV R/W\nroot sleep 4242 3 %s 2 -rw-r--r-- 13 r\nroot sh 4244 wd %s 34 drwxr-xr-x 2 r\n' \
    "${WORK}/held" "${WORK}/held" > "${WORK}/fix/fstat"
if run_runner rollback --dataset state --name pristine; then
    bad "should have refused while a process held the dataset"
else
    ok "refused"
fi
errsays '4242'
errsays '4244'
log_lacks 'zfs rollback'

new_case "FreeBSD: an unlinked-only holder does not block a rollback"
FAKE_PLATFORM=FreeBSD
printf 'tank/state@pristine\n' > "${WORK}/fix/zfs_snapshots"
mkdir -p "${WORK}/held"
printf '%s\n' "${WORK}/held" > "${WORK}/fix/zfs_mountpoint"
# MOUNT is "-" for a file unlinked after opening -- observed live. Such a
# process reads an inode a rollback cannot reach, and counting it would let
# one leaked process veto every reset for the life of the session.
printf 'USER CMD PID FD MOUNT INUM MODE SZ|DV R/W\nroot sleep 4242 3 - 2 -rw-r--r-- 13 r\n' \
    > "${WORK}/fix/fstat"
if run_runner rollback --dataset state --name pristine; then ok "rolled back"; else bad "should not have been blocked"; fi
log_has 'zfs rollback -r tank/state@pristine'

new_case "FreeBSD: a missing fstat is a refusal, not a silent proceed"
FAKE_PLATFORM=FreeBSD
printf 'tank/state@pristine\n' > "${WORK}/fix/zfs_snapshots"
mkdir -p "${WORK}/held"
printf '%s\n' "${WORK}/held" > "${WORK}/fix/zfs_mountpoint"
# The tool is absent from the guest. Before the guard, the 2>/dev/null ate
# "not found", holders printed nothing, and the rollback proceeded unchecked.
rm "${WORK}/bin/fstat"
if run_runner rollback --dataset state --name pristine; then
    bad "should have refused without a way to check open files"
else
    ok "refused"
fi
errsays 'fstat'
errsays 'not rolling back'
log_lacks 'zfs rollback'

new_case "an operating system the check does not know fails closed"
FAKE_PLATFORM=SunOS
printf 'tank/state@pristine\n' > "${WORK}/fix/zfs_snapshots"
mkdir -p "${WORK}/held"
printf '%s\n' "${WORK}/held" > "${WORK}/fix/zfs_mountpoint"
if run_runner rollback --dataset state --name pristine; then
    bad "no holders found must never mean did not look"
else
    ok "refused"
fi
errsays 'no way to check'
log_lacks 'zfs rollback'

new_case "the caller is spared whether it names itself long or short"
FAKE_PLATFORM=Linux
with_engine
printf 'tank/state@pristine\n' > "${WORK}/fix/zfs_snapshots"
# An engine reports short ids; a container asked for its own hostname may give
# either. Both directions have to match, and an exact-match test alone cannot
# tell that -- either comparison would pass it.
printf 'aaa111\nbbb222\n' > "${WORK}/fix/running_containers"
run_runner rollback --dataset state --name pristine \
    --except-container bbb2223333444455556666777788889999
log_has 'podman stop aaa111'
log_lacks 'podman stop bbb222'

new_case "and the other way round, when the engine reports the long form"
FAKE_PLATFORM=Linux
with_engine
printf 'tank/state@pristine\n' > "${WORK}/fix/zfs_snapshots"
printf 'aaa111\nbbb2223333444455556666777788889999\n' > "${WORK}/fix/running_containers"
run_runner rollback --dataset state --name pristine --except-container bbb222
log_has 'podman stop aaa111'
log_lacks 'podman stop bbb222'

new_case "rollback to a snapshot that does not exist is refused, and stops nothing"
FAKE_PLATFORM=Linux
with_engine
: > "${WORK}/fix/zfs_snapshots"
printf 'aaa111\n' > "${WORK}/fix/running_containers"
if run_runner rollback --dataset state --name pristine; then bad "should have refused"; else ok "refused"; fi
errsays 'there is no tank/state@pristine'
# Nothing may be torn down on the way to discovering there was nothing to roll
# back to.
log_lacks 'podman stop'
log_lacks 'zfs rollback'

new_case "a snapshot name that is not one is refused"
FAKE_PLATFORM=Linux
if run_runner snapshot --dataset state --name '../../etc'; then bad "should have refused"; else ok "refused"; fi
errsays 'not a usable snapshot name'
exited 2
log_lacks 'zfs snapshot'

echo
echo "the in-guest trigger"

new_case "control start leaves a wrapper a tenant can run, and a loop watching"
FAKE_PLATFORM=Linux
if run_runner control --project a-project start; then :; else bad "control start should have succeeded"; fi
ctl="${WORK}/pool/control/a-project"
if [ -x "${ctl}/reset" ]; then ok "wrapper is there and executable"; else bad "no wrapper at ${ctl}/reset"; fi
if [ -x "${ctl}/runner.sh" ]; then ok "a private copy of the runner"; else bad "no runner copy"; fi
# The security boundary, asserted rather than assumed: the only thing mounted
# into a container is io/ and the read-only wrapper, and the script the host
# runs as root is in neither.
if [ -e "${ctl}/io/runner.sh" ]; then
    bad "the root-executed runner is inside the directory containers can write"
else
    ok "the runner copy is outside the container-writable directory"
fi
# `stat` disagrees between platforms about what -f means, and -- the part that
# bit -- it does not fail when asked the wrong way. On Linux `stat -f` is
# "filesystem status", so the BSD spelling *succeeds* and prints something that
# is not a mode at all, and an `||` fallback never fires. Found by running this
# suite inside a Linux session, having only ever run it on BSD.
#
# So try both and judge the answer rather than the exit status.
mode=$(stat -c '%a' "${ctl}/runner.sh" 2>/dev/null || true)
case "${mode}" in
    ''|*[!0-7]*) mode=$(stat -f '%Lp' "${ctl}/runner.sh" 2>/dev/null || true) ;;
esac
case "${mode}" in
    700) ok "runner copy is root-only (${mode})" ;;
    *)   bad "runner copy is mode '${mode}', expected 700" ;;
esac
if [ -f "${ctl}/loop.pid" ]; then ok "recorded a pid"; else bad "no pid file"; fi
outsays 'control='
run_runner control --project a-project stop

new_case "a tenant can mark a point from inside, and marking twice keeps the first"
FAKE_PLATFORM=Linux
with_engine
# Nothing exists yet, so the first request should take one.
: > "${WORK}/fix/zfs_snapshots"
: > "${WORK}/fix/running_containers"
run_runner control --project a-project start
ctl="${WORK}/pool/control/a-project"
if [ -x "${ctl}/snapshot" ]; then ok "a snapshot wrapper exists too"; else bad "no snapshot wrapper"; fi
if ( PATH="${WORK}/bin:$PATH" FIX="${WORK}/fix" FIXLOG="${WORK}/log" \
     FAKE_PLATFORM=Linux REAPER_SYSROOT="${WORK}/sysroot" \
     REAPER_POOL_MOUNT="${WORK}/pool" REAPER_RESET_TIMEOUT=20 "${ctl}/snapshot" ) ; then
    ok "the wrapper returned success"
else
    bad "the wrapper failed"
fi
log_has 'zfs snapshot tank/state@pristine'
# It must not roll anything back on the way past.
log_lacks 'zfs rollback'

# Now say it already exists, and ask again: a named point does not move.
printf 'tank/state@pristine\n' > "${WORK}/fix/zfs_snapshots"
: > "${WORK}/log"
if ( PATH="${WORK}/bin:$PATH" FIX="${WORK}/fix" FIXLOG="${WORK}/log" \
     FAKE_PLATFORM=Linux REAPER_SYSROOT="${WORK}/sysroot" \
     REAPER_POOL_MOUNT="${WORK}/pool" REAPER_RESET_TIMEOUT=20 "${ctl}/snapshot" ) ; then
    ok "asking again still succeeds"
else
    bad "asking again should not be an error"
fi
log_lacks 'zfs snapshot'
run_runner control --project a-project stop

new_case "the wrapper refuses to be called by a name it does not know"
FAKE_PLATFORM=Linux
run_runner control --project a-project start
ctl="${WORK}/pool/control/a-project"
cp "${ctl}/reset" "${ctl}/destroy"
if ( PATH="${WORK}/bin:$PATH" REAPER_POOL_MOUNT="${WORK}/pool" "${ctl}/destroy" ) 2>"${WORK}/err"; then
    bad "should have refused"
else
    ok "refused"
fi
errsays "not a verb it knows"
run_runner control --project a-project stop

new_case "a caller id that is not one is refused, and nothing is rolled back"
FAKE_PLATFORM=Linux
with_engine
printf 'tank/state@pristine\n' > "${WORK}/fix/zfs_snapshots"
printf 'aaa111\nbbb222\n' > "${WORK}/fix/running_containers"
run_runner control --project a-project start
ctl="${WORK}/pool/control/a-project"
# A glob as the caller id would spare every container, so the rollback would
# run with the stack still live. It arrives from inside a container, so it is
# not something to take on trust.
printf 'reset\npristine\n*\n' > "${ctl}/io/req.evil.tmp"
mv "${ctl}/io/req.evil.tmp" "${ctl}/io/req.evil"
waited=0
while [ ! -f "${ctl}/io/res.evil" ] && [ "${waited}" -lt 15 ]; do sleep 1; waited=$((waited + 1)); done
if [ -f "${ctl}/io/res.evil" ]; then ok "answered rather than hanging"; else bad "no answer"; fi
log_lacks 'zfs rollback'
if grep -q "refusing a caller id" "${ctl}/loop.log"; then ok "said why"; else bad "should have said why"; fi
run_runner control --project a-project stop

new_case "control start twice does not start a second loop"
FAKE_PLATFORM=Linux
run_runner control --project a-project start
first=$(cat "${WORK}/pool/control/a-project/loop.pid")
if run_runner control --project a-project start; then :; else bad "should have succeeded"; fi
second=$(cat "${WORK}/pool/control/a-project/loop.pid")
if [ "${first}" = "${second}" ]; then ok "same loop, not a second one"; else bad "started a second loop"; fi
errsays 'already running'
run_runner control --project a-project stop

new_case "the wrapper and the loop agree: a request gets served and answered"
FAKE_PLATFORM=Linux
with_engine
printf 'tank/state@pristine\n' > "${WORK}/fix/zfs_snapshots"
: > "${WORK}/fix/running_containers"
run_runner control --project a-project start
ctl="${WORK}/pool/control/a-project"
# Run the wrapper exactly as a tenant would, with the same stubbed PATH.
if ( PATH="${WORK}/bin:$PATH" FIX="${WORK}/fix" FIXLOG="${WORK}/log" \
     FAKE_PLATFORM=Linux REAPER_SYSROOT="${WORK}/sysroot" \
     REAPER_POOL_MOUNT="${WORK}/pool" REAPER_RESET_TIMEOUT=20 "${ctl}/reset" ) ; then
    ok "the wrapper returned success"
else
    bad "the wrapper failed"
fi
log_has 'zfs rollback -r tank/state@pristine'
if ls "${ctl}"/io/req.* >/dev/null 2>&1; then bad "a request was left behind"; else ok "no request left behind"; fi
if ls "${ctl}"/io/res.* >/dev/null 2>&1; then bad "a reply was left behind"; else ok "no reply left behind"; fi
run_runner control --project a-project stop

echo
echo "nothing left running"
CASE="the suite leaves no process behind"
leaked=$(pgrep -f "${RUNDIR}" 2>/dev/null | wc -l | tr -d ' ')
if [ "${leaked}" -eq 0 ]; then
    ok "no stray processes"
else
    bad "${leaked} process(es) still running from this suite"
    pgrep -lf "${RUNDIR}" 2>/dev/null | head -5 | sed 's/^/          /'
fi

echo
printf '%s passed, %s failed\n' "${pass}" "${fail}"
[ "${fail}" -eq 0 ]
