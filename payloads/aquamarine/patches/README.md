# aquamarine

One patch, and the agent stage that applies it.

Aquamarine is the backend Hyprland draws through, and a Hyprland guest gets it
from the distribution's own repository -- `distros/arch.json` names `hyprland`
and pacman brings aquamarine in with it. Nothing here ships a build of it, which
is the difference between this directory and `payloads/ubuntu-26.04-amd64/mesa`:
that Mesa is built when the payload is packed and handed to a guest in an
archive, and this is built in the guest, against what the guest has.

What lives here is the change a Hyprland guest needs to draw at all: aquamarine
builds its renderer only through `EGL_PLATFORM_DEVICE_EXT`, which matches
nothing on a KMS-only device, and the gbm-platform overload it needs is already
written and never called. The patch header says the rest.

`display_aquamarine` in the agent is what applies it, as the recipe's
`CompositorRenderer` step: the patch is carried in the agent binary through
`include_str!` of the file beside this one, so a guest needs a checkout and
nothing else. Which guests reach that step is not a list of desktops -- it is
whether the compositor on the screen has an aquamarine mapped, which
`guest_platform` reads out of its `/proc` entry and a GNOME guest never has.

## What the stage does

The same thing by hand, which is how it was measured for #200:

```sh
git clone --depth 1 --branch v0.15.0 https://github.com/hyprwm/aquamarine.git
cd aquamarine
git apply .../0001-drm-fall-back-to-the-gbm-platform.patch
cmake -B build -G Ninja -DCMAKE_BUILD_TYPE=Release
ninja -C build
```

The build takes seconds -- 18 of them on eight cores -- and needs little a
Hyprland guest has not got: `base-devel` is installed for the display module's
DKMS build, every library aquamarine links is a dependency of the `hyprland`
package, and the stage installs `git`, `cmake` and `ninja` itself. What comes
out is staged at `/opt/vmlord/aquamarine` with a `built-from` file beside it
naming the version it was built from.

## Why it is built in the guest rather than shipped

Because what it has to be built against is whatever aquamarine the guest's own
distribution installed, and that is not knowable when a payload is packed. A
`.so` built against another version is not merely wrong, it is invisible: at a
soname bump the loader passes it over without a word and the guest goes black
again. So the version is read out of the guest on every run -- from
`libaquamarine.so.<version>` in the guest's own library directory, not from the
one the compositor is running, which may already be ours -- and a stamp that
disagrees is a rebuild.

The compositor is pointed at the staged directory by `LD_LIBRARY_PATH` in the
drop-in of the unit that starts it. There is one drop-in and not two, because
there is one `LD_LIBRARY_PATH` and systemd does not append to it: a second file
setting the same variable replaces every directory the first one named, and
whichever of the two lost that race would lose it silently. So
`compositor_drop_in` composes the whole value, and the staged aquamarine goes
first.

Sending the change upstream would remove all of this rather than manage it --
the overload is already there, and what is missing is a fallback nobody wired
up.
