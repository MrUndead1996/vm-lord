# GPU-PV troubleshooting

A VM's GPU is described in two rows, and reading them in the wrong order is the
commonest way to chase the wrong problem:

* **the mode** is what the VM asks for — `None`, `Default` or `Mirror`. It is
  stored with the VM and a failed start never changes it;
* **the status** is what GPU-PV is actually doing right now. It is derived per
  refresh and never stored.

A `Mirror` VM whose guest has not come up yet is not a VM without a GPU. Start
from the status, and from the **code** beside it: the codes below are stable
and are what to match on, while the message next to one carries the
host-specific detail and is free to be reworded.

Before anything here, check that the host meets the
[compatibility requirements](gpu-pv-compatibility.md).

## First, the two rules that are not failures

**A GPU never fails a start.** Every problem on this page leaves a VM running.
If a VM did not start, its cause is not here.

**Nothing is retried.** Not staging, not assignment, not a partial outcome. A
second attempt at a modify HCS refused is a second refusal. Where a fix below
says "restart the VM", that is the retry.

## Status codes

### The VM is not using GPU-PV

| Code | Means | What to do |
| --- | --- | --- |
| `gpu-mode-disabled` | The VM asks for no GPU. | Stop the VM, set the mode to `Default` or `Mirror`, start it. |
| `gpu-vm-not-running` | The VM asks for a GPU but is not running, so nothing is attached. | Nothing. This is not a failure. |
| `gpu-mode-unsupported` | The stored mode is not one this build can apply — a VM written by a newer build, or by the legacy backend. | Stop the VM and set a mode this build offers. |

### The host is still working

