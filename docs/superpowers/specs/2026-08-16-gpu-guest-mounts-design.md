# GPU share manifest and guest mounts design

## Goal

Task #94 is the step where the Plan9 exports the host already builds become
directories a Linux guest can read. The host sends a VM's `GpuShareManifest`
over the agent session; the agent turns each share into a mount target of its
own choosing, mounts it read-only over 9p, makes the libraries in it
loadable, and says back what it did. This is what turns #88's exports and
#93's payload from a configured share list into a GPU userspace the guest can
actually link against.

## Scope

This task owns two things: the wire -- the messages that carry a manifest and
its report, and the capability that gates them -- and the guest end that acts
on it. It does not decide when a manifest is sent for a real VM, and it does
not derive a `VmGpuStatus` from the report: choosing the manifest at start,
storing the facts and drawing them belongs to #98, and what is inside the
payload belongs to #95 and #96.

What #94 leaves for #98 is a seam of the same shape #92 left for this task:
an `AgentConnection` carries the manifest its VM was started with, and every
session it serves delivers that manifest once, after the challenge. A
reconnect therefore re-mounts nothing on the host and re-sends everything to
the guest, which is exactly what a guest that rebooted needs and what a guest
that only lost its socket ignores as already mounted.

## The messages

`AttachGpuSharesRequest` carries the manifest as the host built it: a repeated
`GpuShare` of a name and a role, and for a driver package the DriverStore
folder name. Roles rather than paths, for the reason `vmlord_core` states --
a host path is useless to the guest and would put the host's topology on the
wire -- and an enum rather than a name to parse, so that an agent installed
months ago is not asked to derive meaning from a string a newer host invented.

`AttachGpuSharesResponse` answers with one `GpuMount` per share: the share's
name, where the guest put it, and whether it is `MOUNTED`, `REFUSED` (the
guest's allowlist does not have a target for it, or the name is not one it
will accept) or `FAILED` (a target it does have, that would not mount or would
not read back). A refusal and a failure are different facts about the host:
one says the two builds disagree about what a share is, the other says the
share is there and broken. `libraries_refreshed` is separate from the mounts
because a set of mounts that all succeeded is still unusable if `ldconfig`
did not run.

Both are behind `CAPABILITY_GPU`, which both peers announce from this task on:
the guest can act on a manifest and the host can send one, which is what the
capability has always meant. The schema gains messages and an enum only, so
the revision moves to **1.2** and an agent from 1.1 simply never sees the
capability agreed.

The guest refuses an attach on a session that did not agree `CAPABILITY_GPU`
with `ERROR_CODE_UNSUPPORTED_REQUEST` rather than trusting that the host would
not have sent it. This build always announces the capability, so the check can
never fire today; it is where the rule is written, and the rule outlives the
build.

## The guest's allowlist

The guest decides where a share goes, from a table with one entry per role:

| Role             | Target                             |
| ---------------- | ---------------------------------- |
| `WslLib`         | `/usr/lib/wsl/lib`                 |
| `DriverPackage`  | `/usr/lib/wsl/drivers/<package>`   |
| `GpuPayload`     | `/opt/vmlord/gpu-payload`          |

The first two are the paths WSL's own GPU userspace uses, which is what the
Mesa D3D12 driver and the vendor libraries in a DriverStore package expect to
find each other at. The payload is VMLord's own and lives under `/opt`, where
software that is neither the distribution's nor the administrator's belongs.

`<package>` is the only part a host contributes, and it is validated again
here: non-empty, no longer than the 96 bytes `vmlord_core` bounds it to, none
of `.` or `..`, and `[A-Za-z0-9._-]` throughout. The host already checks this
before it names a share, and the guest checks it because a path assembled from
a peer's string is exactly the place where "the other side already validated
it" stops being true. A share that fails is `REFUSED`, with the rest of the
manifest still mounted: GPU is best effort on this end too.

A manifest with two shares claiming one target is refused after the first,
rather than mounting one over the other.

## Mounting

