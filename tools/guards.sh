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
# Reports every file outside the allowed paths that matches the forbidden
# pattern. Untracked files are scanned too: a new source file is untracked for
# exactly the window in which this gate is its only reviewer. Build output
# stays out via .gitignore (--exclude-standard).
scan() {
    label=$1
    allow=$2
    forbid=$3
    why=$4

    hits=''
    for f in $(git ls-files --cached --others --exclude-standard | grep -Ev "${allow}"); do
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
    names=$(sed -n 's/^project *= *"\([^"]*\)".*/\1/p' manifest/examples/*.reaper.toml \
            | tr '\n' '|' | sed 's/|$//')
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
they belong in their own .reaper.toml and in manifest/examples, and
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
    #
    # runner/ is allowed because it *is* the platform module -- the one place
    # operating-system specifics are supposed to live. This used to say
    # runner/platform/, from when the runner was going to be a compiled binary
    # with a platform sub-module; it is a single shell script delivered over
    # SSH, so the directory is the boundary.
    #
    # .github/workflows/ is allowed because it names *build hosts*, not
    # guests, and the two are unrelated questions. This seam exists so that
    # adding a guest system is a template build and a registry entry rather
    # than a code change; a release matrix saying which machines compile the
    # binary cannot make that false, and no reaper code reads these files.
    # Narrow deliberately: it is that one directory, and the tenant and
    # provider seams below still apply to it in full -- a workflow naming a
    # tenant, or reaching for a hypervisor's API, is still a failure.
    scan "guest seam" \
        '^(docs/|README\.md$|manifest/examples/|\.reaper\.toml$|runner/|tools/guards\.sh$|\.github/workflows/)' \
        'systemctl|systemd|apt-get|dpkg |zfsutils-linux|/dev/(vd|vtbd|sd|nvme)[a-z0-9]|rc\.conf|\brc\.d\b|\b(ubuntu|debian|freebsd|alpine|centos|fedora)\b' \
        "An operating system leaked out of the runner's platform modules. The
guest seam exists so that adding a system is a template build and a
registry entry, never a code change -- and a single hardcoded device
path or init command is enough to make that false."
}

guard_provider() {
    # Hypervisor vocabulary belongs to a provider and its sweeper.
    #
    # Cargo.toml and Cargo.lock are allowed because a workspace root names its
    # members and a lockfile enumerates them: a member called
    # reaper-provider-proxmox appearing there is the seam working rather than
    # leaking. Be honest about what the exemption costs: these files are skipped
    # wholesale, not scanned for names only. It is acceptable because neither
    # carries code, and Cargo.lock is generated rather than written -- so the
    # exemption cannot hide a decision a person made. README.md is documentation.
    #
    # Note what is *not* exempt: the CLI, tests included. Driving the CLI end to
    # end needs a hypervisor, but it does not need a named one -- the provider
    # registry re-exports a stand-in neutrally, so no exemption is required. An
    # exemption is a place coupling can hide; not needing one is better.
    scan "provider seam" \
        '^(docs/|README\.md$|Cargo\.toml$|Cargo\.lock$|providers/|cull/|tools/guards\.sh$)' \
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
