# Display troubleshooting

Start with the display status shown for the VM and the VMLord log (by default
`%LOCALAPPDATA%\VMLord\logs\vmlord.log`). AppSandbox IDD is not a fallback:
setting `VMLORD_BACKEND=legacy` cannot open a display.

## Connect is disabled

| Status or symptom | Meaning | Action |
| --- | --- | --- |
| VM was created without a desktop | The profile is `Headless`. | Create a native-backend VM with the `GNOME` profile. Post-create conversion is not part of the MVP. |
| Desktop is still being installed | cloud-init and the guest recipe have not completed. | Keep the VM running; check COM1, guest network access, DNS, and Ubuntu repository availability. |
| Guest has not reported display services | The agent has not completed the display recipe in this run. | Wait for guest readiness; if it persists, check the agent and guest service journal. |
| VM is stopped | HvSocket services exist only for a running partition. | Start the VM and wait for display readiness. |
| Legacy backend cannot open the display | The retired AppSandbox IDD path was requested. | Unset `VMLORD_BACKEND` and use a VM created by the native HCS backend. |

The corresponding stable status codes are `display-profile-headless`,
`display-vm-not-running`, `display-provisioning-pending` and
`display-guest-pending`.

## Desktop provisioning failures

| Diagnostic code | Meaning | Checks |
| --- | --- | --- |
| `display-package-download-failed` | GNOME packages could not be downloaded. | Verify the VM has NAT connectivity, DNS works, its clock is correct, and `apt update` can reach the configured Ubuntu repositories. |
| `display-package-install-failed` | Packages downloaded but installation failed. | Inspect `/var/log/apt/term.log`, `dpkg --audit`, free disk space, and any interrupted package transaction before retrying. |
| `display-provisioning-timeout` | Desktop installation exceeded the host readiness timeout. | Check cloud-init and apt progress over COM1 or SSH; repair a stalled package operation or increase the configured guest-readiness timeout for a slow mirror. |
| `display-profile-unsupported` | The selected guest/release cannot satisfy the GNOME profile. | Use one of the Ubuntu amd64 releases in the compatibility matrix; retrying the same unsupported profile will not change the result. |
| `display-guest-services-failed` | The payload was installed but its broker/session services did not become ready. | Run the service status and journal commands below, verify the service account and `/run/vmlord/display-broker.sock`, then restart the failed unit. |

## Payload, module, and device failures

| Diagnostic code | Meaning | Checks |
| --- | --- | --- |
| `display-payload-missing` | This release has no payload matching the guest. | Verify Ubuntu release and amd64 architecture, and that the release includes its display payload. |
| `display-payload-invalid` | The archive, manifest, digest, or services do not agree. | Replace the release payload; do not edit an unpacked archive in place. |
| `display-payload-dependencies-failed` | Ubuntu could not install DKMS, build tools, or kernel headers. | Restore guest network/DNS access and verify `apt` can reach Ubuntu repositories. |
| `display-payload-build-failed` | DKMS could not compile the module for the running kernel. | Inspect `dkms status`, the DKMS build log, and installed `linux-headers-$(uname -r)`. |
| `display-payload-module-not-loaded` | The built module did not load. | Inspect `dmesg`; confirm Secure Boot and kernel lockdown are disabled. |
| `display-payload-module-signature-rejected` | The module built and the kernel refused its signature. | Enroll the VM's certificate: it is written to `display/mok.der` beside the VM's state, and `mokutil --import` stages it for MokManager on the next boot. |
| `display-payload-no-device` | `vmlord_drm` loaded but no usable DRM card appeared. | Inspect `dmesg`, `ls -l /dev/dri`, and the broker journal. |
| `display-payload-update-rolled-back` | The update failed but the previous version was restored. | Keep using the VM and preserve both update and DKMS logs for the failed version. |
| `display-payload-update-failed` | Neither the new nor previous payload became usable. | Repair dependencies/module loading, then retry with a known-good release payload. |

Useful guest commands:

```sh
systemctl status vmlord-display-broker.service vmlord-display-session.service
journalctl -u vmlord-display-broker.service -u vmlord-display-session.service -b
dkms status
ls -l /dev/dri
dmesg | grep -E 'vmlord_drm|Lockdown|verification'
```

## Viewer waits, reconnects, or shows no frame

- Confirm both guest services are active and
  `/run/vmlord/display-broker.sock` exists.
- A `Waiting` state after reset is normal until the guest starts. Persistent
  `Authenticating` indicates a control-handshake or version mismatch; keep the
  host and display payload from the same release and inspect both host and guest
  logs.
- A frame or input channel may reconnect without ending the session. Persistent
  reconnects usually indicate a crashing guest service; use `journalctl` above.
- If GDM is present but no frame is produced, inspect `vmlord_drm` in `dmesg`
  and confirm GNOME is running on Wayland. Xorg and other compositors are not in
  the supported matrix.
- Keyboard or pointer state is released when the input channel drops. Reopen
  the viewer if focus/input does not recover after the channel has rebound.

## Clicks land away from where they were aimed

The guest lit its own Hyper-V display beside VMLord's, and the pointer is
mapped across both. The desktop is wider than the window shows, so a click is
delivered to the right of where it was made.

- Check which of the two ways of hiding that display this guest was given. It
  follows the desktop the agent found: a GNOME guest gets
  `/etc/udev/rules.d/62-vmlord-display.rules`, which tags the card
  `mutter-device-ignore`, and any other desktop gets
  `vmlord-display-unbind-hyperv.service`, which unbinds its driver instead.
  Exactly one is installed; the `MODULE_LOAD` stage's line in the display
  status says which.

  ```bash
  ls /etc/udev/rules.d/62-vmlord-display.rules
  systemctl status vmlord-display-unbind-hyperv.service
  ls /sys/class/drm
  ```

