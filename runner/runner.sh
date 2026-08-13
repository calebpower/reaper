#!/bin/sh
#
# The reaper runner.
#
# Delivered over SSH by the CLI at session start and invoked; it is never
# installed into a template and nothing here runs at boot. That is deliberate:
# a runner living in a template would mean rebuilding hand-made templates
# whenever it changed, and version skew between a template and the CLI driving
# it.
#
# POSIX sh on purpose. Everything below is shelling out to zpool, zfs and a
# container engine, so a compiled binary would buy types around subprocess
# calls and cost a cross-build for every guest operating system.
#
# Usage:
#   runner.sh firstboot                     make the pool and datasets; idempotent
#   runner.sh info                          report what exists, as key=value
#   runner.sh workspace --project P         make the work and results directories
#   runner.sh pull REF...                   fetch digest-pinned images
#   runner.sh exec --project P --job PATH [--image REF] [--cache NAME]...
#                                           run a delivered job script
#   runner.sh snapshot --dataset D --name N [--if-absent]
#   runner.sh rollback --dataset D --name N [--except-container ID]
#   runner.sh control --project P {start|stop}
#                                           the in-guest reset trigger
#
# Exit 0 on success, 1 on failure, 2 on a usage error.
set -eu

POOL="${REAPER_POOL:-tank}"

# Where the pool is mounted. `zpool create -m` below puts it here. Overridable
# for the same reason SYSROOT is: the test suite points it at a scratch
# directory so the suite never writes to the machine running it.
POOL_MOUNT="${REAPER_POOL_MOUNT:-/${POOL}}"

# Where a container sees its mounts. Fixed rather than configurable, because
# these paths appear in a tenant's environment and a site that moved them would
# quietly break every manifest written against the documented ones.
MOUNT_WORK="/reaper/work"
MOUNT_CACHE="/reaper/cache"
MOUNT_STATE="/reaper/state"
MOUNT_CONTROL="/reaper/control"
MOUNT_JOB="/reaper/job.sh"
ARC_MAX_MB="${REAPER_ARC_MAX_MB:-1536}"

# Prefix for files written outside the pool. Empty in production; the test
# suite points it at a scratch directory so the suite never writes to the
# machine running it.
SYSROOT="${REAPER_SYSROOT:-}"

# The datasets from the guest contract. images, cache and work are never rolled
# back; state is the only rollback target, which is the whole reason they are
# separate.
DATASETS="images cache state work"

# The only dataset `reset` may roll back, and it is checked here as well as in
# the manifest schema. work carries results outward and cache and images are
# what make the next iteration fast; rolling either of those back would destroy
# the thing the split exists to protect.
ROLLBACKABLE="state"

log()  { printf 'runner: %s\n' "$*" >&2; }
die()  { printf 'runner: %s\n' "$*" >&2; exit 1; }

# A malformed argument, as distinct from something that went wrong. Exit 2, per
# the contract at the top of this file: a caller can then tell "you asked me
# wrongly" from "I could not do it", and the suite can tell a refusal from a
# shell falling over one line later.
usage() { printf 'runner: %s\n' "$*" >&2; exit 2; }

# Announce anything destructive before doing it, so a log read afterwards says
# what happened rather than what was intended.
announce() { printf 'runner: about to %s\n' "$*" >&2; }

platform() { uname -s; }

# ---------------------------------------------------------------------------
# Choosing a disk
#
# This is the part that can destroy a machine, so it fails closed. The rule is
# NOT "the disk that is not the root disk": on a system with a ZFS root,
# `mount` reports a dataset rather than a device, and any rule phrased that way
# needs a special case per platform.
#
# A candidate is a whole disk that is *unused*: no partition table, no
# filesystem signature, not mounted, not in a pool. Exactly one must exist.
# ---------------------------------------------------------------------------

# Basenames of every device any imported pool is using, exactly as reported.
pool_members() {
    zpool status -LP 2>/dev/null \
        | awk '{ for (i = 1; i <= NF; i++) if ($i ~ /^\/dev\//) { n = split($i, p, "/"); print p[n] } }' \
        | sort -u
}

