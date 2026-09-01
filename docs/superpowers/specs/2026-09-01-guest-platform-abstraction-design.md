# Guest platform abstraction design

## Purpose

Task #151 asks for Arch Linux as a guest. Taken literally that is a second
`distros/*.json` file, and it does not work: the guest identity, the package
manager, the library layout and the keyboard file are Ubuntu constants spread
across five crates, and everything a desktop needs is GNOME constants spread
across three more. A second profile would resolve to a URL and then fail on the
first boot, in the agent, with `apt-get: not found`.

So the task this design covers is the one underneath: stop VMLord knowing what
a distribution and a desktop are, and split that knowledge in two -- what the
host declares before a guest exists, and what the agent finds once one does.
Arch is the first consumer, GNOME stays the first desktop, and Hyprland is the
second -- chosen deliberately, because a compositor that shares nothing with
mutter is what keeps the abstraction honest.

Provisioning stays cloud-init's. Installing from an arbitrary ISO was weighed
against this and set aside; the reasoning is recorded below, because it will be
asked again.

`AGENTS.md` forbids large architectural rewrites "unless explicitly requested".
This one is requested. It does not lift the other rule in that list -- traits
with a single implementation -- and that rule shapes the whole decomposition
below.

## The principle: declare what cannot be seen, detect the rest

`crates/core/src/distro.rs` opens by stating the house rule: "A profile is a
table of data, not a trait with one implementation per distribution. Ubuntu and
Fedora differ by a URL template, a default user, an admin group and the name of
a checksum file -- those are fields, not behaviour." `SshDaemon` and `SshUnits`
already follow it, and they are the proof it works: the socket-activated and
standalone shapes of an SSH daemon are described in JSON and no code branches on
a distribution's name.

That rule stands. What this design adds is a second question asked before it:
*who can see the answer?*

**The host declares what it must know before the guest exists.** A cloud image
has to be found and verified before anything boots, and cloud-init runs before
there is a system to ask. So the image templates, the checksum file, the default
user, the admin group, the SSH units, the keyboard files and the packages that
install a desktop stay profile data. Nothing can probe a guest that has not been
created yet.

**The agent detects everything else, inside the guest, at the moment it acts.**
Which package manager is installed is `command -v`. Whether libraries live under
a multiarch directory is whether that directory exists. Which compositor is
running is what answers on the session bus. The agent already works this way in
one place -- `dependencies_are_present` (`crates/agent/src/gpu_kernel.rs:290`)
asks the guest rather than a profile -- and this design makes that the rule
rather than the exception.

Detection is not merely cheaper than declaration here. It is more truthful. A
profile records what VMLord *asked for* when the VM was created; a guest three
months later is whatever it has *become* -- a kernel upgraded, a desktop
replaced, packages added by hand. `crates/core/src/display.rs` already separates
desired state from provisioning outcome from runtime facts, and this design
follows that seam: what was asked for keeps driving the seed, and what is found
drives the recipe.

The one exception is a genuine trait. Mutter's
`org.gnome.Mutter.RemoteDesktop` and wlroots' `wlr-data-control` are different
protocols with different selection-ownership models, not two spellings of one
call, so the guest clipboard gets an interface with the two implementations that
justify it. `AGENTS.md` forbids traits with a single implementation, so that
interface lands with its second implementation right behind it.

The corollary is the ordering. An abstraction merged without its consumer is
untested, because the suite keeps exercising the single Ubuntu path. So the
phases below interleave: each one ends at a guest that boots.

## What was considered and set aside

Installing from an arbitrary ISO and letting the agent work out the rest would
make most of this design unnecessary -- there would be no seed, no desktop to
install, and nothing to declare. It is set aside because the agent cannot get
into a guest VMLord did not provision.

Today the agent arrives through cloud-init, which mounts the `VMLTOOLS` volume
and installs the binary and the secret (`crates/seed/src/user_data.rs:142`);
`crates/platform/src/create.rs:191` builds no such volume for local media. With
no cloud-init there is no first-boot hook, and no way in: vsock needs a listener,
which is the agent; SSH needs a daemon and credentials VMLord never configured.

The alternatives were examined and none is free. A tools volume plus one command
typed by hand works but is not automatic. COM1 is already two-way and proven
(`docs/superpowers/specs/2026-08-11-com1-interactive-input-design.md`), but a
hand-installed guest usually has no serial getty, because `console=ttyS0` is not
on its kernel command line. Writing the three files into the guest's root
filesystem offline is the expensive one: `crates/platform/src/import/drive.rs`
already attaches a VHDX read-write as a physical drive and does sector I/O, so
the block layer exists -- but Windows understands only FAT and NTFS, while guest
roots are ext4, btrfs or xfs, possibly over LVM, possibly inside LUKS. Creating
a file there means an allocator, and an allocator bug corrupts a workspace disk
silently.

