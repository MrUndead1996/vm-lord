# VMLord

> **A native Windows workspace manager for Linux virtual machines, built on the Windows Host Compute System (HCS).**

VMLord is a Windows-native application for creating, managing and using Linux virtual machines as persistent development workspaces.

The project started as a fork and redesign of **AppSandbox**, with the goal of building a Windows-first application focused on long-lived Linux desktops rather than cross-platform sandboxing.

Instead of exposing virtualization internals, VMLord provides a modern desktop experience powered by the Windows virtualization stack.

## Why?

Windows already includes a powerful virtualization platform, but managing Linux desktops on it still requires multiple disconnected tools and a significant amount of manual configuration.

VMLord aims to provide a cohesive experience where a Linux virtual machine behaves like a natural extension of the Windows desktop.

## Built on HCS

VMLord is built around the **Windows Host Compute System (HCS)** API.

Rather than relying on Hyper-V Manager, WMI or PowerShell, it communicates directly with the Windows virtualization platform through HCS, allowing efficient management of virtual machines and their lifecycle.

Other Windows technologies are integrated where appropriate, including:

* Host Compute System (HCS)
* Host Networking Service (HNS)
* Hyper-V Sockets
* GPU Partitioning (GPU-PV)
* Enhanced Session
* Remote Desktop Services

## Vision

A Linux VM should feel like a second desktop, not a remote machine.

VMLord is designed for developers who spend hours every day inside a Linux environment while continuing to use Windows as the host operating system.

The VM is expected to be persistent, customized and used as a primary development workspace.

## Planned Features

* Native Windows desktop application
* Persistent Linux workspaces
* GPU acceleration (GPU-PV)
* Enhanced Session integration
* Dynamic display resolution
* Multi-monitor support
* Audio and clipboard integration
* SSH terminal
* Snapshot management
* Performance monitoring
* Diagnostics
* Workspace provisioning

## Architecture

```text
vmlord-core
    HCS
    HNS
    GPU
    Display
    Networking
    SSH
    Diagnostics

vmlord-ui
    Native Windows GUI

vmlord-cli
    Command Line Interface

vmlord-api
    Automation API
```

The core is completely separated from the user interface, allowing desktop, CLI and future automation tools to share the same backend.

## Technology

* Rust
* windows-rs
* Tokio
* Serde
* Tracing
* egui *(planned)*

## Relationship to AppSandbox

VMLord is heavily inspired by **AppSandbox** and reuses ideas from that project.

The long-term goal, however, is different.

While AppSandbox focuses on isolated application environments across multiple platforms, VMLord focuses exclusively on Windows and on providing the best possible Linux desktop experience using the native Windows virtualization APIs.

No AppSandbox code or binary is part of VMLord any more: the C backend VMLord
booted from was replaced module by module and removed from the distribution.
AppSandbox is MIT-licensed, © 2026 James Stringer, and is credited here for the
ideas and the recorded Windows behaviour VMLord kept from it.

## Status

🚧 Early development. The Windows x64 application runs on the native HCS
backend -- the only backend -- and lists persisted VMs in an egui desktop
shell.

## Building

Every build names its target, because the repository produces a Windows
application and a Linux guest agent. The commands are Cargo aliases, defined in
`.cargo/config.toml`:

| Command | Does | Runs on |
| --- | --- | --- |
| `cargo build -p vmlord` | the application, for the host toolchain | Windows |
| `cargo agent` / `cargo agent-release` | the guest agent, `x86_64-unknown-linux-musl` | Windows, Linux |
| `cargo display-services` | the guest display services, `x86_64-unknown-linux-musl` | Windows, Linux |
| `cargo check-windows` | compile-checks the application and the display viewer through `x86_64-pc-windows-gnu` | WSL |
| `cargo test-windows` | builds and runs the Windows tests, the display viewer's included | WSL |
| `cargo dist` | release build of everything -- `vmlord.exe`, `vmlord-com1.exe`, `vmlord-display.exe` and the agent -- collected into `target/dist/` | Windows |
| `cargo gpu-payload pack ...` | release tooling that packs a prepared GPU payload | Windows, Linux |
| `cargo release-manifest ...` | writes `release-manifest.json` from a finished installer's own bytes | Windows, Linux |

