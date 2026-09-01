# Offline AppSandbox Linux VM Import Design

**Task:** Vikunja #21
**Supersedes:** `2026-08-29-appsandbox-linux-import-design.md` (branch `task-21-appsandbox-import`)
**Scope:** Convert a copy of an AppSandbox Linux guest into a VMLord guest by editing the
copied disk while nothing is running from it. Windows guests, AppSandbox templates, Hyper-V
imports and export back to AppSandbox stay out of scope.

## Goal

One AppSandbox VM — `C:\ProgramData\AppSandbox\ubuntu` — becomes a working VMLord VM, so
that AppSandbox can be uninstalled. The source VM is read and never written.

An import is successful only when the converted copy boots once, VMLord's own SSH key opens
a session, `vmlord-agent` authenticates with its secret, and the display and GPU stacks come
up the way they do on a VM VMLord created itself. A bootable disk is not success.

## Why the guest is not booted to convert it

The superseded design converts the guest from inside itself: it boots the copy in a
throwaway compute system, reaches it over SSH with AppSandbox's key, runs a stepped program
in it, asks for a shutdown, rebuilds the compute system on VMLord's terms and boots a second
time. Nine of that branch's last ten commits are failures of the *boot*, not of the
conversion — the copied guest kept AppSandbox's static address so nothing could reach it,
its agent looped against a host that was not listening, the network had to be handed back in
an order that did not cut the session issuing the command.

The conversion those boots exist to perform is a list of file operations
(`crates/platform/src/appsandbox/convert.py` on that branch): write a key, install a binary,
a secret and a unit, disable five units, delete twelve files, replace a netplan. Nothing in
it needs a running kernel. Every problem above is the cost of running a userland VMLord is
in the middle of dismantling.

Editing the disk instead removes the bootstrap compute system, the SSH bootstrap, the use of
AppSandbox's private key, the two-stage boot, and the ordering hazard between the network
handover and the shutdown. What is left is one boot — the same first boot every created VM
gets, which is also the verification.

## What the source is

AppSandbox does not install Ubuntu with subiquity. `tools/iso-patch/ubuntu_vhdx.c` writes
the disk itself, through its own ext4 writer (`tools/iso-patch/engine/ext4.c`), so the layout
is known exactly rather than guessed:

* GPT, two partitions: 1 = EFI System (FAT32, label `ESP`), 2 = Linux root.
* Root is ext4 with `FILETYPE|EXTENTS` and no journal, no `metadata_csum`, no 64bit, no
  HTREE, 4 KiB blocks, 256-byte inodes.
* No LVM, no LUKS, no separate `/boot`, no btrfs subvolumes.
* `/etc/fstab` is two lines: root by UUID, `LABEL=ESP` at `/boot/efi`. Nothing host-specific
  is mounted from it — the 9P shares AppSandbox uses are mounted by its agent at runtime and
  are not in `fstab`.
* The ESP carries both `\EFI\ubuntu\{shimx64,grubx64,mmx64}.efi` and the removable-media
  fallback `\EFI\BOOT\BOOTX64.EFI`, so a VM with an empty UEFI NVRAM store still boots. This
  matters: an imported VM gets a **fresh VMGS**, and therefore no boot entries.

The live VM's disk is 164 890 673 152 bytes (≈153.6 GiB) against 33 GiB free on `C:` and
386 GiB free on `D:`. VMLord's `vm_storage_path` must point at `D:` before an import is
attempted; this is the first thing that blocks otherwise.

## The complete AppSandbox guest footprint

Three writers leave state in the guest: the disk builder, the first-boot script, and the
running agent. Each row below is classified **remove** (VMLord's own stack conflicts with it,
or it is dead weight naming a program that will not exist), **keep** (it describes the guest,
not AppSandbox), or **replace**.

### Written by the disk builder — `tools/iso-patch/ubuntu_vhdx.c`

