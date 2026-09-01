# Offline AppSandbox Import Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Convert a copy of the AppSandbox Ubuntu guest into a VMLord guest by editing the copied disk offline, register it as a VMLord VM, and boot it once as the verification.

**Architecture:** The conversion is a pure function over a mounted filesystem root in a new host-independent crate `vmlord-appsandbox-convert`. It is driven under WSL by `cargo xtask appsandbox-convert`. The host half is an *adopt* branch in the existing creation pipeline: a VM record built around a disk that already exists, with no image download and no cloud-init seed, which emits the input document the conversion consumes.

**Tech Stack:** Rust 2024, `serde`/`serde_json`, `tracing`, existing crates `vmlord-core`, `vmlord-seed`, `vmlord-agent-protocol`, `vmlord-keys`, `vmlord-platform`, `vmlord-xtask`.

**Spec:** `docs/superpowers/specs/2026-09-01-appsandbox-offline-import-design.md`

## Global Constraints

- Branch: `TASK-21-offline-import`. Commit subjects start with `TASK-21: `.
- Rust only. No C, no FFI, no PowerShell, no WMI (AGENTS.md).
- `unsafe` stays inside platform-specific modules. `vmlord-appsandbox-convert` contains none.
- Log through `tracing`. User-actionable events through `vmlord_core::diagnostic!`.
- A secret has neither `Display` nor `Debug` that shows its value.
- The conversion writes and deletes **only** inside the root it was handed. It never touches an AppSandbox path.
- VMLord-side guest names are imported from `vmlord-seed` / `vmlord-agent-protocol`, never copied as literals into the conversion crate.
- Guest contract values, verbatim: agent binary `/usr/local/lib/vmlord/vmlord-agent` (`0755 root:root`); secret `/etc/vmlord/agent.secret` (`0600 root:root`); unit `/etc/systemd/system/vmlord-agent.service` (`0644`), enabled by the symlink `/etc/systemd/system/multi-user.target.wants/vmlord-agent.service`; netplan `/etc/netplan/90-vmlord.yaml` (`0600`).
- Repository verification: `cargo check-windows` and `cargo test-windows` for the workspace; `cargo test -p vmlord-appsandbox-convert` for the conversion crate, which builds and runs on the Linux host.
- New user-facing UI text would go through `t!` into both catalogues — this plan adds none; the import is driven from a subcommand.

---

## File Structure

**Created**

| File | Responsibility |
|---|---|
| `crates/appsandbox-convert/Cargo.toml` | the new crate |
| `crates/appsandbox-convert/src/lib.rs` | `convert`, `verify`, the error type, module wiring |
| `crates/appsandbox-convert/src/input.rs` | `Conversion` — the input document and its JSON form |
| `crates/appsandbox-convert/src/facts.rs` | reading a root: preconditions, refusals, renderer |
| `crates/appsandbox-convert/src/install.rs` | writing VMLord's own files into the root |
| `crates/appsandbox-convert/src/remove.rs` | the AppSandbox inventory and its removal |
| `crates/appsandbox-convert/src/root.rs` | joining a guest-absolute path onto the root, and refusing escape |
| `crates/appsandbox-convert/src/fixture.rs` | `#[cfg(test)]`-only builder of an AppSandbox-shaped tree |
| `crates/xtask/src/appsandbox_convert.rs` | the command that runs the conversion under WSL |
| `docs/appsandbox-import.md` | rewritten user documentation |

**Modified**

| File | Change |
|---|---|
| `Cargo.toml` | `crates/appsandbox-convert` in `members` and `default-members` |
| `crates/seed/src/lib.rs`, `crates/seed/src/user_data.rs` | make the agent's guest paths and unit text public |
| `crates/xtask/src/main.rs` | dispatch `appsandbox-convert` |
| `.cargo/config.toml` | the `appsandbox-convert` alias |
| `crates/core/src/provisioning.rs` | `VmSource::ExistingDisk` |
| `crates/platform/src/create.rs` | the adopt branch and the input-document emission |
| `crates/platform/src/layout.rs` | `import_input_path` |
| `crates/vmlord/src/main.rs` | the `adopt-disk` subcommand |
| `ARCHITECTURE.md` | replace the statement that AppSandbox VMs are not migrated |

---

### Task 1: The conversion crate and its input document

**Files:**
- Create: `crates/appsandbox-convert/Cargo.toml`, `crates/appsandbox-convert/src/lib.rs`, `crates/appsandbox-convert/src/input.rs`, `crates/appsandbox-convert/src/root.rs`
- Modify: `Cargo.toml:2-49`

**Interfaces:**
- Consumes: nothing.
- Produces: `vmlord_appsandbox_convert::Conversion { root: PathBuf, guest_username: String, vmlord_public_key: String, agent_secret: String, agent_binary: PathBuf, hostname: String, ssh: Option<SshDropIns> }`; `SshDropIns { config_drop_in: String, socket_drop_in: Option<String>, port: u16 }`; `Conversion::from_json(&str) -> Result<Conversion, ConvertError>`; `ConvertError` (`Display`, no `Debug` leak of `agent_secret`); `root::guest_path(root: &Path, absolute: &str) -> Result<PathBuf, ConvertError>`.

- [ ] **Step 1: Write the failing tests**

`crates/appsandbox-convert/src/input.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::Conversion;

    const DOCUMENT: &str = r#"{
        "root": "/mnt/guest",
        "guest_username": "agromov",
        "vmlord_public_key": "ssh-ed25519 AAAAC3Nz vmlord",
        "agent_secret": "c2VjcmV0",
        "agent_binary": "/tmp/vmlord-agent",
        "hostname": "ubuntu",
        "ssh": { "config_drop_in": "/etc/ssh/sshd_config.d/10-vmlord.conf",
                 "socket_drop_in": "/etc/systemd/system/ssh.socket.d/10-vmlord.conf",
                 "port": 2222 }
    }"#;

    #[test]
    fn a_document_names_every_value_the_conversion_takes_from_outside() {
        let conversion = Conversion::from_json(DOCUMENT).expect("a valid document");
        assert_eq!(conversion.guest_username, "agromov");
        assert_eq!(conversion.hostname, "ubuntu");
        assert_eq!(conversion.ssh.as_ref().expect("ssh").port, 2222);
    }

    #[test]
    fn a_document_without_ssh_is_a_guest_vmlord_leaves_the_daemon_alone_in() {
        let document = DOCUMENT.replace(
            r#""ssh": { "config_drop_in": "/etc/ssh/sshd_config.d/10-vmlord.conf",
                 "socket_drop_in": "/etc/systemd/system/ssh.socket.d/10-vmlord.conf",
                 "port": 2222 }"#,
            r#""ssh": null"#,
        );
        assert!(Conversion::from_json(&document).expect("valid").ssh.is_none());
    }

    #[test]
    fn a_missing_field_is_refused_rather_than_defaulted() {
        let error = Conversion::from_json(r#"{"root": "/mnt/guest"}"#).expect_err("refused");
        assert!(error.to_string().contains("guest_username"), "{error}");
    }

    #[test]
    fn the_secret_is_not_in_the_documents_debug_rendering() {
        let conversion = Conversion::from_json(DOCUMENT).expect("a valid document");
        assert!(!format!("{conversion:?}").contains("c2VjcmV0"));
    }
}
```

`crates/appsandbox-convert/src/root.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::guest_path;
    use std::path::Path;

    #[test]
    fn a_guest_absolute_path_lands_under_the_root() {
        let joined = guest_path(Path::new("/mnt/guest"), "/etc/hostname").expect("joined");
        assert_eq!(joined, Path::new("/mnt/guest/etc/hostname"));
    }

    #[test]
    fn a_path_that_is_not_guest_absolute_is_refused() {
        assert!(guest_path(Path::new("/mnt/guest"), "etc/hostname").is_err());
    }

    #[test]
    fn a_path_that_climbs_out_of_the_root_is_refused() {
        assert!(guest_path(Path::new("/mnt/guest"), "/etc/../../escape").is_err());
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vmlord-appsandbox-convert`
Expected: FAIL — the package does not exist yet.

- [ ] **Step 3: Write the crate**

`crates/appsandbox-convert/Cargo.toml`:

```toml
[package]
name = "vmlord-appsandbox-convert"
version.workspace = true
edition.workspace = true
license.workspace = true
authors.workspace = true

[dependencies]
serde = { workspace = true, features = ["derive"] }
serde_json = { workspace = true }
tracing = { workspace = true }
vmlord-agent-protocol = { path = "../agent-protocol" }
vmlord-seed = { path = "../seed" }

[dev-dependencies]
tempfile = { workspace = true }
```

`crates/appsandbox-convert/src/input.rs`:

```rust
//! The document the conversion takes every outside value from.
//!
//! Every name, path and key the conversion acts on comes from here, so that
//! nothing a person chose is ever a literal inside the program.

use std::{fmt, path::PathBuf};

use serde::Deserialize;

use crate::ConvertError;

/// One conversion of one guest.
#[derive(Deserialize)]
pub struct Conversion {
    /// Where the guest's filesystem root is mounted on this machine.
    pub root: PathBuf,
    /// The interactive account VMLord's key goes into.
    pub guest_username: String,
    pub vmlord_public_key: String,
    /// The VM's agent secret, base64 as the host stored it.
    pub agent_secret: String,
    /// The agent binary on *this* machine, copied into the guest.
    pub agent_binary: PathBuf,
    /// What the converted guest calls itself.
    pub hostname: String,
    /// Absent when the guest's daemon is already on the port VMLord recorded.
    pub ssh: Option<SshDropIns>,
}

/// The two drop-ins that move the daemon, named by the distribution profile.
#[derive(Deserialize)]
pub struct SshDropIns {
    pub config_drop_in: String,
    /// Absent where the daemon opens its own port instead of a socket.
    pub socket_drop_in: Option<String>,
    pub port: u16,
}

impl Conversion {
    /// Reads a document, refusing anything it cannot name a value for.
    pub fn from_json(document: &str) -> Result<Self, ConvertError> {
        serde_json::from_str(document)
            .map_err(|error| ConvertError::new(format!("the conversion document is not one: {error}")))
    }
}

/// Everything but the secret, which has no rendering that shows it.
impl fmt::Debug for Conversion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Conversion")
            .field("root", &self.root)
            .field("guest_username", &self.guest_username)
            .field("hostname", &self.hostname)
            .field("agent_binary", &self.agent_binary)
            .field("agent_secret", &"<redacted>")
            .finish_non_exhaustive()
    }
}
```

`crates/appsandbox-convert/src/root.rs`:

```rust
//! Joining a guest-absolute path onto the root the conversion was handed.
//!
//! Every path the conversion touches goes through here. A path that is not
//! guest-absolute, or that climbs back out of the root once its `.` and `..`
//! are resolved, is a refusal rather than a write somewhere else on the
//! machine doing the conversion.

use std::path::{Component, Path, PathBuf};

use crate::ConvertError;

pub(crate) fn guest_path(root: &Path, absolute: &str) -> Result<PathBuf, ConvertError> {
    let Some(relative) = absolute.strip_prefix('/') else {
        return Err(ConvertError::new(format!(
            "{absolute} is not a path inside the guest"
        )));
    };
    let mut joined = root.to_path_buf();
    for component in Path::new(relative).components() {
        match component {
            Component::Normal(part) => joined.push(part),
            Component::CurDir => {}
            _ => {
                return Err(ConvertError::new(format!(
                    "{absolute} leads out of the guest's root"
                )));
            }
        }
    }
    Ok(joined)
}
```

`crates/appsandbox-convert/src/lib.rs`:

```rust
//! Turning a copied AppSandbox Linux guest into a VMLord guest, with nothing
//! running from the disk it is on.
//!
//! The conversion is a function over a mounted filesystem root: it knows
//! nothing of VHDX, of Windows or of how the root came to be mounted. That is
//! what lets the same code run under WSL today and inside a service VM later,
//! and what lets every one of its tests run against a directory tree.

use std::fmt;

mod input;
mod root;

pub use input::{Conversion, SshDropIns};

/// A refusal, or a step that could not be completed.
pub struct ConvertError(String);

impl ConvertError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        let error = Self(message.into());
        tracing::error!("{error}");
        error
    }
}

impl fmt::Display for ConvertError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl fmt::Debug for ConvertError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

impl std::error::Error for ConvertError {}
```

Add to `Cargo.toml`, in `members` after `crates/agent-protocol` and in `default-members` in the same position:

