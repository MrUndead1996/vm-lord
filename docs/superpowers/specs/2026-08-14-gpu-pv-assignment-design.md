# GPU-PV HCS assignment design

## Goal

Task #89 adds the native host-side GPU-PV assignment operation for a running
VMLord VM. A VM configured with `GpuMode::Default` receives HCS assignment mode
`Default`; `GpuMode::Mirror` receives `Mirror`. The operation is best effort:
failure to attach a GPU never changes an otherwise successful VM start.

## Scope

This task contains the HCS command, its safe Rust boundary, and a focused GPU
assignment service ready for a post-start caller. It does not wire the service
into `VmStartPipeline`, add UI state, persist the desired mode or runtime facts,
add retries or agent interaction, or change stopped-only edits. Those lifecycle
concerns belong to task #98: the native backend does not yet persist a GPU mode,
so this task has no reliable desired mode for a pipeline call to consume.

## Components

`platform::hcs` remains the only place that calls Windows HCS APIs. It exposes
the small safe primitive needed to submit a modify-compute-system document and
wait for the operation result. Its error retains the call or completion HRESULT
and the HCS result document when one was returned.

`platform::gpu_assignment` is a Rust-only service. It maps supported
`GpuMode`s to the GPU resource JSON and turns the HCS primitive's outcome into
the domain-facing assignment result. It owns no Windows handle and contains no
`unsafe` code. `None` produces no request; `Unknown` is reported as an
unsupported assignment failure without calling HCS.

The next lifecycle task calls the service after HCS reports a successful start
and records the returned outcome. The service API makes that call best effort:
a `GpuFailure` describes a failed assignment rather than requiring the caller
to stop, retry, or otherwise alter the running VM.

## HCS request and diagnostics

The service sends a `HcsModifyComputeSystem` resource request for the GPU with
the assignment mode selected from the desired mode. The generated document is
constructed by a pure function and serialised with `serde_json`, so its
resource path, request type, and mode are covered by unit tests rather than
being embedded as an untested string.

An HCS call failure and an operation-completion failure both produce a
`GpuFailure` using the existing assignment status code. Its message includes a
zero-padded hexadecimal HRESULT and, when HCS supplies it, the result detail.
The original result text is retained verbatim rather than parsed against an
unstable HCS result schema.

## Data flow

```text
GpuMode (stored desired state)
        |
        v
GpuAssignmentService -- JSON --> HcsSystem safe modify operation -- unsafe --> HCS
        |                                                                  |
        +-- success / GpuFailure <--- HRESULT + result detail -------------+
        |
        v
Task #98 lifecycle caller records outcome; VM start result is unchanged
```

## Error handling

* `Default` and `Mirror` attempt assignment exactly once after a successful
  start.
* `None` is a no-op and does not call HCS.
* An unknown mode fails locally with a clear unsupported-mode diagnostic.
* Any HCS failure becomes a diagnostic result, is logged, and is intentionally
  not propagated as a start failure.
* No partial retry, fallback mode, or teardown is attempted.

## Tests

Unit tests cover exact JSON for `Default` and `Mirror`, no-op/unsupported mode
handling, and error messages containing HRESULT and result detail. The service
is documented as best effort and returns a domain result rather than a start
error, which lets #98 test the eventual pipeline integration independently.
Windows compilation and the complete Windows test suite are the final checks.