Prerequisites:

* Windows: the MSVC toolchain, and `rustup target add x86_64-unknown-linux-musl`
  for the agent. Nothing else -- the agent links with `rust-lld` and needs no C
  cross-compiler.
* WSL: `rustup target add x86_64-pc-windows-gnu` and the MinGW-w64 linker
  (`x86_64-w64-mingw32-gcc`, from `gcc-mingw-w64-x86-64` on Debian and Ubuntu).
  Windows test binaries then run directly, through WSL's interop -- no Wine.

`cargo dist` is Windows-only: release executables must come from MSVC. It also
generates `THIRD-PARTY-LICENSES.txt` from the resolved dependency graph, which
needs the pinned tool:

```powershell
cargo install --locked --features cli cargo-about@0.9.2
```

`--features cli` is not optional: from 0.9 the binary is behind that feature,
and without it the install builds the library, installs nothing, and still
succeeds.

`about.toml` lists the licences the [dependency audit](docs/dependency-licenses.md)
accepted. A dependency arriving under anything else fails rather than being
copied into the notices unread -- on the pull request that introduces it, not
on the tag that would have shipped it.

VMLord's own crates are excluded from the notices: `LICENSE` covers them, and a
file that opens "the work of other authors" should not list them. That is what
`private = { ignore = true }` does, and it works because every workspace crate
declares `publish = false` -- none of them is on crates.io, and saying so is
also what stops one being published by accident.

See **ARCHITECTURE.md** for why each target was chosen.

## Packaging