```toml
    "crates/appsandbox-convert",
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p vmlord-appsandbox-convert`
Expected: PASS, 7 tests.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock crates/appsandbox-convert
git commit -m "TASK-21: Add the offline conversion crate and its input document"
```

---

### Task 2: The fixture, the guest's facts, and the refusals

**Files:**
- Create: `crates/appsandbox-convert/src/fixture.rs`, `crates/appsandbox-convert/src/facts.rs`
- Modify: `crates/appsandbox-convert/src/lib.rs`

**Interfaces:**
- Consumes: `Conversion`, `root::guest_path`, `ConvertError` (Task 1).
- Produces: `facts::GuestFacts { renderer: Renderer, home: PathBuf, uid: u32, gid: u32 }`; `facts::Renderer` (`NetworkManager` | `Networkd`) with `fn name(&self) -> &'static str`; `facts::read(conversion: &Conversion) -> Result<GuestFacts, ConvertError>`; `fixture::AppSandboxGuest` with `fn new() -> Self`, `fn root(&self) -> &Path`, `fn conversion(&self) -> Conversion`, `fn with_network_manager(self) -> Self`, `fn without(self, guest_path: &str) -> Self`.

- [ ] **Step 1: Write the failing tests**

Append to `crates/appsandbox-convert/src/facts.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::{Renderer, read};
    use crate::fixture::AppSandboxGuest;

    #[test]
    fn an_appsandbox_guest_yields_the_account_the_key_goes_to() {
        let guest = AppSandboxGuest::new();
        let facts = read(&guest.conversion()).expect("an AppSandbox guest");
        assert_eq!(facts.home, guest.root().join("home/agromov"));
        assert_eq!(facts.uid, 1000);
        assert_eq!(facts.gid, 1000);
    }

    #[test]
    fn a_guest_with_no_network_manager_unit_is_rendered_by_networkd() {
        let guest = AppSandboxGuest::new();
        assert_eq!(read(&guest.conversion()).expect("facts").renderer, Renderer::Networkd);
    }

    #[test]
    fn a_guest_whose_network_manager_is_enabled_is_rendered_by_it() {
        let guest = AppSandboxGuest::new().with_network_manager();
        assert_eq!(
            read(&guest.conversion()).expect("facts").renderer,
            Renderer::NetworkManager
        );
    }

    #[test]
    fn a_root_that_is_not_ubuntu_is_refused() {
        let guest = AppSandboxGuest::new().without("/etc/os-release");
        let error = read(&guest.conversion()).expect_err("refused");
        assert!(error.to_string().contains("os-release"), "{error}");
    }

    #[test]
    fn a_root_with_nothing_of_appsandbox_in_it_is_refused() {
        let guest = AppSandboxGuest::new()
            .without("/opt/appsandbox/appsandbox-gpu")
            .without("/etc/systemd/system/appsandbox-agent.service");
        let error = read(&guest.conversion()).expect_err("refused");
        assert!(error.to_string().contains("AppSandbox"), "{error}");
    }

    #[test]
    fn a_named_account_that_is_not_in_the_guest_is_refused() {
        let mut conversion = AppSandboxGuest::new().conversion();
        conversion.guest_username = "nobody-here".to_owned();
        let error = read(&conversion).expect_err("refused");
        assert!(error.to_string().contains("nobody-here"), "{error}");
    }

    #[test]
    fn a_guest_vmlord_has_already_converted_is_refused() {
        let guest = AppSandboxGuest::new();
        std::fs::write(
            guest.root().join("etc/systemd/system/vmlord-agent.service"),
            "[Unit]\n",
        )
        .expect("write");
        let error = read(&guest.conversion()).expect_err("refused");
        assert!(error.to_string().contains("vmlord-agent"), "{error}");
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vmlord-appsandbox-convert facts`
Expected: FAIL — `facts` and `fixture` do not exist.

- [ ] **Step 3: Write the fixture and the facts**

`crates/appsandbox-convert/src/fixture.rs`:

```rust
//! An AppSandbox-shaped guest in a temporary directory.
//!
//! Every test in this crate runs against one of these rather than against a
//! mounted disk: the conversion is a function over a directory, so a directory
//! is the whole of what it needs to be exercised on.

use std::{fs, os::unix::fs as unix_fs, path::{Path, PathBuf}};

use tempfile::TempDir;

use crate::Conversion;

/// The files an AppSandbox guest is recognised and converted by.
const FILES: [(&str, &str); 21] = [
    ("/etc/os-release", "NAME=\"Ubuntu\"\nVERSION_ID=\"26.04\"\nID=ubuntu\n"),
    ("/etc/passwd", "root:x:0:0:root:/root:/bin/bash\nagromov:x:1000:1000::/home/agromov:/bin/bash\n"),
    ("/etc/hostname", "ubuntu\n"),
    ("/etc/hosts", "127.0.0.1\tlocalhost\n127.0.1.1\tubuntu\n"),
    ("/etc/netplan/99-appsandbox.yaml", "network:\n  version: 2\n"),
    ("/etc/cloud/cloud.cfg.d/99-disable-network-config.cfg", "network: {config: disabled}\n"),
    ("/home/agromov/.ssh/authorized_keys", "ssh-ed25519 AAAAC3Nz appsandbox\n"),
    ("/usr/local/bin/appsandbox-agent", "ELF"),
    ("/usr/local/bin/appsandbox-audio", "ELF"),
    ("/usr/local/bin/appsandbox-clipboard", "ELF"),
    ("/usr/local/bin/appsandbox-display", "ELF"),
    ("/usr/local/bin/appsandbox-input", "ELF"),
    ("/usr/local/bin/appsandbox-gpu", "#!/bin/sh\n"),
    ("/usr/local/bin/appsandbox-firstboot.sh", "#!/bin/bash\n"),
    ("/etc/systemd/system/appsandbox-agent.service", "[Unit]\n"),
    ("/etc/systemd/user/appsandbox-clipboard.service", "[Unit]\n"),
    ("/etc/modprobe.d/asb_drm.conf", "blacklist hyperv_drm\n"),
    ("/etc/modules-load.d/asb_drm.conf", "asb_drm\n"),
    ("/etc/systemd/user-environment-generators/50-appsandbox-gpu", "#!/bin/sh\n"),
    ("/opt/appsandbox/appsandbox-gpu", "#!/bin/sh\n"),
    ("/var/lib/appsandbox-firstboot.done", ""),
];

pub(crate) struct AppSandboxGuest {
    directory: TempDir,
    agent: PathBuf,
}

impl AppSandboxGuest {
    pub(crate) fn new() -> Self {
        let directory = TempDir::new().expect("a temporary directory");
        let root = directory.path();
        for (path, contents) in FILES {
            let target = root.join(path.trim_start_matches('/'));
            fs::create_dir_all(target.parent().expect("a parent")).expect("mkdir");
            fs::write(&target, contents).expect("write");
        }
        // The enablement is the symlink, so the fixture has the ones a
        // converted guest has to be rid of.
        let wants = root.join("etc/systemd/system/multi-user.target.wants");
        fs::create_dir_all(&wants).expect("mkdir");
        unix_fs::symlink(
            "/etc/systemd/system/appsandbox-agent.service",
            wants.join("appsandbox-agent.service"),
        )
        .expect("symlink");
        let agent = root.join("vmlord-agent-source");
        fs::write(&agent, b"agent").expect("write");
        Self { directory, agent }
    }

    pub(crate) fn root(&self) -> &Path {
        self.directory.path()
    }

    /// Enables NetworkManager the way a guest running it has it enabled.
    pub(crate) fn with_network_manager(self) -> Self {
        let unit = self
            .root()
            .join("etc/systemd/system/multi-user.target.wants/NetworkManager.service");
        unix_fs::symlink("/usr/lib/systemd/system/NetworkManager.service", unit)
            .expect("symlink");
        self
    }

    /// Removes one guest path, for the tests that assert a refusal.
    pub(crate) fn without(self, guest_path: &str) -> Self {
        let _ = fs::remove_file(self.root().join(guest_path.trim_start_matches('/')));
        self
    }

    pub(crate) fn conversion(&self) -> Conversion {
        Conversion::from_json(&format!(
            r#"{{
                "root": {root:?},
                "guest_username": "agromov",
                "vmlord_public_key": "ssh-ed25519 AAAAC3Nz vmlord",
                "agent_secret": "c2VjcmV0",
                "agent_binary": {agent:?},
                "hostname": "imported",
                "ssh": null
            }}"#,
            root = self.root(),
            agent = self.agent,
        ))
        .expect("a valid document")
    }
}
```

`crates/appsandbox-convert/src/facts.rs`:

```rust
//! What the guest itself says, read off the disk rather than asked of a
//! running system.

use std::path::PathBuf;

use crate::{Conversion, ConvertError, root::guest_path};

/// Which of the two renderers manages the guest's interface.
///
/// A netplan naming the one that is not running makes the one that is stop
/// managing the interface entirely, so this is read rather than assumed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Renderer {
    NetworkManager,
    Networkd,
}

impl Renderer {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::NetworkManager => "NetworkManager",
            Self::Networkd => "networkd",
        }
    }
}

pub(crate) struct GuestFacts {
    pub(crate) renderer: Renderer,
    pub(crate) home: PathBuf,
    pub(crate) uid: u32,
    pub(crate) gid: u32,
}

/// The evidence that a guest is one this conversion can act on.
const APPSANDBOX_EVIDENCE: [&str; 2] = [
    "/opt/appsandbox/appsandbox-gpu",
    "/etc/systemd/system/appsandbox-agent.service",
];

pub(crate) fn read(conversion: &Conversion) -> Result<GuestFacts, ConvertError> {
    let root = &conversion.root;

    let release = guest_path(root, "/etc/os-release")?;
    let release = std::fs::read_to_string(&release).map_err(|error| {
        ConvertError::new(format!("{} could not be read: {error}", release.display()))
    })?;
    if !release.lines().any(|line| line.trim() == "ID=ubuntu") {
        return Err(ConvertError::new(
            "the root's os-release does not name Ubuntu",
        ));
    }

    if !APPSANDBOX_EVIDENCE
        .iter()
        .any(|path| guest_path(root, path).is_ok_and(|path| path.exists()))
    {
        return Err(ConvertError::new(
            "the root holds nothing of AppSandbox's: it is not a guest this converts",
        ));
    }

    if guest_path(root, "/etc/systemd/system/vmlord-agent.service")?.exists() {
        return Err(ConvertError::new(
            "the root already has a vmlord-agent unit: it has been converted before",
        ));
    }

    let (home, uid, gid) = account(conversion)?;
    let renderer = if guest_path(
        root,
        "/etc/systemd/system/multi-user.target.wants/NetworkManager.service",
    )?
    .symlink_metadata()
    .is_ok()
    {
        Renderer::NetworkManager
    } else {
        Renderer::Networkd
    };

    Ok(GuestFacts {
        renderer,
        home: root.join(home.trim_start_matches('/')),
        uid,
        gid,
    })
}

/// The named account's home, uid and gid, out of the guest's own `passwd`.
fn account(conversion: &Conversion) -> Result<(String, u32, u32), ConvertError> {
    let path = guest_path(&conversion.root, "/etc/passwd")?;
    let passwd = std::fs::read_to_string(&path)
        .map_err(|error| ConvertError::new(format!("{} could not be read: {error}", path.display())))?;
    for line in passwd.lines() {
        let fields: Vec<&str> = line.split(':').collect();
        if fields.len() >= 6 && fields[0] == conversion.guest_username {
            let uid = fields[2].parse().map_err(|_| {
                ConvertError::new(format!("{}'s uid is not a number", conversion.guest_username))
            })?;
            let gid = fields[3].parse().map_err(|_| {
                ConvertError::new(format!("{}'s gid is not a number", conversion.guest_username))
            })?;
            return Ok((fields[5].to_owned(), uid, gid));
        }
    }
    Err(ConvertError::new(format!(
        "the guest has no account named {}",
        conversion.guest_username
    )))
}
```

In `crates/appsandbox-convert/src/lib.rs`, add beside the existing modules:

```rust
mod facts;
#[cfg(test)]
mod fixture;
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p vmlord-appsandbox-convert`
Expected: PASS, 14 tests.

- [ ] **Step 5: Commit**

```bash
git add crates/appsandbox-convert
git commit -m "TASK-21: Read an imported guest's facts off its own disk"
```

---

### Task 3: Publish the guest contract from `vmlord-seed`

