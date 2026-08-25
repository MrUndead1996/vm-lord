# Display compatibility

VMLord's supported desktop path is the native display stack: `vmlord_drm` in
the guest, the display broker and session services, HvSocket transport, and
`vmlord-display.exe` on the Windows host. AppSandbox IDD is not a fallback.

## Supported matrix

| Guest | Architecture | Desktop session | 1920x1080 | 2560x1440 resize | Reconnect | Evidence |
| --- | --- | --- | --- | --- | --- | --- |
| Ubuntu 22.04 LTS cloud image | amd64 | GDM and GNOME on Wayland | yes | yes | yes | parity gate #128 passed |
| Ubuntu 24.04 LTS cloud image | amd64 | GDM and GNOME on Wayland | yes | yes | yes | parity gate #128 passed |
| Ubuntu 26.04 LTS cloud image | amd64 | GDM and GNOME on Wayland | yes | yes | yes | parity gate #128 passed |

The acceptance record is Vikunja task **#128, "Проверить native display
parity"**, completed at **2026-08-24 15:14:44 UTC** against source revision
`86f748d9054c877f3afd1a064b55e34c7f58d488`. It covers GDM login, keyboard and
mouse input, viewer close and reopen, VM reset, viewer crash isolation, guest
service restart, kernel update with DKMS rebuild, and degraded diagnostics for
a broken payload on every row above. Its performance gates were at least 30
FPS, p95 input-to-display latency at most 100 ms, at most 2% of one guest core
while static and 25% during ordinary work, first image within 2 seconds of
readiness, reconnect within 3 seconds, guest display memory at most 128 MiB and
viewer memory at most 256 MiB at 1440p.

The task record does not pin one guest kernel or display-payload semantic
version: kernel update and DKMS rebuild are themselves matrix cases, and a
release selects the payload matching each Ubuntu release. Compatibility is
therefore stated at the Ubuntu release boundary, not as a promise for one
frozen kernel build. The release artifact retained with #128 is the authority
for the reference host hardware and raw measurements; this repository records
the supported product boundary and the source revision that passed the gate.

## Host and VM requirements

- Windows x64 with Hyper-V and Host Compute Service available. VMLord must run
  elevated.
- A VM created by the native HCS backend with the `GNOME` desktop profile.
  Existing AppSandbox VMs and `Headless` VMs do not acquire this display stack.
- At least 2 virtual CPU cores and 4 GiB RAM are recommended for GNOME.
- One monitor, 640x480 through 2560x1440. Multi-monitor is not part of the MVP.
- GPU-PV is optional. The display transport and its DRM output do not require
  GPU-PV.

## Deliberate limitations

- GNOME on Wayland is the supported compositor/session. Xorg and other desktop
  environments are not in the compatibility matrix.
- Secure Boot must be disabled. The guest DRM module is installed through DKMS
  and is not signed for Secure Boot; signing is tracked separately.
- The first desktop provisioning and DKMS build need guest access to Ubuntu's
  package repositories. After a successful installation, display frames and
  input use HvSocket and do not depend on the VM's IP network.
- The clipboard carries text, HTML and images in both directions, and needs a
  logged-in GNOME session: it is driven through the compositor, so nothing
  crosses at the GDM login screen or on a guest where nobody has signed in.
- File transfer is not part of the clipboard. Copied files are never offered
  and an offer of them is ignored.
- Audio, multi-monitor, Motion codec, and zero-copy capture are not part of the
  MVP display contract.

See [the display user guide](display-user-guide.md) to start a session and
[display troubleshooting](display-troubleshooting.md) when Connect is not
available or the viewer cannot reach Running.
