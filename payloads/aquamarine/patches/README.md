# aquamarine

One patch, and no build behind it yet.

Aquamarine is the backend Hyprland draws through, and a Hyprland guest gets it
from the distribution's own repository -- `distros/arch.json` names `hyprland`
and pacman brings aquamarine in with it. Nothing here fetches it, builds it or
ships it, which is the difference between this directory and
`payloads/ubuntu-26.04-amd64/mesa`, where the patches beside the build that
applies them are part of an archive a guest is handed.

What lives here is the change a Hyprland guest needs to draw at all, kept in the
tree so that the reason is written down and the diff does not have to be
rediscovered: aquamarine builds its renderer only through
`EGL_PLATFORM_DEVICE_EXT`, which matches nothing on a KMS-only device, and the
gbm-platform overload it needs is already written and never called. The patch
header says the rest.

## How it was applied for #200

By hand, on the guest, to measure that it works:

```sh
git clone --depth 1 --branch v0.15.0 https://github.com/hyprwm/aquamarine.git
cd aquamarine
git apply .../0001-drm-fall-back-to-the-gbm-platform.patch
cmake -B build -G Ninja -DCMAKE_BUILD_TYPE=Release
ninja -C build
```

The build takes seconds and needs nothing a Hyprland guest does not already
have: `base-devel` is installed for the display module's DKMS build, and every
library aquamarine links is a dependency of the `hyprland` package. The result
is `libaquamarine.so.14`, the same soname the packaged one carries, so the
compositor takes it through `LD_LIBRARY_PATH` in a drop-in of the unit that
starts it -- the mechanism `vmlord-display-compositor-mesa.conf` documents, and
for the same reason.

## Why that is not the shipping answer

A `.so` put beside a packaged one goes stale the moment the distribution updates
`hyprland` and `aquamarine` together: at a soname bump the override stops being
loaded, silently, and the guest is back to a black screen. Whatever carries this
into a guest has to be built against the aquamarine the guest actually has,
which means building at provisioning time or not at all.

Sending it upstream removes the problem rather than managing it -- the overload
is already there, so the change is a fallback nobody has wired up -- and until
one of the two happens, a Hyprland guest is not fixed by this repository alone.
