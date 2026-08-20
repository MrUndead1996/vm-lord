# GPU-PV compatibility

What a host, a guest and a payload must be for a VMLord VM to render on the
host's GPU. When one of them is not, see
[GPU-PV troubleshooting](gpu-pv-troubleshooting.md); nothing on this page is a
reason a VM fails to start, because GPU-PV is applied best effort and never
decides whether a VM runs.

For why any of it is built this way, see the `GPU:` sections of
**ARCHITECTURE.md**.

## The host

| Requirement | Why | When it is missing |
| --- | --- | --- |
| Windows with Hyper-V and the Host Compute Service, VMLord elevated | HCS is what attaches a partition and what runs the VM at all | `gpu-host-service-unavailable` |
| A GPU Partition Adapter device | This is the device GPU-PV partitions; VMLord enumerates the device interface class through SetupAPI | `gpu-host-no-adapter` |
| A driver package in `System32\DriverStore\FileRepository` for that adapter | The guest mounts the host's own driver package; an adapter whose package cannot be resolved is a real device with nothing to hand a guest | `gpu-host-driver-store-missing`, or a partial assignment when only some adapters resolve |
| The WSL Linux userspace, **both halves** | This is what the guest links against | `gpu-host-linux-payload-missing` |

The two halves of the userspace are the part most often half-installed:

* `System32\lxss\lib` — what the GPU driver puts there, the vendor's own
  libraries.
* `Program Files\WSL\lib` — the Microsoft half the renderer actually links
  against: `libd3d12.so`, `libd3d12core.so`, `libdxcore.so`.

Where WSL is the inbox one, both live in a single directory and there is
nothing to get wrong. Where WSL came from the Store or the standalone
installer, they are split, and a host with only `System32\lxss\lib` looks
installed and cannot render — vendor libraries with nothing to drive them.
VMLord names the half that is missing rather than telling someone who has WSL
to install WSL.

A host with an adapter, a resolved driver package and a live HCS service meets
the **preconditions**. It is not a guarantee: assignment is proven only by
assigning, which needs a running compute system, and the host report is read
before there is one.

### Vendor extensions

Assignment sends `AllowVendorExtension`, which is what lets HCS attach a
vendor's own partition extension. Without it a host with an NVIDIA adapter
refuses the update outright with `HRESULT 0xC0350008` and an empty result
detail. Nothing needs to be configured for this; it is stated because that
HRESULT is what an older build produced and what a search engine will find.

## The guest

Only **Ubuntu**, and only a VM built from a **cloud image**.

| | Supported |
| --- | --- |
| Distribution | Ubuntu, through its `DistroProfile`; no other distribution has a recipe |
| Releases | those the create form offers — 26.04, 24.04, 22.04 |
| Architecture | `amd64` |
| Source | a cloud image; a VM installed from media has no guest triple recorded |
| Agent | required — the GPU is brought up by `vmlord-agent` over HvSocket |

A VM from installation media still gets the WSL and driver-package shares, but
no payload: VMLord records a guest triple only for the images it built itself,
and promises nothing about a system someone installed by hand.

VMs created before the agent and the payload existed cannot be migrated to
GPU-PV. Recreate them.

### The guest needs apt on its first GPU boot

The kernel module is built in the guest by DKMS, from the payload's sources,
against the running kernel's headers. `dkms`, `build-essential` and
`linux-headers-$(uname -r)` come from the guest's own apt and not from the
payload — which is why a first GPU boot needs the guest to reach
`archive.ubuntu.com`, and therefore a network mode that gives it one.

A guest that cannot reach apt fails the `BUILD_DEPENDENCIES` stage and ends up
`Degraded`. It never fails to start.

Every start after the first costs nothing and needs no network: a guest that
already has the module loaded and a `/dev/dxg` that opens short-circuits the
three build stages.

### Kernel upgrades

The kernel is deliberately not part of what a payload is selected by. A
payload's `kernel_release` records what the recipe was **proven on**, not what
it requires, and DKMS's own `AUTOINSTALL=yes` rebuilds the module across
Ubuntu's unattended kernel upgrades with VMLord not involved. A payload that
pinned the kernel would mean an upgrade kills GPU-PV until someone repacks one.

## The payload

The Linux userspace and the kernel module sources reach a host as a pair of
files beside `vmlord.exe`, both named by the payload's own id:

```
gpu-payload/ubuntu-26.04-amd64-7.0.0-28-v2.json
gpu-payload/ubuntu-26.04-amd64-7.0.0-28-v2.zip
```

`cargo gpu-payload pack` produces the pair; `cargo dist --gpu-payload
<directory>` verifies and installs it. The catalog is assembled at runtime from
that directory, which is derived from `current_exe` and from nothing else —
never from a user directory and never from configuration.

Two rules decide what a broken release looks like:

* **A missing catalog is an empty catalog.** No `gpu-payload` directory, an
  empty one, or one that cannot be listed: no entry matches, and the VM starts
  without GPU support. A build without a payload is a build without GPU.
* **A file that is there and wrong fails the catalog.** Unreadable JSON, a
  failed validation, a file whose name is not its `payload_id`, a missing
  archive. A silent absence is the worst way to learn of a broken release. An
  archive no entry claims is ignored.

An entry is selected by distribution, release and architecture. Where a triple
has several entries the newest proven kernel wins.

### Mesa policy

A payload declares how the guest gets its Mesa, and what that buys:

| `mesa_policy` | What the guest installs | Hardware Vulkan |
| --- | --- | --- |
| `distro` | `libgl1-mesa-dri`, `mesa-vulkan-drivers`, `libvulkan1` from the guest's apt | No — Ubuntu does not build Mesa with `microsoft-experimental`, so Vulkan is lavapipe and OpenGL is the hardware path |
| `bundled` | the payload's own Mesa, copied to `/opt/vmlord/wsl-mesa` | Yes, through `dzn` |

The guest reads the policy itself. A policy from a payload built newer than the
agent fails the `USERSPACE` stage it belongs to — after the kernel module has
already built, deliberately, so the failure is where it belongs.

## Modes, and when they can change

| Mode | What is attached |
| --- | --- |
| `None` | nothing; the guest renders in software |
| `Default` | the host's preferred adapter |
| `Mirror` | every GPU-PV capable adapter the host has |

A stored mode this build cannot apply reads back as `Unknown` and is a failure
rather than a silent `None`: the VM was configured to have a GPU and will not
get one.

Two rules follow from how a compute system is built:

* **A GPU mode changes only under a stopped VM, and applies at the next
  start.** The Plan9 section that carries the shares is written when the
  compute system is built and is immutable for the lifetime of a boot, so there
  is no such thing as changing a running VM's GPU.
* **A VM is deleted only while stopped.**

RAM and CPU are different — they are read from the stored configuration at the
next start and do not have this restriction.
