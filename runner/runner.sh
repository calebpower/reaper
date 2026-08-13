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
#   runner.sh firstboot     make the pool and datasets; idempotent
#   runner.sh info          report what exists, as key=value
#
# Exit 0 on success, 1 on failure, 2 on a usage error.
set -eu

POOL="${REAPER_POOL:-tank}"
ARC_MAX_MB="${REAPER_ARC_MAX_MB:-1536}"

# Prefix for files written outside the pool. Empty in production; the test
# suite points it at a scratch directory so the suite never writes to the
# machine running it.
SYSROOT="${REAPER_SYSROOT:-}"

# The datasets from the guest contract. images, cache and work are never rolled
# back; state is the only rollback target, which is the whole reason they are
# separate.
DATASETS="images cache state work"

log()  { printf 'runner: %s\n' "$*" >&2; }
die()  { printf 'runner: %s\n' "$*" >&2; exit 1; }

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

    announce "point ${point_engine} at /${POOL}/images for its image store"
    mkdir -p "${SYSROOT}/etc/containers"
    cat > "${SYSROOT}/etc/containers/storage.conf" <<EOF
# Written by the reaper runner. Images live on the pool so that they survive a
# rollback of tenant state and are never copied by one.
[storage]
driver = "overlay"
graphroot = "/${POOL}/images"
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
        -m "/${POOL}" \
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

case "${1:-}" in
    firstboot) cmd_firstboot ;;
    info)      cmd_info ;;
    -h|--help)
        sed -n '2,/^set -eu/p' "$0" | sed 's/^# \{0,1\}//;$d'
        ;;
    *)
        printf 'usage: %s {firstboot|info}\n' "$0" >&2
        exit 2
        ;;
esac
