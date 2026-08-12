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
diagnostic console. Display, snapshots, and GPU work remain migration work.
AppSandbox macOS code, WebView UI, provisioning tools, and display resources are
not included.