| Code | Means | What to do |
| --- | --- | --- |
| `gpu-assignment-pending` | The host has not attached anything yet. | Wait. Assignment happens right after the compute system starts. |
| `gpu-guest-pending` | The host attached the GPU; the guest has not reported. | Wait — a first GPU boot builds a kernel module. If it never reports, see [the guest never reports](#the-guest-never-reports). |
| `gpu-assignment-unknown` | This VM was started before this VMLord process, so what is attached to it was never observed here. | Nothing. It lasts until the guest's first report, which arrives within seconds of the agent reconnecting. Assignment happens once and cannot be re-observed, which is why this is not called "pending". |

### The host could not hand anything over

| Code | Means | What to do |
| --- | --- | --- |
| `gpu-host-no-adapter` | This host presents no GPU partition adapter. | Check that the GPU driver is installed and that the adapter appears as a GPU Partition Adapter. On a laptop with switchable graphics, check the adapter you expect is the one the host exposes. |
| `gpu-host-service-unavailable` | The Host Compute Service is not answering. | Nothing GPU-specific: the VM should not be running either. Check Hyper-V and the HCS service. This outranks the adapter question — VMLord does not report "no adapters" when it cannot ask. |
| `gpu-host-driver-store-missing` | Adapters are there, but no driver package could be located for any of them in `System32\DriverStore\FileRepository`. | Reinstall the GPU driver. An adapter whose package cannot be resolved is a real device with nothing to hand a guest. |
| `gpu-host-linux-payload-missing` | The WSL Linux userspace is not staged on this host. | Read the message — it names the half that is missing. |
| `gpu-assignment-failed` | The host could not attach the GPU, with the HCS failure beside it. | See [assignment refused by HCS](#assignment-refused-by-hcs). |

The three wordings of `gpu-host-linux-payload-missing` are different problems:

| Message names | Cause | Fix |
| --- | --- | --- |
| `Program Files\WSL\lib` | The vendor's libraries are installed and Microsoft's Direct3D 12 half is not. This is the commonest way to get here and it looks like a working install. | Install or update WSL. |
| `System32\lxss\lib` | The reverse: WSL is installed and the GPU driver put no Linux libraries there. | Install a GPU driver with WSL support. |
| neither half | No Linux userspace at all. | Install WSL. |

### Less GPU than the mode asked for

`gpu-assignment-partial` — the VM works, with less than it asked for. HCS
reports nothing about partiality, so it is derived from what could actually be
exported, and the message says which part is short:

| Message says | Means | What to do |
| --- | --- | --- |
| a driver package was exported for N of M adapters | Some adapters are attached to a guest that cannot mount their drivers. | Usually a second adapter whose driver package does not resolve. Under `Mirror`, `Default` may be the better mode on that host. |
| the Linux GPU payload is not staged for this VM | The adapters are attached and the guest has no userspace to render with. | See [no payload](#the-vm-got-no-payload). |

### The guest has the device

| Code | Means | What to do |
| --- | --- | --- |
| `gpu-guest-ready` | The guest renders on the GPU. This is the working state. | Nothing. |
| `gpu-guest-device-present` | `/dev/dxg` opened and nothing renders on it. | The kernel half worked and the userspace half did not. See [the device is there and nothing renders](#the-device-is-there-and-nothing-renders). |
| `gpu-guest-failed` | The guest cannot use the device it was given. The message names the recipe stage that stopped, or the probe verdict. | See the stage in [recipe stages](#recipe-stages). |

## Symptoms

### Assignment refused by HCS

The failure carries the HRESULT and the raw HCS result detail, because a
version-specific error schema is not something to guess at.

`0xC0350008` with an empty result detail on a host with an NVIDIA adapter is
the signature of a request sent without `AllowVendorExtension`. Current builds
always send it; if you see this, the build is older than the fix.

Assignment is attempted once per start, against the running compute system.
Restart the VM to try again.

### The VM got no payload

The kernel module sources and the Linux userspace reach a host as a pair of
files beside `vmlord.exe`, under `gpu-payload\`. Check that:

* the directory exists beside the executable that is actually running — the
  catalog is read from `current_exe` and from nowhere else;
* the `.json` and the `.zip` are both there, and the `.json` is named for the
  `payload_id` inside it;
* an entry matches the guest: distribution, release and architecture, never the
  kernel.

**A missing catalog is silent by design** — no directory, an empty one, or one
that cannot be listed means no entry matched and the VM starts without GPU
support. A file that is there and is wrong is not silent: it fails the catalog
outright, because that is a broken release.

A VM built from installation media has no guest triple recorded and gets no
payload at all. That is not a fault to fix; recreate it from a cloud image.

### Recipe stages

The guest's recipe runs in order and the first failure ends the run. Every
stage is reported, including the ones that never ran, so a report says what was
skipped rather than leaving it to be guessed.

| Stage | Fails when | What to do |
| --- | --- | --- |
| `DISTRIBUTION` | The guest is not one there is a recipe for. | Only Ubuntu is supported. |
| `PAYLOAD` | The share is not mounted, its `sources.json` is not where it should be, or `dkms.conf` does not name the package. | See [no payload](#the-vm-got-no-payload). Nine skipped stages after a mounted-but-empty directory is the signature of an export pointing at a staging root rather than at a generation — a bug, not a configuration. |
| `BUILD_DEPENDENCIES` | `dkms`, `build-essential` or `linux-headers-$(uname -r)` could not be installed. | **Almost always no network in the guest.** These come from the guest's own apt, deliberately. Give the VM a network mode that reaches `archive.ubuntu.com` and restart it. |
| `MODULE_SOURCE` | The sources could not be copied to `/usr/src`. | Check space in the guest. |
| `MODULE_BUILD` | DKMS did not build the module — most often headers that do not match the running kernel. | Let the guest finish an in-progress kernel upgrade, reboot it, restart the VM. |
| `MODULE_LOAD` | `modprobe` refused the built module. | Check `dmesg` in the guest; Secure Boot rejecting an unsigned module looks like this. |
| `DEVICE` | The module loaded and `/dev/dxg` never appeared. | The host attached no partition to this VM after all — read the assignment row rather than the guest one. |
| `USERSPACE` | Mesa could not be installed, or the payload's `mesa_policy` is one this agent does not know. | A policy the agent does not know means the payload is newer than the guest agent: recreate the VM so it gets the current agent. |
| `VULKAN_ICD` | Never fails — a payload with no Vulkan driver is skipped. | — |
| `ENVIRONMENT` | The profile and generator scripts could not be written. | Check the guest's root filesystem. |

Each stage runs under a wall-clock budget: 300 s for apt, 900 s for a build,
30 s for everything else. A budget that runs out takes the whole process tree
with it, so a stage that times out reports rather than hangs.

`DEVICE` gates the three userspace stages: a guest whose device node never
appeared is reported as skipped there, with that reason, rather than configured
for a driver it cannot open.

### The device is there and nothing renders

The probe's verdict is the guest's own, because the guest is the only side that
saw the output of the programs it ran:

* `RENDERS` — one hardware renderer from OpenGL or Vulkan. Not both, on
  purpose: under the `distro` Mesa policy Ubuntu's Mesa has no
  `microsoft-experimental`, so Vulkan is lavapipe and OpenGL is the only
  hardware path such a guest has. That is a working GPU.
* `DEVICE_ONLY` — `/dev/dxg` opens and nothing above it works. Look at the
  `USERSPACE` stage first.
* `NO_DEVICE` — ends the probe early; the kernel half is the problem, not the
  userspace.

Hardware is decided by a deny list — `llvmpipe`, `softpipe`, `swrast`,
`lavapipe`, `SwiftShader` — and a Vulkan `deviceType` of
`PHYSICAL_DEVICE_TYPE_CPU` is software whatever the device calls itself. A
renderer nobody wrote code against therefore counts as hardware, which is the
milder of the two ways to be wrong.

The probe runs `eglinfo` and `vulkaninfo --summary` through
`/etc/profile.d/vmlord-gpu.sh` — the same file a person gets over SSH. To
reproduce what the probe saw, log in and run them; if they disagree with the
status, the environment file is the difference.

### The guest never reports

The guest reports over HvSocket, not over the network, so this is the agent and
not the VM's networking:

1. Check the VM's agent status. `Offline` means no session at all — the GPU
   status is a symptom.
2. A VM created before `vmlord-agent` existed has no agent to connect.
   Recreate it.
3. Look at the VM's `com1.log` in its own directory for what the guest did
   before it had a network, and `cloud-init-status.log` for whether
   provisioning finished at all.

A first GPU boot legitimately takes a long time: apt, then a DKMS build. Later
starts short-circuit all three build stages and cost nothing.

### The mode will not change

A GPU mode changes only under a stopped VM, and applies at the **next** start.
The shares are written when the compute system is built and are immutable for
the lifetime of a boot, so a running VM's GPU is not something that can be
changed. RAM and CPU do not work this way, which is why the restriction looks
inconsistent from the form.

A VM is likewise deleted only while stopped.

## Where to look

| | Where |
| --- | --- |
| The VM's serial console | `com1.log`, in the VM's own directory |
| Whether provisioning finished | `cloud-init-status.log`, beside it |
| The staged payload for a VM | the VM's `gpu-payload\` child |
| The shared payload cache | `cache\` under the storage root |
| VMLord's own log | wherever the application settings name it |
| What the host can do at all | the GPU capability warnings shown when creating a VM |