# Is this disk claimed by a pool, whole or in part?
#
# Compared prefix-first rather than by deriving a parent from a partition name.
# The obvious approach -- strip trailing digits from "vdb1" to get "vdb" -- also
# turns "nvme0n1", which is a whole disk, into "nvme0n". Asking instead whether
# a pool member is this disk plus a partition suffix gets both right.
in_a_pool() {
    pool_members | awk -v disk="$1" '
        $0 == disk { found = 1 }
        index($0, disk) == 1 {
            rest = substr($0, length(disk) + 1)
            # Partition and slice suffixes: "1", "p1", "s1", "s1a".
            if (rest ~ /^p?[0-9]+$/ || rest ~ /^s[0-9]+[a-z]?$/) { found = 1 }
        }
        END { exit found ? 0 : 1 }
    '
}

# Linux: three single-column questions per disk.
#
# One multi-column query would be fewer calls and is what this used to do. It
# was wrong: with raw output an empty column collapses, so the answer to "is
# there a filesystem?" could land in the position meant for "is it mounted?".
# The result still failed safe, but the two rules became indistinguishable --
# which mutation testing noticed, because breaking one of them changed nothing.
# One column per question has no such ambiguity.
disk_free_linux() {
    # Any child device means partitions exist.
    if [ "$(lsblk -rno NAME "/dev/$1" 2>/dev/null | grep -c .)" -gt 1 ]; then
        return 1
    fi
    # A filesystem signature anywhere in the tree.
    if lsblk -rno FSTYPE "/dev/$1" 2>/dev/null | grep -q .; then
        return 1
    fi
    # Mounted anywhere in the tree.
    if lsblk -rno MOUNTPOINT "/dev/$1" 2>/dev/null | grep -q .; then
        return 1
    fi
    return 0
}

disks_linux() {
    # Two columns, both always populated, so field collapsing cannot bite here.
    lsblk -rno NAME,TYPE 2>/dev/null | awk '$2 == "disk" { print $1 }'
}

# FreeBSD: no single tool says it all, so ask three questions. Any "yes" means
# the disk is spoken for.
disk_free_freebsd() {
    if gpart show "$1" >/dev/null 2>&1; then
        return 1                                  # has a partition table
    fi
    if fstyp "/dev/$1" >/dev/null 2>&1; then
        return 1                                  # has a filesystem on it
    fi
    if mount -p 2>/dev/null | awk '{print $1}' | grep -q "^/dev/$1"; then
        return 1                                  # mounted, partitions or not
    fi
    return 0
}

disks_freebsd() {
    # Optical devices are disks as far as the kernel is concerned and are never
    # what we want.
    #
    # sed rather than grep -v, deliberately: grep exits non-zero when it filters
    # everything away, and under `set -e` that turns "this machine has no
    # candidate disks" into a silent death instead of the refusal it should be.
    sysctl -n kern.disks 2>/dev/null | tr ' ' '\n' | sed '/^$/d; /^cd[0-9]/d'
}

candidates() {
    case "$(platform)" in
        Linux)   candidates_all=$(disks_linux) ;;
        FreeBSD) candidates_all=$(disks_freebsd) ;;
        *)       die "unsupported platform $(platform); the runner knows Linux and FreeBSD" ;;
    esac

    for candidates_d in ${candidates_all}; do
        if in_a_pool "${candidates_d}"; then
            continue
        fi
        case "$(platform)" in
            Linux)   disk_free_linux   "${candidates_d}" || continue ;;
            FreeBSD) disk_free_freebsd "${candidates_d}" || continue ;;
        esac
        printf '%s\n' "${candidates_d}"
    done
}

select_disk() {
    select_found=$(candidates)
    select_count=$(printf '%s' "${select_found}" | grep -c . || true)

    if [ "${select_count}" -eq 0 ]; then
        die "no unused disk to build ${POOL} on. The provider attaches one when it
       creates a session; if this machine was made another way, attach an
       empty disk and try again"
    fi
    if [ "${select_count}" -gt 1 ]; then
        die "more than one unused disk ($(printf '%s' "${select_found}" | tr '\n' ' ')).
       Refusing to guess which one to destroy -- detach the others, or make
       the intended one the only empty disk"
    fi

    printf '%s\n' "${select_found}"
}

# ---------------------------------------------------------------------------
# Pool and datasets
# ---------------------------------------------------------------------------

pool_exists() { zpool list -H -o name "${POOL}" >/dev/null 2>&1; }

engine() {
    if command -v podman >/dev/null 2>&1; then
        printf 'podman\n'
    elif command -v docker >/dev/null 2>&1; then
        printf 'docker\n'
    fi
}

