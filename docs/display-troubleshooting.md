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

## Nothing pastes between the host and the guest

The clipboard is a fourth channel, and it needs a graphical session -- not just
a running VM.

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

## Secure Boot and networking

The MVP module is unsigned. With Secure Boot enabled, the kernel can reject it
even when DKMS built successfully; disable Secure Boot for the VM. The display
session itself uses HvSocket rather than TCP/IP, but first provisioning,
dependency repair, and some kernel updates still require guest network access
to Ubuntu repositories.
