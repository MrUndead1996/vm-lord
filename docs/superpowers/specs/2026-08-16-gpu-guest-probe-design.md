# Guest GPU probe design

## Goal

Task #97 is the step where a guest that has been given a device, a module and
a userspace is asked whether any of it renders. #95 built and loaded
`dxgkrnl`, #96 installed the Mesa above it and wrote the environment that
points a process at it -- and every one of those stages can report `OK` on a
guest where nothing draws. A recipe says what was done; a probe says what
works, and only the second is worth calling a GPU.

The probe answers with what it saw, in one verdict and a list of checks. At
least one hardware renderer is what separates "the guest renders on the GPU"
from "the guest sees the device": a `/dev/dxg` that opens and a Mesa that
falls back to llvmpipe is a VM with a device node and a CPU rasteriser, and
calling that ready would make the state meaningless.

## Scope

This task owns the guest's probe and the wire it answers on, and the host
asking for it and logging what came back. It does not derive `VmGpuFacts`, a
`GpuState` or anything the UI paints -- that is #98, the same boundary #96
drew. Nothing here can stop a VM, and no check is a refusal: a probe that
finds nothing is a report and a guest that renders on the CPU.

## The wire

The schema gains one request, one response and the two enums they need, so the
revision moves to **1.5**. `ProbeGpuRequest` is arm 6 of `Request` and
`ProbeGpuResponse` is arm 7 of `Response` -- the field numbers `Request`
reserved for exactly this, in the comment that has been sitting there since
the protocol was written.

`ProbeGpuRequest` is empty, for the reason `ApplyGpuRecipeRequest` is: what
there is to look at is in the guest, and a field here would be the host
dictating something it cannot know better.

`ProbeGpuResponse` carries:

* `verdict` -- `GPU_PROBE_VERDICT_NO_DEVICE`, `..._DEVICE_ONLY` or
  `..._RENDERS`. The guest decides it, because the guest is the only side that
  saw the output; the host does not re-derive a verdict from the checks, and a
  build that disagreed with itself about what its own checks meant is exactly
  what one field prevents.
* `checks` -- every check, in the order it is attempted, including the ones
  that never ran, as `ApplyGpuRecipeResponse.stages` is. A report that stopped
  at the failure would leave the host guessing whether the rest was skipped or
  the agent hung up.
* `renderer` -- what the hardware renderer calls itself, when one answered:
  `D3D12 (NVIDIA GeForce RTX 4070)` is the single most useful line in this
  whole message for a person reading a log.
* `driver` -- the guest kernel driver, which is `dxgkrnl` on every guest this
  build has a recipe for and is still read rather than assumed.
* `render_node` -- `/dev/dri/renderD128` when the guest has one. The d3d12
  path does not need a DRM render node, so its absence is not a fault; it is
  reported because `GuestGpuDetail` has a field for it and a guest that has
  one is a guest where more than d3d12 is possible.

`GpuProbeCheckState` repeats the three values of `GpuRecipeStageState` rather
than reusing it. They mean the same thing today and are not the same thing: a
recipe stage is work that was done and a check is a fact that was looked at,
and a later value added to one must not appear in the other.

The host branches on `state` and `verdict` alone. A check value it has never
heard of is a line in a log, exactly as an unknown recipe step is.

## The checks

The order is written once, in the agent, as `STEPS` is for the recipe:

1. `DEVICE` -- `/dev/dxg` is a character device that opens. The same test the
   recipe's `DEVICE` stage makes, made again: the recipe ran once, minutes
   ago, and a probe that trusted it would report on a device that may since
   have gone.
2. `KERNEL_MODULE` -- `dxgkrnl` in `/proc/modules`.
3. `LIBRARIES` -- what a renderer has to be able to open: the `d3d12_dri.so`
   the GL path loads, in the bundled prefix or in the distribution's `dri`
   directory; `libd3d12.so` and `libdxcore.so` in the mounted WSL userspace,
   which `d3d12_dri.so` itself dlopens; and `libvulkan.so.1`. A missing
   library is `FAILED` and does not end the probe -- the renderers are what
   decide, and a library check that was wrong about a path must not be able to
   veto a guest that draws.
4. `TOOLS` -- the programs the next two checks run. Best effort, and its
   failure leaves `OPENGL` and `VULKAN` `SKIPPED` rather than `FAILED`:
   "nothing rendered" and "nothing was asked to render" are different facts.
5. `OPENGL` -- a bounded run of `eglinfo`.
6. `VULKAN` -- a bounded run of `vulkaninfo --summary`.
7. `VENDOR` -- whatever vendor tool the mounted userspace happens to carry.
   Diagnostics only; see below.

A failed `DEVICE` ends the probe: everything after it is `SKIPPED` with that
reason, and the verdict is `NO_DEVICE`. Nothing else ends it. A guest that
lost its module but kept its device, or has a library missing, is still asked
whether it renders, because the answer to that question is what the verdict is
made of and a guess would be worse than a run.

## The hardware operation, and why it is an external program

