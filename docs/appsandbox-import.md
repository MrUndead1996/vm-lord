# Importing an AppSandbox Linux VM

VMLord can turn a **copy** of a finished AppSandbox Linux VM into a VMLord VM.
This is a one-way import, not a migration: the AppSandbox VM is only ever read,
and it is still AppSandbox's when the import is over.

The conversion happens **offline** — the copied disk is edited while nothing is
running from it. The guest being dismantled never starts, so none of
AppSandbox's network, agent or SSH machinery is ever in the way. The first time
the imported VM boots, it boots as a VMLord VM, and that boot is the
verification.

## What the import promises about the source

The source VM is never moved, renamed, linked, adopted, started, stopped,
deleted or modified. VMLord reads the source's `disk.vhdx` — and nothing else.

AppSandbox's private key is not used, not copied and not read: an offline
conversion needs no credential into the guest, because it never talks to one.

After a successful import there are two independent VMs: the AppSandbox one,
untouched, and a VMLord one with its own disk, its own key pair and its own
agent secret.

## Prerequisites

* The source VM is **shut down**, and was shut down when the copy was taken.
  This is not a formality. AppSandbox builds its guests' root filesystem
  **without a journal** (`tools/iso-patch/engine/ext4.c` sets no
  `HAS_JOURNAL`), so a copy taken from a running VM is not a dirty filesystem
  that replays on the next boot -- there is nothing to replay. It is a
  structurally inconsistent one, and the guest's own boot-time `fsck` refuses
  it and drops into emergency mode. Data the guest had not yet written is
  simply gone.
* Room for the copy on the volume VMLord stores VMs on, plus headroom. The
  current VM's disk is about 154 GiB. Set **VM storage** in settings to a
  volume with the space before starting.
* The copy is on the **same volume** as the VM storage directory. Adoption
  moves it into place; across volumes a move is a second full copy, which is
  refused rather than done silently.
* An **elevated** prompt. Both `adopt-disk` and `wsl --mount` need one: the
  Host Compute Service refuses a caller that is not an administrator or a
  member of the Hyper-V Administrators group, exactly as it does for ordinary
  VM creation.
* WSL2.
* A built `vmlord-agent` for the guest: `cargo agent` leaves it at
  `target/x86_64-unknown-linux-musl/debug/vmlord-agent`.

## The five steps

### 1. Copy the disk

Copy `C:\ProgramData\AppSandbox\<vm>\disk.vhdx` onto the VM storage volume.
Nothing else from the AppSandbox VM directory is imported — not `vm.vmgs`, not
`vm.vmrs`, not `vm_state.json`, not `display_settings.json`, not its snapshots.

### 2. Adopt it

```
vmlord adopt-disk --name <name> --disk <path to the copy> --username <user> \
                  --disk-gb <size> --release <release> [--ram-mb <size>] \
                  [--cpu-cores <count>] [--ssh-port <port>] [--headless]
```

`--username`, `--disk-gb`, `--ram-mb` and `--cpu-cores` come from the source
VM's own entry in `%ProgramData%\AppSandbox\vms.cfg` (`AdminUser`, `HddGB`,
`RamMB`, `CpuCores`). `--release` is the Ubuntu release the guest runs, which
`ImagePath` in the same file names.

Two of those are required because getting them wrong is not a cosmetic
mistake. `--disk-gb` is recorded as the VM's disk size and a later resize is
checked against it. `--release` is what the VM records as its guest, and a VM
that records no guest can be given neither a display payload nor a GPU one.

An adopted guest is taken to have a desktop, because AppSandbox's own first
boot installs one into every Linux VM it builds; `--headless` says otherwise.
This is what decides whether VMLord provisions its display stack into the
guest at all, so a desktop guest adopted as headless is one whose display
never arrives.

This builds the VM's own files around the copy — a new UUID, fresh firmware and
runtime state, its own `config.json`, an SSH key pair, an agent secret — and
moves the copy in as the VM's system disk. It downloads nothing, writes no
cloud-init seed and attaches no ISO, and it does **not** start the VM.

It prints where it wrote the conversion's input document. That path is always
`<VM storage>\<name>\import-input.json`; a release build has no console of its
own, so read it there if nothing was printed.

### 3. Check the copy before touching it

The copy is the whole VM, and a copy that is not sound is not worth converting.
After attaching it and **before mounting it**:

```
e2fsck -f -n /dev/sdX2
```

Exit code 0 is a sound copy. Anything else means the copy was taken from a
running VM: repair it with `e2fsck -f -y` if the VM is only being looked at,
but for a VM that is going to be used, take the copy again from a source that
is shut down. `e2fsck` restores the filesystem's structure; it cannot restore
what the guest never wrote.

### 4. Convert the disk

Two values in `import-input.json` name the machine doing the mount rather than
the VM, and are the operator's to fill in:

* `root` — where the guest's filesystem root will be mounted;
* `agent_binary` — where `vmlord-agent` is on that machine.

Then, from the same elevated prompt:

```
wsl --mount --vhd <VM storage>\<name>\disks\system.vhdx --bare
```