cap_arc() {
    cap_bytes=$((ARC_MAX_MB * 1024 * 1024))
    announce "cap the ZFS ARC at ${ARC_MAX_MB} MB"

    case "$(platform)" in
        Linux)
            # Live, then persisted. A cache that grows without bound competes
            # with the workload under test for memory, and a database and a
            # browser in one machine will both lose that fight.
            if [ -w "${SYSROOT}/sys/module/zfs/parameters/zfs_arc_max" ] 2>/dev/null; then
                printf '%s\n' "${cap_bytes}" > "${SYSROOT}/sys/module/zfs/parameters/zfs_arc_max"
            fi
            mkdir -p "${SYSROOT}/etc/modprobe.d"
            printf 'options zfs zfs_arc_max=%s\n' "${cap_bytes}" \
                > "${SYSROOT}/etc/modprobe.d/zfs-reaper.conf"
            ;;
        FreeBSD)
            sysctl "vfs.zfs.arc_max=${cap_bytes}" >/dev/null 2>&1 || true
            mkdir -p "${SYSROOT}/etc"
            printf 'vfs.zfs.arc_max=%s\n' "${cap_bytes}" \
                >> "${SYSROOT}/etc/sysctl.conf"
            ;;
    esac
}

point_engine_at_pool() {
    point_engine=$(engine)
    if [ -z "${point_engine}" ]; then
        # Correct, not a failure. A host-execution template has no engine, and
        # on some platforms an engine could not run native binaries anyway.
        log "no container engine here; leaving the image store alone"
        return 0
    fi

    announce "point ${point_engine} at ${POOL_MOUNT}/images for its image store"
    mkdir -p "${SYSROOT}/etc/containers"
    cat > "${SYSROOT}/etc/containers/storage.conf" <<EOF
# Written by the reaper runner. Images live on the pool so that they survive a
# rollback of tenant state and are never copied by one.
[storage]
driver = "overlay"
graphroot = "${POOL_MOUNT}/images"
runroot = "/run/containers/storage"
EOF
}

cmd_firstboot() {
    if pool_exists; then
        # Idempotent by design: the CLI may run this on a session that is
        # already up, and doing so must be free.
        log "${POOL} already exists; nothing to do"
        return 0
    fi

    firstboot_disk=$(select_disk)

    announce "create pool ${POOL} on ${firstboot_disk}, destroying anything on it"
    log "chosen because it is the only disk here with no partitions, no"
    log "filesystem, no mount and no pool membership"

    # No -f. Without it zpool refuses a disk that still looks like it holds
    # something, which is a second opinion on the check above from code that
    # was not written here.
    zpool create \
        -o ashift=12 \
        -O compression=lz4 \
        -O atime=off \
        -m "${POOL_MOUNT}" \
        "${POOL}" "${firstboot_disk}" \
        || die "could not create ${POOL} on ${firstboot_disk}"

    for firstboot_ds in ${DATASETS}; do
        zfs create "${POOL}/${firstboot_ds}" \
            || die "could not create ${POOL}/${firstboot_ds}"
    done

    cap_arc
    point_engine_at_pool

    log "ready: ${POOL} on ${firstboot_disk}"
}

cmd_info() {
    printf 'platform=%s\n' "$(platform)"
    printf 'pool=%s\n' "${POOL}"

    if pool_exists; then
        printf 'pool_present=yes\n'
        printf 'pool_health=%s\n' "$(zpool list -H -o health "${POOL}" 2>/dev/null)"
    else
        printf 'pool_present=no\n'
    fi

    for info_ds in ${DATASETS}; do
        if zfs list -H -o name "${POOL}/${info_ds}" >/dev/null 2>&1; then
            printf 'dataset_%s=present\n' "${info_ds}"
        else
            printf 'dataset_%s=absent\n' "${info_ds}"
        fi
    done

    printf 'engine=%s\n' "$(engine)"
}


# ---------------------------------------------------------------------------
# Running a tenant's work
#
# The job itself arrives as a script the CLI rendered and delivered over stdin:
# every value in it is already quoted, and nothing a tenant wrote is ever
# assembled into an argument here. What is decided here is only *where* the job
# runs, which is the one question that differs between the execution modes.
# ---------------------------------------------------------------------------

# Defence in depth. The manifest schema already constrains these to the same
# shapes, but this is the code that turns them into paths and into an argument
# vector, so it checks for itself rather than trusting a caller it cannot see.
valid_name() {
    printf '%s' "$1" | grep -Eq '^[a-z0-9][a-z0-9._-]{0,63}$'
}