| Path | Disposition | Why |
|---|---|---|
| `/etc/fstab` | keep | root by UUID + `LABEL=ESP`; both survive the copy |
| `/boot/grub/grub.cfg`, ESP `\EFI\ubuntu\*`, `\EFI\BOOT\*` | keep | the boot path |
| `/usr/local/bin/appsandbox-firstboot.sh` | remove | provisioning that has already run |
| `/etc/systemd/system/appsandbox-firstboot.service` + its `multi-user.target.wants` symlink | remove | same |
| `/opt/appsandbox/**` | remove | `agent-src`, `asb_drm-src`, `dxgkrnl-src`, `systemd/`, `wsl-mesa.tar.zst`, `wsl-deps/`, `local-apt/` (≈296 MiB of `.deb`), `local-apt-extras/`, `50-appsandbox-gpu`, `org.gnome.Shell-no-gpu.conf`, `appsandbox-gpu`. Nothing reads it once its consumers are gone, and it is the largest thing the import can give back |
| `/etc/appsandbox-admin-user` | remove | a marker firstboot consumed but never deleted |
| `/etc/appsandbox-ssh-enabled` | remove | same |
| `/etc/appsandbox-{hostname,timezone,locale,keyboard,admin-hash}` | verify absent | firstboot deletes these; the converter asserts rather than assumes |

### Written by the first-boot script

| Path / effect | Disposition | Why |
|---|---|---|
| `/var/lib/appsandbox-firstboot.done` | remove | the marker of a program being removed |
| `/etc/hostname`, `127.0.1.1` in `/etc/hosts` | replace | set to the VMLord VM's name |
| `/etc/localtime`, `/etc/timezone`, `/etc/default/locale`, `/etc/default/keyboard` | keep | describe the guest; VMLord writes the same kinds of file on creation |
| the interactive account (`agromov`), its `$6$` password, groups `sudo plugdev lpadmin`, `~/.config/gnome-initial-setup-done`, the removed GNOME first-login autostart | keep | this is the VM the user wants to keep using |
| `/etc/gdm3/custom.conf` (autologin for that account) | keep | names an account that still exists |
| `/usr/local/bin/appsandbox-{agent,audio,clipboard,display,input}` | remove | the daemons themselves |
| `/etc/systemd/system/appsandbox-{agent,audio,display,input}.service`, `asb-evict-simpledrm.service`, and each `multi-user.target.wants` symlink | remove | their units. **The symlinks are the enablement** — deleting only the unit files leaves systemd with dangling wants |
| `/etc/systemd/user/appsandbox-clipboard.service` | remove | a *user* unit the superseded design's list misses entirely |
| `/etc/modules-load.d/asb_drm.conf` | remove | loads a module being removed |
| `/etc/modules-load.d/dxgkrnl.conf` | remove | **conflicts**: VMLord's GPU payload installs its own dxgkrnl and its own `/etc/modules-load.d/vmlord-dxgkrnl.conf` |
| `/etc/modules-load.d/snd-aloop.conf` | remove | exists only to feed `appsandbox-audio` |
| `/etc/modprobe.d/asb_drm.conf` | remove | **conflicts**: blacklists `hyperv_drm` and `simpledrm`. VMLord does not blacklist either — it tags `hyperv_drm`'s card in `62-vmlord-display.rules` and unbinds `simple-framebuffer` with `vmlord-display-unbind-simpledrm.service`. A blacklist changes what those two expect to find |
| `/usr/src/asb_drm-<v>`, `/usr/src/dxgkrnl-<v>`, `/var/lib/dkms/{asb_drm,dxgkrnl}`, `/lib/modules/<kver>/updates/dkms/{asb_drm,dxgkrnl}.ko*` | remove | **conflicts**: two DKMS trees both named `dxgkrnl`, and a second DRM driver competing for the card VMLord's compositor binds |
| `/opt/wsl-mesa`, `/etc/ld.so.conf.d/wsl-mesa.conf`, `/etc/vulkan/icd.d/dzn_icd.x86_64.json` | remove | **conflicts**: VMLord's Mesa lives at `/opt/vmlord/wsl-mesa` with `/etc/ld.so.conf.d/vmlord-wsl-mesa.conf` and installs an ICD under the same name in the same directory |
| `/opt/appsandbox/wsl-deps`, `/etc/ld.so.conf.d/appsandbox-wsl-deps.conf` | remove | points into the tree being deleted |
| `/etc/systemd/user-environment-generators/50-appsandbox-gpu` | remove | **the environment variables.** It emits `LD_LIBRARY_PATH`, `GALLIUM_DRIVER=d3d12`, `MESA_LOADER_DRIVER_OVERRIDE=d3d12`, `__GLX_VENDOR_LIBRARY_NAME=mesa`, `VK_DRIVER_FILES` into every user-session unit, pointing at `/opt/wsl-mesa`. VMLord's `50-vmlord-gpu` generator emits the same five names pointing at `/opt/vmlord/wsl-mesa`; generators are additive and ordering between them is a filename accident |
| `/etc/systemd/user/org.gnome.Shell@.service.d/no-gpu.conf` | remove | **conflicts, and quietly.** It is a drop-in with `UnsetEnvironment=` for exactly those five names, in the same drop-in directory VMLord puts `vmlord-display-compositor-mesa.conf` into. Both apply. AppSandbox's would strip VMLord's variables from the compositor |
| `/usr/local/bin/appsandbox-gpu` | remove | wrapper for the above |
| `/usr/local/bin/nvidia-smi` → `/usr/lib/wsl/lib/nvidia-smi` | keep | VMLord mounts the host WSL libraries at the same `/usr/lib/wsl/lib`, so the symlink keeps resolving |
| `/usr/lib/wsl/{lib,drivers}` (empty mount points) | keep | VMLord mounts over the same paths |
| `/etc/apt/appsandbox-sources.list.d/*` | remove | `file://` sources into `/opt/appsandbox` |
| installed packages: `build-essential`, `dkms`, `linux-headers-<kver>`, `libasound2-dev`, `libxcb1-dev`, `libxcb-xfixes0-dev`, `libdrm-dev`, `pkg-config`, `zstd`, `openssh-server` | keep | VMLord's display and GPU recipes build kernel modules in the guest and need the same toolchain |
| `systemctl set-default graphical.target`; masked `sleep/suspend/hibernate/hybrid-sleep.target`; `/etc/systemd/logind.conf.d/10-appsandbox-nosleep.conf`; `/etc/dconf/profile/user` + `/etc/dconf/db/local.d/00-appsandbox-nosleep` + compiled db | keep | policy for a VM with nobody at a seat; VMLord wants the same |
| `/etc/default/grub.d/99-appsandbox-no-efifb.cfg` (`video=efifb:off video=simplefb:off`) | remove the file, leave `grub.cfg` | see *Open risk: the kernel command line* |
| `ssh.service`/`ssh.socket` unmasked + enabled, `ufw allow OpenSSH` | keep | VMLord offers SSH too |

