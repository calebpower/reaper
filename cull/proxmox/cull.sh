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

DRY_RUN=0
for _arg in "$@"; do
	case "$_arg" in
		-n|--dry-run) DRY_RUN=1 ;;
		-h|--help)
			echo "usage: $0 [--dry-run]"
			echo
			echo "Destroys expired ephemeral guests. With --dry-run, reports what"
			echo "it would destroy and touches nothing."
			exit 0 ;;
		*) echo "unknown option: $_arg" >&2; exit 2 ;;
	esac
done

PVE_HOST="${PVE_HOST:-bass:8006}"
PVE_POOL="${PVE_POOL:-cal/ephemeral}"
VMID_MIN="${VMID_MIN:-9000}"
VMID_MAX="${VMID_MAX:-9099}"
CRED="${CRED:-/usr/local/etc/pve-reaper/token}"
STOP_TIMEOUT="${STOP_TIMEOUT:-60}"

# Self-signed PVE cert: set PVE_INSECURE=1. If you have a real cert from
# your CA, leave it unset and point PVE_CACERT at the CA bundle instead.
# The flags are assembled as an argument list inside api() rather than as a
# string here: joining "--cacert /path" into one variable and re-splitting it
# unquoted broke any CA path with a space in it, and the shellcheck disable
# that permitted the re-split was justified only for the single-flag case.

log() { logger -t pve-reap -p daemon.info "$*"; echo "pve-reap: $*"; }
err() { logger -t pve-reap -p daemon.err  "$*"; echo "pve-reap: $*" >&2; }

[ -r "$CRED" ] || { err "cannot read credential file $CRED"; exit 1; }
# shellcheck source=/dev/null
# The credential's path is configurable, so there is nothing for shellcheck to
# read. The directive was two lines further up, where it annotated the wrong
# command and therefore did nothing.
. "$CRED"
[ -n "${PVE_TOKEN:-}" ] || { err "PVE_TOKEN unset in $CRED"; exit 1; }

api() {
	_method="$1"; _path="$2"
	set -- -sS --fail-with-body
	if [ -n "${PVE_INSECURE:-}" ]; then
		set -- "$@" --insecure
	elif [ -n "${PVE_CACERT:-}" ]; then
		set -- "$@" --cacert "${PVE_CACERT}"
	fi
	curl "$@" \
		-X "$_method" \
		-H "Authorization: $PVE_TOKEN" \
		"https://${PVE_HOST}/api2/json${_path}"
}

# Prints the guest's current status, or nothing and a non-zero exit when the
# query itself failed. The distinction matters: a failed query used to read as
# "already gone", and the sweeper then proceeded as if a stop had finished
# that it actually knows nothing about.
vm_status() {
	_body=$(api GET "/nodes/$1/qemu/$2/status/current" 2>/dev/null) || return 1
	printf '%s' "$_body" | jq -r '.data.status // empty'
}

now=$(date +%s)
reaped=0
skipped=0

# Templates are excluded, not merely left alone.
#
# A template has to live in the pool -- the credential's right to clone is
# scoped to it, so a template outside would be unusable. But a template carries
# no expiry tag, and reporting one as untagged on every run would bury the
# report that matters: a real untagged guest means a create failed part-way
# through. An alarm that fires every five minutes forever is an alarm nobody
# reads.
#
# Fetched, checked, and only then iterated.
#
# This used to be one pipeline: api | jq | while ... done. A pipeline takes its
# exit status from the last command, so a failed API call produced an empty
# loop and a clean exit -- an outage was indistinguishable from an idle
# cluster, which is the worst way for a backstop to fail. `set -e` cannot help;
# POSIX sh has no pipefail.
#
# Iterating from a here-document rather than a pipe also keeps the loop in this
# shell, so the counters below survive it. In the pipeline form they were
# incremented in a subshell and silently discarded.
if ! resources=$(api GET "/cluster/resources?type=vm"); then
	err "cannot list guests; the API is unreachable or the credential is refused"
	exit 1
fi

# tags arrive semicolon-separated; @tsv keeps fields unambiguous
if ! rows=$(printf '%s' "$resources" | jq -r --arg pool "$PVE_POOL" '
		.data[]
		| select(.pool == $pool)
		| select((.template // 0) != 1)
		| [.vmid, .node, (if (.status // "") == "" then "unknown" else .status end), (.tags // "")]
		| @tsv'); then
	err "the API answered with something that is not the guest list we expected"
	exit 1
fi

while IFS="$(printf '\t')" read -r vmid node status tags; do
	[ -n "$vmid" ] || continue

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

	if [ "$DRY_RUN" -eq 1 ]; then
		log "vmid $vmid: dry run, leaving it alone"
		reaped=$((reaped + 1))
		continue
	fi

	if [ "$status" != "stopped" ]; then
		if ! api POST "/nodes/$node/qemu/$vmid/status/stop" >/dev/null; then
			err "vmid $vmid: stop request failed; will retry next run"
			continue
		fi

		waited=0
		cur=""
		while [ "$waited" -lt "$STOP_TIMEOUT" ]; do
			sleep 3
			waited=$((waited + 3))
			# A failed query is "unknown", never "gone": proceeding to
			# the destroy on the strength of an error would be acting
			# on a guest whose state we could not read.
			cur=$(vm_status "$node" "$vmid") || cur="unknown"
			[ "$cur" = "stopped" ] && break
		done

		cur=$(vm_status "$node" "$vmid") || cur="unknown"
		if [ "$cur" != "stopped" ] && [ -n "$cur" ]; then
			err "vmid $vmid did not stop within ${STOP_TIMEOUT}s (last status: $cur); will retry next run"
			continue
		fi
	fi

	if api DELETE "/nodes/$node/qemu/$vmid?purge=1&destroy-unreferenced-disks=1" >/dev/null; then
		log "vmid $vmid destroyed"
		reaped=$((reaped + 1))
	else
		err "vmid $vmid: destroy failed; will retry next run"
	fi
done <<ROWS
$rows
ROWS

if [ "$DRY_RUN" -eq 1 ]; then
	log "dry run: would have reaped $reaped, skipped $skipped"
else
	log "reaped $reaped, skipped $skipped"
fi

exit 0