The agent is a statically linked musl binary with no C toolchain behind it, by
a rule **AGENTS.md** states outright. It can neither link nor `dlopen`
`libEGL` or `libvulkan`, so it cannot itself hold a GL context. Every real
operation on this GPU is therefore another program, run with a time budget.

The programs are the distribution's own: `mesa-utils` (which pulls
`mesa-utils-bin`, where `eglinfo` lives) and `vulkan-tools` (`vulkaninfo`).
Mesa's and Khronos's, not a vendor's -- which is what makes the probe
vendor-neutral, and what makes it work the same on a guest whose host has an
NVIDIA, an AMD or an Intel adapter behind `/dev/dxg`.

They are installed by the `TOOLS` check, present-first as every other apt
stage in this recipe: a second start of a VM installs nothing and needs no
network. Two considered alternatives and why not:

* **`/dev/dxg` ioctls straight from the agent.** No packages at all, and no
  answer to the question being asked: it is the dxgkrnl ABI rather than a
  graphics API, it is brittle across module versions, and a successful adapter
  enumeration says nothing about whether Mesa can draw with it.
* **A probe binary in the payload.** The payload is built on the host, and
  under the `distro` Mesa policy it carries no userspace at all -- the probe
  would be missing exactly where it matters most.

Each program runs as `sh -c '. /etc/profile.d/vmlord-gpu.sh; exec …'`, through
the file #96 wrote. Setting the same variables from inside the agent would be
a second copy of that decision, and running through the file proves the thing
a person actually gets over SSH: if the environment is wrong, the probe fails,
which is the point.

## What counts as hardware

One pure function over a renderer's name, and it is a deny list:
`llvmpipe`, `softpipe`, `swrast`, `lavapipe`, `SwiftShader`. Everything else
is hardware.

A deny list rather than an allow list of `d3d12`, because an allow list is a
probe that reports "no hardware renderer" on the first guest whose stack
renders through something this build was not written against, and the failure
mode of the deny list is the milder one: a new software rasteriser would be
reported as hardware once, until its name is added here.

Vulkan has one more fact to read and it is used: `deviceType` of
`PHYSICAL_DEVICE_TYPE_CPU` is software whatever the device is called. A driver
that reports `CPU` and a name nobody recognises is still not a GPU.

`vulkaninfo --summary` is parsed for `driverName`, `deviceName` and
`deviceType` per device; `eglinfo` for every `OpenGL renderer string:` line it
prints, across all the platforms it walks. Both are read as text and neither
is trusted to have a fixed shape: a run that produced no line the parser
recognises is a check that `FAILED` with the program's own output attached,
never a silent absence.

## The verdict

* `RENDERS` -- at least one hardware renderer answered, from either API. One
  is enough, and it has to be: Ubuntu's Mesa carries the d3d12 gallium driver
  and is not built with `microsoft-experimental`, so under the `distro` policy
  Vulkan is lavapipe and GL is the only hardware path a guest has. A verdict
  that wanted both would report failure on the policy the payload's own author
  chose.
* `DEVICE_ONLY` -- `/dev/dxg` opened and no hardware renderer answered.
* `NO_DEVICE` -- `/dev/dxg` did not.

## Vendor tools

`nvidia-smi` in the mounted WSL userspace, `rocm-smi` beside it: run with a
short budget, first line kept, recorded as one check. It never touches the
verdict and no other check depends on it. A guest whose vendor tool prints an
adapter and whose Mesa renders on llvmpipe is not ready, and a guest with no
vendor tool at all that draws through d3d12 is -- which is the whole reason
the verdict is made of renderers and not of vendor output.

## The host

`ProbeGpuRequest` is sent once per session, after the recipe report of the
same session, from where the recipe is sent: the probe asks about a userspace
the recipe has just installed. `PROBE_REQUEST_ID` follows `APPLY_REQUEST_ID`,
and the response is logged check by check at the volume each earns, exactly as
`report_recipe` does. Nothing is kept: the next session probes again, and
turning this into `VmGpuFacts` is #98.

A session whose agent does not speak `CAPABILITY_GPU` is sent no probe, as it
is sent no manifest and no recipe.

## Tests

What decides is pure, and that is what the tests drive:

* renderer classification: each software name, a d3d12 name, a name nobody
  knows, and the empty string;
* `vulkaninfo --summary` parsing: a lavapipe-only guest, a device that is not
  a CPU, several devices, and output the parser does not recognise;
* `eglinfo` parsing: several platforms, one renderer, none;
* the verdict from a set of checks: renders, device only, no device;
* the check list: a full run, a failed `DEVICE` that skips the rest with its
  reason, and tools that did not install leaving the renderers `SKIPPED`;
* the wire: a probe report round-trips, as the recipe report does.

`apt`, `eglinfo` and `vulkaninfo` cannot run under `cargo test` and are proven
by hand on a real VM, as #95's module build and #96's userspace were.
`cargo test -p vmlord-agent`, `cargo agent`, `cargo test-windows` and
`cargo check-windows` are the final checks.