### Written by the running agent — `tools/linux/agent/appsandbox-agent.c`

| Path / effect | Disposition | Why |
|---|---|---|
| `/etc/netplan/99-appsandbox.yaml` | replace | a static `172.22.142.2/24` on a subnet AppSandbox served. `set_ip` also **deleted every other `*.yaml` in `/etc/netplan`**, so there is no `50-cloud-init.yaml` left to fall back on |
| `/etc/cloud/cloud.cfg.d/99-disable-network-config.cfg` | **keep** | see *Cloud-init* below — this is a deliberate departure from the superseded design, which removed it |
| `~agromov/.ssh/authorized_keys` | replace | the agent writes this file with `"w"`, so it holds AppSandbox's key and nothing else |
| `/etc/ld.so.conf.d/wsl.conf` | remove | written per boot to list the 9P mount paths |
| 9P mounts under `/usr/lib/wsl/*` | nothing to do | never in `fstab`; they do not survive a shutdown |

## What VMLord requires in return

From `vmlord-seed`, `vmlord-agent-protocol` and the agent, a VMLord guest is:

| Path | Content |
|---|---|
| `~<user>/.ssh/authorized_keys` | the VM's own public key, `0600`, owned by the user, `.ssh` `0700` |
| `/usr/local/lib/vmlord/vmlord-agent` | the agent binary, `0755 root:root` — taken from beside `vmlord.exe` on the host |
| `/etc/vmlord/agent.secret` | the VM's agent secret, `0600 root:root` |
| `/etc/systemd/system/vmlord-agent.service` | the unit `vmlord-seed` writes, `0644`, enabled through the `multi-user.target.wants` symlink |
| `/etc/netplan/90-vmlord.yaml` | `dhcp4: true` on `match: {name: "e*"}`, `0600`, with the renderer the guest actually runs |
| `/etc/ssh/sshd_config.d/10-vmlord.conf` and `/etc/systemd/system/ssh.socket.d/10-vmlord.conf` | only when the chosen SSH port is not the one the guest already listens on |