**Files:**
- Modify: `crates/seed/src/lib.rs:130-140`, `crates/seed/src/user_data.rs:100-115,310-315`

**Interfaces:**
- Consumes: nothing.
- Produces: `vmlord_seed::AGENT_BINARY_PATH: &str`, `vmlord_seed::AGENT_UNIT_PATH: &str`, `vmlord_seed::AGENT_UNIT_NAME: &str`, `vmlord_seed::AGENT_UNIT: &str`.

- [ ] **Step 1: Write the failing test**

Append to `crates/seed/src/lib.rs`'s `mod tests`:

```rust
#[test]
fn the_agent_contract_the_seed_writes_is_the_one_it_publishes() {
    let seed = super::build(&super::tests::request_with_agent());
    assert!(seed.user_data.contains(super::AGENT_UNIT_PATH));
    assert!(seed.user_data.contains(super::AGENT_BINARY_PATH));
    assert!(seed.user_data.contains(super::AGENT_UNIT_NAME));
    assert_eq!(super::AGENT_UNIT_NAME, "vmlord-agent.service");
}
```

If `request_with_agent()` does not already exist in that module, add it there, built from the module's existing `SeedRequest` fixture with `agent_secret: Some(AGENT_SECRET)`.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p vmlord-seed`
Expected: FAIL — `AGENT_UNIT_PATH` is not found in `vmlord_seed`.

- [ ] **Step 3: Publish the constants**

In `crates/seed/src/user_data.rs`, remove the two private constants and use the public ones:

```rust
use crate::{AGENT_BINARY_PATH, AGENT_FILE, AGENT_UNIT, AGENT_UNIT_NAME, AGENT_UNIT_PATH, SeedRequest, scalar};
```

replacing `AGENT_SERVICE_PATH` with `AGENT_UNIT_PATH` and `AGENT_SERVICE` with `AGENT_UNIT` at their two use sites, and replacing the literal `"/usr/local/lib/vmlord/vmlord-agent"` in `agent_install_commands` with `AGENT_BINARY_PATH.to_owned()` and the literal `"vmlord-agent.service"` with `AGENT_UNIT_NAME.to_owned()`.

In `crates/seed/src/lib.rs`, beside `AGENT_FILE`:

```rust
/// Where the agent is installed in a guest.
///
/// Public because a guest VMLord did not create -- one imported from
/// AppSandbox -- is brought to the same contract by a different program, and a
/// second copy of these four names is a copy that falls behind this one.
pub const AGENT_BINARY_PATH: &str = "/usr/local/lib/vmlord/vmlord-agent";
/// The unit that runs it, by name.
pub const AGENT_UNIT_NAME: &str = "vmlord-agent.service";
/// And where that unit is installed.
pub const AGENT_UNIT_PATH: &str = "/etc/systemd/system/vmlord-agent.service";
/// What the unit says.
pub const AGENT_UNIT: &str = "[Unit]\nDescription=VMLord guest agent\nConditionPathExists=/etc/vmlord/agent.secret\n\n[Service]\nExecStart=/usr/local/lib/vmlord/vmlord-agent\nUser=root\nRestart=always\nRestartSec=5\n\n[Install]\nWantedBy=multi-user.target\n";
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p vmlord-seed && cargo check-windows`
Expected: PASS; the check reports no errors.

- [ ] **Step 5: Commit**

```bash
git add crates/seed
git commit -m "TASK-21: Publish the agent's guest contract from the seed"
```

---

### Task 4: Install VMLord's own files into the root

**Files:**
- Create: `crates/appsandbox-convert/src/install.rs`
- Modify: `crates/appsandbox-convert/src/lib.rs`

**Interfaces:**
- Consumes: `Conversion`, `GuestFacts`, `Renderer`, `guest_path`, `ConvertError`.
- Produces: `install::run(conversion: &Conversion, facts: &GuestFacts) -> Result<(), ConvertError>`; `install::NETPLAN_PATH: &str = "/etc/netplan/90-vmlord.yaml"`; `install::SECRET_PATH` re-exported from `vmlord_agent_protocol::auth::GUEST_SECRET_PATH`.

- [ ] **Step 1: Write the failing tests**

Append to `crates/appsandbox-convert/src/install.rs`:

```rust
#[cfg(test)]
mod tests {
    use std::{fs, os::unix::fs::PermissionsExt};

    use super::{NETPLAN_PATH, run};
    use crate::{facts, fixture::AppSandboxGuest};

    fn converted(guest: &AppSandboxGuest) -> crate::Conversion {
        let conversion = guest.conversion();
        let facts = facts::read(&conversion).expect("facts");
        run(&conversion, &facts).expect("installed");
        conversion
    }

    #[test]
    fn vmlords_key_replaces_the_one_the_source_left_behind() {
        let guest = AppSandboxGuest::new();
        converted(&guest);
        let keys = fs::read_to_string(guest.root().join("home/agromov/.ssh/authorized_keys"))
            .expect("read");
        assert!(keys.contains("ssh-ed25519 AAAAC3Nz vmlord"), "{keys}");
        assert!(!keys.contains("appsandbox"), "{keys}");
        let mode = fs::metadata(guest.root().join("home/agromov/.ssh/authorized_keys"))
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn the_agent_its_secret_and_its_unit_are_installed_at_the_seeds_paths() {
        let guest = AppSandboxGuest::new();
        converted(&guest);
        let root = guest.root();
        assert_eq!(
            fs::read(root.join("usr/local/lib/vmlord/vmlord-agent")).expect("read"),
            b"agent"
        );
        assert_eq!(
            fs::read_to_string(root.join("etc/vmlord/agent.secret")).expect("read"),
            "c2VjcmV0\n"
        );
        assert_eq!(
            fs::read_to_string(root.join("etc/systemd/system/vmlord-agent.service")).expect("read"),
            vmlord_seed::AGENT_UNIT
        );
    }

    #[test]
    fn the_secret_is_readable_by_root_alone_and_the_binary_is_executable() {
        let guest = AppSandboxGuest::new();
        converted(&guest);
        let mode = |path: &str| {
            fs::metadata(guest.root().join(path)).expect("metadata").permissions().mode() & 0o777
        };
        assert_eq!(mode("etc/vmlord/agent.secret"), 0o600);
        assert_eq!(mode("usr/local/lib/vmlord/vmlord-agent"), 0o755);
        assert_eq!(mode("etc/systemd/system/vmlord-agent.service"), 0o644);
    }

    #[test]
    fn the_unit_is_enabled_by_the_symlink_systemd_reads() {
        let guest = AppSandboxGuest::new();
        converted(&guest);
        let link = guest
            .root()
            .join("etc/systemd/system/multi-user.target.wants/vmlord-agent.service");
        assert_eq!(
            fs::read_link(&link).expect("a symlink"),
            std::path::Path::new(vmlord_seed::AGENT_UNIT_PATH)
        );
    }

    #[test]
    fn the_netplan_asks_for_an_address_and_names_the_renderer_that_runs() {
        let guest = AppSandboxGuest::new().with_network_manager();
        converted(&guest);
        let netplan =
            fs::read_to_string(guest.root().join(NETPLAN_PATH.trim_start_matches('/'))).expect("read");
        assert!(netplan.contains("dhcp4: true"), "{netplan}");
        assert!(netplan.contains("renderer: NetworkManager"), "{netplan}");
        assert!(!netplan.contains("$RENDERER"), "{netplan}");
    }

    #[test]
    fn the_hostname_becomes_the_one_the_document_names() {
        let guest = AppSandboxGuest::new();
        converted(&guest);
        assert_eq!(
            fs::read_to_string(guest.root().join("etc/hostname")).expect("read"),
            "imported\n"
        );
        let hosts = fs::read_to_string(guest.root().join("etc/hosts")).expect("read");
        assert!(hosts.contains("127.0.1.1\timported"), "{hosts}");
    }

    #[test]
    fn installing_twice_leaves_what_installing_once_did() {
        let guest = AppSandboxGuest::new();
        let conversion = converted(&guest);
        let facts = facts::read(&conversion).expect("facts");
        run(&conversion, &facts).expect("installed again");
        let keys = fs::read_to_string(guest.root().join("home/agromov/.ssh/authorized_keys"))
            .expect("read");
        assert_eq!(keys.matches("vmlord").count(), 1, "{keys}");
    }

    #[test]
    fn the_ssh_drop_ins_are_written_only_when_the_document_asks_for_them() {
        let guest = AppSandboxGuest::new();
        converted(&guest);
        assert!(!guest.root().join("etc/ssh/sshd_config.d/10-vmlord.conf").exists());

        let guest = AppSandboxGuest::new();
        let mut conversion = guest.conversion();
        conversion.ssh = Some(crate::SshDropIns {
            config_drop_in: "/etc/ssh/sshd_config.d/10-vmlord.conf".to_owned(),
            socket_drop_in: Some("/etc/systemd/system/ssh.socket.d/10-vmlord.conf".to_owned()),
            port: 2222,
        });
        let facts = facts::read(&conversion).expect("facts");
        run(&conversion, &facts).expect("installed");
        assert_eq!(
            fs::read_to_string(guest.root().join("etc/ssh/sshd_config.d/10-vmlord.conf"))
                .expect("read"),
            "Port 2222\n"
        );
        let socket =
            fs::read_to_string(guest.root().join("etc/systemd/system/ssh.socket.d/10-vmlord.conf"))
                .expect("read");
        assert!(socket.contains("ListenStream=\nListenStream=0.0.0.0:2222"), "{socket}");
    }
}
```

Note the `facts::read` in this test module needs `pub(crate)` visibility, which Task 2 already gives it. Note also that `facts::read` refuses a root that already has a `vmlord-agent` unit — the idempotency test therefore calls `install::run` twice against facts read *before* the first run, which is what the code above does.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vmlord-appsandbox-convert install`
Expected: FAIL — `install` does not exist.

- [ ] **Step 3: Write the installer**

`crates/appsandbox-convert/src/install.rs`:

```rust
//! Everything VMLord's own that an imported guest has to have.
//!
//! This runs before anything of AppSandbox's is taken away, so that the guest
//! is at no point left with neither stack.

use std::{
    fs,
    io::Write,
    os::unix::fs::{self as unix_fs, PermissionsExt},
    path::Path,
};

use vmlord_agent_protocol::auth::GUEST_SECRET_PATH;
use vmlord_seed::{AGENT_BINARY_PATH, AGENT_UNIT, AGENT_UNIT_NAME, AGENT_UNIT_PATH};

use crate::{
    Conversion, ConvertError,
    facts::{GuestFacts, Renderer},
    root::guest_path,
};

/// Where VMLord's network configuration goes.
///
/// Numbered like any other file rather than above the source application's,
/// because that one is removed rather than outranked: netplan merges what it
/// finds, and two documents claiming one interface is the failure that
/// outranking would leave in place.
pub(crate) const NETPLAN_PATH: &str = "/etc/netplan/90-vmlord.yaml";
const WANTS_DIRECTORY: &str = "/etc/systemd/system/multi-user.target.wants";

pub(crate) fn run(conversion: &Conversion, facts: &GuestFacts) -> Result<(), ConvertError> {
    authorized_key(conversion, facts)?;
    agent(conversion)?;
    netplan(conversion, facts)?;
    hostname(conversion)?;
    ssh(conversion)?;
    Ok(())
}

/// The VM's own key, and only it.
///
/// Written whole rather than appended to: the file holds the source
/// application's key and nothing else -- its agent wrote it with `"w"` on
/// every boot -- so there is nothing in it to preserve, and appending would
/// leave a key whose private half VMLord does not hold.
fn authorized_key(conversion: &Conversion, facts: &GuestFacts) -> Result<(), ConvertError> {
    let directory = facts.home.join(".ssh");
    let keys = directory.join("authorized_keys");
    create_dir_all(&directory)?;
    write(&keys, format!("{}\n", conversion.vmlord_public_key).as_bytes())?;
    set_mode(&directory, 0o700)?;
    set_mode(&keys, 0o600)?;
    own(&directory, facts.uid, facts.gid)?;
    own(&keys, facts.uid, facts.gid)?;
    Ok(())
}

