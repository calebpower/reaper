# Building the Ubuntu 26.04 template

Serves `exec: container` tenants: toolchains arrive as digest-pinned images, so
this template carries none.

Roughly 40 minutes, most of it waiting on the installer.

> Screen wording drifts between point releases. Where this runbook and the
> installer disagree, the installer is right — and the disagreement is a bug in
> this document worth fixing.

## 1. Confirm the media is already there

**Storage → (a storage with `ISO image` content) → ISO Images.** Look for:

```
ubuntu-26.04-live-server-amd64.iso
```

If it is not there, **stop**. reaper never fetches media, and neither should
you on its behalf — ask whoever administers the cluster to add it, naming that
file. See the media boundary in [`../guests.md`](../guests.md).

There is no `26.04.1` yet. That is worth knowing: a young LTS moves, and this
template will want rebuilding when the point release lands.

## 2. Create the virtual machine

**Create VM**, then work through the tabs.

| Tab | Setting | Value |
|---|---|---|
| General | VM ID | one free identifier in reaper's range |
| | Name | `tmpl-ubuntu-2604` |
| | Resource pool | the pool reaper is configured for |
| OS | ISO image | `ubuntu-26.04-live-server-amd64.iso` |
| | Type / Version | Linux / 6.x - 2.6 kernel |
| System | Qemu Agent | **ticked** |
| | Everything else | defaults |
| Disks | Bus/Device | **VirtIO Block**, `virtio0` |
| | Storage | the storage sessions will use |
| | Disk size | **8 GiB** |
| CPU | Cores | 2 |
| Memory | Memory | 2048 MiB |
| Network | Model | **VirtIO (paravirtualized)** |

Two of those matter more than the rest.

**Qemu Agent must be ticked.** It is how reaper learns the machine's address;
without it a session comes up and is never reachable.

**One disk, 8 GiB.** Do not add a second. The provider attaches the data disk
when it creates a session — that is what keeps a clone cheap.

Start the machine and open its console.

## 3. Install

- **Type**: choose **Ubuntu Server (minimized)**. A smaller image clones faster,
  and every package that is not here is a package that cannot drift.
- **Network**: DHCP. Note nothing down; sessions get their own addresses.
- **Storage**: use the entire disk, and **untick "Set up this disk as an LVM
  group"**. Plain ext4 on a partition. LVM buys flexibility this machine will
  never use, and costs a layer between the runner and the truth about what is
  on a disk.
- **Profile**: your name as you like; server name `template`; username
  **`reaper`**.
- **SSH**: tick **Install OpenSSH server**. Paste your session public key when
  it offers to import one. If you skip it here you will have to add it by hand
  in step 4.
- **Featured snaps**: none.

Let it finish, then reboot and log in at the console.

## 4. Install what the contract requires

```sh
sudo apt-get update
sudo apt-get install --no-install-recommends -y \
    zfsutils-linux podman qemu-guest-agent rsync
```

Four packages, and each is in the guest contract for a reason: ZFS is the
rollback mechanism, podman runs the toolchains, the guest agent is how the
address is discovered, and rsync is how a working tree gets in and results get
out.

**Do not install a language toolchain.** Not a JDK, not Node, not Rust. On a
container-execution template a compiler nobody declared is a compiler nobody can
pin, and something will quietly depend on it inside a month.

Enable the agent and confirm ZFS loaded:

```sh
sudo systemctl enable --now qemu-guest-agent
sudo modprobe zfs && zfs version
```

If you did not import a key during the install, add it now:

```sh
mkdir -p ~/.ssh && chmod 700 ~/.ssh
echo 'ssh-ed25519 AAAA... your session key' >> ~/.ssh/authorized_keys
chmod 600 ~/.ssh/authorized_keys
```

## 5. Record what you built

```sh
dpkg-query -W -f='${Package} ${Version}\n' \
    zfsutils-linux podman qemu-guest-agent openssh-server rsync
lsb_release -ds; uname -r
```

Keep that output. It goes in the commit that registers the template, and it is
the only thing that makes a rebuild reproducible.

## 6. Clean, so the clone is not a copy of this boot

A template that carries its first boot's identity produces clones that collide
with each other. Run all of this:

```sh
sudo cloud-init clean --logs --seed     # so it runs again, and regenerates host keys
sudo rm -f /etc/ssh/ssh_host_*          # cloud-init makes fresh ones on first boot
sudo truncate -s 0 /etc/machine-id      # regenerated at boot; must be empty, not absent
sudo rm -f /var/lib/dbus/machine-id
sudo ln -s /etc/machine-id /var/lib/dbus/machine-id
sudo apt-get clean
sudo rm -rf /var/log/journal/* /var/lib/systemd/random-seed
sudo rm -f ~/.bash_history
```

Two notes on that list.

`machine-id` is **truncated, not deleted**. An empty file means "generate one";
a missing file means something different to some tooling, and the difference has
cost people whole afternoons.

Host keys are deleted because two machines sharing one is the kind of thing
nobody notices until it matters. cloud-init regenerates them on first boot,
which is why it is cleaned rather than removed.

Then shut down — do not reboot:

```sh
sudo shutdown -h now
```

## 7. Convert and protect

1. Right-click the machine → **Convert to template**.
2. **Options → Protection → Edit → ticked.** Protection is what stops a stray
   destroy from taking the thing every session is made from. reaper refuses to
   touch templates as well, but two guards are the point.
3. Back it up: **Backup → Backup now**, to somewhere that is not the storage the
   template lives on.

## 8. Register it

Add it to the site registry — see [`../site-config.md`](../site-config.md):

```toml
[guests."ubuntu-26.04"]
template = "<the identifier you used>"
```

## 9. Prove it

Do not trust the build; test it. The phase's acceptance criteria are:

1. `reaper up` against this guest produces a machine that reports an address.
2. Firstboot builds the pool and the four datasets — `zfs list` over SSH shows
   `tank`, `tank/images`, `tank/cache`, `tank/state`, `tank/work`.
3. `podman info` runs.
4. **A second clone works too.** This is the one people skip. It proves the
   first boot did not dirty the template — if machine-id or host keys survived
   step 6, two clones will collide and you want to find that now.
5. Note how long the clone took. That number decides whether the storage
   fallback in the original plan is needed, so it belongs in the README.
