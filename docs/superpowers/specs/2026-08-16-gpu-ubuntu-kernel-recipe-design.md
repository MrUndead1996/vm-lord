# Ubuntu GPU kernel recipe design

## Goal

Task #95 is the step where a guest that has the GPU payload mounted gets a
working `/dev/dxg`. The host asks the agent to apply its recipe; the agent
decides whether this guest is one the recipe knows, builds the `dxgkrnl`
module out of the payload through DKMS, makes it load on every boot, and says
back, stage by stage, what happened. It is what turns #94's mounted
directories into a kernel interface the userspace of #96 and the probe of #97
have something to talk to.

## Scope

This task owns the kernel half of the Ubuntu recipe and the wire that carries
it: the `ApplyGpuRecipe` exchange, the guest module that runs the stages, and
the host sending it once per session and logging the report. It does not
install Mesa or any userspace library (#96), does not render anything or
decide that a GPU works (#97), and does not derive a `VmGpuStatus` or draw
anything (#98). The report is facts only -- a stage and what became of it --
because naming a state is #98's job and a backend never names one.

Nothing here can stop a VM. Every failure is a stage that failed, an answer to
the host, and a VM that keeps running with less GPU than it asked for.

## Where the build dependencies come from

`dkms`, `build-essential` and `linux-headers-$(uname -r)` are installed from
the guest's own apt, over the guest's network, and not staged in the payload.

AppSandbox did the opposite: the host resolved an apt closure per (release,
kernel) pair, wrote the `.deb` files into the rootfs, and the first-boot script
installed them with the archive lists switched off. It works offline, and it
costs an apt dependency resolver on a Windows host, hundreds of megabytes per
kernel in every payload, and a payload that goes stale the moment the guest
upgrades its kernel. VMLord provisions its Ubuntu guests through cloud-init
over NAT, so a guest that cannot reach `archive.ubuntu.com` is already a guest
that did not finish provisioning.

The payload therefore carries exactly one thing this task needs: the `dxgkrnl`
sources. The consequence is stated rather than hidden: a VM with no network
gets a failed `BUILD_DEPENDENCIES` stage and a `Degraded` GPU, and starts
normally.

Because apt is only reached when something is actually missing, a second start
of the same VM never touches the network at all: the module is already built,
installed and loaded, and the recipe short-circuits before its first stage that
would.

## What "supported" means

The hard gate is distribution, release and architecture. A guest that is not
Ubuntu, not the release the payload was built for, or not `amd64` is a
`SKIPPED` `DISTRIBUTION` stage with the reason in it, and nothing else runs.

The kernel release is not a gate. DKMS builds against the headers of the
*running* kernel, so an exact match with the payload's `kernel_release` is not
needed to compile, and requiring one would mean that the unattended kernel
upgrade Ubuntu performs on its own kills GPU-PV until someone repacks a payload
on the host. What `kernel_release` records is the kernel the recipe was
*proven* on, so a mismatch is a line in the report and never a refusal. DKMS's
own `AUTOINSTALL=yes` is what carries the module across the next kernel upgrade
without VMLord being involved at all.

## The wire

The schema gains messages and enum values only, so the revision moves to
**1.3** and an agent from 1.2 simply never sees the request.

`ApplyGpuRecipeRequest` is empty on purpose. Everything the guest needs to
decide is either in the guest -- `/etc/os-release`, `uname` -- or in the
payload it was told to mount one message earlier, and a field here would be the
host dictating something it cannot know better than the guest does.

`ApplyGpuRecipeResponse` is a `repeated GpuRecipeStage`, one per step the
recipe has, in the order they were attempted. A stage is a `GpuRecipeStep`, a
`GpuRecipeStageState` -- `OK`, `SKIPPED` or `FAILED` -- and free text for the
host's log. The steps are `DISTRIBUTION`, `PAYLOAD`, `BUILD_DEPENDENCIES`,
`MODULE_SOURCE`, `MODULE_BUILD`, `MODULE_LOAD` and `DEVICE`.

A list of stages rather than a verdict, for the same reason `VmGpuFacts` is not
a `VmGpuStatus`: "the module built and `/dev/dxg` never appeared" and "the
headers would not install" are one word apart in a summary and are different
problems. #96 and #97 add steps to the enum and nothing else to the schema,
which is what makes this a minor bump rather than a message per task.

Every stage is reported, including the ones that never ran: a report that stops
at the failure leaves the host guessing whether the rest was skipped or the
agent hung up.

The request is refused with `ERROR_CODE_UNSUPPORTED_REQUEST` on a session that
did not agree `CAPABILITY_GPU`, and the existing rule refuses anything before
the challenge is answered. This build always announces the capability, so the
check cannot fire today; it is where the rule is written, and the rule outlives
the build.

## The stages

Each stage runs only if the ones before it left the recipe able to continue.
The first `FAILED`, and any `SKIPPED` that means the recipe does not apply
here, ends the run; the remaining steps are still reported, as `SKIPPED` with
the reason.

1. **`DISTRIBUTION`** -- `/etc/os-release` (`ID`, `VERSION_ID`) and the machine
   from `uname`. `GpuRecipe::Ubuntu` is chosen by `ID=ubuntu` and nothing else;
   a distribution with no recipe is `SKIPPED`, which is the "unsupported
   release gives Degraded and does not stop the VM" rule in one place.
2. **`PAYLOAD`** -- the payload is mounted at `/opt/vmlord/gpu-payload`, its
   `sources.json` parses, its `target` agrees with this guest on release and
   architecture, and `content/dxgkrnl/dkms.conf` names a package and a version.
   The payload's `kernel_release` is compared to the running one and the
   difference, if any, is recorded here.
3. **short circuit** -- `dxgkrnl` in `/proc/modules` and a `/dev/dxg` that is a
   character device mean the guest already has what this recipe delivers.
   `BUILD_DEPENDENCIES`, `MODULE_SOURCE` and `MODULE_BUILD` are `SKIPPED` as
   already satisfied, and `MODULE_LOAD` and `DEVICE` still run: the
   `modules-load.d` file is what a module loaded by hand does not have.
4. **`BUILD_DEPENDENCIES`** -- what is missing is what is installed. `dkms` on
   `PATH`, a working `cc`, and `/lib/modules/$(uname -r)/build` are checked
   first, and a guest that has all three never runs apt. Otherwise
   `apt-get install -y dkms build-essential linux-headers-$(uname -r)` with
   `DEBIAN_FRONTEND=noninteractive`; if that fails, one `apt-get update` and
   one retry, because a cloud image's package lists are as old as the image.
5. **`MODULE_SOURCE`** -- `content/dxgkrnl` is copied to
   `/usr/src/<package>-<version>`. A copy rather than a symlink because the
   payload is mounted read-only over 9p and DKMS writes beside the sources it
   is given. Idempotent: a destination that already holds the same files is
   left alone, so a reconnect does not rewrite the tree DKMS is registered
   against.
6. **`MODULE_BUILD`** -- `dkms add`, `dkms build` and `dkms install` for that
   package and version against the running kernel. A version already installed
   for this kernel is `SKIPPED`. A build that fails reports the tail of the
   DKMS `make.log` rather than an exit code, because an exit code from a
   compiler is not a diagnosis.
7. **`MODULE_LOAD`** -- `/etc/modules-load.d/vmlord-dxgkrnl.conf` is written
   with the module name, and `modprobe dxgkrnl` runs. The file is what makes
   the next boot work: a module loaded only by `modprobe` is gone after the
   reboot, and GPU-PV breaks silently on a VM that was fine yesterday.
8. **`DEVICE`** -- `/dev/dxg` exists, is a character device, and opens. Opening
   it is what separates a device node the kernel created from one left behind
   by a module that is no longer there.

## Running programs, with a bound

The agent already runs `ldconfig`; this task adds `apt-get`, `dkms` and
`modprobe`. All three are distribution-owned operations with no library form,
and reimplementing dpkg's dependency resolution or DKMS's kernel bookkeeping
inside the agent would be a second implementation of something the distribution
already ships.

Every one of them runs through one helper: spawn, capture stdout and stderr,
wait with a budget, kill the process when it is exceeded, and keep the last
~20 lines of output for the stage's message. The budgets are 300 s for apt,
900 s for `dkms build`, and 30 s for everything else. Unbounded is not an
option: a hung `apt-get` behind a broken NAT would be a guest agent that never
answers again.

The whole recipe runs synchronously inside the session, where `attach` already
runs. The host tolerates that: its read timeout surfaces as `FrameError::Idle`
and it simply keeps waiting, and it sends nothing that needs an answer
meanwhile. A background thread would buy a session that answers heartbeats
during a build, at the price of two conversations on one socket and a report
that arrives unsolicited.

Between stages the recipe checks the shutdown flag the signal handler sets. A
guest that is being stopped abandons the remaining stages as `SKIPPED` and
answers, rather than holding systemd open for the rest of a kernel build.

## The host's part

`agent_session` sends `ApplyGpuRecipe` once per session, immediately after the
attach report and on the same conditions: an open, authenticated session that
agreed `CAPABILITY_GPU` and a VM that had a manifest to attach. A guest with no
GPU shares has nothing to build a module for.

The report is logged stage by stage and kept nowhere. A failed stage is a
warning in the host log, not an error that ends the session, and not a reason
to retry: the next session re-applies the recipe anyway, and a retry loop
around a kernel build is how a guest ends up compiling continuously.

## The payload layout this recipe expects

Fixing it here because the payload archive is packed outside this repository
and the recipe is what gives its contents meaning:

```
/opt/vmlord/gpu-payload/
├── sources.json                 provenance, and the target this was built for
├── licenses/…                   the license texts the catalog declares
└── content/dxgkrnl/
    ├── dkms.conf                PACKAGE_NAME, PACKAGE_VERSION, AUTOINSTALL=yes
    ├── Kbuild
    ├── dxgkrnl_compat.h         out-of-tree compat shims
    ├── *.c, *.h                 from microsoft/WSL2-Linux-Kernel
    └── include/uapi/misc/d3dkmthk.h
```

The production catalog stays empty after this task: filling it needs an archive
built with `cargo xtask gpu-payload pack` and published somewhere with its
digest, which is neither code nor a decision this task makes.

## Tests

The stages that touch the system cannot run under `cargo test` -- there is no
Ubuntu, no DKMS and no `/dev/dxg` behind it -- so, as in #94, everything that
decides is a pure function and those are what the tests drive:

* `/etc/os-release` parsing: quoted and unquoted values, a missing `ID`, a
  distribution with no recipe;
* the compatibility check: the right guest, a wrong release, a wrong
  architecture, and a kernel that differs from the payload's, which must be
  recorded and not refuse;
* `dkms.conf` parsing: name and version, a missing field, a value with quotes;
* the stage plan: a full run, a run that stops at the first failure with the
  rest reported as skipped, the already-satisfied short circuit, and a shutdown
  between two stages;
* the command runner: a program that succeeds, one that exits non-zero, one
  that outruns its budget and is killed, and output kept to its last lines;
* the wire: a report that survives the round trip, an apply on a session
  without `CAPABILITY_GPU` refused as unsupported, and a host that sends the
  request once per session and logs every stage.

`cargo test -p vmlord-agent`, `cargo agent`, `cargo test-windows` and
`cargo check-windows` are the final checks. The build itself is proven on a
real VM by hand, which is also what #97 will automate a probe for.