valid_image() {
    printf '%s' "$1" | grep -Eq '^[A-Za-z0-9._-]+(:[0-9]+)?(/[A-Za-z0-9._-]+)+@sha256:[0-9a-f]{64}$'
}

work_dir()    { printf '%s/work/%s\n' "${POOL_MOUNT}" "$1"; }
state_dir()   { printf '%s/state\n' "${POOL_MOUNT}"; }
# Two directories, and the split is a security boundary rather than tidiness.
#
# control_dir holds things the *host* executes as root -- the runner copy, the
# pid file. It is never mounted anywhere.
#
# control_io is the only part a container sees, and it is writable because
# requests have to land somewhere. Nothing in it is ever executed by the host.
# The job script is already mounted read-only for exactly this reason; putting
# a root-executed script inside a container-writable directory would have
# handed any toolchain image root on the guest.
control_dir() { printf '%s/control/%s\n' "${POOL_MOUNT}" "$1"; }
control_io()  { printf '%s/control/%s/io\n' "${POOL_MOUNT}" "$1"; }

# Warm caches persist between runs and are never rolled back. A cold run gets
# the same variable and the same path inside a container, pointing at a
# directory that is emptied first.
#
# Not "no variable at all", which is what this used to do: a tenant's command
# that names a cache path -- the documented way to use one -- then expanded it
# to nothing and failed inside the toolchain with a message about an empty
# string. Determinism mode means the cache is not warm. It does not mean the
# tenant writes their command twice.
cache_dir() {
    if [ -n "${COLD:-}" ]; then
        printf '%s/cold/%s\n' "${POOL_MOUNT}" "$1"
    else
        printf '%s/cache/%s\n' "${POOL_MOUNT}" "$1"
    fi
}

# A cache name as an environment variable suffix. A manifest name may carry a
# dot or a hyphen; an environment variable may not.
cache_var() {
    printf 'REAPER_CACHE_%s' "$(printf '%s' "$1" | tr 'a-z.-' 'A-Z__')"
}

cmd_workspace() {
    [ "${1:-}" = "--project" ] || usage "workspace: expected --project"
    [ $# -ge 2 ] || usage "workspace: --project needs a value"
    valid_name "$2" || usage "workspace: $2 is not a usable project name"

    workspace_work=$(work_dir "$2")
    # Both, and both idempotent. The results directory has to exist before the
    # first job runs or the reverse channel has nothing to read, and a failure
    # there would read as though results had been lost rather than never made.
    mkdir -p "${workspace_work}/out" || die "cannot make ${workspace_work}/out"
    printf 'work=%s\nout=%s/out\n' "${workspace_work}" "${workspace_work}"
}

cmd_pull() {
    [ $# -gt 0 ] || usage "pull: nothing to pull"

    pull_engine=$(engine)
    if [ -z "${pull_engine}" ]; then
        # A host-execution guest may legitimately have no engine, and on some
        # platforms one could not run these images anyway. Loud, and not fatal:
        # a pre-pull is an optimisation, and refusing here would cost a session
        # that is otherwise perfectly usable.
        log "no container engine here; not pulling $# image(s)"
        return 0
    fi

    for pull_ref in "$@"; do
        valid_image "${pull_ref}" || usage "pull: ${pull_ref} is not a digest-pinned image"
        announce "pull ${pull_ref}"
        "${pull_engine}" pull "${pull_ref}" >&2 || die "could not pull ${pull_ref}"
        pull_last=${pull_ref}
    done

    # Then prove the engine can actually *start* one.
    #
    # Pulling exercises none of the container runtime: an engine with a broken
    # network backend pulls perfectly, reports itself healthy, and fails only
    # when something tries to run. That is exactly how a template shipped here
    # with podman installed and no packet-filter tooling for netavark to drive,
    # and the failure surfaced minutes later inside a build as an error about a
    # missing `nft` binary. Checking here costs one container start and moves
    # the discovery to the moment the template is first used.
    if ! "${pull_engine}" run --rm "${pull_last}" /bin/true >&2; then
        die "${pull_engine} can fetch images here but cannot run one. That is a
       fault in this guest's template rather than in anything a tenant wrote --
       usually a container engine installed without the packet-filter tooling
       its network backend needs. Until it is fixed, only host execution will
       work on this guest"
    fi
}

cmd_exec() {
    exec_project=""; exec_job=""; exec_image=""; exec_caches=""; COLD=""

    while [ $# -gt 0 ]; do
        case "$1" in
            --cold) COLD=1; shift ;;
            --project|--job|--image|--cache)
                [ $# -ge 2 ] || usage "exec: $1 needs a value"
                case "$1" in
                    --project) exec_project=$2 ;;
                    --job)     exec_job=$2 ;;
                    --image)   exec_image=$2 ;;
                    --cache)   exec_caches="${exec_caches} $2" ;;
                esac
                shift 2 ;;
            *) usage "exec: unexpected argument $1" ;;
        esac
    done

    [ -n "${exec_project}" ] || usage "exec: no --project"
    [ -n "${exec_job}" ]     || usage "exec: no --job"
    valid_name "${exec_project}" || usage "exec: ${exec_project} is not a usable project name"
    [ -r "${exec_job}" ] || usage "exec: no job script at ${exec_job}"

    exec_work=$(work_dir "${exec_project}")
    [ -d "${exec_work}" ] || die "exec: no working tree at ${exec_work}; sync first"
    mkdir -p "${exec_work}/out"
    # State is a dataset firstboot made; control is per project. Both have to
    # exist before a container tries to mount them, or the engine creates a
    # directory owned by nobody in their place.
    mkdir -p "$(state_dir)" "$(control_io "${exec_project}")"
    # The wrapper is mounted as a file, so it has to exist before a container
    # starts or the engine invents a directory in its place.
    [ -e "$(control_dir "${exec_project}")/reset" ] || cmd_control --project "${exec_project}" start

    for exec_c in ${exec_caches}; do
        valid_name "${exec_c}" || usage "exec: ${exec_c} is not a usable cache name"
        exec_dir=$(cache_dir "${exec_c}")
        if [ -n "${COLD}" ]; then
            # Emptied rather than merely separate. A cold run that inherited
            # the last cold run's output would answer a different question from
            # the one determinism mode is asked.
            announce "empty the cold ${exec_c} cache at ${exec_dir}"
            rm -rf "${exec_dir}" || die "cannot clear the cold ${exec_c} cache"
        fi
        mkdir -p "${exec_dir}" || die "cannot make the ${exec_c} cache"
    done

    if [ -n "${exec_image}" ]; then
        exec_in_container
    else
        exec_on_host
    fi
}

