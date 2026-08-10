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
}
