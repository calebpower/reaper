# Building the FreeBSD 15.1 template

Serves `exec: host` tenants — suites that must exercise the host operating
system itself, and native binaries no container here can run.

Roughly 30 minutes. The FreeBSD installer is terse and fast.

**Most of what went wrong building the Ubuntu template does not apply here**,
and it is worth knowing why rather than copying its cleanup blindly:

| Ubuntu problem | Here |
|---|---|
| `cloud-init clean` destroyed the network configuration | No cloud-init. Networking is `rc.conf` and survives |
| Host keys never regenerated (`ConditionFirstBoot`) | `rc.d/sshd` regenerates missing keys on **every** start, unconditionally. No custom unit needed |
| Deleting host keys cut ssh off instantly | sshd is a persistent daemon, not socket-activated. Existing connections survive |
| DHCP identity derived from `machine-id` | `dhclient` identifies by MAC already |

What *does* still apply: clear the machine identity as the last act, stop the
machine **hard** rather than shutting down, and let the provider attach the data
disk rather than baking one in.

> Screen wording drifts between releases. Where this runbook and the installer
> disagree, the installer is right, and this document has a bug.

## 1. Confirm the media is already there

**Storage → (a storage with `ISO image` content) → ISO Images.** Look for:

```
FreeBSD-15.1-RELEASE-amd64-disc1.iso
```

If it is not there, **stop** and ask whoever administers the cluster, naming
that file. reaper never fetches media — see [`../guests.md`](../guests.md).

## 2. Create the virtual machine

| Tab | Setting | Value |
|---|---|---|
| General | VM ID | one free identifier in reaper's range |
| | Name | `tmpl-freebsd-151` |
| | Resource pool | the pool reaper is configured for |
| OS | ISO image | `FreeBSD-15.1-RELEASE-amd64-disc1.iso` |
| | Type | **Other** |
| System | Qemu Agent | **ticked** |
| Disks | Bus/Device | **VirtIO Block**, `virtio0` |
| | Storage | the storage sessions will use |
| | Disk size | **8 GiB** |
| CPU | Cores | 2 |
| Memory | Memory | 2048 MiB |
| Network | Model | **VirtIO (paravirtualized)** |

As with the other template: the agent must be ticked, and there is exactly one
disk. The provider attaches the data disk per session.

Start the machine and open its console.

## 3. Install

- **Install**, keymap default unless you want otherwise.
- **Hostname**: `template`.
- **Distribution select**: deselect everything optional. No `lib32`, no `ports`,
  no `src`, no `tests`, no debug sets. Base and kernel only — this template is
  cloned repeatedly and every gigabyte is copied each time.
- **Partitioning**: **Auto (UFS)**, entire disk, GPT.

  UFS rather than ZFS-on-root, deliberately. A session then has exactly one
  pool — `tank`, on the disk the provider attaches — which keeps the mental
  model identical to the other guest. It also avoids a real trap: on a
  ZFS-root system `mount -p /` reports a dataset rather than a device, so
  anything reasoning about "the root disk" has to special-case it.

- **Root password**: set one. You will want console access at one in the morning
  eventually.
- **Network**: the VirtIO interface, IPv4, DHCP. IPv6 as you prefer.
- **Services at boot**: tick **sshd**. `ntpd` is reasonable. Leave `dumpdev`
  off — crash dumps on a disposable machine are weight with no reader.
- **Add user**: username **`reaper`**, and add it to group **`wheel`** so it can
  use `su`/`sudo`. Accept the defaults otherwise.

Finish, exit to a live shell or reboot and log in at the console.

## 4. Install what the contract requires

```sh
pkg install -y qemu-guest-agent rsync
```

Two packages. ZFS is already in the base system, so there is nothing to install
for it — the difference from the other guest is where ZFS comes from, not what
it does.

**On language toolchains.** This template ships with none, and that is a real
limitation stated plainly: it cannot yet serve a host-execution tenant, because
those need a compiler the template provides. Adding one is a deliberate,
recorded act — install it, note the version in step 5, and rebuild the backup.
That cost is exactly the tradeoff host execution makes, and it is why container
execution is the default.

Enable ZFS and the agent:

```sh
sysrc zfs_enable=YES
sysrc qemu_guest_agent_enable=YES
echo 'zfs_load="YES"' >> /boot/loader.conf
service qemu-guest-agent start
kldload zfs && zfs version
```