exec_in_container() {
    valid_image "${exec_image}" || usage "exec: ${exec_image} is not a digest-pinned image"

    exec_engine=$(engine)
    [ -n "${exec_engine}" ] || die "exec: ${exec_image} was asked for, and there is
       no container engine here. Either this guest's template is missing one, or
       the manifest wants host execution for this verb"

    # --rm because a session is disposable and a stopped container left behind
    # would only accumulate. The job script is mounted read-only: nothing inside
    # has any business rewriting what it was asked to run.
    set -- run --rm \
        -w "${MOUNT_WORK}" \
        -v "${exec_work}:${MOUNT_WORK}" \
        -v "${exec_job}:${MOUNT_JOB}:ro" \
        -v "$(state_dir):${MOUNT_STATE}" \
        -v "$(control_io "${exec_project}"):${MOUNT_CONTROL}/io" \
        -v "$(control_dir "${exec_project}")/reset:${MOUNT_CONTROL}/reset:ro" \
        -e "REAPER_WORK=${MOUNT_WORK}" \
        -e "REAPER_OUT=${MOUNT_WORK}/out" \
        -e "REAPER_STATE=${MOUNT_STATE}" \
        -e "REAPER_CONTROL=${MOUNT_CONTROL}"

    for exec_c in ${exec_caches}; do
        set -- "$@" -v "$(cache_dir "${exec_c}"):${MOUNT_CACHE}/${exec_c}"
        set -- "$@" -e "$(cache_var "${exec_c}")=${MOUNT_CACHE}/${exec_c}"
    done

    set -- "$@" "${exec_image}" /bin/sh "${MOUNT_JOB}"
    "${exec_engine}" "$@"
}

exec_on_host() {
    # env rather than exporting into this shell: the job gets exactly the
    # variables it was promised, and nothing this script happens to be holding.
    set -- "REAPER_WORK=${exec_work}" "REAPER_OUT=${exec_work}/out" \
        "REAPER_STATE=$(state_dir)" "REAPER_CONTROL=$(control_dir "${exec_project}")"
    for exec_c in ${exec_caches}; do
        set -- "$@" "$(cache_var "${exec_c}")=$(cache_dir "${exec_c}")"
    done

    cd "${exec_work}" || die "cannot enter ${exec_work}"
    env "$@" /bin/sh "${exec_job}"
}


