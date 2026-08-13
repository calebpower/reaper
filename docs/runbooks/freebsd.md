# Building the FreeBSD 15.1 template

Serves `exec: host` tenants — suites that must exercise the host operating
system itself, and native binaries no container here can run.

Roughly 30 minutes. The FreeBSD installer is terse and fast.

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

sysrc -f /etc/ssh/sshd_config PermitRootLogin=prohibit-password 2>/dev/null || \
    printf 'PermitRootLogin prohibit-password\n' >> /etc/ssh/sshd_config
service sshd restart
```

Unlike the other guest, this one refuses root over SSH by default, so that line
is required. `prohibit-password` allows a key and still refuses a password.

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

## 6. Clean, so the clone is not a copy of this boot

```sh
rm -f /etc/ssh/ssh_host_*        # rc.d/sshd regenerates missing keys at boot
rm -f /etc/hostid                # regenerated at boot; two machines sharing one is trouble
rm -rf /var/log/* /var/tmp/*
rm -f /var/db/entropy/* /entropy
rm -f /root/.history /home/reaper/.history
pkg clean -ay
```

Deleting the host keys is safe here: FreeBSD's `rc.d/sshd` generates any that
are missing on the next boot, with no cloud-init involved. `hostid` matters for
the same reason — ZFS records it on pool import, and two machines claiming one
identity is a confusing failure much later.

Then shut down, do not reboot:

```sh
shutdown -p now
```

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
