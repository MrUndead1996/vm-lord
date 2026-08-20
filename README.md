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

## Status

🚧 Early development. The Windows x64 application runs on the native HCS
backend and lists persisted VMs in an egui desktop shell. The AppSandbox legacy
backend remains as a transitional fallback, selected only by setting
`VMLORD_BACKEND=legacy`.

## Building

Every build names its target, because the repository produces a Windows
application and a Linux guest agent. The commands are Cargo aliases, defined in
`.cargo/config.toml`:

| Command | Does | Runs on |
| --- | --- | --- |
| `cargo build -p vmlord` | the application, for the host toolchain | Windows |
| `cargo agent` / `cargo agent-release` | the guest agent, `x86_64-unknown-linux-musl` | Windows, Linux |
| `cargo check-windows` | compile-checks the application through `x86_64-pc-windows-gnu` | WSL |
| `cargo test-windows` | builds and runs the Windows tests | WSL |
| `cargo dist` | release build of everything, collected into `target/dist/` | Windows |
| `cargo gpu-payload pack ...` | release tooling that packs a prepared GPU payload | Windows, Linux |

Prerequisites:

* Windows: the MSVC toolchain, and `rustup target add x86_64-unknown-linux-musl`
  for the agent. Nothing else -- the agent links with `rust-lld` and needs no C
  cross-compiler.
* WSL: `rustup target add x86_64-pc-windows-gnu` and the MinGW-w64 linker
  (`x86_64-w64-mingw32-gcc`, from `gcc-mingw-w64-x86-64` on Debian and Ubuntu).
  Windows test binaries then run directly, through WSL's interop -- no Wine.

`cargo dist` is Windows-only: release executables must come from MSVC. See
**ARCHITECTURE.md** for why each target was chosen.

## Running

Build with `cargo build -p vmlord`, then launch the elevated executable with:

```powershell
Start-Process -FilePath .\target\debug\vmlord.exe -Verb RunAs -Wait
```

The HCS backend requires elevation, so `cargo run` cannot launch the UAC-marked
executable directly. The build stages the prebuilt
`third_party/appsandbox/x64/appsandbox_core.dll` next to the executable. See
`third_party/appsandbox/NOTICE.md` for the pinned artifact and license details.

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

Display and snapshots remain migration work. AppSandbox macOS code, WebView UI,
provisioning tools, and display resources are not included.