# ---------------------------------------------------------------------------
# Snapshots, and rolling back to one
#
# The rule that must hold forever: **nothing here ever constructs or seeds
# tenant state.** A snapshot is whatever the tenant's own stack-up produced,
# hostility included -- a database deliberately started in an awkward charset
# stays that way. Rollback-to-pristine is legitimate exactly because the
# snapshot was earned through the real path once, and a convenience here that
# pre-seeded something friendly would silently destroy that.
# ---------------------------------------------------------------------------

valid_snapname() {
    printf '%s' "$1" | grep -Eq '^[a-z0-9][a-z0-9._-]{0,63}$'
}

# A dataset a caller is allowed to name, resolved to its full path.
rollback_target() {
    for _r in ${ROLLBACKABLE}; do
        if [ "$1" = "${_r}" ]; then
            printf '%s/%s\n' "${POOL}" "$1"
            return 0
        fi
    done
    return 1
}

snapshot_exists() {
    zfs list -t snapshot -H -o name "$1@$2" >/dev/null 2>&1
}

cmd_snapshot() {
    snap_dataset=""; snap_name=""; snap_if_absent=""

    while [ $# -gt 0 ]; do
        case "$1" in
            --if-absent) snap_if_absent=1; shift ;;
            --dataset|--name)
                [ $# -ge 2 ] || usage "snapshot: $1 needs a value"
                case "$1" in
                    --dataset) snap_dataset=$2 ;;
                    --name)    snap_name=$2 ;;
                esac
                shift 2 ;;
            *) usage "snapshot: unexpected argument $1" ;;
        esac
    done

    [ -n "${snap_dataset}" ] || usage "snapshot: no --dataset"
    [ -n "${snap_name}" ]    || usage "snapshot: no --name"
    valid_snapname "${snap_name}" || usage "snapshot: ${snap_name} is not a usable snapshot name"
    snap_target=$(rollback_target "${snap_dataset}") \
        || usage "snapshot: ${snap_dataset} is not a dataset this may snapshot (only: ${ROLLBACKABLE})"

    if snapshot_exists "${snap_target}" "${snap_name}"; then
        if [ -n "${snap_if_absent}" ]; then
            log "${snap_target}@${snap_name} already exists; keeping the one that is there"
            return 0
        fi
        die "${snap_target}@${snap_name} already exists. Snapshots are not
       overwritten here: the whole value of a named point is that it does not
       move under you. Pick another name"
    fi

    announce "snapshot ${snap_target}@${snap_name}"
    zfs snapshot "${snap_target}@${snap_name}" \
        || die "could not snapshot ${snap_target}@${snap_name}"
    printf 'snapshot=%s@%s\n' "${snap_target}" "${snap_name}"
}

# Stop the tenant's containers, optionally sparing one.
#
# The exception is for the in-guest trigger: a driver container that asks for a
# reset has to survive it, and stopping the caller would look exactly like the
# reset having crashed.
stop_containers() {
    stop_except=${1:-}
    stop_engine=$(engine)
    [ -n "${stop_engine}" ] || return 0

    for stop_id in $("${stop_engine}" ps -q 2>/dev/null); do
        # Compared both ways: `ps -q` gives a short id and a caller reporting
        # its own hostname gives a short one too, but neither is guaranteed.
        if [ -n "${stop_except}" ]; then
            case "${stop_except}" in
                "${stop_id}"*) continue ;;
            esac
            case "${stop_id}" in
                "${stop_except}"*) continue ;;
            esac
        fi
        announce "stop container ${stop_id}"
        "${stop_engine}" stop "${stop_id}" >&2 || log "could not stop ${stop_id}"
    done
}