Trust the session key **for root**, and let root in by key:

```sh
mkdir -p /root/.ssh && chmod 700 /root/.ssh
echo 'ssh-ed25519 AAAA... your session key' >> /root/.ssh/authorized_keys
chmod 600 /root/.ssh/authorized_keys

printf 'PermitRootLogin prohibit-password\n' >> /etc/ssh/sshd_config
service sshd restart
```

Required, and more strictly than on the other guest: FreeBSD ships
`PermitRootLogin no`, refusing root entirely, where Ubuntu ships
`prohibit-password` and already permits a key. `prohibit-password` allows a key
and still refuses a password.

reaper connects as root because a session is a whole disposable machine whose
blast radius is the sandbox, and because an unprivileged user would mean an
escalation step that differs per guest -- `sudo` is not in the base system here.
The `reaper` user stays for console use.

## 5. Record what you built

```sh
pkg query '%n %v' qemu-guest-agent rsync
freebsd-version -kru; uname -a
```

Keep that output for the commit that registers the template.

## 6a. What went wrong the first time, and why it took so long

**The first build of this template produced clones with no ssh at all.** The
cause is now known exactly, and it is worth reading before you seal anything,
because the mistake is easy to repeat and invisible afterwards.

### The finding

Six zero-byte files:

```
-rw-------  1 root  wheel  0  /etc/ssh/ssh_host_ecdsa_key
-rw-r--r--  1 root  wheel  0  /etc/ssh/ssh_host_ecdsa_key.pub
-rw-------  1 root  wheel  0  /etc/ssh/ssh_host_ed25519_key
                              ... and the rsa pair
```

FreeBSD regenerates missing host keys on every boot, which is why this runbook
says no custom unit is needed. But look at what `/etc/rc.d/sshd` actually tests:

```sh
if [ -f "${keyfile}" ] ; then
        info "$ALG host key exists."
        return 0
```

A zero-byte file **is** `-f`. The script reports the key exists, generates
nothing, and sshd then fails to load an empty key file. That is the
`failed precmd routine for sshd` in the logs, on every boot, forever. No clone
of this template could ever accept an ssh connection.

### Where the empty files came from

From this runbook, in the step that said to stop the machine **hard**.

That instruction was carried over from the Ubuntu runbook, where it is correct
and load-bearing: a graceful shutdown there lets systemd write its `machine-id`
back out and undoes the clean. FreeBSD has no such behaviour, and UFS with soft
updates is far less forgiving of losing power mid-write than a journalling
filesystem is.

The evidence is unambiguous. On the sealed image, **every file written during
the final session is zero bytes, and nothing else on the disk is** — the host
keys, `/boot/loader.conf`, and `/etc/rc.local`. The superblock's clean flag is
`0`, so the filesystem was still dirty when the power went. The `sync` in the
clean sequence was not enough: on UFS, `sync` schedules the writes and does not
wait for the soft-update dependency chain to complete. A clean shutdown does.

The cruellest detail: `/etc/rc.local` was a *previous attempt to fix this very
symptom* — an early script calling `ssh-keygen -A`. It is in the table below as
"did not help". It never ran, because it was truncated to nothing by the same
power-off that caused the problem it was written to solve.

### What that means for the table of refuted hypotheses

Kept, because knowing what was ruled out is worth as much as the answer -- with
one correction.

| Hypothesis | Result |
|---|---|
| `sshd_enable` not set | Refuted -- it is `YES` |
| Malformed `sshd_config` | Refuted -- `sshd -t` passes |
| Entropy starvation | Refuted -- and `/entropy` and `/var/db/entropy/saved-entropy.1` are both present and 4096 bytes on the sealed image |
| Missing `/etc/hostid` | Refuted -- failed with `hostid` present too |
| The attached data disk | Refuted -- a clone with only its boot disk failed identically |
| An explicit early rc script running `ssh-keygen -A` | **Not refuted.** The script was zero bytes and never ran |

The second reported symptom -- "the guest agent did not start either" -- was
wrong. A clone boots to multi-user perfectly well and the agent reports an
address; `rc` continues past a failed `sshd` rather than stopping. What actually
happened is that the agent's first answer often lists an IPv6 address, and the
CLI took it and could not route to it. That was a real bug and is fixed; see
`docs/STATUS.md`.

