//! The guest password, in the only form VMLord is willing to hand on.
//!
//! Hashing happens here, on the host, before the seed is built: what reaches
//! `vmlord-seed` -- and through it the seed volume, which stays attached to a
//! running VM -- is a `$6$` SHA-512-crypt entry, never the plaintext. The
//! plaintext lives in `Password`, which has no `Display` and a redacting
//! `Debug`, and it ends its journey at this function.
//!
//! AppSandbox did the same thing in `unix_password_hash`
//! (`src/backend_win/disk_util.c:2235`), down to drawing the salt from
//! `BCryptGenRandom`. Its SHA-512-crypt was hand-written in C; this is the
//! RustCrypto implementation, which is worth more than a second translation of
//! a specification whose every step is a chance to be subtly wrong.

use sha_crypt::{PasswordHasher, ShaCrypt};
use vmlord_core::{Password, RepositoryError};
use windows::Win32::Security::Cryptography::{BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom};

use crate::error::windows_error;

/// Bytes of randomness behind the salt.
///
/// Twelve bytes encode to exactly sixteen crypt-base64 characters, which is
/// the longest salt SHA-512-crypt reads; longer ones are truncated. That is 96
/// bits of entropy, which is what AppSandbox aimed for -- though it drew
/// sixteen characters from twelve bytes by cycling them, so several characters
/// there repeat. These twelve are used once each.
const SALT_BYTES: usize = 12;

/// Hashes `password` into a `$6$` SHA-512-crypt entry with a fresh salt.
///
/// The entry carries `rounds=5000` explicitly. That is the default the
/// specification defines, so the field changes nothing about the digest -- and
/// a hash that names its own cost is one fewer thing to infer when reading a
/// seed by hand.
pub fn hash_password(password: &Password) -> Result<String, RepositoryError> {
    let salt = random_salt()?;
    let hash = ShaCrypt::SHA512
        .hash_password_with_salt(password.as_str().as_bytes(), &salt)
        .map_err(|error| {
            let error = RepositoryError::new(format!("failed to hash the password: {error}"));
            tracing::error!("{error}");
            error
        })?;

    tracing::debug!("hashed the password into a SHA-512-crypt entry");
    Ok(hash.as_str().to_string())
}

/// Draws the salt from the system RNG.
///
/// The salt is not a secret -- it is printed in the hash it belongs to -- but
/// it has to be unpredictable, so it comes from `BCryptGenRandom` rather than
/// from anything cheaper.
fn random_salt() -> Result<[u8; SALT_BYTES], RepositoryError> {
    let mut salt = [0u8; SALT_BYTES];
    // SAFETY: `salt` is a live buffer for the whole call, and the system
    // preferred RNG needs no algorithm handle.
    unsafe { BCryptGenRandom(None, &mut salt, BCRYPT_USE_SYSTEM_PREFERRED_RNG) }
        .ok()
        .map_err(|error| {
            let error = windows_error("generate the password salt", None, error);
            tracing::error!("{error}");
            error
        })?;
    Ok(salt)
}

#[cfg(test)]
mod tests {
    use sha_crypt::{PasswordVerifier, ShaCrypt};
    use vmlord_core::Password;

    use super::hash_password;

    /// The published `$6$` vectors, from the specification's own test set.
    ///
    /// They pin the algorithm rather than this module's use of it: a hash that
    /// only agrees with itself would pass every other test here and still be
    /// rejected by the guest.
    const VECTORS: [(&str, &str); 3] = [
        (
            "$6$saltstring$svn8UoSVapNtMuq1ukKS4tPQd8iKwSMHWjl/O817G3uBnIFNjnQJuesI68u4OTLiBFdcbYEdFCoEOfaS35inz1",
            "Hello world!",
        ),
        (
            "$6$rounds=10000$saltstringsaltstring$OW1/O6BYHV6BcXZu8QVeXbDWra3Oeqh0sbHbbMCVNSnCM/UrjmM0Dp8vOuZeHBy/YTBmSK6H9qs/y3RnOaw5v.",
            "Hello world!",
        ),
        (
            "$6$rounds=5000$toolongsaltstring$lQ8jolhgVRVhY4b5pZKaysCLi0QBxGoNeKQzQ3glMhwllF7oGDZxUhx1yxdYcz/e1JSbq3y6JMxxl8audkUEm0",
            "This is just a test",
        ),
    ];

    /// The alphabet crypt encodes both the salt and the digest in.
    const CRYPT_ALPHABET: &str = "./0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

    /// Splits `$6$rounds=5000$<salt>$<digest>` into its salt and digest.
    fn parts(hash: &str) -> (&str, &str) {
        let fields: Vec<&str> = hash.split('$').collect();
        assert_eq!(fields.len(), 5, "unexpected hash shape: {hash}");
        assert_eq!(fields[1], "6");
        assert_eq!(fields[2], "rounds=5000");
        (fields[3], fields[4])
    }

    #[test]
    fn reproduces_the_published_vectors() {
        for (hash, password) in VECTORS {
            ShaCrypt::SHA512
                .verify_password(password.as_bytes(), hash)
                .unwrap_or_else(|error| panic!("vector {hash} should verify: {error}"));
        }
    }

    #[test]
    fn a_wrong_password_does_not_verify() {
        let (hash, password) = VECTORS[0];
        assert!(
            ShaCrypt::SHA512
                .verify_password(format!("{password} ").as_bytes(), hash)
                .is_err()
        );
    }

    #[test]
    fn hashes_a_password_into_an_entry_the_guest_accepts() {
        let hash = hash_password(&Password::new("correct horse battery staple")).unwrap();

        assert!(hash.starts_with("$6$"), "got {hash}");
        ShaCrypt::SHA512
            .verify_password(b"correct horse battery staple", hash.as_str())
            .expect("the entry must verify against the password it was made from");
    }

    #[test]
    fn the_entry_holds_no_trace_of_the_plaintext() {
        let hash = hash_password(&Password::new("hunter2")).unwrap();

        assert!(!hash.contains("hunter2"), "got {hash}");
    }

    #[test]
    fn the_salt_is_sixteen_characters_of_the_crypt_alphabet() {
        let hash = hash_password(&Password::new("swordfish")).unwrap();

        let (salt, digest) = parts(&hash);
        assert_eq!(salt.len(), 16, "got {salt}");
        assert_eq!(digest.len(), 86, "got {digest}");
        for field in [salt, digest] {
            assert!(
                field.chars().all(|c| CRYPT_ALPHABET.contains(c)),
                "got {field}"
            );
        }
    }

    /// A salt reused between two VMs would let one hash be tested against the
    /// other, which is the whole reason the salt is drawn per call.
    #[test]
    fn each_call_draws_a_fresh_salt() {
        let first = hash_password(&Password::new("swordfish")).unwrap();
        let second = hash_password(&Password::new("swordfish")).unwrap();

        assert_ne!(parts(&first).0, parts(&second).0);
        assert_ne!(first, second);
    }

    /// Non-ASCII passwords are hashed over their UTF-8 bytes, which is what
    /// Linux compares against on login.
    #[test]
    fn hashes_a_password_outside_ascii() {
        let hash = hash_password(&Password::new("пароль")).unwrap();

        ShaCrypt::SHA512
            .verify_password("пароль".as_bytes(), hash.as_str())
            .expect("the entry must verify against the password it was made from");
    }
}
