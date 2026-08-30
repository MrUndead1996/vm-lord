# Importing an AppSandbox Linux VM

VMLord can copy a finished AppSandbox Linux VM and turn the copy into a VMLord
VM. This is a one-way import, not a migration: the AppSandbox VM is only ever
read, and it is still AppSandbox's when the import is over.

## What the import promises about the source

The source VM is **never** moved, renamed, linked, adopted, started, stopped,
deleted or modified. VMLord reads three things from AppSandbox storage:

* `vms.cfg`, to learn what VMs exist and how each was configured;
* the VM's `disk.vhdx`, which is copied out;
* the AppSandbox private key, which is *named* but never read by VMLord and
  never copied into VMLord storage. It is handed to `ssh.exe` as a path, so no
  private key material passes through an argument, a log or the import journal.

After a successful import you have two independent VMs: the AppSandbox one,
untouched, and a VMLord one with its own disk, its own key pair and its own
agent secret.

## What can be imported

A discovered VM is importable only when all of the following hold. Anything
else is listed with the reason it was refused, so a VM that is missing from the
list is a VM VMLord could not see at all.

| Requirement | Why |
|---|---|
| `OsType` is Linux | Windows guests are out of scope. |
| It is not a template | A template is not a finished VM. |
| `InstallComplete=1` | An unfinished installation has nothing to convert. |
| It is not running | A disk being written to cannot be copied consistently. |
| `SshEnabled=1` | The conversion talks to the guest over SSH and has no other way in. |
| `SshDeployKey=1` | Without a deployed key there is no credential to connect with. |
| Its disk file exists and is the one its configuration names | A mismatch means the configuration describes something else. |
| Its network and GPU modes are ones VMLord can reproduce | An imported VM must be a VM VMLord can actually run. |
| Its SSH port is a usable port | |
| No other AppSandbox VM claims the same disk | Two VMs sharing one disk cannot both be copied safely. |

Hyper-V VMs, Windows guests, AppSandbox templates and exporting a VMLord VM
back to AppSandbox are all out of scope.

## Guest prerequisites

The conversion runs as the guest's own administrative user over SSH and
escalates with `sudo -n`. Two things must therefore be true inside the guest
before an import will get past its first step, and both are checked and named
explicitly before anything is uploaded:

* **Passwordless sudo** for that user. AppSandbox creates its admin user as a
  *password* sudoer and writes no `NOPASSWD` drop-in, so on a stock AppSandbox
  guest this has to be arranged first. VMLord has no credential that would let
  it do better, and driving AppSandbox's own root agent is deliberately not
  done: the import disables AppSandbox's components, it does not use them.
* **`python3`** on the guest's `PATH`. The conversion is one fixed Python
  program, uploaded with a manifest and checked by digest before root runs it.

## Capacity

Before starting, make sure the VMLord storage volume has at least the size of
the source `disk.vhdx` **plus about 10 GiB** of working headroom. The copy is
written into VMLord's own staging directory and promoted into the VM directory
only once it is complete, so the peak requirement is the copy itself plus room
for the VM's other files.

## What an import does

1. **Validating** -- the source is re-read and re-checked. The choice you made
   in the dialog may be minutes old, and a VM that started running since is one
   VMLord must not copy.
2. **Copying** -- `disk.vhdx` is copied into VMLord staging, with progress and
   cancellation. The source is opened for reading only.
3. **Creating** -- the copy is promoted into the VM directory and a compute
   system is built around it.
4. **Starting the copied guest** -- the first boot runs on NAT with SSH and
   with VMLord's GPU and display integration switched off. Nothing is asked of
   the guest that a plain Linux system cannot answer.
5. **Converting** -- over that one SSH session, VMLord deploys its own key,
   installs the VMLord agent and its unit, disables AppSandbox's units, proves
   every replacement is in place, removes what is now obsolete, and asks the
   guest to shut down. Every step is checked on every pass, so a resumed
   conversion re-verifies what it skips rather than trusting it.
6. **Restarting** -- the guest boots again, this time as an ordinary VMLord VM
   with the agent, the display share and the GPU share the import asked for.
7. **Verifying** -- SSH answers with VMLord's key, the agent unit is active,
   and the display and GPU shares are mounted where the agent puts them.
8. **Finished** -- ordinary VM metadata is written last, and the recovery
   journal is removed only after that write is durable.

The payloads a converted guest uses are **not** unpacked into its disk. They
reach it exactly the way a created VM's do: from Plan9 shares the host offers
on the second boot. The conversion records what the guest reported itself to be
so the host can pick the right ones.

## Stopping an import

What stopping costs depends on how far it got, and the dialog says which it is:

* **Before the copied guest is changed** (validating, copying, creating,
  starting the first boot) -- everything VMLord made is removed. The source is
  never part of that.
* **Once conversion may have run a command** -- the copy is kept. A guest that
  was half converted is not one to silently throw away, so the import becomes
  an *unfinished import* waiting for you.

## Unfinished imports

An import that failed after the guest was touched, or that was interrupted by
VMLord closing, is retained on disk and reported at the next start. It appears
in the import dialog under **Unfinished imports**, never in the VM list: an
import that nothing verified must not be shown as a healthy VM.

* **Retry** carries on from what the journal already confirms. The disk is not
  copied twice, the compute system is rebuilt around the copy that is already
  there, and the conversion re-checks every step it skips. A retry never rolls
  back -- a failure leaves the import retained exactly as it found it.
* **Discard** removes everything VMLord made for it: the compute system, its
  metadata, the VM directory and the journal. It removes nothing of
  AppSandbox's, and it refuses outright if the recorded destination is not one
  exact VMLord VM directory.

A retry taken after VMLord was restarted has no discovery behind it. If the
copy is already in place, that does not matter; if the import still needs the
source, it says so and asks you to look for AppSandbox VMs again first.