fn agent(conversion: &Conversion) -> Result<(), ConvertError> {
    let binary = guest_path(&conversion.root, AGENT_BINARY_PATH)?;
    create_dir_all(binary.parent().expect("a parent"))?;
    let bytes = fs::read(&conversion.agent_binary).map_err(|error| {
        ConvertError::new(format!(
            "the agent at {} could not be read: {error}",
            conversion.agent_binary.display()
        ))
    })?;
    write(&binary, &bytes)?;
    set_mode(&binary, 0o755)?;
    own(&binary, 0, 0)?;

    let secret = guest_path(&conversion.root, GUEST_SECRET_PATH)?;
    create_dir_all(secret.parent().expect("a parent"))?;
    // Created, narrowed, and only then written: the secret never exists under
    // permissions wider than the ones it ends up with.
    let mut file = fs::File::create(&secret)
        .map_err(|error| ConvertError::new(format!("{} could not be created: {error}", secret.display())))?;
    set_mode(&secret, 0o600)?;
    file.write_all(format!("{}\n", conversion.agent_secret).as_bytes())
        .and_then(|()| file.flush())
        .map_err(|error| ConvertError::new(format!("{} could not be written: {error}", secret.display())))?;
    own(&secret, 0, 0)?;

    let unit = guest_path(&conversion.root, AGENT_UNIT_PATH)?;
    create_dir_all(unit.parent().expect("a parent"))?;
    write(&unit, AGENT_UNIT.as_bytes())?;
    set_mode(&unit, 0o644)?;
    own(&unit, 0, 0)?;

    // The symlink *is* the enablement: `systemctl enable` writes exactly this
    // and nothing else, so an offline conversion writes it directly rather
    // than asking a systemd that is not running.
    let wants = guest_path(&conversion.root, WANTS_DIRECTORY)?;
    create_dir_all(&wants)?;
    let link = wants.join(AGENT_UNIT_NAME);
    let _ = fs::remove_file(&link);
    unix_fs::symlink(AGENT_UNIT_PATH, &link)
        .map_err(|error| ConvertError::new(format!("{} could not be linked: {error}", link.display())))
}

fn netplan(conversion: &Conversion, facts: &GuestFacts) -> Result<(), ConvertError> {
    let path = guest_path(&conversion.root, NETPLAN_PATH)?;
    create_dir_all(path.parent().expect("a parent"))?;
    let document = format!(
        "network:\n  version: 2\n  renderer: {}\n  ethernets:\n    vmlordnic:\n      match: {{ name: \"e*\" }}\n      dhcp4: true\n      dhcp6: false\n",
        match facts.renderer {
            Renderer::NetworkManager => Renderer::NetworkManager,
            Renderer::Networkd => Renderer::Networkd,
        }
        .name()
    );
    write(&path, document.as_bytes())?;
    set_mode(&path, 0o600)
}

fn hostname(conversion: &Conversion) -> Result<(), ConvertError> {
    let path = guest_path(&conversion.root, "/etc/hostname")?;
    write(&path, format!("{}\n", conversion.hostname).as_bytes())?;

    let hosts_path = guest_path(&conversion.root, "/etc/hosts")?;
    let hosts = fs::read_to_string(&hosts_path).unwrap_or_default();
    let mut lines: Vec<String> = hosts
        .lines()
        .filter(|line| !line.starts_with("127.0.1.1"))
        .map(ToOwned::to_owned)
        .collect();
    lines.push(format!("127.0.1.1\t{}", conversion.hostname));
    write(&hosts_path, format!("{}\n", lines.join("\n")).as_bytes())
}

fn ssh(conversion: &Conversion) -> Result<(), ConvertError> {
    let Some(drop_ins) = &conversion.ssh else {
        return Ok(());
    };
    let config = guest_path(&conversion.root, &drop_ins.config_drop_in)?;
    create_dir_all(config.parent().expect("a parent"))?;
    write(&config, format!("Port {}\n", drop_ins.port).as_bytes())?;

    if let Some(socket_drop_in) = &drop_ins.socket_drop_in {
        let socket = guest_path(&conversion.root, socket_drop_in)?;
        create_dir_all(socket.parent().expect("a parent"))?;
        // The empty `ListenStream=` is not a stray line: systemd appends to a
        // list setting, so without it the socket would listen on the
        // distribution's port as well as this one.
        write(
            &socket,
            format!(
                "[Socket]\nListenStream=\nListenStream=0.0.0.0:{port}\nListenStream=[::]:{port}\n",
                port = drop_ins.port
            )
            .as_bytes(),
        )?;
    }
    Ok(())
}

fn create_dir_all(path: &Path) -> Result<(), ConvertError> {
    fs::create_dir_all(path)
        .map_err(|error| ConvertError::new(format!("{} could not be created: {error}", path.display())))
}

fn write(path: &Path, bytes: &[u8]) -> Result<(), ConvertError> {
    fs::write(path, bytes)
        .map_err(|error| ConvertError::new(format!("{} could not be written: {error}", path.display())))
}

fn set_mode(path: &Path, mode: u32) -> Result<(), ConvertError> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|error| ConvertError::new(format!("{} could not be given mode {mode:o}: {error}", path.display())))
}

/// Ownership, where the conversion runs with the privilege to set it.
///
/// A conversion run without it -- a fixture in a test, a dry run by hand --
/// still writes every file and every mode; only the owner is left as it was,
/// and the verification says so.
fn own(path: &Path, uid: u32, gid: u32) -> Result<(), ConvertError> {
    match unix_fs::chown(path, Some(uid), Some(gid)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            tracing::warn!("{} keeps its owner: {error}", path.display());
            Ok(())
        }
        Err(error) => Err(ConvertError::new(format!(
            "{} could not be given owner {uid}:{gid}: {error}",
            path.display()
        ))),
    }
}
```

Add `mod install;` to `crates/appsandbox-convert/src/lib.rs`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p vmlord-appsandbox-convert`
Expected: PASS, 22 tests.

- [ ] **Step 5: Commit**

```bash
git add crates/appsandbox-convert
git commit -m "TASK-21: Install VMLord's guest contract into a mounted root"
```

---

### Task 5: Remove AppSandbox's guest state

**Files:**
- Create: `crates/appsandbox-convert/src/remove.rs`
- Modify: `crates/appsandbox-convert/src/lib.rs`, `crates/appsandbox-convert/src/fixture.rs`

**Interfaces:**
- Consumes: `Conversion`, `guest_path`, `ConvertError`.
- Produces: `remove::run(conversion: &Conversion) -> Result<(), ConvertError>`; the inventory constants `remove::UNITS: [&str; 6]`, `remove::FILES: [&str; 27]`, `remove::TREES: [&str; 6]`, `remove::DKMS_PACKAGES: [&str; 2]`.

- [ ] **Step 1: Extend the fixture and write the failing tests**

In `crates/appsandbox-convert/src/fixture.rs`, extend `FILES` with the rest of the inventory so the removal has something to remove — add these entries:

```rust
    ("/etc/systemd/system/appsandbox-audio.service", "[Unit]\n"),
    ("/etc/systemd/system/appsandbox-display.service", "[Unit]\n"),
    ("/etc/systemd/system/appsandbox-input.service", "[Unit]\n"),
    ("/etc/systemd/system/asb-evict-simpledrm.service", "[Unit]\n"),
    ("/etc/systemd/user/org.gnome.Shell@.service.d/no-gpu.conf", "[Service]\n"),
    ("/etc/modules-load.d/dxgkrnl.conf", "dxgkrnl\n"),
    ("/etc/modules-load.d/snd-aloop.conf", "snd-aloop\n"),
    ("/etc/ld.so.conf.d/wsl-mesa.conf", "/opt/wsl-mesa/lib\n"),
    ("/etc/ld.so.conf.d/appsandbox-wsl-deps.conf", "/opt/appsandbox/wsl-deps\n"),
    ("/etc/ld.so.conf.d/wsl.conf", "/usr/lib/wsl/lib\n"),
    ("/etc/vulkan/icd.d/dzn_icd.x86_64.json", "{}"),
    ("/etc/apt/appsandbox-sources.list.d/appsandbox-local.list", "deb [trusted=yes] file:/opt/appsandbox/local-apt\n"),
    ("/etc/default/grub.d/99-appsandbox-no-efifb.cfg", "GRUB_CMDLINE_LINUX_DEFAULT=\n"),
    ("/etc/appsandbox-admin-user", "agromov"),
    ("/etc/appsandbox-ssh-enabled", ""),
    ("/opt/wsl-mesa/lib/x86_64-linux-gnu/libGL.so", "ELF"),
    ("/usr/src/asb_drm-1.0.0/dkms.conf", "PACKAGE_NAME=asb_drm\n"),
    ("/usr/src/dxgkrnl-1.0.0/dkms.conf", "PACKAGE_NAME=dxgkrnl\n"),
    ("/var/lib/dkms/asb_drm/1.0.0/source", ""),
    ("/var/lib/dkms/dxgkrnl/1.0.0/source", ""),
    ("/lib/modules/6.14.0-24-generic/updates/dkms/asb_drm.ko", "ELF"),
    ("/lib/modules/6.14.0-24-generic/updates/dkms/dxgkrnl.ko", "ELF"),
    ("/usr/local/bin/nvidia-smi", "ELF"),
```

and change the `FILES` array length in its declaration from `21` to `44`. In `AppSandboxGuest::new`, add the four remaining enablement symlinks beside the one already there, for `appsandbox-audio.service`, `appsandbox-display.service`, `appsandbox-input.service` and `asb-evict-simpledrm.service`, each pointing at `/etc/systemd/system/<name>`.

`crates/appsandbox-convert/src/remove.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::run;
    use crate::fixture::AppSandboxGuest;

    fn cleared() -> AppSandboxGuest {
        let guest = AppSandboxGuest::new();
        run(&guest.conversion()).expect("removed");
        guest
    }

    #[test]
    fn every_appsandbox_unit_and_its_enablement_symlink_are_gone() {
        let guest = cleared();
        for unit in [
            "appsandbox-agent.service",
            "appsandbox-audio.service",
            "appsandbox-display.service",
            "appsandbox-input.service",
            "asb-evict-simpledrm.service",
        ] {
            assert!(!guest.root().join("etc/systemd/system").join(unit).exists(), "{unit}");
            assert!(
                guest
                    .root()
                    .join("etc/systemd/system/multi-user.target.wants")
                    .join(unit)
                    .symlink_metadata()
                    .is_err(),
                "{unit} is still enabled"
            );
        }
    }

    #[test]
    fn the_user_level_clipboard_unit_is_gone_too() {
        let guest = cleared();
        assert!(!guest.root().join("etc/systemd/user/appsandbox-clipboard.service").exists());
    }

    #[test]
    fn the_compositor_drop_in_that_unsets_vmlords_environment_is_gone() {
        let guest = cleared();
        assert!(
            !guest
                .root()
                .join("etc/systemd/user/org.gnome.Shell@.service.d/no-gpu.conf")
                .exists()
        );
    }

    #[test]
    fn the_environment_generator_that_competes_with_vmlords_is_gone() {
        let guest = cleared();
        assert!(
            !guest
                .root()
                .join("etc/systemd/user-environment-generators/50-appsandbox-gpu")
                .exists()
        );
    }

    #[test]
    fn both_dkms_trees_and_their_built_modules_are_gone() {
        let guest = cleared();
        for path in [
            "usr/src/asb_drm-1.0.0",
            "usr/src/dxgkrnl-1.0.0",
            "var/lib/dkms/asb_drm",
            "var/lib/dkms/dxgkrnl",
            "lib/modules/6.14.0-24-generic/updates/dkms/asb_drm.ko",
            "lib/modules/6.14.0-24-generic/updates/dkms/dxgkrnl.ko",
        ] {
            assert!(!guest.root().join(path).exists(), "{path}");
        }
    }

    #[test]
    fn the_mesa_tree_its_linker_lines_and_its_icd_are_gone() {
        let guest = cleared();
        for path in [
            "opt/wsl-mesa",
            "etc/ld.so.conf.d/wsl-mesa.conf",
            "etc/ld.so.conf.d/appsandbox-wsl-deps.conf",
            "etc/ld.so.conf.d/wsl.conf",
            "etc/vulkan/icd.d/dzn_icd.x86_64.json",
        ] {
            assert!(!guest.root().join(path).exists(), "{path}");
        }
    }

    #[test]
    fn the_module_configuration_that_blacklists_what_vmlord_expects_is_gone() {
        let guest = cleared();
        for path in [
            "etc/modprobe.d/asb_drm.conf",
            "etc/modules-load.d/asb_drm.conf",
            "etc/modules-load.d/dxgkrnl.conf",
            "etc/modules-load.d/snd-aloop.conf",
        ] {
            assert!(!guest.root().join(path).exists(), "{path}");
        }
    }

    #[test]
    fn the_static_netplan_goes_and_the_cloud_init_lock_stays() {
        let guest = cleared();
        assert!(!guest.root().join("etc/netplan/99-appsandbox.yaml").exists());
        assert!(
            guest
                .root()
                .join("etc/cloud/cloud.cfg.d/99-disable-network-config.cfg")
                .exists(),
            "cloud-init would write a second netplan for the same interface"
        );
    }

    #[test]
    fn the_staged_tree_the_markers_and_the_firstboot_program_are_gone() {
        let guest = cleared();
        for path in [
            "opt/appsandbox",
            "etc/appsandbox-admin-user",
            "etc/appsandbox-ssh-enabled",
            "var/lib/appsandbox-firstboot.done",
            "usr/local/bin/appsandbox-firstboot.sh",
            "etc/apt/appsandbox-sources.list.d",
            "etc/default/grub.d/99-appsandbox-no-efifb.cfg",
        ] {
            assert!(!guest.root().join(path).exists(), "{path}");
        }
    }

    #[test]
    fn what_describes_the_guest_rather_than_the_source_application_stays() {
        let guest = cleared();
        for path in ["etc/fstab", "usr/local/bin/nvidia-smi", "etc/passwd"] {
            let _ = path;
        }
        assert!(guest.root().join("usr/local/bin/nvidia-smi").exists());
        assert!(guest.root().join("etc/passwd").exists());
    }

    #[test]
    fn removing_twice_is_removing_once() {
        let guest = cleared();
        run(&guest.conversion()).expect("removed again");
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vmlord-appsandbox-convert remove`
Expected: FAIL — `remove` does not exist.