cmd_rollback() {
    roll_dataset=""; roll_name=""; roll_except=""

    while [ $# -gt 0 ]; do
        case "$1" in
            --dataset|--name|--except-container)
                [ $# -ge 2 ] || usage "rollback: $1 needs a value"
                case "$1" in
                    --dataset)          roll_dataset=$2 ;;
                    --name)             roll_name=$2 ;;
                    --except-container) roll_except=$2 ;;
                esac
                shift 2 ;;
            *) usage "rollback: unexpected argument $1" ;;
        esac
    done

    [ -n "${roll_dataset}" ] || usage "rollback: no --dataset"
    [ -n "${roll_name}" ]    || usage "rollback: no --name"
    valid_snapname "${roll_name}" || usage "rollback: ${roll_name} is not a usable snapshot name"
    roll_target=$(rollback_target "${roll_dataset}") \
        || usage "rollback: ${roll_dataset} is not a dataset this may roll back (only: ${ROLLBACKABLE})"

    snapshot_exists "${roll_target}" "${roll_name}" \
        || die "there is no ${roll_target}@${roll_name} to roll back to. A session
       takes @pristine after its first successful run, and 'reaper snapshot'
       names one whenever you like"

    # Stop first. The contract is that the next run restarts the stack, because
    # a process holding credentials or sessions from before the rollback must
    # never survive it.
    stop_containers "${roll_except}"

    # -r discards snapshots taken after this one. That is ZFS's behaviour rather
    # than a choice made here, and it is worth saying out loud because it
    # silently removes named checkpoints.
    announce "roll ${roll_target} back to @${roll_name}, discarding anything since"
    if ! zfs rollback -r "${roll_target}@${roll_name}"; then
        die "could not roll ${roll_target} back to @${roll_name}. If it says the
       dataset is busy, something still has files open on it -- which is the
       refusal working: rolling the filesystem out from under a live process is
       the one outcome worse than not resetting at all"
    fi
    printf 'rolled_back=%s@%s\n' "${roll_target}" "${roll_name}"
}

# ---------------------------------------------------------------------------
# The in-guest trigger
#
# A driver container has to be able to ask for a reset without knowing that ZFS
# exists, and it cannot run commands on the guest. So something in the guest
# listens -- the first resident process this design allows inside one, confined
# to a single directory and a single verb.
#
# Request files and rename, rather than a socket or a FIFO: neither template
# ships netcat, and a FIFO handshake in shell is delicate about who opens what
# and in which order. A rename is atomic and needs nothing but sh.
# ---------------------------------------------------------------------------

cmd_control() {
    ctl_project=""; ctl_action=""

    while [ $# -gt 0 ]; do
        case "$1" in
            --project)
                [ $# -ge 2 ] || usage "control: --project needs a value"
                ctl_project=$2; shift 2 ;;
            start|stop|serve) ctl_action=$1; shift ;;
            *) usage "control: unexpected argument $1" ;;
        esac
    done

    [ -n "${ctl_project}" ] || usage "control: no --project"
    valid_name "${ctl_project}" || usage "control: ${ctl_project} is not a usable project name"
    [ -n "${ctl_action}" ] || usage "control: expected start, stop or serve"

    ctl_dir=$(control_dir "${ctl_project}")
    ctl_io=$(control_io "${ctl_project}")
    ctl_pid="${ctl_dir}/loop.pid"
    # Set for every action, not only for start. `serve` runs as its own
    # process and reaches this function the same way -- when only start set
    # this, the loop died under `set -u` on its first request, and the only
    # thing that noticed was the wrapper's timeout five minutes later.
    ctl_runner="${ctl_dir}/runner.sh"

    case "${ctl_action}" in
        start) control_start ;;
        stop)  control_stop ;;
        serve) control_serve ;;
    esac
}

control_running() {
    [ -f "${ctl_pid}" ] || return 1
    kill -0 "$(cat "${ctl_pid}" 2>/dev/null)" 2>/dev/null
}

