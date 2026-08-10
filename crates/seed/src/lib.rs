//! The NoCloud seed VMLord writes for cloud-init: two documents, and the rules
//! for printing them.
//!
//! The request is flat rather than a borrowed `Provisioning` for one reason:
//! `Provisioning` carries the password in the clear, and this crate has no
//! business seeing it. What arrives here is the `$6$` hash and the public key,
//! both produced elsewhere, so "no plaintext password in the document" is a
//! property of the types rather than a lucky outcome checked afterwards.

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