- [ ] **Step 3: Write the removal**

`crates/appsandbox-convert/src/remove.rs`:

```rust
//! Everything of AppSandbox's, and only it.
//!
//! The inventory is read out of AppSandbox's own sources: what its disk
//! builder wrote, what its first-boot script installed, and what its agent
//! wrote while it ran. Each entry is here because VMLord's own stack collides
//! with it or because it names a program the conversion is removing -- not
//! because it carries the application's name.

use std::{fs, path::Path};

use crate::{Conversion, ConvertError, root::guest_path};

/// The units whose files and enablement symlinks both go.
pub(crate) const UNITS: [&str; 6] = [
    "appsandbox-agent.service",
    "appsandbox-audio.service",
    "appsandbox-display.service",
    "appsandbox-input.service",
    "asb-evict-simpledrm.service",
    // A *user* unit: installed under `/etc/systemd/user`, not beside the rest.
    "appsandbox-clipboard.service",
];

/// Single files, each by its guest-absolute path.
pub(crate) const FILES: [&str; 27] = [
    "/usr/local/bin/appsandbox-agent",
    "/usr/local/bin/appsandbox-audio",
    "/usr/local/bin/appsandbox-clipboard",
    "/usr/local/bin/appsandbox-display",
    "/usr/local/bin/appsandbox-input",
    "/usr/local/bin/appsandbox-gpu",
    "/usr/local/bin/appsandbox-firstboot.sh",
    "/etc/systemd/system/appsandbox-firstboot.service",
    "/etc/systemd/system/multi-user.target.wants/appsandbox-firstboot.service",
    // Emits the same five Mesa variables VMLord's own generator emits, at
    // AppSandbox's prefix. Generators are additive: both would run.
    "/etc/systemd/user-environment-generators/50-appsandbox-gpu",
    // And this unsets those five names for the compositor -- VMLord's included,
    // since it is a drop-in on the same template unit.
    "/etc/systemd/user/org.gnome.Shell@.service.d/no-gpu.conf",
    "/etc/modprobe.d/asb_drm.conf",
    "/etc/modules-load.d/asb_drm.conf",
    // VMLord's GPU payload installs a dxgkrnl of its own.
    "/etc/modules-load.d/dxgkrnl.conf",
    "/etc/modules-load.d/snd-aloop.conf",
    "/etc/ld.so.conf.d/wsl-mesa.conf",
    "/etc/ld.so.conf.d/appsandbox-wsl-deps.conf",
    // Written per boot by the agent, listing the 9P mounts it made.
    "/etc/ld.so.conf.d/wsl.conf",
    "/etc/vulkan/icd.d/dzn_icd.x86_64.json",
    "/etc/default/grub.d/99-appsandbox-no-efifb.cfg",
    // A static address on a subnet AppSandbox served.
    "/etc/netplan/99-appsandbox.yaml",
    "/etc/appsandbox-admin-user",
    "/etc/appsandbox-ssh-enabled",
    "/etc/appsandbox-hostname",
    "/etc/appsandbox-timezone",
    "/etc/appsandbox-locale",
    "/var/lib/appsandbox-firstboot.done",
];

/// Directories removed whole.
pub(crate) const TREES: [&str; 6] = [
    "/opt/appsandbox",
    "/opt/wsl-mesa",
    "/etc/apt/appsandbox-sources.list.d",
    "/usr/src/asb_drm",
    "/usr/src/dxgkrnl",
    "/etc/systemd/user/org.gnome.Shell@.service.d",
];

/// The DKMS packages whose source, state and built modules go.
pub(crate) const DKMS_PACKAGES: [&str; 2] = ["asb_drm", "dxgkrnl"];

pub(crate) fn run(conversion: &Conversion) -> Result<(), ConvertError> {
    let root = &conversion.root;

    // The symlinks before the units they point at: a unit file removed first
    // leaves systemd a dangling want, which is a state neither program owns.
    for unit in UNITS {
        remove_file(&guest_path(root, "/etc/systemd/system/multi-user.target.wants")?.join(unit))?;
        remove_file(&guest_path(root, "/etc/systemd/user/graphical-session.target.wants")?.join(unit))?;
        remove_file(&guest_path(root, "/etc/systemd/system")?.join(unit))?;
        remove_file(&guest_path(root, "/etc/systemd/user")?.join(unit))?;
    }

    for file in FILES {
        remove_file(&guest_path(root, file)?)?;
    }

    for tree in TREES {
        // `/usr/src/<package>` is versioned: the constant names the stem and
        // every directory beginning with it goes.
        remove_matching_trees(&guest_path(root, tree)?)?;
    }

    for package in DKMS_PACKAGES {
        remove_tree(&guest_path(root, &format!("/var/lib/dkms/{package}"))?)?;
        remove_built_modules(root, package)?;
    }

    Ok(())
}

/// The `.ko` a DKMS install left under every kernel's `updates` directory.
fn remove_built_modules(root: &Path, package: &str) -> Result<(), ConvertError> {
    let modules = guest_path(root, "/lib/modules")?;
    let Ok(kernels) = fs::read_dir(&modules) else {
        return Ok(());
    };
    for kernel in kernels.flatten() {
        for directory in ["updates/dkms", "updates", &format!("updates/{package}")] {
            let built = kernel.path().join(directory);
            for extension in ["ko", "ko.zst", "ko.xz"] {
                remove_file(&built.join(format!("{package}.{extension}")))?;
            }
        }
    }
    Ok(())
}

/// Removes `stem` and every sibling whose name begins with `stem-`.
fn remove_matching_trees(stem: &Path) -> Result<(), ConvertError> {
    remove_tree(stem)?;
    let (Some(parent), Some(name)) = (stem.parent(), stem.file_name().and_then(|name| name.to_str()))
    else {
        return Ok(());
    };
    let Ok(entries) = fs::read_dir(parent) else {
        return Ok(());
    };
    let prefix = format!("{name}-");
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with(&prefix) {
            remove_tree(&entry.path())?;
        }
    }
    Ok(())
}

/// A path that is not there is the state this asks for, not a failure.
fn remove_file(path: &Path) -> Result<(), ConvertError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(ConvertError::new(format!(
            "{} could not be removed: {error}",
            path.display()
        ))),
    }
}

fn remove_tree(path: &Path) -> Result<(), ConvertError> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(ConvertError::new(format!(
            "{} could not be removed: {error}",
            path.display()
        ))),
    }
}
```

Add `mod remove;` to `crates/appsandbox-convert/src/lib.rs`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p vmlord-appsandbox-convert`
Expected: PASS, 33 tests.

- [ ] **Step 5: Commit**

```bash
git add crates/appsandbox-convert
git commit -m "TASK-21: Remove the source application's guest state"
```

---

### Task 6: The verification pass and the linker cache

**Files:**
- Create: `crates/appsandbox-convert/src/verify.rs`
- Modify: `crates/appsandbox-convert/src/lib.rs`

**Interfaces:**
- Consumes: everything from Tasks 1-5.
- Produces: `pub fn verify(conversion: &Conversion) -> Result<(), ConvertError>`; `ldconfig::run(root: &Path, runner: &LdconfigRunner)`; `pub type LdconfigRunner = Box<dyn Fn(&Path) -> Result<(), String> + Send + Sync>`.

- [ ] **Step 1: Write the failing tests**

`crates/appsandbox-convert/src/verify.rs`:

```rust
#[cfg(test)]
mod tests {
    use std::{
        fs,
        sync::{Arc, Mutex},
    };

    use super::{ldconfig, verify};
    use crate::{facts, fixture::AppSandboxGuest, install, remove};

    fn fully_converted() -> AppSandboxGuest {
        let guest = AppSandboxGuest::new();
        let conversion = guest.conversion();
        let facts = facts::read(&conversion).expect("facts");
        install::run(&conversion, &facts).expect("installed");
        remove::run(&conversion).expect("removed");
        guest
    }

    #[test]
    fn a_converted_root_passes() {
        let guest = fully_converted();
        verify(&guest.conversion()).expect("a converted guest");
    }

    #[test]
    fn an_unconverted_root_fails() {
        let guest = AppSandboxGuest::new();
        let error = verify(&guest.conversion()).expect_err("not converted");
        assert!(error.to_string().contains("vmlord-agent"), "{error}");
    }

    #[test]
    fn a_root_where_one_appsandbox_unit_came_back_fails() {
        let guest = fully_converted();
        fs::write(
            guest.root().join("etc/systemd/system/appsandbox-input.service"),
            "[Unit]\n",
        )
        .expect("write");
        let error = verify(&guest.conversion()).expect_err("still AppSandbox's");
        assert!(error.to_string().contains("appsandbox-input.service"), "{error}");
    }

    #[test]
    fn a_root_whose_secret_is_readable_by_more_than_root_fails() {
        use std::os::unix::fs::PermissionsExt;
        let guest = fully_converted();
        fs::set_permissions(
            guest.root().join("etc/vmlord/agent.secret"),
            fs::Permissions::from_mode(0o644),
        )
        .expect("chmod");
        let error = verify(&guest.conversion()).expect_err("too readable");
        assert!(error.to_string().contains("agent.secret"), "{error}");
    }

    #[test]
    fn a_root_whose_netplan_asks_for_no_address_fails() {
        let guest = fully_converted();
        fs::write(guest.root().join("etc/netplan/90-vmlord.yaml"), "network:\n")
            .expect("write");
        let error = verify(&guest.conversion()).expect_err("no address");
        assert!(error.to_string().contains("90-vmlord.yaml"), "{error}");
    }

    #[test]
    fn the_linker_cache_is_rebuilt_against_the_root_it_was_given() {
        let guest = AppSandboxGuest::new();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorder = seen.clone();
        let runner: ldconfig::LdconfigRunner = Box::new(move |root| {
            recorder.lock().expect("lock").push(root.to_path_buf());
            Ok(())
        });
        ldconfig::run(guest.root(), &runner);
        assert_eq!(seen.lock().expect("lock").as_slice(), [guest.root().to_path_buf()]);
    }

