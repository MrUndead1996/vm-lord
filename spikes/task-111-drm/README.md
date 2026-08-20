# TASK-111 — what to run on a real VM, and what each answer decides

The spike asks whether VMLord needs a DRM kernel module of its own. Source
reading narrows the field to three candidates; only a running guest separates
them:

1. **the stock `hyperv_drm`** — VMLord VMs already carry a synthetic video
   device (`VideoMonitor`, 1024x768, `crates/platform/src/hcs_config.rs`), so
   this driver binds with nothing installed;
2. **in-tree `vkms`** — a virtual KMS device with a writeback connector, if
   Ubuntu ships it and if GNOME will bind to it;
3. **a VMLord module** — the AppSandbox `asb_drm` shape: one platform device,
   one CRTC, a VIRTUAL connector with a synthesized EDID, primary and cursor
   planes that move no pixels, shipped through DKMS.

Every candidate has to clear the same four bars, and the run below is
arranged so each bar is a separate, readable answer:

| Bar | Answered by | Why it is disqualifying |
|---|---|---|
| A card exists and udev tags it for `seat0` | `stock`, section *udev seat tagging* | logind hands a session no device it has not tagged, whatever the driver does |
| GDM's compositor binds it before login | `greeter`, sections *graphical session* and *which DRM device the greeter opened* | a display stack that needs a logged-in user cannot show a login screen |
| Modes up to 2560x1440 are accepted | `stock`, section *mode setting* | the desktop target for the epic is 1440p |
| The framebuffer is readable from outside the compositor | `greeter`, section *reading the greeter's framebuffer* | this is the whole capture path; nothing else in the stack matters if it fails |

## The VM

Create it in VMLord, elevated, as a throwaway — `desktop` installs GNOME onto
it and it is not worth keeping afterwards.

* Ubuntu **24.04** cloud image (the first proven target of epic #9).
* **GPU off.** VMLord's GPU payload stages its own Mesa under
  `/opt/vmlord/wsl-mesa` and puts it ahead of the distribution's, so a GPU VM
  answers a question about the WSL userspace, not about the display stack
  Ubuntu ships. The GPU combination is worth its own run afterwards -- the
  first one already showed it is not neutral -- but not as the baseline.
* **4096 MB RAM or more.** GNOME on llvmpipe on less than that swaps and
  every timing in the report becomes a measurement of the swap.
* **24 GB disk or more** — `ubuntu-desktop-minimal` is a few GB on top of a
  cloud image.
* **Networking on.** Unlike the GPU e2e tests, this one installs packages, so
  it needs a working `apt`.
* **SSH enabled**, deploy key. Everything below runs over SSH; the greeter
  stage deliberately looks at a machine nobody has logged into locally.

## Getting the two files into the guest

From an elevated PowerShell on the host, with `<VM>` the VM directory VMLord
created and `<ip>` the guest address:

```powershell
scp -i "<VM>\keys\id_ed25519" `
    \\wsl.localhost\Ubuntu-22.04\home\machi\vm-lord\spikes\task-111-drm\probe.sh `
    \\wsl.localhost\Ubuntu-22.04\home\machi\vm-lord\spikes\task-111-drm\plane_capture.c `
    dev@<ip>:~/
```

If `scp` cannot reach the guest, both files are plain text: open an SSH
session and paste each one through `cat > probe.sh <<'EOF' … EOF`.

## The run

```bash
sudo sh probe.sh stock      # ~3 min, needs apt
sudo sh probe.sh desktop    # ~10 min, installs GNOME, then reboots itself
# … the VM reboots. Reconnect over SSH. DO NOT log in at the graphical
#   console: the greeter is the subject of the next stage.
sudo sh probe.sh greeter    # ~1 min
sudo sh probe.sh pattern    # ~1 min, stops GDM briefly and restarts it
sudo sh probe.sh collect    # prints the path of a tarball
```

`pattern` is the control: it stops GDM, lets `modetest` paint a test pattern
of its own, and reads that back through the same code path. A blank capture
has two causes that no log distinguishes -- nothing can be read out of this
driver, or nothing was ever drawn into it -- and this stage is what tells them
apart. Bars in `pattern-*.ppm` mean the capture path is sound.

`desktop` reboots the machine out from under the SSH session — that is
expected, not a failure. `greeter` wants the machine sitting at GDM with
nobody logged in, because "a compositor runs before any user session" is
exactly the claim being tested; logging in first would answer a different and
easier question.

Then copy the tarball off the guest, plus any `.ppm` files in
`/var/log/vmlord-drm-spike/`:

```powershell
scp -i "<VM>\keys\id_ed25519" dev@<ip>:/tmp/vmlord-drm-spike-*.tar.gz .
```

The `.ppm` matters more than the logs. If one exists and shows the GDM
greeter, the capture path is proven on that driver — a picture of the login
screen taken by a process that is not the compositor is the finding this
spike exists to produce.

## If 1440p is refused

The `stock` stage sets modes with nothing else holding DRM master, so a
refusal there is the driver's own limit, and the likely cause is the
framebuffer the host reserved for a 1024x768 `VideoMonitor`. That is a VMLord
constant, not a Hyper-V law, so the follow-up experiment is one line:
raise `VIDEO_WIDTH`/`VIDEO_HEIGHT` in `crates/platform/src/hcs_config.rs`,
create a second VM, and run `stock` again. If the accepted modes follow the
constant, `hyperv_drm` stays a candidate at a fixed resolution chosen at
create time; if they do not, it is out, because dynamic resolution (#120) is
in the epic.

## What comes back to me

The tarball. The stage logs carry every command and its exit status, so a
stage that failed halfway is still worth sending — a refusal is an answer
here, and most of the disqualifying answers above arrive as one.