One idea from that examination is worth keeping on record because it is cheap
and unblocks the rest: the EFI System Partition is FAT32, Windows writes it
natively, VMLord's chipset is always UEFI (`crates/platform/src/hcs_config.rs:101`),
and for a systemd-boot guest the kernel command line lives there. Adding
`console=ttyS0` to it is what would give COM1 a getty to talk to. That is a
future task, not this one.

## What is coupled, and where

Every claim here was read out of the tree at commit `3c661a2`.

### Guest identity

`validated_release` (`crates/image/src/distro.rs:19`) accepts `NN.NN` and
nothing else, on purpose: the string is pasted into a URL. Arch publishes no
such number. `parse_os_release` (`crates/agent/src/gpu_recipe.rs:116`) returns
`None` unless `/etc/os-release` carries `VERSION_ID`; Arch's carries
`ID=arch` and `BUILD_ID=rolling` and no `VERSION_ID` at all, so guest facts
never assemble and both recipes stop before their first stage.

The same `distribution` + `release` pair keys both payload catalogues --
`display_selector` (`crates/platform/src/metadata.rs:200`) and
`ManifestTarget::matches` (`crates/gpu-payload/src/manifest.rs:31`) -- and
appears in every `payload.spec.json`. One notion of "release" runs through
`vmlord-image`, `vmlord-core`, `vmlord-platform`, `vmlord-display-payload` and
`vmlord-gpu-payload`.

Arch's images resolve cleanly once that notion admits a rolling one:
`https://geo.mirror.pkgbuild.com/images/latest/Arch-Linux-x86_64-cloudimg.qcow2`
with `Arch-Linux-x86_64-cloudimg.qcow2.SHA256` beside it, in the exact format
`parse_sha256sums` already reads, and a qcow2 the importer already opens.

### Package management in the guest

