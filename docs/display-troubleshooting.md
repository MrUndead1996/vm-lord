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

## Secure Boot and networking

The MVP module is unsigned. With Secure Boot enabled, the kernel can reject it
even when DKMS built successfully; disable Secure Boot for the VM. The display
session itself uses HvSocket rather than TCP/IP, but first provisioning,
dependency repair, and some kernel updates still require guest network access
to Ubuntu repositories.
