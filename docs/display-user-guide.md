# Display user guide

## Create a desktop VM

1. Create an Ubuntu cloud-image VM with the `GNOME` desktop profile. Supported
   releases are listed in the [compatibility matrix](display-compatibility.md).
2. Give the VM network access to Ubuntu's package repositories for its first
   provisioning run. Two CPU cores and 4 GiB RAM are the recommended minimum.
3. Start the VM. VMLord installs GNOME, builds the `vmlord_drm` DKMS module,
   starts the guest display services, and reports their state in the VM list.
4. Wait until the display status says it is ready. Provisioning can take
   several minutes on the first boot; the VM remains usable through COM1 or
   SSH while display setup is degraded.

## Open and use the display

Select the running VM and press **Connect**. VMLord launches
`vmlord-display.exe`; pressing Connect again focuses the existing window. The
viewer authenticates the guest over the per-VM secret, then binds separate
control, frame, input, and clipboard HvSocket channels.

- Sign in through GDM with the account created for the VM.
- Resize the window to request a matching guest mode. Modes are limited to one
  monitor and at most 2560x1440.
- Close the window to end capture. Closing or crashing the viewer does not stop
  the VM, and opening it again starts a fresh authenticated session.
- A transient channel or guest-service restart reconnects automatically. A VM
  reset keeps the window waiting until the guest services return.

## The clipboard

Copying in the guest and pasting on the host works in both directions, for
text, HTML and images.

- It follows the window. A selection crosses only while the display window has
  keyboard focus, so a VM in the background neither reads what you copy
  elsewhere nor replaces what is on your clipboard. What you copied while the
  window was unfocused is offered to the guest when you come back to it.
- It needs a signed-in desktop. The clipboard lives inside GNOME, so nothing
  crosses at the GDM login screen; it starts working once you have logged in.
- Nothing crosses until it is pasted. Each side announces what it has and sends
  the contents only when the other asks for them.
- Text and HTML are limited to 8 MiB and a picture to 32 MiB. A larger
  selection is refused without disturbing the session.
- Files and folders are carried too, in both directions, with the same focus
  rule: copy in one, paste in the other. A folder brings everything inside it.
- Only ordinary files and folders cross. Shortcuts, symbolic links, junctions,
  pipes and devices are refused, and refusing one cancels that copy rather than
  pasting part of a tree.
- A file may be up to 1 GB and one copy up to 4 GB by default. Change either in
  `settings.toml`:

  ```toml
  [clipboard_files]
  max_file_size = "1GB"
  max_transfer_size = "4GB"
  retention = "24h"
  ```

  Sizes take `B`, `KB`, `MB` or `GB` and are binary multiples; `retention`
  takes `s`, `m` or `h`.
- Pasted files are copies. They are written into a private folder of your own
  before they appear on the clipboard, and on Windows they are kept for
  `retention` -- a day by default -- so a paste still works after the window is
  closed. Move what you want to keep somewhere of your own.
- A copy that is cancelled, times out, or is interrupted by the window losing
  focus leaves nothing half-written behind.

Display traffic does not traverse the VM's IP network. Removing network access
after provisioning does not disconnect an existing display, but future package
or DKMS repairs may need Ubuntu's repositories again.

## Updating the guest display payload

When VMLord reports a newer display payload, update it while the VM is running.
The guest verifies the new module and services before accepting them. If the
new version fails verification, it rolls back to the previous working version
and reports that result instead of silently leaving the desktop unusable.

## Logs and diagnostics

Application and viewer logs are written to the configured VMLord log file
(by default `%LOCALAPPDATA%\VMLord\logs\vmlord.log`). The VM's display status
is the first diagnostic to read: it distinguishes provisioning, payload,
module, device, service, and connection failures. See
[display troubleshooting](display-troubleshooting.md) for the corresponding
checks.