Four call sites run `apt-get` by name with `DEBIAN_FRONTEND` in the
environment: `crates/agent/src/display_kernel.rs:844` (dkms, build-essential and
the running kernel's headers), `crates/agent/src/display_kernel.rs:1436` (the
AppIndicator extension), `crates/agent/src/gpu_kernel.rs:261` (headers again)
and `crates/agent/src/gpu_kernel.rs:551` (the distribution's Mesa). Headers are
named by interpolation -- `linux-headers-{kernel_release}` -- which is a Debian
convention; Arch names the package after the kernel it belongs to
(`linux-headers`, `linux-lts-headers`, `linux-zen-headers`) and never after a
version.

The seed installs the desktop through cloud-init's own `packages:` key
(`crates/seed/src/user_data.rs:67`), which is distribution-neutral by design.
On Arch it is not neutral in effect: cloud-init's Arch module runs `pacman -S`,
and installing into a month-old image without a full `-Syu` is a partial upgrade
-- the one operation Arch documents as unsupported.

### Library layout

`library_triplet` (`crates/agent/src/gpu_recipe.rs:285`) answers
`x86_64-linux-gnu` and `crates/agent/src/gpu_kernel.rs:511`,
`crates/agent/src/gpu_probe.rs:229` and `crates/agent/src/gpu_render.rs:186`
build paths from it. So does a file that is easy to miss -- the shipped drop-in
`payloads/display/module/vmlord-display-compositor-mesa.conf` ends with
`Environment=LD_LIBRARY_PATH=/usr/lib/x86_64-linux-gnu`. Arch has no multiarch
directory; its libraries are in `/usr/lib`.

### Keyboard

`/etc/default/keyboard` is a constant (`crates/seed/src/user_data.rs:311`), its
content is Debian's `XKBMODEL`/`XKBLAYOUT` shell file, and `scalar::shell`
escapes for that file specifically. Arch configures the console through
`/etc/vconsole.conf` and X11/Wayland through
`/etc/X11/xorg.conf.d/00-keyboard.conf`.

### GNOME

Five couplings, and one non-coupling worth recording so nobody re-opens it.

* **Clipboard.** `crates/display-services/src/mutter.rs` drives
  `org.gnome.Mutter.RemoteDesktop`. Its surface is already narrow -- `open`,
  `listen`, `own`, `read`, `read_mime`, `write`, `refuse` and an `Event`
  receiver -- and `clipboard_main` touches nothing else.
* **Tray extension.** `ensure_appindicator_extension`
  (`crates/display-services/src/tray_main.rs:409`) calls
  `org.gnome.Shell.Extensions.EnableExtension`;
  `install_appindicator_extension` (`crates/agent/src/display_kernel.rs:1430`)
  apt-installs the package that provides it. The tray itself is `ksni`, a plain
  StatusNotifierItem, and needs no change for another desktop.
* **Compositor isolation.** The Mesa drop-in is written to
  `/etc/systemd/user/org.gnome.Shell@.service.d/`
  (`crates/agent/src/display_kernel.rs:51`), which works because GNOME's
  compositor is a templated user unit covering both the greeter and the logged-in
  session.
* **Output selection.** `payloads/display/module/62-vmlord-display.rules` hides
  the guest's synthetic Hyper-V display by tagging it `mutter-device-ignore`,
  sorting after mutter's own `61-mutter.rules` so the tag is added rather than
  replaced. The tag means nothing outside mutter.
* **`DesktopProfile`.** `crates/core/src/display.rs:38` is `Headless | Gnome`,
  it is an on-disk format, and it is matched on in `crates/ui/src/lib.rs`,
  `crates/app/src/lib.rs`, `crates/platform/src/display_prepare.rs:55` and the
  seed.

The non-coupling: `crates/display-services/src/seat.rs` finds the user at the
screen by reading `/run/systemd/sessions`, which is logind. It works under any
compositor that opens a graphical session, and this design does not touch it.

Also found: `DesktopSetup::display_manager` (`crates/core/src/distro.rs:233`) is
declared, populated in `distros/ubuntu.json`, asserted in one test, and read by
no production code. It is dead.

### What Hyprland adds

Hyprland is in this epic because designing the desktop profile against GNOME
alone produces the wrong shape. Two of the five couplings above have no Hyprland
analogue at all.

Output selection is the clearer one. wlroots has no device-ignore tag, so
Hyprland lights both cards, and task #121 already measured what that costs: an
absolute pointer stretched across a desktop that is partly invisible, with
clicks landing about a third of a screen from where they were aimed. Keeping the
desktop on VMLord's output is therefore a per-desktop mechanism -- a udev tag
under GNOME, a `monitor=` rule or a disabled `hyperv_drm` under Hyprland -- and
not a field holding a tag name.

Compositor isolation is the other. GNOME's compositor is a systemd user unit, so
a drop-in reaches it. Hyprland is launched from a login shell or through UWSM,
and a drop-in has nothing to attach to; the same `LD_LIBRARY_PATH` has to be
delivered another way.

And one thing the tree cannot answer. The DRM module was written against
mutter's behaviour: `payloads/display/module/vmlord_drm.c:211` and
`crates/display-services/src/cursor.rs` assume the compositor puts the pointer
in the cursor plane rather than the primary framebuffer, which is why capture
composites it, and `payloads/display/module/README.md:55` records that the
module deliberately does not set `DRIVER_CURSOR_HOTSPOT` because mutter reads
that as a reason not to light the output. wlroots handles cursor planes and
hotspots on its own terms. Whether the module needs a change is not derivable
from source and is scheduled below as an investigation with a live guest, whose
honest outcome may be "no change".

## The work, in three phases

Fifteen tasks. Each phase ends at a guest that boots, which is what keeps the
abstractions from outrunning their evidence.

### Phase 1 -- the distribution layer

1. **Rolling guest identity.** Admit a release that is not `NN.NN` through
   `validated_release`, `parse_os_release` (fall back to `BUILD_ID`), the
   `GuestTarget` key and the `payload.spec.json` schema. No behaviour changes for
   Ubuntu; its releases keep their current spelling.
2. **Guest facts grow a detected platform.** `GuestFacts` carries a
   distribution, a release, an architecture and a kernel release, all read from
   the guest. It gains the rest of what the recipes need to stop guessing: which
   package manager is installed, whether libraries sit under a multiarch
   directory, and what a desktop looks like on this system. This is the
   foundation the next three tasks consume.
3. **Package installation goes through the detected manager.** One installation
   point in the agent replacing the four `apt-get` sites. The command and its
   non-interactive environment come from what was detected; the package names --
   dkms, toolchain, headers, Mesa -- come from a small table keyed by the
   detected manager, because `linux-headers-$(uname -r)` and `linux-headers` are
   conventions of apt and pacman rather than of Ubuntu and Arch.
4. **Library paths come from detected facts.** Replace `library_triplet` with
   the detected layout, through `gpu_kernel`, `gpu_probe`, `gpu_render` and the
   `LD_LIBRARY_PATH` line of the compositor drop-in, which stops being a shipped
   constant.
5. **The keyboard stays profile data.** The seed writes the file or files the
   profile names, in the form it names, with the escaping that form needs. This
   one is not detected: cloud-init runs before there is a guest to ask.
6. **The display payload stops being keyed by distribution and release.** The
   three shipped specs -- `payloads/display/ubuntu-{22,24,26}.04-amd64/payload.spec.json`
   -- differ only in base image and target label: same version, same protocol
   range, and one `Dockerfile` and one `prepare.sh` serve all three. The content
   is DKMS sources, static musl binaries, udev rules and units, none of which
   knows what a package manager is. So `target` becomes provenance -- what this
   archive was proven to build on -- and the catalogue stops requiring an exact
   match on it. Per-target build proofs stay; the key goes.
7. **Arch cloud image profile.** `distros/arch.json`: URL templates, checksum
   file, default user, admin group, SSH units, keyboard, desktop packages, and
   the package-refresh policy cloud-init needs -- on Arch a bare `pacman -S` into
   a month-old image is a partial upgrade, which the distribution documents as
   unsupported.
   **Gate: an Arch guest boots, cloud-init finishes, SSH answers, and the agent
   builds the display module without a line of Arch-specific code in it.**

### Phase 2 -- the desktop layer

8. **What was asked for and what is there become two things.** `DesktopProfile`
   stays desired state and keeps driving the seed's package list. Beside it, the
   agent reports the desktop it actually finds. `display.rs` already draws this
   distinction for provisioning; this extends it to the desktop itself, and it
   is what makes #127 -- changing a VM's desktop later -- nearly free.
   `DesktopSetup::display_manager` is declared, populated and read by no
   production code: it is given a job here or deleted.
9. **The tray extension follows the detected desktop.** Installing and enabling
   an AppIndicator extension is something a GNOME session needs. A desktop that
   shows StatusNotifierItems natively needs nothing, and the recipe should find
   that out rather than be told.
10. **Compositor isolation follows the detected desktop.** How the compositor is
    kept off the payload's Mesa depends on how it is started -- a user-unit
    drop-in reaches GNOME's; Hyprland, started from a login shell or through
    UWSM, has no unit to attach one to.
11. **Output selection follows the detected desktop.** The `mutter-device-ignore`
    rule becomes one way of hiding the Hyper-V display among others.
    **Gate: an Arch guest comes up with a GNOME desktop, and the Ubuntu guest is
    unchanged.**

### Phase 3 -- the second compositor

12. **`GuestClipboard` behind a trait.** The trait plus the Mutter
    implementation, with no behaviour change. The existing tests in `mutter.rs`
    and `clipboard_files.rs` are the anchor that proves it.
13. **The wlroots implementation.** `wlr-data-control` / `ext-data-control`, and
    a choice made from what the session advertises rather than from a name.
14. **Cursor plane behaviour under wlroots.** An investigation against a live
    Hyprland guest: where the pointer lands, whether the output lights, whether
    the missing `DRIVER_CURSOR_HOTSPOT` matters. Its output is a finding and, if
    the finding demands it, a module change.
15. **The Hyprland desktop.** By this point most of what a desktop needs is
    detected, so what is left to declare is the seed's package list, a greeter
    and an autologin story. Everything else is the recipe reacting to what it
    finds.
    **Gate: a Hyprland guest is usable in the viewer -- pixels, pointer,
    clipboard, tray.**

## Testing

The suite is the safety net for every task in phases 1 and 2, and it is already
there: the seed, the catalogues, the recipes and the clipboard all have tests
that encode today's Ubuntu and GNOME behaviour. The rule for those phases is
that existing tests keep passing unchanged wherever the observable result for an
Ubuntu guest is unchanged -- a rewritten test in that position is evidence the
refactoring moved behaviour, not just structure.

New tests are table tests: a profile in, a rendered seed or a resolved command
out. That is what the data-over-traits principle buys, and `user_data.rs` and
`distro.rs` already test in exactly that shape.

What the suite cannot cover is every gate. A guest that boots, a desktop that
comes up and a pointer that lands where it was aimed are live-guest checks,
which is why each phase names one.

## Out of scope

* Migrating existing VMs. MVP rule: there are no users, and old VMs are
  recreated.
* Distributions beyond Arch. Fedora and SUSE are what `SshUnits` was shaped for
  and they stay hypothetical here.
* Desktops beyond GNOME and Hyprland.
* Changing a created VM's desktop afterwards -- that is #127.
* A GPU payload for Arch. Phase 1 makes the GPU recipe distribution-neutral;
  building and proving Mesa and dxgkrnl for a rolling target is its own task,
  and unlike the display payload it cannot be shared -- `mesa_policy: bundled`
  means Mesa is compiled against the base image's glibc and shipped as binaries,
  and the target pins a kernel release.
* Installing from arbitrary media, and the agent bootstrap it would need.
* Writing `console=ttyS0` into a guest's ESP to give COM1 a getty.
