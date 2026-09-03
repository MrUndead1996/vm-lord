//! The NoCloud seed VMLord writes for cloud-init: two documents, and the rules
//! for printing them.
//!
//! The request is flat rather than a borrowed `Provisioning` for one reason:
//! `Provisioning` carries the password in the clear, and this crate has no
//! business seeing it. What arrives here is the `$6$` hash and the public key,
//! both produced elsewhere, so "no plaintext password in the document" is a
//! property of the types rather than a lucky outcome checked afterwards.

mod iso;
mod meta_data;
mod scalar;
mod user_data;

use vmlord_core::{KeyboardFile, PackageRefresh, SshAccess, SshDaemon};

/// Everything the two documents are printed from.
pub struct SeedRequest<'a> {
    /// Becomes `local-hostname`.
    pub vm_name: &'a str,
    /// Becomes `instance-id`. Formatted from the VM's `Uuid` by the caller,
    /// which keeps `uuid` out of this crate's dependencies.
    pub instance_id: &'a str,
    pub username: &'a str,
    /// The `$6$` SHA-512-crypt hash. `None` is a key-only login.
    pub password_hash: Option<&'a str>,
    /// The public key, in `authorized_keys` form.
    pub authorized_key: Option<&'a str>,
    /// Whether the guest runs an SSH daemon and, if it does, on what port.
    pub ssh: SshAccess,
    pub locale: &'a str,
    /// The XKB layout name the guest's keyboard is set to.
    pub keyboard: &'a str,
    /// The files that layout has to be written into, as the distribution's
    /// profile names them.
    ///
    /// Named by the profile rather than known here for the reason
    /// [`SeedRequest::ssh_daemon`] is: one distribution keeps the layout in a
    /// shell file, another splits the console from the graphical session, and
    /// a generator that knew both would have to be edited for the third.
    pub keyboard_files: &'a [KeyboardFile],
    pub timezone: &'a str,
    /// The group that grants administrative rights: `sudo` or `wheel`.
    pub admin_group: &'a str,
    /// How the distribution runs its SSH daemon: the units to disable when SSH
    /// is off, and the files to write when it is on.
    pub ssh_daemon: &'a SshDaemon,
    /// The VM's agent secret, base64 as `auth::Secret::to_base64` prints it.
    /// `None` is a VM whose guest runs no agent.
    ///
    /// Already encoded rather than a `Secret`, for the reason the password
    /// arrives here hashed: this crate prints documents and has no business
    /// holding the thing itself.
    pub agent_secret: Option<&'a str>,
    /// The packages that install the guest's desktop, from the distribution's
    /// own archives.
    ///
    /// Empty is a headless VM, which is not the same as a VM whose desktop
    /// failed to install: nothing was asked for, so nothing is missing. Names
    /// rather than a profile, because this crate prints documents and has no
    /// business knowing what GNOME is.
    pub desktop_packages: &'a [String],
    /// What the distribution needs done to its packages before those are
    /// installed.
    ///
    /// Read only when there is something to install: a headless VM adds no
    /// package, so there is nothing an upgrade would be making room for, and
    /// upgrading a guest it was never asked to touch is not this seed's
    /// business.
    pub package_refresh: PackageRefresh,
}

/// The two documents that go into the seed volume.
///
/// No `Debug`: `user_data` holds the password hash, and a hash has no business
/// in a log line.
pub struct Seed {
    pub user_data: String,
    pub meta_data: String,
}

/// Builds both documents.
///
/// Infallible by construction. Values arrive validated by
/// `Provisioning::validate`, which rejects control characters, and everything
/// else survives quoting, so there is no input this can refuse. Failure starts
/// in #59, where the documents meet a filesystem.
#[must_use]
pub fn build(request: &SeedRequest<'_>) -> Seed {
    tracing::debug!(
        "building a seed for VM \"{}\" ({}): user \"{}\", password {}, key {}, {}",
        request.vm_name,
        request.instance_id,
        request.username,
        if request.password_hash.is_some() {
            "hashed"
        } else {
            "unset"
        },
        if request.authorized_key.is_some() {
            "deployed"
        } else {
            "absent"
        },
        match request.ssh {
            SshAccess::Disabled => "SSH off".to_owned(),
            SshAccess::Enabled { port, .. } => format!("SSH on port {port}"),
        }
    );

    Seed {
        user_data: user_data::render(request),
        meta_data: meta_data::render(request),
    }
}

/// The label cloud-init hunts a NoCloud seed by. Uppercase because a volume
/// identifier is spelled in ISO9660's own alphabet, unlike the file names below.
const VOLUME_ID: &str = "CIDATA";

/// The names cloud-init opens inside the volume. Neither fits ISO9660's
/// alphabet -- a hyphen is not a d-character at any level -- and both are
/// written literally anyway, because that is what the guest has to see.
const USER_DATA: &str = "user-data";
const META_DATA: &str = "meta-data";

/// Packs the seed into the ISO9660 image the VM boots with.
///
/// Bytes rather than a file: this crate knows no filesystem, and the VM's
/// directory belongs to the platform layer that writes them out.
#[must_use]
pub fn image(seed: &Seed) -> Vec<u8> {
    iso::build(
        VOLUME_ID,
        &[
            (USER_DATA, seed.user_data.as_bytes()),
            (META_DATA, seed.meta_data.as_bytes()),
        ],
    )
}