### How it was found, eventually

Not from the console, and not by more inference. The clone's disk was attached
to a running Linux guest as a second disk and mounted read-only, which makes the
whole sealed image readable without booting anything: the configuration, the
logs, the file sizes, and the superblock's own clean flag.

That technique is worth remembering. It costs one running guest, needs no
console and no extra privilege, and it answers "what did the template actually
seal?" directly rather than by deduction. `mount -t ufs -o ro,ufstype=ufs2`.
Note that Linux cannot *write* UFS -- `CONFIG_UFS_FS_WRITE` is not set on a
stock Ubuntu kernel -- so this is a reading instrument only.

## 6. Clean, so the clone is not a copy of this boot

```sh
pkg clean -ay
rm -rf /var/log/* /var/tmp/*
rm -f /root/.history /home/reaper/.history
rm -f /etc/hostid /etc/machine-id
sync
```

**Host keys are deleted here**, exactly as on the other guest. An earlier
version of this runbook said to keep them, reasoning that FreeBSD regenerates
them anyway. That reasoning was drawn from an image where the files were present
and *empty*, which is the one case FreeBSD cannot handle -- see 6a. Absent files
are regenerated on every boot, reliably, and that has now been proven by two
clones with distinct keys.

Keeping them would also mean every session sharing one host key, which is worth
avoiding on its own account.

Unlike the other guest, deleting them does not cut off your own connection:
sshd here is a persistent daemon rather than socket-activated, so the session
you are working in survives.

The entropy seed is left alone: removing it was tried while chasing that bug and
made no difference, so there is no reason to strip randomness a clone could use.

Then shut the machine down **cleanly**:

```sh
shutdown -p now
```

**Not a hard power-off**, and this is the one place this runbook differs from
the other guest for a reason that cost an evening. See 6a: a hard stop here
leaves UFS mid-write, and every file touched in this session survives as a
zero-length stub -- including the host keys, which then permanently prevent
their own regeneration. `sync` does not save you; it schedules the writes
without waiting for the soft-update dependencies to retire.

There is nothing to protect against on this platform by stopping hard. The
Ubuntu runbook stops hard because systemd persists `machine-id` on the way
down; FreeBSD's identity files have no such behaviour, and `/etc/hostid` and
`/etc/machine-id` stay empty across a clean shutdown.

Why each of the last three lines:

**Both identity files.** FreeBSD 15 carries `/etc/hostid` *and*
`/etc/machine-id`. `hostid` is derived from `kern.hostuuid` and regenerated at
boot when absent; ZFS records it on pool import, so two machines claiming one
identity is a confusing failure much later. Removing only one leaves the clone
half-inheriting.

**The hard stop.** For the same reason as the other guest: a graceful shutdown
gives the running system an opportunity to write its identity back out, undoing
the step. A power-off gives it none.

## 7. Convert and protect

1. Right-click the machine → **Convert to template**.
2. **Options → Protection → Edit → ticked.**
3. Back it up, to somewhere that is not the storage the template lives on.

## 8. Register it

```toml
[guests."freebsd-15.1"]
template = "<the identifier you used>"
```

See [`../site-config.md`](../site-config.md).

## 9. Prove it

**Check the sealed image before converting**, because everything below costs a
clone and this costs nothing. From another machine with the disk attached, or
from the machine itself before you shut it down:

```sh
find /etc /boot -type f -size 0
```

`/etc/hostid` and `/etc/machine-id` should be the only entries, and they are
empty deliberately. **Anything else in that list is a file the shutdown lost**,
and if `/etc/ssh/ssh_host_*` appears, this template is already broken in the
exact way 6a describes.

1. `reaper up` against this guest produces a machine that reports an address.
2. Firstboot builds the pool and datasets — `zfs list` over SSH shows `tank` and
   its four children.
3. `zpool status tank` is `ONLINE`.
4. **A second clone works too**, and does not collide with the first. If
   `hostid` or the host keys survived step 6, this is where you find out.
5. Note the clone time for the README.

There is no container engine here, and that is correct: podman on this platform
runs foreign-format images under emulation and cannot execute native binaries,
which is the entire reason `exec: host` exists. Firstboot notices there is no
engine and skips the image-store configuration rather than failing.