Each mount is what AppSandbox's agent did, which is the only way a Hyper-V
Plan9 share can be mounted from Linux: open `AF_VSOCK` to CID 2 -- the host
partition -- on port 50001, where HCS's Plan9 server listens, and hand the
connected descriptor to the kernel's 9p client through
`trans=fd,rfdno=N,wfdno=N`. The share is selected by `aname=<share name>`,
which is why a share name is restricted to characters that cannot be read as
structure in a comma-separated option string. `version=9p2000.L`,
`access=any`, `msize=65536` and `cache=loose` complete the options, and the
flags are `MS_RDONLY | MS_NODEV | MS_NOSUID`: read-only is stated twice and
independently, by the share's flag on the host and by the mount here, and a
host directory has no business supplying device nodes or setuid binaries to a
guest. The descriptor is closed after `mount` returns, because the kernel took
its own reference.

## Reconciling rather than mounting

An agent reconnects, and a host re-sends the manifest on every session. So the
attach is a reconcile against what is mounted now, read from
`/proc/self/mountinfo`:

* a target already carrying the 9p mount of the share the manifest names is
  left alone if it reads back;
* a target carrying a 9p mount of a *different* share -- a manifest that
  changed between boots -- is lazily unmounted and mounted again;
* a target carrying a mount that no longer reads back is lazily unmounted and
  mounted again, at most once, which is the bound on remounting: a share the
  host has taken away must not turn the agent into a loop of mount attempts;
* a 9p mount under one of the three roots that the manifest no longer names is
  unmounted, so a VM that lost an adapter does not keep a dead directory.

Lazily -- `MNT_DETACH` -- because a process still holding a file on a dead
share must not stop the guest from getting a working one.

The health check is `read_dir` on the target: a 9p mount whose transport died
fails it with `EIO` or `ENOTCONN`, where a `stat` of the mount point can still
succeed from the dentry cache. It runs after every mount and against every
mount that was already there, which is what makes a session after a VMLord
restart repair the guest rather than merely agree with it.

## Making the libraries loadable

Mounted directories that contain `.so` files are written, one per line, into
`/etc/ld.so.conf.d/vmlord-gpu.conf`, and `ldconfig` is run. The file is
rewritten from the current set every time rather than appended to, so a
manifest that lost a share loses its line; that, and not a lock, is what makes
the attach idempotent. A directory with no shared objects is not listed --
the payload's own layout is #95's to decide, and a cache entry for a directory
with nothing to load is noise in every `ldconfig` run afterwards.

`ldconfig` is the one external program the agent runs. There is no library
form of it, and writing `/etc/ld.so.cache` by hand would be a second
implementation of a format the distribution owns.

## Unmounting at shutdown

`SIGTERM` -- which is what `systemctl stop` and a guest shutdown send -- sets a
flag, and the agent leaves its loop, unmounts every 9p mount it finds under the
three roots, removes its `ld.so.conf.d` file, runs `ldconfig` once more and
exits successfully. Best effort throughout: a guest that is going down anyway
is not helped by an agent that refuses to exit because a mount was busy.

The flag is all the signal handler does, because a handler may call almost
nothing; the unmounting happens on the main thread when the current session
ends, which is bounded by the socket's read timeout.

The cleanup reads the mount table rather than a list the process kept, so an
agent that was upgraded and restarted still cleans up the mounts its
predecessor made.

## Tests

The syscalls are not testable in this repository -- there is no Hyper-V Plan9
server behind a `cargo test` -- so the parts that decide are pure functions and
those are what the tests drive:

* the allowlist: a target per role, a package name with a separator, `..`, an
  empty one, an over-long one, and two shares claiming one target;
* `mountinfo` parsing: a 9p mount with its `aname`, a mount of another
  filesystem at a target, an unrelated mount, and a malformed line;
* the reconcile plan against a table: nothing mounted, the same share already
  mounted, a different share at the target, and a share the manifest dropped;
* the `ld.so.conf.d` content: only directories with shared objects, and a
  rewrite that drops a share that went away;
* the wire: a manifest that survives the round trip through protobuf, an
  attach on a session without `CAPABILITY_GPU` refused as unsupported, and a
  host that delivers its manifest once per session and logs the report.

`cargo test -p vmlord-agent`, `cargo agent`, `cargo test-windows` and
`cargo check-windows` are the final checks.
