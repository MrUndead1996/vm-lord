//! The SSH key pair a single VM is reachable by.
//!
//! One pair per VM rather than one pair for all of them: AppSandbox kept a
//! single key under `%ProgramData%\AppSandbox\ssh\id_appsandbox`, where the
//! compromise of one sandbox reached every other one.
//!
//! Generating a pair is portable and has nothing to say about Windows, so it
//! lives here; putting the private half on disk under a restricted ACL is
//! `vmlord-platform`'s business.

use ssh_key::{Algorithm, LineEnding, PrivateKey, rand_core::OsRng};
use vmlord_core::RepositoryError;
use zeroize::Zeroizing;

/// A VM's key pair, already in the two textual forms it is used in.
///
/// No `Debug`, by design: the private half must have no way of printing
/// itself. `Password` in `vmlord-core` protects the same thing the same way.
pub struct VmKeyPair {
    private_openssh: Zeroizing<String>,
    public_openssh: String,
}

impl VmKeyPair {
    /// The private half, as an OpenSSH PEM document with LF line endings and
    /// no passphrase.
    ///
    /// Unencrypted deliberately: VMLord connects to the guest by itself,
    /// without anyone to type a passphrase, and a passphrase stored next to
    /// the key it protects protects nothing. The file's DACL is the defence.
    #[must_use]
    pub fn private_openssh(&self) -> &str {
        &self.private_openssh
    }

    /// The public half, as a single `authorized_keys` line without a trailing
    /// newline.
    #[must_use]
    pub fn public_openssh(&self) -> &str {
        &self.public_openssh
    }
}

/// Generates a fresh ed25519 pair for the VM named `vm_name`.
pub fn generate(vm_name: &str) -> Result<VmKeyPair, RepositoryError> {
    let mut key = PrivateKey::random(&mut OsRng, Algorithm::Ed25519)
        .map_err(|error| failed("generate an ed25519 key pair", vm_name, &error))?;
    key.set_comment(comment_for(vm_name));

    let private_openssh = key
        .to_openssh(LineEnding::LF)
        .map_err(|error| failed("serialize the private key", vm_name, &error))?;
    let public_openssh = key
        .public_key()
        .to_openssh()
        .map_err(|error| failed("serialize the public key", vm_name, &error))?;

    log::debug!("generated an ed25519 key pair for VM \"{vm_name}\"");
    Ok(VmKeyPair {
        private_openssh,
        public_openssh,
    })
}

/// The comment the public key carries, so that a key found in a guest's
/// `authorized_keys` names where it came from.
///
/// Control characters are dropped rather than escaped: the comment is the tail
/// of a line in `authorized_keys`, and a newline inside it would start a
/// second entry.
fn comment_for(vm_name: &str) -> String {
    let name: String = vm_name.chars().filter(|c| !c.is_control()).collect();
    format!("vmlord@{name}")
}

fn failed(operation: &str, vm_name: &str, error: &ssh_key::Error) -> RepositoryError {
    let error = RepositoryError::new(format!(
        "failed to {operation} for VM \"{vm_name}\": {error}"
    ));
    log::error!("{error}");
    error
}

#[cfg(test)]
mod tests {
    use ssh_key::{Algorithm, PrivateKey, PublicKey};

    use super::generate;

    #[test]
    fn the_private_half_is_an_openssh_document_openssh_itself_can_read() {
        let pair = generate("dev-linux").expect("a key pair should be generated");

        assert!(
            pair.private_openssh()
                .starts_with("-----BEGIN OPENSSH PRIVATE KEY-----\n"),
            "{}",
            pair.private_openssh()
        );
        let parsed = PrivateKey::from_openssh(pair.private_openssh())
            .expect("the private key should parse back");
        assert_eq!(parsed.algorithm(), Algorithm::Ed25519);
        assert!(!parsed.is_encrypted(), "the key must carry no passphrase");
    }

    #[test]
    fn the_public_half_is_one_authorized_keys_line() {
        let pair = generate("dev-linux").expect("a key pair should be generated");

        assert!(pair.public_openssh().starts_with("ssh-ed25519 "));
        // The line goes into a YAML list inside user-data; a newline in it
        // would end the entry rather than sit inside it.
        assert!(!pair.public_openssh().contains('\n'));
        let parsed =
            PublicKey::from_openssh(pair.public_openssh()).expect("the public key should parse");
        assert_eq!(parsed.algorithm(), Algorithm::Ed25519);
    }

    /// The whole point of a key pair: the half deployed into the guest has to
    /// be the half the host's private key opens.
    #[test]
    fn the_two_halves_belong_to_each_other() {
        let pair = generate("dev-linux").expect("a key pair should be generated");

        let private =
            PrivateKey::from_openssh(pair.private_openssh()).expect("the private key should parse");
        let public =
            PublicKey::from_openssh(pair.public_openssh()).expect("the public key should parse");

        assert_eq!(private.public_key().key_data(), public.key_data());
    }

    #[test]
    fn every_vm_gets_a_key_of_its_own() {
        let one = generate("dev-linux").expect("a key pair should be generated");
        let other = generate("dev-linux").expect("a key pair should be generated");

        assert_ne!(one.public_openssh(), other.public_openssh());
    }

    #[test]
    fn the_comment_names_the_vm_the_key_belongs_to() {
        let pair = generate("dev-linux").expect("a key pair should be generated");

        assert!(
            pair.public_openssh().ends_with(" vmlord@dev-linux"),
            "{}",
            pair.public_openssh()
        );
    }

    /// The comment is the tail of an `authorized_keys` line, so a newline in
    /// the VM name is not a typo but a second entry in the file.
    #[test]
    fn a_control_character_in_the_name_never_reaches_the_comment() {
        let pair = generate("dev\nssh-rsa AAAA").expect("a key pair should be generated");

        assert_eq!(pair.public_openssh().lines().count(), 1);
        assert!(pair.public_openssh().ends_with(" vmlord@devssh-rsa AAAA"));
    }
}