Everything else VMLord installs in a guest — the display module and its three services, the
`vmlord-display` account, the GPU payload, dxgkrnl, Mesa, the udev rule, the initramfs
rebuild — is installed **by the agent at runtime** from host shares, on an imported VM
exactly as on a created one. None of it belongs in the conversion, and the superseded
branch's attempts to verify it during the import are what its final commit was still fixing.

### Cloud-init

The guest has cloud-init installed and no datasource: nothing gives an imported VM a `cidata`
volume. With `99-disable-network-config.cfg` removed, cloud-init falls back to writing its
own `50-cloud-init.yaml` describing DHCP on the first NIC — a second netplan document
matching the same interface as `90-vmlord.yaml`. Netplan merges both, and two `ethernets`
keys claiming one NIC is the exact failure AppSandbox's own comment describes. The file
stays.

### Renderer

`90-vmlord.yaml` must name the renderer that is actually running, or the one that is stops
managing the interface. The branch resolved this in the guest because it was already in the
guest. Offline it is read off the disk: NetworkManager is the renderer when
`/etc/systemd/system/multi-user.target.wants/NetworkManager.service` (or the
`display-manager`-pulled equivalent) is present and the binary exists; otherwise
`networkd`. The check is on the same evidence `systemctl is-enabled` would use.

## Architecture

**The conversion is a function over a mounted root directory.** It takes a path to a
filesystem root and an input document, and performs the delta above. It knows nothing about
VHDX, WSL, Hyper-V or Windows, and it holds no `unsafe` code and no platform calls.

That boundary is the whole design. How the root gets mounted is a separate, replaceable
concern:

* **Now:** WSL2. `wsl --mount --vhd <copy> --bare` attaches the copy to the WSL2 kernel;
  the root partition is mounted read-write by hand. This needs an elevated prompt and a WSL
  installation, which is acceptable for a one-off and unacceptable for a shipped feature.
* **Later, for shipping:** a service VM. VMLord runs virtual machines; it can boot a small
  Linux with the copy attached as a *second* disk, run the same conversion over the mount,
  and power off. No WSL, no administrator, no SSH, no AppSandbox key, and nothing of
  AppSandbox's userland ever executes. The conversion code does not change.
* **Rejected — Docker:** Docker Desktop's Linux lives in its own WSL2 VM that cannot be
  handed a VHDX; reaching one would mean `qemu-nbd` and an `nbd` module the Docker Desktop
  kernel does not promise. Docker is a build-time toolchain in this repository
  (`payloads/*/Dockerfile`), not a host dependency.
* **Rejected — a native ext4 writer:** AppSandbox's own writer only *creates* filesystems.
  Modifying a live ext4 by hand on a 154 GiB disk is not a place to hand-roll a driver.

Because the conversion is a directory function, it is tested against a fixture tree in a
temporary directory: no VHDX, no ext4, no elevation, no VM. The tests assert the delta, the
refusals, and idempotency.

### Where the code lives

* `crates/appsandbox-convert` (new, platform-independent): the delta as data (paths, units,
  the netplan template) and the function that applies it to a root, plus its verification
  pass. The VMLord-side names are imported from `vmlord-seed` and `vmlord-agent-protocol`
  rather than copied, so a change to the unit text or the secret's path reaches an imported
  guest and a created one alike.
* `crates/xtask`: the command that runs it — `cargo xtask appsandbox-convert --root <path>
  --input <input.json>` — so the one-off is a repeatable, reviewed, committed operation
  rather than a shell session nobody can repeat.

### Ordering and idempotency

The conversion runs in one order, and every step is safe to run again:

1. **Refuse early.** Assert the root is what it claims: `/etc/os-release` names Ubuntu, the
   named account exists in `/etc/passwd` with a home under `/home`, `/opt/appsandbox` or an
   AppSandbox unit is present (this really is an AppSandbox guest), and no `vmlord-agent`
   unit is already installed by a previous partial run that failed differently.
