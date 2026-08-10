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

use vmlord_core::SshAccess;

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
    pub ssh: SshAccess,
    pub locale: &'a str,
    pub keyboard: &'a str,
    pub timezone: &'a str,
    /// The group that grants administrative rights: `sudo` or `wheel`.
    pub admin_group: &'a str,
    /// The units that carry the SSH daemon, disabled when SSH is off.
    pub ssh_units: &'a [String],
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
    log::debug!(
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
            SshAccess::Disabled => "SSH off",
            SshAccess::Enabled { .. } => "SSH on",
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

#[cfg(test)]
mod tests {
    use super::{SeedRequest, build};
    use vmlord_core::SshAccess;

    #[test]
    fn a_seed_carries_both_documents() {
        let seed = build(&SeedRequest {
            vm_name: "my-vm",
            instance_id: "vmlord-4f1c0e5a",
            username: "dev",
            password_hash: Some("$6$rounds=4096$salt$hash"),
            authorized_key: None,
            ssh: SshAccess::Enabled { deploy_key: false },
            locale: "en_US.UTF-8",
            keyboard: "us",
            timezone: "Europe/Moscow",
            admin_group: "sudo",
            ssh_units: &[],
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
            ssh: SshAccess::Enabled { deploy_key: true },
            locale: "en_US.UTF-8",
            keyboard: "us",
            timezone: "Europe/Moscow",
            admin_group: "sudo",
            ssh_units: &[],
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
