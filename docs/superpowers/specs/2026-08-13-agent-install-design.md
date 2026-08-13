# Installing the guest agent (#91)

A VM created from a cloud image gets `vmlord-agent` installed on its first
boot, running as root under systemd, holding an authenticated session to the
host. This is the guest half of the socket #90 already listens on.

Scope is installation and the first connect. Reconnect with bounded backoff,
capability negotiation and session recovery are #92; GPU capability and Plan9
mounts are #94.

## Decisions

Four questions were settled before design, and each closes an alternative:

* **The agent connects in this task rather than staying a stub.** A unit with
  `Restart=always` around a program that prints a version and exits is a
  restart loop, and "the agent is installed" would have nothing to verify it
  by. With the connect included, a freshly created VM reports
  `AgentStatus::Online`, which is what makes #90 and #91 provable together.
* **A separate tools volume, per VM, in the VM's own directory.** Not merged
  into the seed: the seed is per-VM, secret-bearing and narrowed by
  `vm_key::restrict_to_owner`, while the agent is the same binary on every VM.
  Not a shared image under the storage root either: that file's lifetime would
  sit outside every VM that references it, and `config.json` -- what a start
  rebuilds the compute system from -- would name a path nobody owns. Roughly
  two megabytes beside a VHDX is not a cost worth that question.
* **A missing agent binary is best effort, not a refusal.** The application
  looks for `vmlord-agent` beside its own executable, the way the legacy DLL is
  found. When it is absent -- a `cargo run` build, an unpacked release missing
  a file -- the VM is created without the tools volume, without a secret in the
  seed and without the unit, and a warning names the path that was searched.
  This matches the epic's rule that GPU support is applied best effort and
  never blocks a VM, and keeps development builds able to create VMs.
* **No upgrade path for an installed agent.** The volume is built when the VM
  is created; a later application version does not touch guests already
  provisioned. The epic recreates old VMs rather than migrating them.

## The guest

`crates/agent` grows from one file to three, and the crate sets
`unsafe_code = "allow"` in its own manifest -- the way `crates/platform` does,
so the exception is a property of one crate rather than of the workspace.

* `vsock.rs` is the transport and the only place with `unsafe`.
  `connect(cid, port)` opens `AF_VSOCK`/`SOCK_STREAM`, fills a `sockaddr_vm`
  with `VMADDR_CID_HOST` and the agent's port, sets `SO_RCVTIMEO`, and returns
  a stream that implements `Read + Write` and closes its descriptor on drop.
  Written against `libc` -- already in the lock file -- rather than the `vsock`
  crate, which adds `nix` for forty lines the host side wrote by hand too.
  Everything above this module therefore reads and writes plain bytes.
* `session.rs` is the client half of `platform::agent_session`: hello with
  `ProtocolVersion::current()`, no capabilities (`CAPABILITY_GPU` is #94's
  promise) and the build's version string; then the challenge, answered with
  `auth::tag` over the secret; then a loop that sends a heartbeat when the read
  times out, answers what the host asks, and refuses anything this build has no
  arm for with `ERROR_CODE_UNSUPPORTED_REQUEST`.
* `main.rs` reads the secret from `auth::GUEST_SECRET_PATH` -- the constant,
  never a second spelling of the path -- connects, and runs the session. A
  clean hang-up exits zero; a fault writes a line to stderr, which journald
  keeps, and exits non-zero. Restarting is systemd's job until #92 makes it the
  agent's.

## The host

`vmlord-seed` already owns an ISO9660 writer that takes any set of files, so
the tools volume is built there rather than in a second crate that would
duplicate it: `tools_image(agent: &[u8]) -> Vec<u8>`, volume `VMLTOOLS`, one
file named `vmlord-agent`. The crate's subject widens from "the seed" to "the
media the first boot reads", and both names live in one module so the volume
and the command that copies out of it cannot disagree.

`write_provisioning` writes `<vm>/tools.iso` beside `seed.iso`. No narrowed
DACL -- there is no secret in it -- but the same `HcsGrantVmAccess`, since the
VM has to read it. `HcsVmConfigBuilder::build` takes the path as an `Option`
and adds attachment `"2"` of type `Iso` when it is present; a local-media VM
still has exactly two attachments, because it runs no cloud-init and gets no
agent. Rollback is unchanged: any failure after the directory exists removes
the whole directory, and nothing enumerates the files a half-built VM left.

## The first boot

`SeedRequest::agent_secret` stays the single switch. When it is `Some`,
`user-data` additionally carries:

* a `write_files` entry for `/etc/systemd/system/vmlord-agent.service`, mode
  `0644`: `ConditionPathExists` on the secret, `ExecStart` at
  `/usr/local/lib/vmlord/vmlord-agent`, `Restart=always`, `RestartSec=5`,
  `WantedBy=multi-user.target`. It runs as root with no sandboxing directives,
  because #94 has this process mounting filesystems;
* `runcmd`, one command per step so that a failure does not take the rest down
  with it: create the mount point under `/run` and the install directory,
  `mount -o ro -L VMLTOOLS`, `install -m 0755 -o root -g root` the binary onto
  the disk, `umount`, `systemctl daemon-reload`, `systemctl enable --now
  vmlord-agent.service`.

The binary is copied onto the disk rather than executed from the volume, so a
booted guest does not depend on the volume still being attached.

## Testing

* `crates/seed` unit tests: the unit file and the install commands appear
  exactly when `agent_secret` is `Some`, and not at all when it is `None`.
* `crates/seed/tests/mount.rs`, which mounts an image with a real kernel, gains
  the tools volume: the label, the file name, and the bytes read back whole.
* `hcs_config`: three attachments for a cloud image, two for local media.
* `crates/agent`: the order of the messages against a peer made of bytes, the
  way `agent_session`'s own tests do it. `vsock.rs` stays thin precisely
  because nothing but a hypervisor can exercise it.
* On a real host, by hand: a freshly created Ubuntu VM reports
  `AgentStatus::Online`.

`ARCHITECTURE.md` gains the installation path and the reason a cloud VM now has
three SCSI attachments.
