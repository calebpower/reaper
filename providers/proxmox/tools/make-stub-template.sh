#!/bin/sh
#
# Create a throwaway template for testing reaper against a real cluster.
#
# A diskless virtual machine converted to a template clones instantly and
# exercises every API path reaper uses -- clone, tag, configure, start, stop,
# destroy -- without an operating system, an installer, or ten minutes of
# waiting. It is not a guest anything can actually run on, and it is not meant
# to be: it exists so the session lifecycle can be proven before the real
# templates are built.
#
# Reads the same configuration reaper does, so there is one place where the
# endpoint and credential live.
#
# Usage: make-stub-template.sh [--dry-run] [<id>]
#
# Exit 0 on success, 1 on failure, 2 on a usage or configuration problem.
set -eu

DRY=0
ID=""
for arg in "$@"; do
    case "${arg}" in
        --dry-run) DRY=1 ;;
        -h|--help)
            sed -n '2,/^set -eu/p' "$0" | sed 's/^# \{0,1\}//;$d'
            exit 0
            ;;
        -*) echo "unknown option: ${arg}" >&2; exit 2 ;;
        *)  ID="${arg}" ;;
    esac
done

config="${REAPER_CONFIG:-${HOME}/.config/reaper/config.toml}"
[ -f "${config}" ] || { echo "no configuration at ${config}" >&2; exit 2; }

# Pull one key out of the [proxmox] table. Deliberately small: this reads the
# handful of scalar keys reaper writes there and nothing else. If it ever needs
# to understand TOML properly, that is a sign this belongs in the binary.
setting() {
    awk -v key="$1" '
        /^\[/          { in_section = ($0 == "[proxmox]") ; next }
        !in_section    { next }
        $1 == key      { sub(/^[^=]*=[ \t]*/, ""); gsub(/^"|"[ \t]*$/, ""); print; exit }
    ' "${config}"
}

api=$(setting api)
node=$(setting node)
pool=$(setting pool)
token_file=$(setting token_file)
tls=$(setting tls)

for required in api node pool token_file; do
    eval "value=\${${required}}"
    [ -n "${value}" ] || { echo "[proxmox].${required} is not set in ${config}" >&2; exit 2; }
done

# shellcheck disable=SC2088
# Deliberate: expanding a leading ~ ourselves, because it came out of a config
# file rather than the shell, where it would never have been expanded.
case "${token_file}" in
    "~/"*) token_file="${HOME}/${token_file#\~/}" ;;
esac
[ -f "${token_file}" ] || { echo "no token at ${token_file}" >&2; exit 2; }
token=$(tr -d '\n' < "${token_file}")

curl_opts="--silent --show-error --fail-with-body"
[ "${tls}" = "insecure" ] && curl_opts="${curl_opts} --insecure"

api_call() { # api_call <method> <path> [form...]
    method="$1"; path="$2"; shift 2
    # shellcheck disable=SC2086
    # curl_opts is a deliberate word-split list of flags, not a filename.
    curl ${curl_opts} -X "${method}" \
        -H "Authorization: PVEAPIToken=${token}" \
        "$@" "${api}/api2/json${path}"
}

if [ -z "${ID}" ]; then
    echo "no identifier given; pass one inside the range reaper is configured for" >&2
    exit 2
fi
case "${ID}" in
    ''|*[!0-9]*) echo "identifier ${ID} is not a number" >&2; exit 2 ;;
esac

echo "configuration : ${config}"
echo "endpoint      : ${api} (node ${node}, pool ${pool}, tls ${tls})"
echo "will create   : diskless machine ${ID}, then convert it to a template"

if [ "${DRY}" -eq 1 ]; then
    echo
    echo "dry run: checking the endpoint answers and the pool is visible"
    api_call GET "/pools/${pool}" >/dev/null && echo "pool ${pool} is visible"
    echo "nothing was created"
    exit 0
fi

echo
echo "creating ${ID}"
# No disk, no boot media, minimal everything. The guest agent is enabled so the
# shape matches a real template even though nothing will ever answer.
api_call POST "/nodes/${node}/qemu" \
    --data-urlencode "vmid=${ID}" \
    --data-urlencode "name=reaper-stub" \
    --data-urlencode "pool=${pool}" \
    --data-urlencode "memory=512" \
    --data-urlencode "cores=1" \
    --data-urlencode "agent=1" \
    --data-urlencode "description=Throwaway template for reaper testing. No OS. Safe to delete." \
    >/dev/null

echo "converting ${ID} to a template"
api_call POST "/nodes/${node}/qemu/${ID}/template" >/dev/null

echo
echo "done. Register it as a guest in ${config}:"
echo
echo "    [guests.\"stub\"]"
echo "    template = \"${ID}\""
echo
echo "Remove it when you are finished:"
echo "    curl ${curl_opts} -X DELETE -H \"Authorization: PVEAPIToken=\$(cat ${token_file})\" \\"
echo "        ${api}/api2/json/nodes/${node}/qemu/${ID}"