- A guest that has neither has no `vmlord_drm` device: both mechanisms ask for
  `/sys/devices/platform/vmlord_drm.0/drm` first, so that a module that never
  built leaves the guest a desktop on the console rather than none at all. Fix
  the build and the mechanism follows.
- A guest whose desktop was replaced after it was created gets the other
  mechanism on the next display recipe, and the one it no longer needs is
  removed. Restarting the VM is what runs it.

## Nothing pastes between the host and the guest

The clipboard is a channel of its own, and it needs a graphical session -- not
just a running VM.

- Sign in through GDM first. Nothing crosses at the login screen, because the
  clipboard belongs to the compositor and the daemon that reaches it starts
  with the user's session.
- Check the daemon inside the guest:

  ```bash
  systemctl --user status vmlord-display-clipboard.service
  journalctl --user -u vmlord-display-clipboard -b
  ```

  Its journal carries mime types, byte counts and outcomes, and never the
  contents of a selection.
- Give the display window focus. A selection crosses only while the window has
  the keyboard, so a paste attempted from a background window has nothing to
  paste.
- Check the size. Text and HTML stop at 8 MiB and a picture at 32 MiB; a larger
  selection is cancelled and the journal says which kind it was.
- Unlock the guest's screen. While it is locked the compositor refuses to hand
  out a session at all -- the journal says `Session creation inhibited` -- and
  the daemon retries until the screen comes back.
- Copy again. The daemon learns about a selection from the change, not from the
  selection itself, so one made before it attached (or before a lock) is
  invisible to it until the next copy.
- Copied files paste nothing, by design.
- If the daemon is running and the window is focused but nothing crosses, the
  broker may have refused it: the clipboard socket is served only to the uid of
  the active graphical session on `seat0`, so a second user signed in over SSH
  cannot take it. The broker's journal names the uid it refused.

## The guest has no sound

Sound is a channel of its own, served by a system daemon that needs no login.

- Check the daemon inside the guest:

  ```bash
  systemctl status vmlord-display-audio.service
  journalctl -u vmlord-display-audio -b
  ```

  Its journal carries the format, frame counts, stream positions and outcomes,
  and never a sample.
- Check that the loopback is there. `cat /proc/asound/cards` should list
  `Loopback`; if it does not, `modprobe snd-aloop` and look at whether
  `/etc/modules-load.d/vmlord-audio.conf` survived. The module ships with every
  supported release's kernel, so it is not something to install.
- A journal line about the loopback not opening usually means something else
  holds it, or the daemon is not in the `audio` group -- `systemctl show
  vmlord-display-audio -p SupplementaryGroups` says whether the unit asks for
  it.
- Check the guest's own output device. GNOME should show one output, named
  **VMLord audio**. If it shows *Dummy Output* instead, the desktop has no
  device at all: `/etc/pipewire/pipewire.conf.d/51-vmlord-audio.conf` is
  missing or was not read. PipeWire's ALSA monitor refuses the whole loopback
  card while the daemon holds one of its devices, which is why the output is a
  statically declared node rather than a discovered card.
- Check that the viewer is not muted: **Mute audio** in the window's system
  menu, which is remembered per VM between sessions.
- Check the host's output. If the viewer's log says the host has no audio
  output, the session is fine and Windows has no working endpoint; the sound
  starts as soon as one appears.
- An idle desktop sends nothing at all, so silence with nothing playing is the
  ordinary state rather than a fault.

## The tray icon is not in the guest's panel

The tray is a user service of the graphical session, like the clipboard
daemon, and it shows through GNOME's AppIndicator extension.

- Check the service inside the guest:

  ```bash
  systemctl --user status vmlord-display-tray.service
  journalctl --user -u vmlord-display-tray -b
  ```

  Its journal carries every click it forwarded and every answer it could not
  deliver; a missing broker or a missing watcher is waited through, not an
  error that ends it.
- Check that an AppIndicator extension is installed and enabled. On the
  supported desktops Ubuntu's own ships with the desktop; if neither is
  there, install `gnome-shell-extension-appindicator` and sign out and in
  once -- the tray asks the running shell to enable it when it starts, and
  again on every reconnect, usually the next **Restart services**. On a
  desktop that is not GNOME nothing is installed and nothing is enabled: the
  tray needs only something on the session bus owning
  `org.kde.StatusNotifierWatcher`, which a panel that shows tray icons
  already does. Its journal says when it found neither a host nor a shell to
  ask.
- Check the broker socket. The menu still builds with no broker attached,
  but every click is dropped until the attach returns; the journal says
  when it does.

## Secure Boot

The module is signed. Each guest generates its own MOK at
`/var/lib/shim-signed/mok/`, DKMS signs every build with it -- including the
rebuilds an unattended kernel upgrade triggers, with VMLord closed -- and
VMLord copies the certificate to `display/mok.der` beside the VM's state.

What is not done is the enrollment. `MokList` is written by MokManager alone,
from the firmware console, and VMLord's VMs have none: they are created
straight through HCS and do not appear in Hyper-V Manager. Secure Boot must
therefore stay off for a VMLord VM. With it on and the certificate not
enrolled, the display is `Degraded` with
`display-payload-module-signature-rejected`, and the VM itself keeps running.

## Networking

The display session uses HvSocket rather than TCP/IP, but first provisioning,
dependency repair, and some kernel updates still require guest network access
to Ubuntu repositories.
