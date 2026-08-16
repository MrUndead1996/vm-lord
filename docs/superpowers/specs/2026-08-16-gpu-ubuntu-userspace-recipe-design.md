# Ubuntu GPU userspace recipe design

## Goal

Task #96 is the step where a guest that already has `/dev/dxg` gets a userspace
that renders on it. #95 built and loaded `dxgkrnl`; a kernel interface with no
Mesa above it is a device node nobody opens. The recipe gains three stages --
the Mesa userspace the payload's policy calls for, the Vulkan ICD it may carry,
and the environment that makes a process pick them -- and answers the host in
the same stage list it already answers with.

## Scope

This task owns the userspace half of the Ubuntu recipe: installing or staging
Mesa, registering a Vulkan ICD, and writing the environment. It does not render
anything or decide that a GPU works (#97), does not derive a `VmGpuStatus` or
draw anything (#98), and touches no audio, input, clipboard or desktop
configuration -- those were AppSandbox's VM image concerns and are not GPU.

Nothing here can stop a VM, and nothing here is a verdict. A stage that fails
is a stage in the report and a guest that renders on the CPU.

## The wire

The schema gains three `GpuRecipeStep` values and nothing else, so the revision
moves to **1.4**. `ApplyGpuRecipeResponse` was written as "every step, in the
order it is attempted, including the ones that never ran", which is what makes
a task that adds steps a minor bump rather than a message of its own -- exactly
what #95's design said the userspace task would be.

* `GPU_RECIPE_STEP_USERSPACE` (8) -- the Mesa userspace the payload's policy
  calls for.
* `GPU_RECIPE_STEP_VULKAN_ICD` (9) -- the Vulkan driver the payload carries,
  registered where the loader looks.
* `GPU_RECIPE_STEP_ENVIRONMENT` (10) -- what makes a process in this guest pick
  that userspace.

The host does not change at all. It logs the stages in the order they arrive
and branches on `state` alone, so a step it has never heard of is a line in a
log and never a decision. `STEPS` in the agent stays the one place the order is
written.

## Where the stages sit, and what stops them

The three run after `DEVICE`, and `DEVICE` becomes a stage that can end the
recipe. It is the last one of #95 today and therefore returns nothing; a guest
whose device node never appeared must not go on to be configured for a driver
that cannot open it, so a failed `DEVICE` leaves the userspace stages `SKIPPED`
with that reason.

The kernel half's short circuit -- module already loaded, device already
answering -- does not skip the userspace half. It skips the three stages that
would build a module, and `MODULE_LOAD` and `DEVICE` already run past it. Every
userspace stage has its own idempotence: a copied tree that is already
byte-for-byte the same, a symlink that already points there, a file that
already holds that text. The second start of a VM reports them `SKIPPED` and
touches neither apt nor the disk.

## The policy is the payload's, and the stage reads it itself

`sources.json` carries `mesa_policy`, which `vmlord-gpu-payload` already
validates against the catalog entry on the host: `distro` or `bundled`. The
guest honours both.

`USERSPACE` reads that field itself, through a `parse_mesa_policy` beside the
other parsers, rather than `PAYLOAD` reading it into `PayloadTarget` and
carrying it forward. A policy that is missing, or a value from a payload built
newer than this agent, must fail the stage it belongs to. Folding it into
`PAYLOAD` would mean an unreadable userspace field refuses a kernel module that
would have built and a `/dev/dxg` that would have worked.

## `USERSPACE` under `distro`

Check first, apt second, as `BUILD_DEPENDENCIES` does. When
`/usr/lib/<triplet>/dri/d3d12_dri.so` and `libvulkan.so.1` are already present
the stage is `SKIPPED`, which is what lets the second start of a VM work with
no network at all. Otherwise
`apt-get install -y libgl1-mesa-dri mesa-vulkan-drivers libvulkan1` with
`DEBIAN_FRONTEND=noninteractive`, the same 300 s budget, and the same single
`apt-get update` and one retry, because a cloud image's package lists are as
old as the image.

The triplet comes from the architecture `GuestFacts` already knows (`amd64` →
`x86_64-linux-gnu`), not from a constant: an agent that hard-codes one
architecture's library path is an agent that silently installs nothing on the
other.

The stage's message says what this policy does and does not give: Ubuntu's Mesa
carries the d3d12 gallium driver and is not built with
`microsoft-experimental`, so Vulkan on a `distro` payload is lavapipe. That is
a fact for the host's log, not a refusal -- the payload's author chose the
policy, and whether GL alone is enough is #97's question.

## `USERSPACE` under `bundled`

`content/mesa` must be in the payload; a policy that promises a Mesa tree and a
payload without one is `FAILED` with the path that is not there. It is copied
to `/opt/vmlord/wsl-mesa` with the same idempotent `copy_tree` #95 stages
module sources with -- `OK` when something changed, `SKIPPED` when the tree is
already the same.

A copy rather than loading straight out of the read-only 9p mount, even though
the linker needs no write access there: the mount lives exactly as long as the
agent's session, and the linker cache, the `ld.so.conf.d` line and the ICD
symlink all outlive a reboot. A guest whose `ld.so.cache` points at a directory
that is no longer mounted, with `VK_DRIVER_FILES` naming a file that is gone,
is the silent failure this recipe exists to avoid. The price is the tree on the
VM's disk once.

`/etc/ld.so.conf.d/vmlord-wsl-mesa.conf` then names
`/opt/vmlord/wsl-mesa/lib/<triplet>` and `ldconfig` runs. A separate file from
the `vmlord-gpu.conf` #94 rewrites from the current set of mounts: sharing one
file would mean that dropping a GPU share erases a line that has nothing to do
with shares.

## `VULKAN_ICD`

`SKIPPED` under `distro` -- the distribution registers its own ICDs, and a
second registration of the same files would be VMLord owning a decision apt
already made.

Under `bundled`, the `*.json` files in `<prefix>/share/vulkan/icd.d` are
symlinked into `/etc/vulkan/icd.d`. The names come from the payload rather than
a constant: AppSandbox's own notes record a README promising
`microsoft_icd.x86_64.json` where Mesa 25.3 shipped `dzn_icd.x86_64.json`, and
a hard-coded name is a stage that reports success on a file it never found.

A payload with no ICD is `SKIPPED` and not `FAILED`. A payload that carries GL
and no Vulkan is a legitimate payload, and whether a guest has enough of a
renderer is #97's judgement, not a stage's.

## `ENVIRONMENT`

One builder produces the document from a list of name/value pairs and one
condition; two files consume it. `/etc/systemd/user-environment-generators/50-vmlord-gpu`
prints `NAME=VALUE` and is mode 0755, so every user-session service and
everything started from one inherits it.
`/etc/profile.d/vmlord-gpu.sh` prints `export NAME=VALUE`, which is what covers
SSH -- the only way anything runs in an MVP guest.

Scripts with the probe inside rather than a static `environment.d` file of
finished values, because the probe has to run on every start: the file survives
a reboot and `/dev/dxg` does not, and a VM restarted into `GpuMode::None` with
a static `MESA_LOADER_DRIVER_OVERRIDE=d3d12` is a guest where GL stops working
entirely. The condition is `/dev/dxg` present and the library directory there.

The variables:

* `LD_LIBRARY_PATH` -- the Mesa prefix under `bundled`, and `/usr/lib/wsl/lib`
  under both. The second is not redundant with the `ld.so.conf.d` line #94
  writes: a vendor's WSL libraries are opened by unversioned SONAME
  (`libcuda.so`, `libnvidia-encode.so`), `ldconfig` caches only the `.so.1`
  entries, and the read-only 9p mount is where the unversioned symlink cannot
  be created.
* `GALLIUM_DRIVER=d3d12` and `MESA_LOADER_DRIVER_OVERRIDE=d3d12` -- both. The
  first is direct gallium selection on the GLX path, the second is the DRI
  loader used by EGL and Wayland clients; setting one gives accelerated GLX and
  llvmpipe on EGL.
* `__GLX_VENDOR_LIBRARY_NAME=mesa`.
* `VK_DRIVER_FILES` -- only when `VULKAN_ICD` registered something. Without it
  the loader also picks up lavapipe and the stock ICDs, and adapter selection
  stops being predictable.

Both files are written through the existing `write_if_different`, so a session
that changes nothing reports the stage `SKIPPED`.

## Tests

What decides is a pure function, as in #94 and #95, and those are what the
tests drive:

* `mesa_policy` parsing: both policies, a missing field, a value from a newer
  payload;
* the triplet from an architecture, including one this build does not know;
* choosing ICD files out of a directory listing, and a listing with none;
* the environment document for `bundled` with an ICD, `bundled` without one,
  and `distro` -- in both the generator and the `profile.d` form;
* the stage plan: a full run, a failed `DEVICE` that leaves three stages
  `SKIPPED`, and a second run that changes nothing.

`apt`, `ldconfig` and writing into `/etc` cannot run under `cargo test` and are
proven by hand on a real VM, as the module build in #95 was. `cargo test -p
vmlord-agent`, `cargo agent`, `cargo test-windows` and `cargo check-windows`
are the final checks.
