#!/bin/sh
#
# pve-reap -- destroy expired ephemeral Proxmox VMs
#
# Stateless: reads current truth from the PVE API each run and acts on it.
# A missed run delays cleanup; it never loses or double-destroys anything.
#
# Acts ONLY on guests that satisfy all of:
#   - member of $PVE_POOL
#   - VMID within $VMID_MIN..$VMID_MAX
#   - carry a tag of the form expires-<unix-epoch>, now past
#
# Anything in the pool without a valid expires- tag is reported, never
# destroyed. That is deliberate: a missing tag means the harness failed
# partway through a create, and silently deleting is the wrong response
# to an unknown state.

set -eu

PVE_HOST="${PVE_HOST:-bass:8006}"
PVE_POOL="${PVE_POOL:-cal/ephemeral}"
VMID_MIN="${VMID_MIN:-9000}"
VMID_MAX="${VMID_MAX:-9099}"
CRED="${CRED:-/usr/local/etc/pve-reaper/token}"
STOP_TIMEOUT="${STOP_TIMEOUT:-60}"

# Self-signed PVE cert: set PVE_INSECURE=1. If you have a real cert from
# your CA, leave it unset and point PVE_CACERT at the CA bundle instead.
if [ -n "${PVE_INSECURE:-}" ]; then
	TLSOPT="--insecure"
elif [ -n "${PVE_CACERT:-}" ]; then
	TLSOPT="--cacert ${PVE_CACERT}"
else
	TLSOPT=""
fi

log() { logger -t pve-reap -p daemon.info "$*"; echo "pve-reap: $*"; }
err() { logger -t pve-reap -p daemon.err  "$*"; echo "pve-reap: $*" >&2; }

# shellcheck source=/dev/null
[ -r "$CRED" ] || { err "cannot read credential file $CRED"; exit 1; }
. "$CRED"
[ -n "${PVE_TOKEN:-}" ] || { err "PVE_TOKEN unset in $CRED"; exit 1; }

api() {
	_method="$1"; _path="$2"
	curl -sS --fail-with-body $TLSOPT \
		-X "$_method" \
		-H "Authorization: $PVE_TOKEN" \
		"https://${PVE_HOST}/api2/json${_path}"
}

# Returns the guest's current status string, or empty if it is gone.
vm_status() {
	api GET "/nodes/$1/qemu/$2/status/current" 2>/dev/null \
		| jq -r '.data.status // empty'
}

now=$(date +%s)
reaped=0
skipped=0

# tags arrive semicolon-separated; @tsv keeps fields unambiguous
api GET "/cluster/resources?type=vm" \
	| jq -r --arg pool "$PVE_POOL" '
		.data[]
		| select(.pool == $pool)
		| [.vmid, .node, (.status // "unknown"), (.tags // "")]
		| @tsv' \
	| while IFS="$(printf '\t')" read -r vmid node status tags; do

	# Defense in depth: the ACL already confines the token to the pool,
	# but a VMID guard means a misconfigured pool membership still
	# cannot reach a guest outside the ephemeral range.
	if [ "$vmid" -lt "$VMID_MIN" ] || [ "$vmid" -gt "$VMID_MAX" ]; then
		err "vmid $vmid is in $PVE_POOL but outside ${VMID_MIN}-${VMID_MAX}; refusing to touch it"
		skipped=$((skipped + 1))
		continue
	fi

	expires=$(printf '%s' "$tags" | tr ';' '\n' | sed -n 's/^expires-\([0-9][0-9]*\)$/\1/p' | head -1)

	if [ -z "$expires" ]; then
		err "vmid $vmid has no valid expires- tag (tags: ${tags:-none}); leaving it alone"
		skipped=$((skipped + 1))
		continue
	fi

	[ "$now" -lt "$expires" ] && continue

	age=$((now - expires))
	log "vmid $vmid on $node expired ${age}s ago (status: $status); reaping"

	if [ "$status" != "stopped" ]; then
		if ! api POST "/nodes/$node/qemu/$vmid/status/stop" >/dev/null; then
			err "vmid $vmid: stop request failed; will retry next run"
			continue
		fi

		waited=0
		while [ "$waited" -lt "$STOP_TIMEOUT" ]; do
			sleep 3
			waited=$((waited + 3))
			cur=$(vm_status "$node" "$vmid")
			[ -z "$cur" ] && break              # already gone
			[ "$cur" = "stopped" ] && break
		done

		cur=$(vm_status "$node" "$vmid")
		if [ -n "$cur" ] && [ "$cur" != "stopped" ]; then
			err "vmid $vmid did not stop within ${STOP_TIMEOUT}s; will retry next run"
			continue
		fi
	fi

	if api DELETE "/nodes/$node/qemu/$vmid?purge=1&destroy-unreferenced-disks=1" >/dev/null; then
		log "vmid $vmid destroyed"
		reaped=$((reaped + 1))
	else
		err "vmid $vmid: destroy failed; will retry next run"
	fi
done

exit 0