2. **Add VMLord's own,** before anything of AppSandbox's is taken away: the key, the agent
   binary, the secret, the unit and its enablement symlink, the netplan, the SSH drop-ins.
   At no point is the guest left with neither stack.
3. **Disable and remove AppSandbox's:** the enablement symlinks first, then the unit files,
   then the binaries, the module configuration, the DKMS trees and built modules, the Mesa
   tree and its environment generator and compositor drop-in, the `ld.so.conf.d` entries,
   the leftover markers, `/opt/appsandbox`.
4. **Repair what those removals invalidate:** `ldconfig -r <root>` so the cache no longer
   names deleted directories.
5. **Verify against the disk, not against what step 2 and 3 believed.** Re-read every file
   the conversion wrote, every file it deleted, every symlink, every mode and owner, and the
   absence of every AppSandbox unit name anywhere under `/etc/systemd`.

The verification is a separate pass over the same root, and can be run on its own against a
disk converted earlier.

## Registering the converted disk

The conversion needs the VM's public key and agent secret, which VMLord generates. So the
VM record exists first and the disk is converted into it:

1. the copy is placed at the VM's disk path under `vm_storage_path`;
2. VMLord creates the VM record around that existing disk — new UUID, fresh VMGS and VMRS,
   `config.json`, key pair, agent secret, metadata — without downloading an image, without
   building a disk and without a seed ISO;
3. the input document is written out of that record;
4. the disk is mounted and converted;
5. the VM is started, and the first boot is the verification.

Step 2 is the one new host-side seam: an *adopt* path beside `create`, which is `create`
minus the disk build and minus the seed. The alternative — creating an ordinary VM and
swapping its disk afterwards — needs no new code at all but leaves a downloaded image and a
seed ISO to throw away and a metadata record describing a guest that no longer exists. The
adopt path is small enough to be worth it.

The discovery of AppSandbox VMs, the `vms.cfg` parser, the guarded copy with progress and
cancellation, the import journal and the UI on `task-21-appsandbox-import` are unaffected by
this design and are the parts of that branch worth carrying over. Its conversion layer —
`conversion.rs`, `source_agent.rs`, `convert.py`, and the bootstrap halves of `pipeline.rs`,
`worker.rs`, `ssh.rs` and `hcs_config.rs` — is what this design replaces.

## Open risk: the kernel command line

`99-appsandbox-no-efifb.cfg` put `video=efifb:off video=simplefb:off` into the generated
`grub.cfg`. Regenerating `grub.cfg` offline means a chroot with `/proc`, `/sys` and `/dev`
bound, which is a heavier operation than everything else here and the one step a service VM
would do differently from WSL. The conversion therefore removes the drop-in and leaves
`grub.cfg` alone: the stale options only suppress legacy framebuffer paths, and VMLord's own
display stack unbinds `simple-framebuffer` itself and tags `hyperv_drm`'s card by udev rather
than depending on either being present at boot. Whether that holds is checked on the first
boot; if the display stack needs the options gone, the fix is one `update-grub` in the guest
after it is up, not a chroot during the conversion.

## Testing

* the conversion and its verification against fixture roots: a full AppSandbox tree, a tree
  already converted (idempotency), a tree with each precondition violated in turn (refusal),
  a tree where `NetworkManager` is enabled and one where it is not (renderer);
* proof that every path the conversion writes or deletes is inside the root it was given;
* proof that the secret never reaches a log, an error or a `Debug` rendering;
* the VMLord-side names come from `vmlord-seed`/`vmlord-agent-protocol` — a test asserts the
  conversion's unit text is the seed's, not a copy;
* the adopt path: a VM record built around an existing disk, with no image download and no
  seed;
* `cargo check-windows` and `cargo test-windows` for the repository.

The manual end-to-end test is the point of the task: convert the copy of
`C:\ProgramData\AppSandbox\ubuntu`, boot it once, confirm SSH with VMLord's key, the agent's
handshake, the display session and the GPU probe, then confirm the AppSandbox VM's files are
byte-identical to their pre-test hashes.

## Out of scope

Export from VMLord, VM templates, Windows guests, Hyper-V imports, importing more than one
VM at a time, and productizing the mount mechanism as the service VM. The last is the
expected follow-up and the reason the conversion is a directory function.