and inside WSL, as root:

```
lsblk                                    # find the disk; its second partition is the root
mkdir -p /mnt/vmlord-import
mount -t ext4 /dev/sdX2 /mnt/vmlord-import
cargo appsandbox-convert --input /mnt/c/.../import-input.json
umount /mnt/vmlord-import
```

and back on Windows, naming the VHD rather than a disk number -- a VHD
attached with `--bare` is taken by WSL whole and never becomes a disk Windows
lists, so it has no `PHYSICALDRIVE` number to give:

```
wsl --unmount <VM storage>\<name>\disks\system.vhdx
```

The conversion runs its own verification before it returns. `--verify-only`
reads a root back without changing it.

The source disk's layout is a plain GPT with an EFI system partition and one
ext4 root — AppSandbox builds it that way itself, with no LVM and no LUKS — so
the second partition is the one to mount.

### 5. Start it

Start the VM in VMLord. The first boot is the verification: the guest takes an
address from VMLord's DHCP, SSH opens with VMLord's key, the agent
authenticates with its secret, and the display and GPU stacks install
themselves from the host's shares exactly as they do on a VM VMLord created.

The first boot also finishes two things an offline conversion cannot do. The
initramfs still carries AppSandbox's kernel modules -- they were built into it
when they were installed, and removing them from `/lib/modules` does not remove
the copies inside the image -- and `grub.cfg` still carries that application's
`video=` options, because it was generated while their drop-in was there. Both
are regenerated by programs that need the guest's own kernel and a working
`/proc`. The conversion installs a one-shot,
`vmlord-import-repair.service`, that runs `update-initramfs -u` and
`update-grub` on the first boot and then disables itself.

## What the conversion changes in the guest

**It installs** VMLord's guest contract, and only that:

| Path | What |
|---|---|
| `~<user>/.ssh/authorized_keys` | the VM's own public key, `0600`, and nothing else |
| `/usr/local/lib/vmlord/vmlord-agent` | the agent, `0755 root:root` |
| `/etc/vmlord/agent.secret` | the VM's agent secret, `0600 root:root` |
| `/etc/systemd/system/vmlord-agent.service` | the unit, enabled by its `multi-user.target.wants` symlink |
| `/etc/netplan/90-vmlord.yaml` | DHCP, naming whichever renderer the guest actually runs |
| `/etc/hostname`, `/etc/hosts` | the VM's name |
| `/etc/ssh/sshd_config.d/10-vmlord.conf` and the socket drop-in | only when a port was asked for |
| `/etc/systemd/system/vmlord-import-repair.service` | the one-shot above, enabled by its own symlink |

**It removes** what AppSandbox's stack collides with or what names a program
being removed: its five system units and its user-level clipboard unit, with
their enablement symlinks; its five daemons and its GPU wrapper; its first-boot
script, unit and marker; `/opt/appsandbox` whole, including the local apt
mirror; `/opt/wsl-mesa` with its `ld.so.conf.d` lines and its Vulkan ICD; the
`asb_drm` and `dxgkrnl` DKMS trees with the modules they built; the
`modules-load.d` and `modprobe.d` files that load and blacklist them; the
user-environment generator that sets Mesa's five variables at AppSandbox's
prefix; the compositor drop-in that unsets those same five names; the static
netplan; the leftover markers; and the `grub.d` drop-in.

Three of those are conflicts rather than tidiness: the second `dxgkrnl` DKMS
package would compete with VMLord's own, the environment generator with
VMLord's own, and the compositor drop-in would unset the very variables
VMLord's generator sets.

**It leaves alone** everything that describes the guest rather than AppSandbox:
the user account with its password and groups, the desktop and its autologin,
the locale, keyboard and timezone, the installed packages (VMLord's own display
and GPU recipes build kernel modules in the guest and need that toolchain), the
sleep and idle policy, `/etc/fstab`, and the `nvidia-smi` symlink — VMLord
mounts the host's WSL libraries at the same path.

It also **keeps** `/etc/cloud/cloud.cfg.d/99-disable-network-config.cfg`.
An imported VM has no cloud-init datasource; with that file gone, cloud-init
would write a fallback netplan for the same interface `90-vmlord.yaml` claims,
and netplan merges both.

## What cannot be imported

Windows guests, AppSandbox templates, unfinished installations, a disk whose
root is not Ubuntu, and export from VMLord back to AppSandbox.

And a copy taken from a VM that was running. That one is worth naming on its
own, because it produces a disk that converts perfectly and then will not boot:
the conversion has no way to see it, and the guest's own `fsck` is what finds
out.

## When something goes wrong

The conversion checks the root before it writes anything: not Ubuntu, nothing
of AppSandbox's in it, no such account, or already converted — each refuses,
and a refused conversion has changed nothing.

Past that point it is idempotent: running it again on the same root is running
it once. A conversion that failed part-way can simply be run again, and
`--verify-only` says whether a root is already as the conversion leaves it.

The source VM is untouched in every case. A copy that is not worth keeping is
deleted like any other file, and the VM's own record with `vmlord`'s ordinary
delete.
