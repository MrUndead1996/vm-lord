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

## Agent Tooling

Use specialized context tools before broad manual repository inspection when they fit the task.

### Serena

Use Serena as the default tool for code navigation and symbol-aware changes:

* locate symbols, definitions, references, callers, and implementations;
* inspect relationships between types, functions, and modules;
* prefer symbol-aware edits and refactors over broad text replacement;
* use Serena before repeated `grep`/`rg` + file reads when the question is about code structure.

Fall back to normal file search/read tools when Serena cannot resolve generated code, build artifacts, configuration, scripts, documentation, or unsupported language constructs.

### Context7

Use Context7 for authoritative, up-to-date documentation about external libraries, frameworks, SDKs, and APIs.

* use it to verify current API signatures, behavior, configuration, and version-specific details;
* prefer primary/upstream documentation returned through Context7;
* do not use Context7 as evidence for facts about this repository; inspect the repository itself for local behavior and conventions;
* when local code and current upstream documentation differ, preserve existing project behavior unless the task explicitly requires migration.

### Repomix

Use Repomix when a task needs compact context across a large subsystem or many related files, especially for:

* architecture review and planning;
* cross-module changes;
* code review and migration analysis;
* preparing a bounded repository snapshot for another agent/model.

Keep Repomix output focused. Include only relevant paths and exclude secrets, credentials, generated output, dependency/vendor directories, build artifacts, and other high-volume irrelevant files.

Do not use Repomix as a substitute for precise symbol navigation when Serena can answer the question directly.

### Tool Priority

For repository work, prefer this order when applicable:

1. Serena for symbols, references, code structure, and targeted edits.
2. Context7 for external API/library documentation and version-specific behavior.
3. Repomix for large, bounded repository context and cross-cutting analysis.
4. Normal file search/read (`rg`, `grep`, direct reads) as a fallback or for non-code text.

Do not call tools mechanically. Use the smallest amount of context needed to make and verify the change.

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

## Delegation to the Local LLM (houtini/houtini-lm)

Delegate bounded, self-contained side tasks to the local LLM via houtini instead of doing them directly:

* drafting commit messages and merge-request descriptions;
* drafting explanations, summaries, or documentation prose;
* brainstorming approaches before committing to one.

Do not delegate work that needs verification against the actual repository or toolchain, such as:

* reading exact API signatures, dependency versions, or codebase conventions;
* writing or editing code;
* anything validated by `cargo build`/`test`/`clippy` or a live environment (e.g. Hyper-V).

## Decision Priority

When multiple solutions are possible, prefer:

1. Correctness
2. Simplicity
3. Maintainability
4. Native Windows integration
5. Performance

## Commands:
- WSL build: ```cargo build --target=x86_64-pc-windows-gnu```