/// The label the guest mounts the tools volume by.
///
/// A second volume rather than more files in the seed: the seed is per-VM and
/// carries secrets, and the agent is the same binary on every VM. Uppercase
/// for the reason `CIDATA` is.
const TOOLS_VOLUME_ID: &str = "VMLTOOLS";

/// The agent's name inside that volume, and the name the guest installs it
/// under. Spelled here and used by `user_data`, so the volume and the command
/// that copies out of it cannot disagree.
pub(crate) const AGENT_FILE: &str = "vmlord-agent";

/// Packs the guest agent into the read-only volume the first boot installs it
/// from.
///
/// One file, because that is what the guest needs and a second one would be a
/// second thing to keep in step with the commands in `user-data`.
#[must_use]
pub fn tools_image(agent: &[u8]) -> Vec<u8> {
    iso::build(TOOLS_VOLUME_ID, &[(AGENT_FILE, agent)])
}

/// The one SSH daemon description the crate's own tests borrow.
///
/// A static because `SeedRequest` borrows it and the tests build
/// `SeedRequest<'static>` fixtures; Ubuntu's because it is the profile VMLord
/// ships, and a test that agreed only with a made-up profile would prove
/// nothing about the seed a real VM gets.
#[cfg(test)]
pub(crate) static UBUNTU_SSH: std::sync::LazyLock<SshDaemon> =
    std::sync::LazyLock::new(|| vmlord_core::ubuntu().ssh);

/// Ubuntu's keyboard files, borrowed by the same fixtures and for the same
/// reason.
#[cfg(test)]
pub(crate) static UBUNTU_KEYBOARD: std::sync::LazyLock<Vec<KeyboardFile>> =
    std::sync::LazyLock::new(|| vmlord_core::ubuntu().keyboard);

#[cfg(test)]
mod tests {
    use super::{SeedRequest, UBUNTU_KEYBOARD, UBUNTU_SSH, build};
    use vmlord_core::{PackageRefresh, SshAccess, SshPort};

    /// The two constants the guest finds the agent by: the label `runcmd`
    /// mounts and the name it copies from.
    #[test]
    fn the_tools_image_carries_the_agent_on_a_vmltools_volume() {
        let agent = b"\x7fELF not really, but bytes are bytes".to_vec();

        let bytes = super::tools_image(&agent);

        assert_eq!(&bytes[16 * 2048 + 40..16 * 2048 + 48], b"VMLTOOLS");
        assert!(
            bytes
                .windows(super::AGENT_FILE.len())
                .any(|window| window == super::AGENT_FILE.as_bytes()),
            "the root should name the agent"
        );
        assert!(
            bytes
                .windows(agent.len())
                .any(|window| window == agent.as_slice()),
            "the agent should be stored whole"
        );
    }

    #[test]
    fn a_seed_carries_both_documents() {
        let seed = build(&SeedRequest {
            vm_name: "my-vm",
            instance_id: "vmlord-4f1c0e5a",
            username: "dev",
            password_hash: Some("$6$rounds=4096$salt$hash"),
            authorized_key: None,
            ssh: SshAccess::Enabled {
                deploy_key: false,
                port: SshPort::DEFAULT,
            },
            locale: "en_US.UTF-8",
            keyboard: "us",
            keyboard_files: &UBUNTU_KEYBOARD,
            timezone: "Europe/Moscow",
            admin_group: "sudo",
            ssh_daemon: &UBUNTU_SSH,
            agent_secret: None,
            desktop_packages: &[],
            package_refresh: PackageRefresh::Lists,
        });

        assert!(seed.user_data.starts_with("#cloud-config\n"));
        assert!(seed.meta_data.contains("instance-id: 'vmlord-4f1c0e5a'"));
    }

    /// The three constants cloud-init actually depends on, checked through the
    /// public entry point: the label it searches for and the two names it opens.
    #[test]
    fn the_image_carries_both_documents_on_a_cidata_volume() {
        let seed = build(&SeedRequest {
            vm_name: "my-vm",
            instance_id: "vmlord-4f1c0e5a",
            username: "dev",
            password_hash: None,
            authorized_key: Some("ssh-ed25519 AAAA vmlord"),
            ssh: SshAccess::Enabled {
                deploy_key: true,
                port: SshPort::DEFAULT,
            },
            locale: "en_US.UTF-8",
            keyboard: "us",
            keyboard_files: &UBUNTU_KEYBOARD,
            timezone: "Europe/Moscow",
            admin_group: "sudo",
            ssh_daemon: &UBUNTU_SSH,
            agent_secret: None,
            desktop_packages: &[],
            package_refresh: PackageRefresh::Lists,
        });

        let bytes = super::image(&seed);

        assert_eq!(&bytes[16 * 2048 + 40..16 * 2048 + 46], b"CIDATA");
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("user-data"), "the root should name user-data");
        assert!(text.contains("meta-data"), "the root should name meta-data");
        assert!(
            text.contains(&seed.user_data),
            "user-data should be stored whole"
        );
        assert!(
            text.contains(&seed.meta_data),
            "meta-data should be stored whole"
        );
    }
}