control_start() {
    mkdir -p "${ctl_io}" || die "cannot make ${ctl_io}"

    if control_running; then
        log "the control loop is already running as $(cat "${ctl_pid}")"
        return 0
    fi

    # A copy of this script, taken now. The CLI re-delivers the runner before
    # every remote operation, and rewriting a file a running shell is still
    # reading from is a good way to make it behave unaccountably.
    cp "$0" "${ctl_runner}" || die "cannot copy the runner to ${ctl_runner}"
    # Root-executable and root-only. It lives outside ${ctl_io} deliberately;
    # see the comment on control_dir.
    chmod 0700 "${ctl_runner}"

    # The wrapper a tenant actually runs. It hides the protocol so that nothing
    # inside a container needs to know any of this.
    cat > "${ctl_dir}/reset" <<'WRAPPER'
#!/bin/sh
# Ask the guest to roll this project's state back, and wait for it.
#
#   reset [snapshot-name]      default: pristine
#
# Exits with the status of the rollback. Run it from anywhere that can see this
# directory -- inside a container it is mounted at /reaper/control.
set -eu
dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
name=${1:-pristine}
# Bounded wait, in seconds. Overridable so a test suite need not sit through
# the production default.
limit=${REAPER_RESET_TIMEOUT:-300}

# Whatever identifies this caller. Under a container engine the hostname is the
# container's own id, which is what stops the reset from stopping the thing
# that asked for it.
caller=$(hostname 2>/dev/null || echo unknown)
id="${caller}-$$"

io="${dir}/io"
printf '%s\n%s\n%s\n' reset "${name}" "${caller}" > "${io}/req.${id}.tmp"
mv "${io}/req.${id}.tmp" "${io}/req.${id}"

# Bounded. A wrapper that waits forever on a loop that has died is worse than
# one that gives up and says so.
waited=0
while [ ! -f "${io}/res.${id}" ]; do
    sleep 1
    waited=$((waited + 1))
    if [ "${waited}" -gt "${limit}" ]; then
        rm -f "${io}/req.${id}"
        echo "reset: no answer from the guest in ${limit}s; is the control loop running?" >&2
        exit 1
    fi
done

rc=$(cat "${io}/res.${id}")
rm -f "${io}/res.${id}"
exit "${rc}"
WRAPPER
    chmod 0755 "${ctl_dir}/reset"

    # Detached, so it outlives the connection that started it.
    nohup "${ctl_runner}" control --project "${ctl_project}" serve \
        >> "${ctl_dir}/loop.log" 2>&1 &
    printf '%s\n' "$!" > "${ctl_pid}"
    log "control loop started as $(cat "${ctl_pid}"), watching ${ctl_io}"
    printf 'control=%s\n' "${ctl_io}"
}

control_stop() {
    if control_running; then
        announce "stop the control loop $(cat "${ctl_pid}")"
        kill "$(cat "${ctl_pid}")" 2>/dev/null || true
    fi
    rm -f "${ctl_pid}"
}

control_serve() {
    log "serving ${ctl_io}"
    while :; do
        for control_req in "${ctl_io}"/req.*; do
            [ -e "${control_req}" ] || continue
            case "${control_req}" in *.tmp) continue ;; esac

            control_id=${control_req##*/req.}
            control_verb=$(sed -n 1p "${control_req}" 2>/dev/null)
            control_snap=$(sed -n 2p "${control_req}" 2>/dev/null)
            control_from=$(sed -n 3p "${control_req}" 2>/dev/null)

            # Removed before it is acted on. A request that kills the loop
            # would otherwise be retried on every pass, forever.
            rm -f "${control_req}"

            # The caller id comes from inside a container and is used as a
            # `case` pattern when deciding what not to stop. A value of `*`
            # would match every container and spare the lot, so the rollback
            # would then run with the stack still live and fail on a busy
            # dataset -- a confusing way to discover an unvalidated string.
            if [ -n "${control_from}" ] && ! printf '%s' "${control_from}" \
                | grep -Eq '^[A-Za-z0-9][A-Za-z0-9_.-]{0,63}$'; then
                log "control: refusing a caller id that is not one: ${control_from}"
                control_verb=""
            fi

            control_rc=0
            if [ ! -x "${ctl_runner}" ]; then
                log "control: ${ctl_runner} is gone; cannot serve"
                control_verb=""
            fi
            case "${control_verb}" in
                reset)
                    "${ctl_runner}" rollback \
                        --dataset state \
                        --name "${control_snap:-pristine}" \
                        --except-container "${control_from}" || control_rc=$?
                    ;;
                *)
                    log "control: ignoring an unknown request ${control_verb:-<empty>}"
                    control_rc=2
                    ;;
            esac

            printf '%s\n' "${control_rc}" > "${ctl_io}/res.${control_id}.tmp"
            mv "${ctl_io}/res.${control_id}.tmp" "${ctl_io}/res.${control_id}"
        done
        sleep 1
    done
}

case "${1:-}" in
    firstboot) cmd_firstboot ;;
    info)      cmd_info ;;
    workspace) shift; cmd_workspace "$@" ;;
    pull)      shift; cmd_pull "$@" ;;
    exec)      shift; cmd_exec "$@" ;;
    snapshot)  shift; cmd_snapshot "$@" ;;
    rollback)  shift; cmd_rollback "$@" ;;
    control)   shift; cmd_control "$@" ;;
    -h|--help)
        sed -n '2,/^set -eu/p' "$0" | sed 's/^# \{0,1\}//;$d'
        ;;
    *)
        printf 'usage: %s {firstboot|info|workspace|pull|exec|snapshot|rollback|control}\n' "$0" >&2
        exit 2
        ;;
esac
