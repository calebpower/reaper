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
    zfsutils-linux podman nftables qemu-guest-agent rsync
```

Five packages, and each is in the guest contract for a reason: ZFS is the
rollback mechanism, podman runs the toolchains, the guest agent is how the
address is discovered, and rsync is how a working tree gets in and results get
out.

**`nftables` is the one this runbook originally missed**, and it is worth
knowing why. `--no-install-recommends` is right -- it is what keeps a template
from acquiring a package set nobody chose -- but podman only *recommends* the
packet-filter tooling its network backend drives. Without it podman installs,
starts, reports itself healthy, and pulls images perfectly. It fails the first
time anything tries to *run* a container, minutes into a build, with an error
about a missing `nft` binary that reads like a fault in the tenant's toolchain.

The check in step 9 exists because of that, and so does the one the runner now
performs after a pre-pull.

**Do not install a language toolchain.** Not a JDK, not Node, not Rust. On a
container-execution template a compiler nobody declared is a compiler nobody can
pin, and something will quietly depend on it inside a month.

Until this is installed the guest agent does not exist, so **the address cannot
be discovered through the API** and has to be read from the console. That is a
one-time cost while building the template, not something sessions ever pay.

Enable the agent and confirm ZFS loaded:

```sh
sudo systemctl enable --now qemu-guest-agent
sudo modprobe zfs && zfs version
```

Trust the session key **for root**:

```sh
sudo mkdir -p /root/.ssh && sudo chmod 700 /root/.ssh
echo 'ssh-ed25519 AAAA... your session key' | sudo tee -a /root/.ssh/authorized_keys
sudo chmod 600 /root/.ssh/authorized_keys
```

reaper connects as root, and `PermitRootLogin prohibit-password` is already the
default here, so key authentication works and passwords do not.

Root, because a session is a whole disposable machine whose blast radius is the
sandbox -- the original design accepts rootful containers for the same reason.
An unprivileged user would mean an escalation step that differs per guest: one
platform's `sudo` is a package, another's `su` wants a password. The `reaper`
user stays for console use when something has gone wrong.

## 5. Record what you built

```sh
dpkg-query -W -f='${Package} ${Version}\n' \
    zfsutils-linux podman qemu-guest-agent openssh-server rsync
lsb_release -ds; uname -r
```

Keep that output. It goes in the commit that registers the template, and it is
the only thing that makes a rebuild reproducible.

## 6. Clean, so the clone is not a copy of this boot

Everything in this section was learned by getting it wrong first. Read the
notes; several of these steps look optional and are not.

### 6a. Make the network survive cloning

```sh
sudo tee /etc/netplan/99-reaper.yaml >/dev/null <<'EOF'
network:
  version: 2
  ethernets:
    any-ethernet:
      match:
        name: "en*"
      dhcp4: true
      dhcp-identifier: mac
EOF
sudo chmod 600 /etc/netplan/99-reaper.yaml
sudo netplan apply
```

Two things here, both load-bearing.

**Matched by name pattern, never by MAC.** Every clone gets a fresh MAC, so a
MAC-pinned configuration leaves sessions with no network at all.

**`dhcp-identifier: mac`**, because the default derives the DHCP identity from
`machine-id`. Two clones that failed to regenerate one would present the same
identity and can be handed the same lease.

Applying this changes the machine's own address, because the DHCP server sees a
new client. That is the change working, not a fault.

### 6b. Guarantee host keys on every clone

```sh
sudo tee /etc/systemd/system/sshd-host-keys.service >/dev/null <<'EOF'
[Unit]
Description=Ensure sshd host keys exist before ssh accepts connections
ConditionPathExistsGlob=!/etc/ssh/ssh_host_*_key
Before=ssh.socket ssh.service sshd.service
After=local-fs.target
DefaultDependencies=no

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/usr/bin/ssh-keygen -A

[Install]
WantedBy=ssh.socket
EOF
sudo systemctl daemon-reload
sudo systemctl enable sshd-host-keys.service
```

Without this, **every clone comes up refusing all SSH connections.** The
distribution ships `sshd-keygen.service` for this, but it is gated on
`ConditionFirstBoot=yes`, and that flag is already false by the time
`ssh.socket` pulls it in. The clone boots, generates a new machine-id, and still
has no host keys.

`DefaultDependencies=no` is not decoration. Without it the unit is ordered after
`basic.target`, which comes after `sockets.target`, which contains `ssh.socket`
— while the unit also declares `Before=ssh.socket`. That is an ordering cycle,
and systemd breaks it by **deleting the start job for `ssh.socket`**, leaving the
machine with no ssh at all. The symptom is identical to missing host keys and
the cause is entirely different.

`WantedBy=ssh.socket` only. Adding `multi-user.target` reintroduces the cycle.

### 6c. Do not run `cloud-init clean`

It removes the generated netplan configuration, and with no datasource to
regenerate from, the clone comes up with **no network**. Earlier versions of this
runbook said to run it. They were wrong.

### 6d. The final sequence, in this order, then power off hard

```sh
sudo apt-get clean
sudo rm -rf /var/log/journal/* /var/lib/systemd/random-seed
sudo rm -f /root/.bash_history /home/reaper/.bash_history
sudo rm -f /etc/ssh/ssh_host_*
sudo rm -f /etc/machine-id && sudo touch /etc/machine-id
sync
```

Then stop the machine **hard** — a power-off from the hypervisor, not
`shutdown`. Do not reconnect afterwards.

Three reasons the order and the hard stop matter:

**Deleting the host keys kills SSH immediately.** ssh is socket-activated, so
each connection spawns a fresh `sshd` that reads the keys at startup. The moment
they are gone, new connections are refused. Nothing may follow this step but
`sync`.

**`machine-id` is emptied, not deleted.** An empty file is what makes systemd
treat the next boot as a first boot and generate a fresh identity.

**A clean shutdown rewrites it.** The running system persists its in-memory
machine-id on the way down, so a graceful `shutdown` undoes this step and every
clone inherits the template's identity. A hard stop gives it no opportunity.

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
3. **A container actually runs** — `podman run --rm <some pinned image> /bin/true`.
   Not `podman info`, which is what this said first and which is exactly the
   check that let a template ship unable to start a container at all. `info`
   inspects configuration; it exercises no part of the runtime or the network
   backend.
4. **A second clone works too.** This is the one people skip. It proves the
   first boot did not dirty the template — if machine-id or host keys survived
   step 6, two clones will collide and you want to find that now.
5. Note how long the clone took. That number decides whether the storage
   fallback in the original plan is needed, so it belongs in the README.