    #[test]
    fn a_linker_cache_that_cannot_be_rebuilt_does_not_fail_the_conversion() {
        let guest = AppSandboxGuest::new();
        let runner: ldconfig::LdconfigRunner = Box::new(|_| Err("no ldconfig here".to_owned()));
        ldconfig::run(guest.root(), &runner);
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vmlord-appsandbox-convert verify`
Expected: FAIL — `verify` does not exist.

- [ ] **Step 3: Write the verification**

`crates/appsandbox-convert/src/verify.rs`:

```rust
//! Reading the converted root back.
//!
//! A separate pass over the same disk rather than a tally the conversion kept:
//! what the steps believed they wrote is not evidence, and this pass can be
//! run on its own against a root converted at some other time.

use std::{fs, os::unix::fs::PermissionsExt, path::Path};

use vmlord_agent_protocol::auth::GUEST_SECRET_PATH;
use vmlord_seed::{AGENT_BINARY_PATH, AGENT_UNIT_NAME, AGENT_UNIT_PATH};

use crate::{
    Conversion, ConvertError,
    install::NETPLAN_PATH,
    remove::{FILES, TREES, UNITS},
    root::guest_path,
};

/// Rebuilding the linker cache, which naming deleted directories has invalidated.
pub(crate) mod ldconfig {
    use std::{path::Path, process::Command};

    /// How the cache is rebuilt. A seam so that the tests can watch the call
    /// without a `ldconfig` on the machine running them.
    pub type LdconfigRunner = Box<dyn Fn(&Path) -> Result<(), String> + Send + Sync>;

    #[must_use]
    pub fn system() -> LdconfigRunner {
        Box::new(|root| {
            let output = Command::new("ldconfig")
                .arg("-r")
                .arg(root)
                .output()
                .map_err(|error| error.to_string())?;
            if output.status.success() {
                Ok(())
            } else {
                Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
            }
        })
    }

    /// A cache that cannot be rebuilt is reported and not fatal: it is
    /// rebuilt again by the guest on its next boot, and refusing the whole
    /// conversion over it would leave a guest that is otherwise converted.
    pub fn run(root: &Path, runner: &LdconfigRunner) {
        match runner(root) {
            Ok(()) => tracing::info!("rebuilt the linker cache under {}", root.display()),
            Err(error) => tracing::warn!(
                "the linker cache under {} was not rebuilt: {error}",
                root.display()
            ),
        }
    }
}

/// Reads the converted root back and reports the first thing that is not as
/// the conversion leaves it.
pub fn verify(conversion: &Conversion) -> Result<(), ConvertError> {
    let root = &conversion.root;

    let installed: [(&str, u32); 3] = [
        (AGENT_BINARY_PATH, 0o755),
        (GUEST_SECRET_PATH, 0o600),
        (AGENT_UNIT_PATH, 0o644),
    ];
    for (path, mode) in installed {
        let file = guest_path(root, path)?;
        let metadata = fs::metadata(&file)
            .map_err(|error| ConvertError::new(format!("{path} is not installed: {error}")))?;
        if metadata.permissions().mode() & 0o777 != mode {
            return Err(ConvertError::new(format!(
                "{path} does not have the permissions the conversion installs it with"
            )));
        }
    }

    let link = guest_path(root, "/etc/systemd/system/multi-user.target.wants")?.join(AGENT_UNIT_NAME);
    if fs::read_link(&link).ok().as_deref() != Some(Path::new(AGENT_UNIT_PATH)) {
        return Err(ConvertError::new(format!(
            "{AGENT_UNIT_NAME} is not enabled: {} does not point at it",
            link.display()
        )));
    }

    let netplan = guest_path(root, NETPLAN_PATH)?;
    let document = fs::read_to_string(&netplan)
        .map_err(|error| ConvertError::new(format!("{NETPLAN_PATH} is not there: {error}")))?;
    if !document.contains("dhcp4: true") {
        return Err(ConvertError::new(format!(
            "{NETPLAN_PATH} does not ask for an address"
        )));
    }
    if document.contains("$RENDERER") {
        return Err(ConvertError::new(format!(
            "{NETPLAN_PATH} never had its renderer filled in"
        )));
    }

    let keys = guest_path(root, "/etc/passwd").and_then(|_| {
        let home = crate::facts::home_of(conversion)?;
        Ok(home.join(".ssh/authorized_keys"))
    })?;
    let authorized = fs::read_to_string(&keys)
        .map_err(|error| ConvertError::new(format!("{} is not there: {error}", keys.display())))?;
    if !authorized.lines().any(|line| line == conversion.vmlord_public_key) {
        return Err(ConvertError::new(format!(
            "VMLord's key is not in {}",
            keys.display()
        )));
    }

    for unit in UNITS {
        for directory in ["/etc/systemd/system", "/etc/systemd/user"] {
            let path = guest_path(root, directory)?.join(unit);
            if path.exists() {
                return Err(ConvertError::new(format!("{unit} is still installed")));
            }
        }
        let want = guest_path(root, "/etc/systemd/system/multi-user.target.wants")?.join(unit);
        if want.symlink_metadata().is_ok() {
            return Err(ConvertError::new(format!("{unit} is still enabled")));
        }
    }

    for file in FILES {
        if guest_path(root, file)?.exists() {
            return Err(ConvertError::new(format!("{file} is still there")));
        }
    }
    for tree in TREES {
        if guest_path(root, tree)?.exists() {
            return Err(ConvertError::new(format!("{tree} is still there")));
        }
    }

    Ok(())
}
```

In `crates/appsandbox-convert/src/facts.rs`, add the helper the verification borrows:

```rust
/// The named account's home under the root, for a pass that needs only that.
pub(crate) fn home_of(conversion: &Conversion) -> Result<PathBuf, ConvertError> {
    let (home, _, _) = account(conversion)?;
    Ok(conversion.root.join(home.trim_start_matches('/')))
}
```

Add `mod verify;` and `pub use verify::verify;` to `crates/appsandbox-convert/src/lib.rs`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p vmlord-appsandbox-convert`
Expected: PASS, 40 tests.

- [ ] **Step 5: Commit**

```bash
git add crates/appsandbox-convert
git commit -m "TASK-21: Read a converted root back rather than trust the steps"
```

---

### Task 7: The conversion itself

**Files:**
- Modify: `crates/appsandbox-convert/src/lib.rs`

**Interfaces:**
- Consumes: `facts::read`, `install::run`, `remove::run`, `verify`, `ldconfig`.
- Produces: `pub fn convert(conversion: &Conversion, ldconfig: &LdconfigRunner) -> Result<(), ConvertError>`; `pub use verify::ldconfig::{LdconfigRunner, system as system_ldconfig};`.

- [ ] **Step 1: Write the failing tests**

Append to `crates/appsandbox-convert/src/lib.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::{convert, verify};
    use crate::fixture::AppSandboxGuest;

    fn quiet_ldconfig() -> crate::LdconfigRunner {
        Box::new(|_| Ok(()))
    }

    #[test]
    fn a_converted_guest_verifies() {
        let guest = AppSandboxGuest::new();
        convert(&guest.conversion(), &quiet_ldconfig()).expect("converted");
        verify(&guest.conversion()).expect("verified");
    }

    #[test]
    fn a_conversion_refused_by_the_preconditions_changes_nothing() {
        let guest = AppSandboxGuest::new().without("/etc/os-release");
        assert!(convert(&guest.conversion(), &quiet_ldconfig()).is_err());
        assert!(
            guest.root().join("etc/systemd/system/appsandbox-agent.service").exists(),
            "a refused conversion removed something anyway"
        );
    }

    #[test]
    fn the_secret_never_reaches_an_error() {
        let guest = AppSandboxGuest::new();
        let mut conversion = guest.conversion();
        conversion.guest_username = "nobody-here".to_owned();
        let error = convert(&conversion, &quiet_ldconfig()).expect_err("refused");
        assert!(!error.to_string().contains("c2VjcmV0"), "{error}");
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vmlord-appsandbox-convert --lib tests`
Expected: FAIL — `convert` does not exist.

- [ ] **Step 3: Write the orchestration**

In `crates/appsandbox-convert/src/lib.rs`:

```rust
pub use verify::ldconfig::{LdconfigRunner, system as system_ldconfig};

/// Converts a mounted AppSandbox guest into a VMLord guest.
///
/// The order is the whole of the safety: the guest is refused before anything
/// is written, VMLord's own is installed before AppSandbox's is taken away, and
/// the root is read back afterwards rather than reported from what the steps
/// believed.
pub fn convert(conversion: &Conversion, ldconfig: &LdconfigRunner) -> Result<(), ConvertError> {
    tracing::info!("converting the guest at {}", conversion.root.display());
    let facts = facts::read(conversion)?;
    install::run(conversion, &facts)?;
    remove::run(conversion)?;
    verify::ldconfig::run(&conversion.root, ldconfig);
    verify(conversion)?;
    tracing::info!("converted the guest at {}", conversion.root.display());
    Ok(())
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p vmlord-appsandbox-convert`
Expected: PASS, 43 tests.

- [ ] **Step 5: Commit**

```bash
git add crates/appsandbox-convert
git commit -m "TASK-21: Convert a mounted guest in one refusable order"
```

---

### Task 8: The command that runs it

**Files:**
- Create: `crates/xtask/src/appsandbox_convert.rs`
- Modify: `crates/xtask/src/main.rs:14-52`, `crates/xtask/Cargo.toml`, `.cargo/config.toml`

**Interfaces:**
- Consumes: `vmlord_appsandbox_convert::{Conversion, convert, verify, system_ldconfig}`.
- Produces: `appsandbox_convert::run(arguments: impl Iterator<Item = String>) -> Result<(), String>`, dispatched from `main` on the task name `appsandbox-convert`.

- [ ] **Step 1: Write the failing test**

`crates/xtask/src/appsandbox_convert.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::Arguments;

    #[test]
    fn a_document_and_a_mode_are_parsed() {
        let arguments = Arguments::parse(
            ["--input", "/tmp/input.json"].into_iter().map(ToOwned::to_owned),
        )
        .expect("parsed");
        assert_eq!(arguments.input.to_string_lossy(), "/tmp/input.json");
        assert!(!arguments.verify_only);
    }

    #[test]
    fn verify_only_is_recognised() {
        let arguments = Arguments::parse(
            ["--input", "/tmp/input.json", "--verify-only"]
                .into_iter()
                .map(ToOwned::to_owned),
        )
        .expect("parsed");
        assert!(arguments.verify_only);
    }

    #[test]
    fn a_missing_input_is_refused_with_the_usage() {
        let error = Arguments::parse(std::iter::empty()).expect_err("refused");
        assert!(error.contains("--input"), "{error}");
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p xtask`
Expected: FAIL — the module does not exist.

- [ ] **Step 3: Write the command**

`crates/xtask/src/appsandbox_convert.rs`:

```rust
//! `cargo appsandbox-convert` -- the offline conversion, run against a root
//! mounted on this machine.
//!
//! The mount is not this command's business: under WSL the copy is attached
//! with `wsl --mount --vhd <copy> --bare` and its root partition mounted by
//! hand, and the same conversion runs against whatever root it is given.

use std::{fs, path::PathBuf};

use vmlord_appsandbox_convert::{Conversion, convert, system_ldconfig, verify};

pub(crate) struct Arguments {
    pub(crate) input: PathBuf,
    pub(crate) verify_only: bool,
}

const USAGE: &str = "usage: cargo appsandbox-convert --input <document.json> [--verify-only]";

impl Arguments {
    pub(crate) fn parse(arguments: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut input = None;
        let mut verify_only = false;
        let mut arguments = arguments;
        while let Some(argument) = arguments.next() {
            match argument.as_str() {
                "--input" => {
                    input = Some(PathBuf::from(
                        arguments.next().ok_or_else(|| format!("--input needs a path\n{USAGE}"))?,
                    ));
                }
                "--verify-only" => verify_only = true,
                other => return Err(format!("unknown argument `{other}`\n{USAGE}")),
            }
        }
        Ok(Self {
            input: input.ok_or_else(|| format!("--input is required\n{USAGE}"))?,
            verify_only,
        })
    }
}

pub(crate) fn run(arguments: impl Iterator<Item = String>) -> Result<(), String> {
    let arguments = Arguments::parse(arguments)?;
    let document = fs::read_to_string(&arguments.input)
        .map_err(|error| format!("{} could not be read: {error}", arguments.input.display()))?;
    let conversion = Conversion::from_json(&document).map_err(|error| error.to_string())?;

    if arguments.verify_only {
        return verify(&conversion).map_err(|error| error.to_string());
    }
    convert(&conversion, &system_ldconfig()).map_err(|error| error.to_string())
}
```

In `crates/xtask/src/main.rs`, add `mod appsandbox_convert;` beside the other modules and the arm:

```rust
        Some("appsandbox-convert") => appsandbox_convert::run(env::args().skip(2)),
```

In `crates/xtask/Cargo.toml`, add to `[dependencies]`:

```toml
vmlord-appsandbox-convert = { path = "../appsandbox-convert" }
```

In `.cargo/config.toml`, under `[alias]`:

```toml
# The offline AppSandbox conversion, run against a mounted guest root. Linux
# only, and root: it writes files into another system's filesystem and sets
# their owners. See docs/appsandbox-import.md.
appsandbox-convert = ["run", "-p", "xtask", "--", "appsandbox-convert"]
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p xtask && cargo test -p vmlord-appsandbox-convert`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/xtask .cargo/config.toml Cargo.lock
git commit -m "TASK-21: Run the offline conversion from a command"
```

---

### Task 9: A VM source that is a disk which already exists

**Files:**
- Modify: `crates/core/src/provisioning.rs:34-43`

**Interfaces:**
- Consumes: `Provisioning`, `SshAccess`, `SshDaemon`.
- Produces: `VmSource::ExistingDisk { path: String, provisioning: Provisioning, ssh_daemon: SshDaemon }`.

- [ ] **Step 1: Write the failing test**

Append to the test module in `crates/core/src/provisioning.rs`:

```rust
#[test]
fn an_existing_disk_carries_the_provisioning_a_seed_would_have_applied() {
    let source = VmSource::ExistingDisk {
        path: "D:\\vms\\imported\\disk.vhdx".to_owned(),
        provisioning: Provisioning {
            username: "agromov".to_owned(),
            password: None,
            ssh: SshAccess::Enabled {
                port: SshPort::new(22).expect("a port"),
                deploy_key: true,
            },
            locale: "en_US.UTF-8".to_owned(),
            keyboard: "us".to_owned(),
            timezone: "Etc/UTC".to_owned(),
            desktop: DesktopProfile::None,
        },
        ssh_daemon: crate::UBUNTU_SSH_DAEMON.clone(),
    };
    let VmSource::ExistingDisk { provisioning, .. } = &source else {
        panic!("the variant just built");
    };
    assert_eq!(provisioning.username, "agromov");
}
```

If `crates/core` has no publicly reachable `SshDaemon` value to clone in a test, build one inline from `SshDaemon { units: SshUnits::SocketActivated { socket: "ssh.socket".to_owned(), socket_drop_in: "/etc/systemd/system/ssh.socket.d/10-vmlord.conf".to_owned(), service: "ssh.service".to_owned() }, config_drop_in: "/etc/ssh/sshd_config.d/10-vmlord.conf".to_owned() }` instead of referencing a constant.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p vmlord-core`
Expected: FAIL — `VmSource` has no variant `ExistingDisk`.

- [ ] **Step 3: Add the variant**

In `crates/core/src/provisioning.rs`, inside `enum VmSource`:

```rust
    /// A disk that already holds a system, brought to VMLord's contract by
    /// something other than a seed.
    ///
    /// It carries a `Provisioning` because the record still has to say which
    /// account VMLord's key went into and on what port the daemon answers --
    /// and an `SshDaemon` because no distribution profile was chosen for it:
    /// the guest was observed, not selected.
    ExistingDisk {
        path: String,
        provisioning: Provisioning,
        ssh_daemon: SshDaemon,
    },
```

Add `SshDaemon` to the file's `use` list if it is not already there. Then compile and fix every `match` on `VmSource` the compiler reports — `crates/platform/src/create.rs`, `crates/platform/src/hcs_config.rs` and `crates/platform/src/repository.rs` each have some. Give each new arm the behaviour of `LocalMedia` for now (no seed, no tools volume); Task 10 replaces that where it matters.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p vmlord-core && cargo check-windows`
Expected: PASS; the check reports no errors.

- [ ] **Step 5: Commit**

```bash
git add crates/core crates/platform
git commit -m "TASK-21: Name a VM source that is a disk which already exists"
```

---

### Task 10: The adopt branch and the document it emits

**Files:**
- Modify: `crates/platform/src/create.rs:164-360,462-535`, `crates/platform/src/layout.rs`

**Interfaces:**
- Consumes: `VmSource::ExistingDisk` (Task 9), `vmlord_keys::generate`, `auth::Secret::generate`, `vmlord_seed::{AGENT_BINARY_PATH, AGENT_UNIT_PATH}`.
- Produces: `layout::import_input_path(vm_directory: &Path) -> PathBuf` (the file `import-input.json`); `VmCreationPipeline::create` accepting `VmSource::ExistingDisk` and writing that document.

- [ ] **Step 1: Write the failing tests**

Append to the test module in `crates/platform/src/create.rs`:

```rust
#[test]
fn adopting_a_disk_downloads_no_image_and_writes_no_seed() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let vm_directory = directory.path().join("imported");
    let disk = vm_directory.join("disk").join("system.vhdx");
    std::fs::create_dir_all(disk.parent().expect("a parent")).expect("mkdir");
    std::fs::write(&disk, b"a disk that already exists").expect("write");

    let imported = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let watcher = imported.clone();
    let pipeline = VmCreationPipeline::for_test(
        |_, _| panic!("an adopted disk is not created"),
        move |_, _, _, _| {
            watcher.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        },
        |_, _| Ok(()),
        |_, _| Ok(()),
        |_, _| Ok(()),
        |_| Ok(()),
    );
    let store = MetadataStore::new(directory.path().join("vms.json"));
    let request = adopt_request(&disk);

    pipeline
        .create(&store, &request, &vm_directory, &BuildMonitor::silent())
        .expect("adopted");

    assert!(!imported.load(std::sync::atomic::Ordering::SeqCst), "an image was imported");
    assert!(!layout::seed_path(&vm_directory).exists(), "a seed was written");
    assert_eq!(std::fs::read(&disk).expect("read"), b"a disk that already exists");
}

#[test]
fn adopting_a_disk_writes_the_document_the_conversion_consumes() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let vm_directory = directory.path().join("imported");
    let disk = vm_directory.join("disk").join("system.vhdx");
    std::fs::create_dir_all(disk.parent().expect("a parent")).expect("mkdir");
    std::fs::write(&disk, b"disk").expect("write");

    let pipeline = VmCreationPipeline::for_test(
        |_, _| Ok(()),
        |_, _, _, _| Ok(()),
        |_, _| Ok(()),
        |_, _| Ok(()),
        |_, _| Ok(()),
        |_| Ok(()),
    );
    let store = MetadataStore::new(directory.path().join("vms.json"));
    pipeline
        .create(&store, &adopt_request(&disk), &vm_directory, &BuildMonitor::silent())
        .expect("adopted");

    let document =
        std::fs::read_to_string(layout::import_input_path(&vm_directory)).expect("the document");
    assert!(document.contains("\"guest_username\": \"agromov\""), "{document}");
    assert!(document.contains("ssh-"), "{document}");
    assert!(document.contains("\"hostname\": \"imported\""), "{document}");
}

#[test]
fn an_adopted_disk_that_is_not_there_is_refused_before_the_directory_is_made() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let vm_directory = directory.path().join("imported");
    let pipeline = VmCreationPipeline::for_test(
        |_, _| Ok(()),
        |_, _, _, _| Ok(()),
        |_, _| Ok(()),
        |_, _| Ok(()),
        |_, _| Ok(()),
        |_| Ok(()),
    );
    let store = MetadataStore::new(directory.path().join("vms.json"));
    let error = pipeline
        .create(
            &store,
            &adopt_request(&vm_directory.join("disk").join("system.vhdx")),
            &vm_directory,
            &BuildMonitor::silent(),
        )
        .expect_err("refused");
    assert!(error.to_string().contains("does not exist"), "{error}");
}
```

Add the helper the three tests share, beside them:

```rust
/// A request that adopts `disk` as VM `imported`.
fn adopt_request(disk: &std::path::Path) -> VmCreateRequest {
    let mut request = super::tests::minimal_cloud_request();
    request.name = "imported".to_owned();
    let VmSource::CloudImage { provisioning, .. } = request.source.clone() else {
        panic!("the fixture is a cloud request");
    };
    request.source = VmSource::ExistingDisk {
        path: disk.to_string_lossy().into_owned(),
        provisioning,
        ssh_daemon: vmlord_core::SshDaemon {
            units: vmlord_core::SshUnits::SocketActivated {
                socket: "ssh.socket".to_owned(),
                socket_drop_in: "/etc/systemd/system/ssh.socket.d/10-vmlord.conf".to_owned(),
                service: "ssh.service".to_owned(),
            },
            config_drop_in: "/etc/ssh/sshd_config.d/10-vmlord.conf".to_owned(),
        },
    };
    request
}
```

If the existing test module has no `minimal_cloud_request()`, build the request inline from the same fields the module's other tests use.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test-windows -p vmlord-platform create`
Expected: FAIL — `layout::import_input_path` does not exist and the adopt arm behaves like local media.

- [ ] **Step 3: Write the adopt branch**

In `crates/platform/src/layout.rs`:

```rust
/// The document the offline conversion of an adopted disk consumes.
///
/// Written beside the VM rather than handed out: it names the VM's public key
/// and its agent secret, which the conversion is the only other holder of.
pub(crate) fn import_input_path(vm_directory: &Path) -> PathBuf {
    vm_directory.join("import-input.json")
}
```

In `crates/platform/src/create.rs`, inside `create`, the `ExistingDisk` arm of the source match:

```rust
                VmSource::ExistingDisk {
                    path,
                    provisioning,
                    ssh_daemon,
                } => {
                    // Nothing is written to the disk here: it already holds a
                    // system, and what brings that system to VMLord's contract
                    // is the offline conversion, not this pipeline.
                    let disk = Path::new(path);
                    if !disk.is_file() {
                        return Err(RepositoryError::new(format!(
                            "the disk to adopt does not exist: {}",
                            disk.display()
                        )));
                    }
                    monitor.report(BuildStep::Provisioning);
                    write_adoption(
                        vm_directory,
                        &request.name,
                        provisioning,
                        ssh_daemon,
                        agent.as_deref(),
                    )?;
                }
```

and, beside `write_provisioning`:

```rust
/// The VM's own key and secret, and the document that carries them to the
/// conversion.
///
/// The same two secrets a created VM gets, minted the same way -- but there is
/// no seed to put them in, because an adopted guest has no cloud-init to read
/// one. They reach the guest through the offline conversion instead, which is
/// why the document naming them is written here and nowhere else.
fn write_adoption(
    vm_directory: &Path,
    vm_name: &str,
    provisioning: &Provisioning,
    ssh_daemon: &SshDaemon,
    agent: Option<&[u8]>,
) -> Result<(), RepositoryError> {
    let pair = vmlord_keys::generate(vm_name)?;
    vm_key::write_key_pair(vm_directory, &pair)?;

    let agent_secret = agent.map(|_| auth::Secret::generate().to_base64());
    if let Some(agent_secret) = &agent_secret {
        write_restricted(
            &layout::agent_secret_path(vm_directory),
            format!("{}\n", agent_secret.as_str()).as_bytes(),
            "the agent secret",
        )?;
    }

    let ssh = match provisioning.ssh {
        SshAccess::Enabled { port, .. } => {
            let socket = match &ssh_daemon.units {
                SshUnits::SocketActivated { socket_drop_in, .. } => {
                    serde_json::Value::String(socket_drop_in.clone())
                }
                SshUnits::Service { .. } => serde_json::Value::Null,
            };
            serde_json::json!({
                "config_drop_in": ssh_daemon.config_drop_in,
                "socket_drop_in": socket,
                "port": port.get(),
            })
        }
        SshAccess::Disabled => serde_json::Value::Null,
    };

    let document = serde_json::json!({
        "root": "/mnt/vmlord-import",
        "guest_username": provisioning.username,
        "vmlord_public_key": pair.public_openssh(),
        "agent_secret": agent_secret.as_ref().map_or("", |secret| secret.as_str()),
        "agent_binary": "./vmlord-agent",
        "hostname": vm_name,
        "ssh": ssh,
    });
    let document = serde_json::to_string_pretty(&document).map_err(|error| {
        RepositoryError::new(format!("the import document could not be printed: {error}"))
    })?;
    write_restricted(
        &layout::import_input_path(vm_directory),
        document.as_bytes(),
        "the import document",
    )
}
```

`root` and `agent_binary` are placeholders the operator edits for the machine doing the mount — the document says so in the documentation Task 13 writes. Add the `use` items the new code needs (`Provisioning`, `SshAccess`, `SshDaemon`, `SshUnits`, `serde_json`), and add `serde_json` to `crates/platform/Cargo.toml` if it is not already a dependency.

In the same function's caller, make the mapping's `ssh` and `ssh_daemon` fields carry the adopted values:

```rust
            ssh: match &request.source {
                VmSource::LocalMedia { .. } => None,
                VmSource::CloudImage { provisioning, .. }
                | VmSource::ExistingDisk { provisioning, .. } => provisioning.ssh_config(),
            },
            ssh_daemon: match &request.source {
                VmSource::LocalMedia { .. } => None,
                VmSource::CloudImage { image, .. } => Some(image.profile.ssh.clone()),
                VmSource::ExistingDisk { ssh_daemon, .. } => Some(ssh_daemon.clone()),
            },
```

and make `agent` be read for an adopted disk too:

```rust
        let agent = match &request.source {
            VmSource::CloudImage { .. } | VmSource::ExistingDisk { .. } => (self.agent_reader)(),
            VmSource::LocalMedia { .. } => None,
        };
```

An adopted VM gets no tools volume — the agent reaches it through the conversion, not a seed — so leave `tools_path` as `None` for it:

```rust
        let tools_path = match &request.source {
            VmSource::CloudImage { .. } => agent.as_ref().map(|_| layout::tools_path(vm_directory)),
            VmSource::LocalMedia { .. } | VmSource::ExistingDisk { .. } => None,
        };
```

In `hcs_config.rs`, the adopted source's media is the disk itself and there is no seed to attach: give `ExistingDisk` the same treatment `LocalMedia` gets, minus the DVD device, so the configuration attaches the system disk alone.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test-windows -p vmlord-platform && cargo check-windows`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/platform
git commit -m "TASK-21: Build a VM record around a disk that already exists"
```

---

### Task 11: The subcommand that adopts a disk

**Files:**
- Modify: `crates/vmlord/src/main.rs`

**Interfaces:**
- Consumes: `load_backend(&AppSettings)`, `VmCreateRequest`, `VmSource::ExistingDisk`.
- Produces: `vmlord.exe adopt-disk --name <name> --disk <path> --username <user> [--ssh-port <port>]`, which prints the path of the written `import-input.json` and exits.

- [ ] **Step 1: Write the failing test**

Add to `crates/vmlord/src/main.rs` a `mod tests` (or extend the existing one):

```rust
#[cfg(test)]
mod adopt_tests {
    use super::AdoptArguments;

    #[test]
    fn every_value_the_adoption_needs_is_parsed() {
        let arguments = AdoptArguments::parse(
            [
                "--name", "imported",
                "--disk", "D:\\vms\\imported\\disk.vhdx",
                "--username", "agromov",
                "--ssh-port", "22",
            ]
            .into_iter()
            .map(ToOwned::to_owned),
        )
        .expect("parsed");
        assert_eq!(arguments.name, "imported");
        assert_eq!(arguments.username, "agromov");
        assert_eq!(arguments.ssh_port, Some(22));
    }

    #[test]
    fn a_name_is_required() {
        let error = AdoptArguments::parse(
            ["--disk", "d.vhdx", "--username", "a"].into_iter().map(ToOwned::to_owned),
        )
        .expect_err("refused");
        assert!(error.contains("--name"), "{error}");
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test-windows -p vmlord`
Expected: FAIL — `AdoptArguments` does not exist.

- [ ] **Step 3: Write the subcommand**

In `crates/vmlord/src/main.rs`, before the GUI is started:

```rust
/// What `adopt-disk` takes.
///
/// A subcommand rather than a screen: adopting a disk is the second half of an
/// import whose first half is a copy made outside VMLord and a conversion run
/// under WSL. When the import ships as a feature it gets a screen; until then
/// this is the seam that does not pretend the flow is finished.
struct AdoptArguments {
    name: String,
    disk: PathBuf,
    username: String,
    ssh_port: Option<u16>,
}

const ADOPT_USAGE: &str =
    "usage: vmlord adopt-disk --name <name> --disk <path> --username <user> [--ssh-port <port>]";

impl AdoptArguments {
    fn parse(arguments: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut name = None;
        let mut disk = None;
        let mut username = None;
        let mut ssh_port = None;
        let mut arguments = arguments;
        while let Some(argument) = arguments.next() {
            let mut value = || {
                arguments
                    .next()
                    .ok_or_else(|| format!("{argument} needs a value\n{ADOPT_USAGE}"))
            };
            match argument.as_str() {
                "--name" => name = Some(value()?),
                "--disk" => disk = Some(PathBuf::from(value()?)),
                "--username" => username = Some(value()?),
                "--ssh-port" => {
                    ssh_port = Some(
                        value()?
                            .parse()
                            .map_err(|_| format!("--ssh-port is not a port\n{ADOPT_USAGE}"))?,
                    );
                }
                other => return Err(format!("unknown argument `{other}`\n{ADOPT_USAGE}")),
            }
        }
        Ok(Self {
            name: name.ok_or_else(|| format!("--name is required\n{ADOPT_USAGE}"))?,
            disk: disk.ok_or_else(|| format!("--disk is required\n{ADOPT_USAGE}"))?,
            username: username.ok_or_else(|| format!("--username is required\n{ADOPT_USAGE}"))?,
            ssh_port,
        })
    }
}
```

and, at the top of `main`, before any window is opened:

```rust
    if std::env::args().nth(1).as_deref() == Some("adopt-disk") {
        return match adopt_disk(std::env::args().skip(2)) {
            Ok(path) => {
                println!("{}", path.display());
                std::process::ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("vmlord: {error}");
                std::process::ExitCode::FAILURE
            }
        };
    }
```

with `adopt_disk` building a `VmCreateRequest` whose source is `VmSource::ExistingDisk`, whose `provisioning` carries the given username, `SshAccess::Enabled { port, deploy_key: true }` for the given port (defaulting to 22) and the guest's own locale, keyboard and timezone left at the settings' defaults, whose `ssh_daemon` is Ubuntu's socket-activated profile, and calling the repository's create through `load_backend(&settings)`. It returns `layout`'s `import-input.json` path, which the repository reports back through the created mapping's VM directory.

If `main` currently returns `()`, change its signature to `std::process::ExitCode` and return `ExitCode::SUCCESS` at its end.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test-windows -p vmlord && cargo check-windows`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/vmlord
git commit -m "TASK-21: Adopt a converted disk from a subcommand"
```

---

### Task 12: The whole-workspace gate

**Files:** none — this task runs the repository's own checks and fixes what they report.

- [ ] **Step 1: Run the workspace tests**

Run: `cargo test -p vmlord-appsandbox-convert && cargo test -p xtask && cargo test-windows`
Expected: PASS. Fix any failure before continuing; do not adjust a test to match behaviour that is wrong.

- [ ] **Step 2: Run the Windows check and clippy**

Run: `cargo check-windows && cargo clippy --all-targets -- -D warnings`
Expected: no errors, no warnings.

- [ ] **Step 3: Run the agent build, which the conversion installs the output of**

Run: `cargo agent`
Expected: PASS — `target/x86_64-unknown-linux-musl/debug/vmlord-agent` exists.

- [ ] **Step 4: Commit anything the fixes changed**

```bash
git add -A
git commit -m "TASK-21: Close what the workspace checks reported"
```

If nothing changed, skip the commit.

---

### Task 13: Documentation

**Files:**
- Modify: `docs/appsandbox-import.md` (replacing the branch's version wholesale), `ARCHITECTURE.md`

- [ ] **Step 1: Rewrite the user documentation**

`docs/appsandbox-import.md` covers, in this order:

1. What the import promises about the source: it is read, never started, moved, modified or deleted; VMLord reads `vms.cfg` and copies `disk.vhdx`, and never touches AppSandbox's private key at all — the offline conversion needs no credential into the guest.
2. Prerequisites: `vm_storage_path` on a volume with room for the copy (≈154 GiB for the current VM) plus headroom; the source VM stopped; WSL2 with an elevated prompt for the mount.
3. The three steps, with the exact commands:
   - copy the disk to the VM's directory;
   - `vmlord.exe adopt-disk --name <name> --disk <path> --username <user>`, which prints the path of `import-input.json`;
   - edit that document's `root` and `agent_binary` for the machine doing the mount, then
     ```
     wsl --mount --vhd <copy> --bare
     sudo mount -t ext4 /dev/sdX2 /mnt/vmlord-import
     sudo cargo appsandbox-convert --input <import-input.json>
     sudo umount /mnt/vmlord-import
     wsl --unmount <copy>
     ```
   - start the VM; the first boot is the verification.
4. What the conversion changes in the guest — the inventory from the spec, summarised as three lists: what it installs, what it removes, and what it deliberately leaves alone (the account, the desktop, the locale, the packages, the sleep policy, `nvidia-smi`).
5. What is not supported: Windows guests, templates, unfinished installations, a running source VM, export back to AppSandbox.
6. Recovery: a conversion that refuses has changed nothing; a conversion that fails part-way leaves a copy that can be re-run against — the conversion is idempotent — or deleted, and the source VM is untouched either way.

- [ ] **Step 2: Update ARCHITECTURE.md**

Find the statement that AppSandbox VMs are not migrated and replace it with a paragraph naming: the offline conversion as a function over a mounted root in `vmlord-appsandbox-convert`; the adopt path in the creation pipeline; the fact that the mount mechanism is WSL today and a service VM when the import ships; and the fact that display and GPU provisioning reach an imported guest through the agent at runtime, exactly as they reach a created one.

- [ ] **Step 3: Commit**

```bash
git add docs/appsandbox-import.md ARCHITECTURE.md
git commit -m "TASK-21: Document the offline AppSandbox import"
```

---

### Task 14: The import itself

This task produces no code. It is the run the whole plan exists for, and its output is a working VM plus the notes that go into the task.

- [ ] **Step 1: Record the source before touching anything**

On Windows, with the AppSandbox VM stopped, record the SHA-256, size and modification time of `C:\ProgramData\AppSandbox\vms.cfg` and of `C:\ProgramData\AppSandbox\ubuntu\disk.vhdx`. These are compared again at the end: the promise is that the source is unchanged.

- [ ] **Step 2: Point VMLord's storage at a volume with room**

Set `vm_storage_path` to a directory on `D:`. Confirm at least 200 GiB free.

- [ ] **Step 3: Place the copy**

Copy `disk.vhdx` into the VM directory VMLord will use, as its system disk. (The copy the user has already made can be moved into place instead of copied again.)

- [ ] **Step 4: Adopt it**

```
vmlord.exe adopt-disk --name ubuntu-imported --disk D:\vms\ubuntu-imported\disk\system.vhdx --username agromov
```

Expected: the path of `import-input.json` on stdout, a VM listed in VMLord, and no download.

- [ ] **Step 5: Convert it**

Edit `root` and `agent_binary` in the document, then, from an elevated prompt and inside WSL as root, mount the copy and run `cargo appsandbox-convert --input <document>`.

Expected: the command exits zero, having run its own verification pass.

- [ ] **Step 6: Boot it once**

Start the VM in VMLord. Confirm, in this order: the guest takes an address from VMLord's DHCP; SSH opens with VMLord's key; the agent authenticates and reports the guest's identity; the display session appears; the GPU probe completes.

- [ ] **Step 7: Confirm the source is untouched**

Re-take the hashes from Step 1 and compare.

- [ ] **Step 8: Record what happened**

Add a comment to Vikunja #21 with the outcome, anything the first boot needed by hand (in particular whether the stale `video=efifb:off` kernel command line mattered — the spec's one open risk), and what that implies for the service-VM follow-up.

---

## Self-Review

**Spec coverage:** source facts → Task 14 Step 1/7; inventory removal → Task 5; VMLord contract → Tasks 3, 4; cloud-init decision → Task 5's `the_static_netplan_goes_and_the_cloud_init_lock_stays`; renderer → Tasks 2, 4; the directory-function boundary → Tasks 1-7 (no platform dependency in the crate); WSL as one mount mechanism among several → Task 8's command taking a root; ordering and idempotency → Tasks 4, 5, 7; verification pass → Task 6; registration and the adopt path → Tasks 9, 10, 11; the kernel-command-line risk → Task 5 (file removed) and Task 14 Step 8 (observed on the first boot); testing → every task, plus Task 12; documentation → Task 13.

**Placeholders:** the only deliberate placeholders are the `root` and `agent_binary` values in the emitted document, which name the machine doing the mount and cannot be known by the host that writes it; Task 13 documents that they are edited.

**Type consistency:** `Conversion`, `SshDropIns`, `GuestFacts`, `Renderer`, `ConvertError`, `LdconfigRunner`, `convert`, `verify`, `guest_path`, `AGENT_BINARY_PATH`, `AGENT_UNIT_PATH`, `AGENT_UNIT_NAME`, `AGENT_UNIT`, `GUEST_SECRET_PATH`, `NETPLAN_PATH`, `import_input_path`, `VmSource::ExistingDisk` are used under the same names and shapes in every task that touches them.
