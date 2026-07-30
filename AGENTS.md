# AGENTS.md

# VMLord

This document defines the development rules for contributors and AI coding agents.

For architecture details, see **ARCHITECTURE.md**.

## Project Status

VMLord is currently migrating from the AppSandbox C backend to a Rust-native architecture.

The current implementation intentionally reuses the AppSandbox backend through a small FFI layer.

The backend is temporary and will be replaced incrementally.

## Development Principles

* Implement all new application code in Rust.
* Keep the FFI layer as small as possible.
* Do not expose C types outside the FFI layer.
* Avoid modifying the AppSandbox backend unless necessary.
* Replace backend components one module at a time.
* Prefer native Windows APIs over PowerShell, WMI or external processes.
* Isolate all `unsafe` code inside platform-specific modules.

## UI Rules

* The UI must not contain business logic.
* The UI must never call Windows APIs directly.
* The UI communicates only with the application layer.

## Code Style

Prefer:

* simple solutions
* explicit code
* small modules
* descriptive names
* idiomatic Rust

Avoid:

* unnecessary abstractions
* traits with a single implementation
* large architectural rewrites unless explicitly requested

## Migration

When porting functionality from AppSandbox:

* preserve behavior before improving it;
* do not translate C code line-by-line;
* design clean Rust APIs;
* treat the C backend as an implementation detail.

## Documentation

Keep documentation up to date.

Update **ARCHITECTURE.md** whenever architectural decisions change.

## Workflow

* Move a task to `Doing` when starting work on it.
* Complete each task in a dedicated branch.
* Commit the completed work before pushing the branch to the remote repository.
* Open a merge request only after receiving explicit user approval.
* Move a task to `Review` after creating its merge request.
* Assign every merge request to `mrundead` and request a review from `mrundead`.

## Commits

The repository-level `git config` belongs to the project owner.

Automated agents must author commits under their own identity by passing:

```powershell
GIT_AUTHOR_NAME=agent GIT_AUTHOR_EMAIL=agent@vmlord.local GIT_COMMITTER_NAME=agent GIT_COMMITTER_EMAIL=agent@vmlord.local
```

Prefix every commit subject with the task number when it is known.

Format commit subjects as `TASK-<No>: comment`.

Examples:

* `TASK-2: Add VM update workflow`
* `TASK-15: Refine SSH connection errors`

## Decision Priority

When multiple solutions are possible, prefer:

1. Correctness
2. Simplicity
3. Maintainability
4. Native Windows integration
5. Performance
