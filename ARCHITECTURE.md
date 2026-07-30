# ARCHITECTURE.md

# VMLord Architecture

## Overview

VMLord is a native Windows application for managing Linux virtual machines using the Windows Host Compute System (HCS).

The architecture is intentionally layered to separate user interface, business logic and platform-specific implementation.

During the initial development phase the project reuses the AppSandbox C backend through a thin FFI layer.

The long-term objective is a fully Rust-native implementation.

---

# Design Goals

The architecture should:

* isolate platform-specific code
* isolate unsafe Rust
* keep the UI independent from virtualization logic
* allow gradual replacement of the legacy backend
* remain testable
* support future CLI and automation APIs

---

# High-Level Architecture

Current architecture:

```text
+---------------------------+
|         UI (Rust)         |
+---------------------------+
              |
              v
+---------------------------+
|     Application Layer     |
+---------------------------+
              |
              v
+---------------------------+
|      Rust Core API        |
+---------------------------+
              |
              v
+---------------------------+
|         FFI Layer         |
+---------------------------+
              |
              v
+---------------------------+
| AppSandbox C Backend      |
+---------------------------+
              |
              v
+---------------------------+
| Windows HCS / HNS / APIs  |
+---------------------------+
```

---

# Target Architecture

After migration:

```text
+---------------------------+
|         UI (Rust)         |
+---------------------------+
              |
              v
+---------------------------+
|     Application Layer     |
+---------------------------+
              |
              v
+---------------------------+
|      Rust Core            |
+---------------------------+
              |
              v
+---------------------------+
| Windows APIs              |
| HCS                       |
| HNS                       |
| GPU-PV                    |
| HvSocket                  |
+---------------------------+
```

No C code should remain.

---

# Layers

## UI

Responsibilities:

* windows
* dialogs
* controls
* rendering
* user interaction

The UI never calls Windows APIs directly.

---

## Application

Coordinates user actions.

Examples:

* Start VM
* Stop VM
* Import image
* Connect display
* Open terminal

Business workflows belong here.

---

## Core

Contains all virtualization logic.

This layer exposes safe Rust APIs.

It knows nothing about the UI.

---

## Platform

Contains:

* Windows API wrappers
* FFI
* unsafe Rust

This is the only layer allowed to interact with operating system APIs.

---

# Current Backend

The AppSandbox backend is currently responsible for:

* HCS integration
* VM lifecycle
* networking
* display
* GPU configuration

Rust communicates with it through FFI.

The backend is considered an implementation detail.

## Implemented scaffold

The initial executable is a Windows x64 Cargo workspace with these dependencies:

```text
vmlord (composition root)
  -> ui (egui/eframe)
  -> app (workflows)
  -> core (safe domain models)
  -> legacy-backend (dynamic C FFI)
  -> appsandbox_core.dll
```

`legacy-backend` is the only crate that may contain `unsafe` code, raw C
pointers, or Windows DLL loading. It dynamically loads the prebuilt
`appsandbox_core.dll` placed next to `vmlord.exe`; no C types cross into `core`,
`app`, or `ui`.

The current UI initializes the backend, shows availability and diagnostics,
lists known VMs, and can create Linux VMs from ISO images. It submits safe
requests through the application layer; only `legacy-backend` maps them to
AppSandbox's C API. The Start action invokes `asb_vm_start`; Stop invokes the
graceful `asb_vm_shutdown`; Force stop invokes `asb_vm_stop`. Connect invokes
`asb_vm_open_display`, which opens or focuses the temporary AppSandbox IDD
window after the guest display driver is ready. It calls `asb_detach` on exit so
it never stops VMs. Snapshots remain future application-layer work.

---

# Planned Modules

```
core/

    vm/
    workspace/
    images/
    networking/
    display/
    gpu/
    ssh/
    diagnostics/
    settings/

platform/

    hcs/
    hns/
    gpu/
    hvsocket/
    ffi/

app/

ui/
```

---

# Migration Strategy

Backend replacement happens incrementally.

Recommended order:

1. HCS
2. HNS
3. Networking
4. GPU
5. Display
6. SSH
7. Diagnostics

Each completed Rust module replaces its C equivalent.

No module should exist in both languages permanently.

---

# FFI Principles

The FFI layer should remain extremely small.

Responsibilities:

* data conversion
* handle conversion
* string conversion
* error conversion

Business logic must never be implemented in FFI.

---

# Unsafe Rust

Unsafe code belongs only inside:

* platform
* ffi
* Windows wrappers

Everything above should expose safe Rust interfaces.

---

# Dependency Rules

Allowed dependency direction:

```
UI
↓

Application
↓

Core
↓

Platform
↓

Windows APIs
```

Reverse dependencies are forbidden.

---

# Future Components

The architecture should support additional frontends without modifying the core.

Examples:

* Desktop GUI
* CLI
* REST API
* Automation tools
* LLM integrations

All of them should use the same application layer.

---

# Philosophy

VMLord is **not** intended to become another Hyper-V Manager.

The project focuses on providing an excellent Linux desktop workspace experience on Windows while keeping the internal architecture simple, modular and easy to evolve.