The installer is [Inno Setup](https://jrsoftware.org/isinfo.php) 6.6 and is
declarative packaging only: it places files, offers shortcuts and registers an
uninstaller. Settings, distribution profiles and updates belong to the
application. From the repository root on Windows:

```powershell
cargo dist --gpu-payload <dir> --display-payload <dir>
powershell -File installer\check.ps1 target\dist
iscc installer\vmlord.iss
cargo release-manifest --tag v0.1.0 `
    --installer target\installer\VMLord-0.1.0-x86_64-setup.exe `
    --output target\installer\release-manifest.json
```

`check.ps1` runs first because the installer copies whatever was staged: a
binary that failed to build would otherwise ship as a file missing from
Program Files rather than as a build error. It also reads the script back to
confirm both installation modes are still offered.

The setup program installs into `{autopf}\VMLord` -- Program Files when the
user chooses an all-users installation and elevates, their own Programs
directory when they do not. Uninstalling removes only what was installed;
`%LOCALAPPDATA%\VMLord`, which holds settings, VMs and images, is left alone.

There is no code-signing certificate. The SHA-256 in `release-manifest.json` is
an integrity check that the downloaded installer is the published one; it is
not publisher authentication, and Windows still asks before running it.

## Releasing

Source lives on GitHub at
[MrUndead1996/vm-lord](https://github.com/MrUndead1996/vm-lord), which is where
pull requests and releases happen, and is mirrored to Forgejo at
`https://git.mrundead.org/mrundead/vm-lord`.

Every pull request runs `.github/workflows/ci.yml`: the workspace tests and the
build automation's on Windows against MSVC, the two guest programs
cross-compiled to musl, the third-party licence notices, `cargo fmt --all
--check`, and the workflow validator.

`cargo run -p xtask -- workflow-check` reads the workflows back as data before
they can surprise anyone. Every workflow must have `contents: read` default
permissions and pin every action to a commit SHA rather than to a movable tag;
the release must additionally run on `v*` alone, with only the `release` job
able to write.

**Cutting a release.** Everything is driven by the tag, and the tag has to
agree with `Cargo.toml`:

```sh
git tag v0.1.0 && git push origin v0.1.0
```

`.github/workflows/release.yml` then checks that the tag is reachable from
`main` and matches the workspace version, runs the tests, builds the
distribution, checks the staging, compiles the installer, writes
`release-manifest.json`, recomputes its SHA-256 with a second tool, and creates
a **draft** release carrying the installer, the manifest, `SHA256SUMS.txt`, the
third-party notices and a source archive.

The draft is never published automatically. Download the installer, confirm its
SHA-256 equals the manifest's, and install it in both scope modes before
publishing. Until it is published, no VMLord in the world can discover it: the
update check reads the latest published release.

A release built by CI carries no GPU or display payload. Not because they
cannot be built there -- `./rebuild_payload.sh` and
`payloads/ubuntu-26.04-amd64/prepare.sh` reproduce both from sources pinned by
commit -- but because nothing in the workflow builds them yet: they need Docker
and a Linux runner, so it would take a second job and an artifact handed to the
Windows one. Until then, build them locally and pass them to `cargo dist`.
VMLord runs without them, with no GPU-PV and no guest display.

**The mirror.** Forgejo pulls; GitHub does not push. The mirror is configured
on the Forgejo repository itself (Settings -> Mirror Settings) as a pull mirror
of `https://github.com/MrUndead1996/vm-lord.git`, and it runs on Forgejo's own
schedule.

Pulling rather than pushing is what the network allows -- Forgejo's SSH is not
reachable from GitHub's runners -- but it is also the better shape. No port is
exposed to the internet for it, GitHub holds no credential that can write to
Forgejo, and the copy has no way to write back to the thing it copies. The
price is that the mirror lags by up to one interval rather than updating with
the push.

Forgejo force-updates its own refs when it pulls, which is correct here: the
mirror is a copy, and GitHub is the history it copies. Nothing should ever be
committed to the mirror directly.

**What a user is trusting.** There is no Authenticode certificate, so
SmartScreen warns about the installer and will keep warning until enough people
have run it. What the release does offer is that the bytes are the published
ones: the manifest's hash is generated from the installer's own bytes, checked
again independently, published beside it, and verified by VMLord before it
launches anything. A user who prefers not to rely on the in-application update
can download the installer from the releases page and compare the hash
themselves.

## Running

Build with `cargo build -p vmlord`, then launch the elevated executable with:

```powershell
Start-Process -FilePath .\target\debug\vmlord.exe -Verb RunAs -Wait
```

The HCS backend requires elevation, so `cargo run` cannot launch the UAC-marked
executable directly. The build stages nothing beside the executable: VMLord
ships no third-party runtime.

The shell targets `x86_64-pc-windows-msvc` and creates Linux workspaces either
from a local ISO, which boots to the distribution's own installer, or from an
Ubuntu cloud image, which needs no one at the keyboard: there the image is
downloaded and imported into a VHDX, and the guest is provisioned
by cloud-init from a NoCloud seed VMLord writes itself, with COM1 available as a
diagnostic console.

GPU-PV runs on the native backend: a VM asks for `None`, `Default` or `Mirror`,
the host attaches what it can and exports the driver package and Linux
userspace beside it, and the guest agent brings the device up and reports what
it renders on. It is applied best effort and never decides whether a VM starts.
See **[docs/gpu-pv-compatibility.md](docs/gpu-pv-compatibility.md)** for what a
host, a guest and a payload must be, and
**[docs/gpu-pv-troubleshooting.md](docs/gpu-pv-troubleshooting.md)** for
reading a GPU status that is not what it should be.

The native display supports GNOME on Wayland for Ubuntu 22.04, 24.04 and 26.04
amd64. See the **[display compatibility matrix](docs/display-compatibility.md)**,
**[user guide](docs/display-user-guide.md)** and
**[troubleshooting guide](docs/display-troubleshooting.md)**. Snapshots remain
migration work.

## License

VMLord is free software licensed under the
[GNU General Public License, version 3 or later](LICENSE).
