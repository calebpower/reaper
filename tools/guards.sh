#!/bin/sh
#
# Seam guards.
#
# reaper is meant to host any program, on any guest, under any hypervisor. That
# is only true for as long as no tenant, operating system or hypervisor has
# leaked out of the place it belongs -- and the leak is never a decision anyone
# announces. It is a one-line special case at 2am that works.
#
# So the seams are checked mechanically, in the source-as-data spirit of the
# testing methodology's Tier 3: parse the project's own source and assert
# structural claims about it. Milliseconds, no build, no network.
#
# Usage: guards.sh [tenant|guest|provider]   (no argument runs all three)
#
# Exit 0 if every seam holds, 1 otherwise.
set -eu

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "${root}"

failures=0

# scan <label> <allow-regex> <forbid-regex> <why>
#
# Reports every tracked file outside the allowed paths that matches the
# forbidden pattern. Tracked files only: build output and untracked scratch are
# not the project's source.
scan() {
    label=$1
    allow=$2
    forbid=$3
    why=$4

    hits=''
    for f in $(git ls-files | grep -Ev "${allow}"); do
        # Binary files and deleted-but-tracked paths are skipped rather than
        # reported as errors; neither is source anyone can leak through.
        [ -f "${f}" ] || continue
        if match=$(grep -nIiE "${forbid}" "${f}" 2>/dev/null); then
            hits="${hits}$(printf '%s\n' "${match}" | sed "s|^|  ${f}:|")
"
        fi
    done

    if [ -n "${hits}" ]; then
        failures=$((failures + 1))
        echo "FAIL  ${label}"
        echo "${why}" | sed 's/^/      /'
        printf '%s' "${hits}"
        echo
    else
        echo "ok    ${label}"
    fi
}

guard_tenant() {
    # The forbidden names are read out of the examples rather than written
    # here, which is why this guard needs no exemption for itself: it does not
    # contain the words it polices. Add an example, and it is policed from then
    # on with no edit to this file.
    names=$(grep -h '^project:' manifest/examples/*.reaper.yaml \
            | awk '{print $2}' | tr '\n' '|' | sed 's/|$//')
    if [ -z "${names}" ]; then
        echo "FAIL  tenant seam"
        echo "      no project names found in manifest/examples; this guard is"
        echo "      not checking anything, which is worse than it failing."
        failures=$((failures + 1))
        return
    fi

    scan "tenant seam" \
        '^(docs/|manifest/examples/)' \
        "\\b(${names})\\b" \
        "A tenant name appeared in framework code. Tenants are configuration:
they belong in their own .reaper.yaml and in manifest/examples, and
nowhere else. If the framework needs to know which project it is
working on, the manifest is how it finds out."
}

guard_guest() {
    # Operating-system specifics belong in the runner's platform modules, where
    # they are declared as such, and in documentation. Everywhere else they are
    # an assumption that only one guest exists.
    #
    # manifest/examples is allowed because guest *names* are tenant-chosen
    # strings that quite reasonably say which system they mean.
    scan "guest seam" \
        '^(docs/|README\.md$|manifest/examples/|runner/platform/|tools/guards\.sh$)' \
        'systemctl|systemd|apt-get|dpkg |zfsutils-linux|/dev/(vd|vtbd|sd|nvme)[a-z0-9]|rc\.conf|\brc\.d\b|\b(ubuntu|debian|freebsd|alpine|centos|fedora)\b' \
        "An operating system leaked out of the runner's platform modules. The
guest seam exists so that adding a system is a template build and a
registry entry, never a code change -- and a single hardcoded device
path or init command is enough to make that false."
}

guard_provider() {
    # Hypervisor vocabulary belongs to a provider and its sweeper.
    #
    # Cargo.toml is allowed because a workspace root must name its members, and
    # a member at providers/proxmox is the seam working rather than leaking.
    # README.md is allowed because it is documentation.
    scan "provider seam" \
        '^(docs/|README\.md$|Cargo\.toml$|providers/|cull/|tools/guards\.sh$)' \
        '\bpve\b|\bvmid\b|\bupid\b|proxmox|pveapitoken|\b(9000|9099)\b' \
        "Hypervisor vocabulary leaked into the core. Machine identifiers,
identifier ranges, resource pools, task handles and API tokens belong
behind the provider trait. Note what this guard can and cannot do: it
proves the boundary has not been crossed, not that the boundary is in
the right place. Only a second provider would prove that."
}

case "${1:-all}" in
    tenant)   guard_tenant ;;
    guest)    guard_guest ;;
    provider) guard_provider ;;
    all)      guard_tenant; guard_guest; guard_provider ;;
    *)        echo "usage: $0 [tenant|guest|provider]" >&2; exit 2 ;;
esac

[ "${failures}" -eq 0 ]
