//! The VM's own SSH key pair on disk, and the DACL its secrets are written
//! under.
//!
//! The private half is one of the two secrets VMLord stores in a file -- the
//! other is the password hash inside `seed.iso` -- so the file carries an
//! explicit DACL rather than whatever the storage root hands down.
//! `restrict_to_owner` is shared with the seed for that reason: a hash left
//! readable beside a key that is not would be an odd place to stop.

use std::{
    fs::{self, File},
    io::{self, ErrorKind, Write},
    path::Path,
};

use vmlord_core::RepositoryError;
use vmlord_keys::VmKeyPair;
use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree},
        Security::{
            Authorization::{
                ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
                SDDL_REVISION_1, SE_FILE_OBJECT, SetNamedSecurityInfoW,
            },
            DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, GetSecurityDescriptorOwner,
            GetTokenInformation, OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
            PSECURITY_DESCRIPTOR, PSID, TOKEN_QUERY, TOKEN_USER, TokenUser,
        },
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    },
    core::{BOOL, HSTRING, PWSTR},
};

use crate::{error::windows_error, layout};

/// Writes `pair` into the VM's directory: the private half under a restricted
/// DACL, the public half beside it.
///
/// The order matters. The file is created empty, its DACL is narrowed, and
/// only then does the key go in: between `create_new` and
/// `SetNamedSecurityInfoW` the file carries whatever the storage root hands
/// down, and what sits there during that window must not be a private key.
pub fn write_key_pair(vm_directory: &Path, pair: &VmKeyPair) -> Result<(), RepositoryError> {
    let private_path = layout::ssh_key_path(vm_directory);
    let keys_directory = private_path
        .parent()
        .expect("the private key path always has a parent under vm_directory");
    fs::create_dir_all(keys_directory)
        .map_err(|error| io_failure("create the keys directory", keys_directory, &error))?;

    let mut file = match File::create_new(&private_path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            let error = RepositoryError::new(format!(
                "VM directory {} already has an SSH key",
                vm_directory.display()
            ));
            tracing::error!("{error}");
            return Err(error);
        }
        Err(error) => return Err(io_failure("create the private key", &private_path, &error)),
    };

    // A failure here is a failure of the whole write: the file exists, and
    // leaving a private key under inherited permissions is worse than failing.
    if let Err(error) = restrict_to_owner(&private_path) {
        drop(file);
        let _ = fs::remove_file(&private_path);
        return Err(error);
    }

    file.write_all(pair.private_openssh().as_bytes())
        .and_then(|()| file.flush())
        .map_err(|error| io_failure("write the private key", &private_path, &error))?;

    let public_path = layout::ssh_public_key_path(vm_directory);
    fs::write(&public_path, pair.public_openssh())
        .map_err(|error| io_failure("write the public key", &public_path, &error))?;

    tracing::debug!("wrote an SSH key pair into {}", keys_directory.display());
    Ok(())
}

/// Reads the VM's public key back, as an `authorized_keys` line.
///
/// A VM with no key at all is a normal state -- SSH can be turned off -- so an
/// absent file is `None` rather than a failure.
pub fn read_public_key(vm_directory: &Path) -> Result<Option<String>, RepositoryError> {
    let path = layout::ssh_public_key_path(vm_directory);
    match fs::read_to_string(&path) {
        Ok(key) => Ok(Some(key.trim_end().to_string())),
        Err(error) if error.kind() == ErrorKind::NotFound => {
            tracing::debug!("VM directory {} holds no SSH key", vm_directory.display());
            Ok(None)
        }
        Err(error) => Err(io_failure("read the public key", &path, &error)),
    }
}

fn io_failure(operation: &str, path: &Path, error: &io::Error) -> RepositoryError {
    let error = RepositoryError::new(format!(
        "failed to {operation} at {}: {error}",
        path.display()
    ));
    tracing::error!("{error}");
    error
}

/// A wide string the Windows API allocated and this process has to release.
struct LocalString(PWSTR);

impl Drop for LocalString {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the pointer came from an API documented to allocate it
            // with `LocalAlloc`, and this runs exactly once per value.
            unsafe {
                let _ = LocalFree(Some(HLOCAL(self.0.0.cast())));
            }
        }
    }
}

impl LocalString {
    fn to_owned_string(&self) -> String {
        // SAFETY: the pointer is a NUL-terminated wide string owned by `self`.
        unsafe { self.0.to_string() }.unwrap_or_default()
    }
}

/// A security descriptor the Windows API allocated and this process has to
/// release.
struct LocalDescriptor(PSECURITY_DESCRIPTOR);

impl Drop for LocalDescriptor {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: as for `LocalString` -- API-allocated, released once.
            unsafe {
                let _ = LocalFree(Some(HLOCAL(self.0.0)));
            }
        }
    }
}

/// Restricts `path` to SYSTEM, the Administrators group and the user VMLord
/// runs as, and makes that user the file's owner.
///
/// The user's own entry is not a concession: an administrator's unelevated
/// token does not carry the Administrators group, so without it
/// `ssh -i <path>` from an ordinary console fails with access denied -- and
/// being used by hand is what the key is for. It is also the exact shape
/// Win32-OpenSSH insists on: it refuses a key whose DACL is wider than the
/// owner plus SYSTEM and Administrators.
pub(crate) fn restrict_to_owner(path: &Path) -> Result<(), RepositoryError> {
    let sid = current_user_sid()?;
    // `FA` is full access; `P` protects the list from everything the parent
    // directory would otherwise hand down.
    let sddl = format!("O:{sid}D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;{sid})");
    apply_security_descriptor(path, &sddl)?;
    tracing::debug!(
        "restricted {} to SYSTEM, Administrators and {sid}",
        path.display()
    );
    Ok(())
}

/// The SID of the user this process runs as, in string form.
fn current_user_sid() -> Result<String, RepositoryError> {
    let mut token = HANDLE::default();
    // SAFETY: `GetCurrentProcess` returns a pseudo-handle that needs no
    // closing; `token` is closed below on both paths.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }
        .map_err(|error| fail("open the process token", None, error))?;

    let mut needed = 0u32;
    // SAFETY: the first call is the documented way of asking for the size; it
    // fails with ERROR_INSUFFICIENT_BUFFER and fills `needed`.
    let _ = unsafe { GetTokenInformation(token, TokenUser, None, 0, &mut needed) };
    let mut buffer = vec![0u8; needed as usize];
    // SAFETY: `buffer` is `needed` bytes long, which is the size the call
    // above asked for.
    let result = unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            Some(buffer.as_mut_ptr().cast()),
            needed,
            &mut needed,
        )
    };
    // SAFETY: `token` came from the successful `OpenProcessToken` above and is
    // closed exactly once here.
    let closed = unsafe { CloseHandle(token) };
    result.map_err(|error| fail("read the token user", None, error))?;
    closed.map_err(|error| fail("close the process token", None, error))?;

    // SAFETY: on success the buffer holds a `TOKEN_USER` followed by the SID
    // it points at, and `buffer` outlives the read below.
    let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
    let mut text = PWSTR::null();
    // SAFETY: `user.User.Sid` points into `buffer`, which is still alive.
    unsafe { ConvertSidToStringSidW(user.User.Sid, &mut text) }
        .map_err(|error| fail("convert the user SID to a string", None, error))?;
    Ok(LocalString(text).to_owned_string())
}

/// Applies the owner and the DACL spelled by `sddl` to `path`.
fn apply_security_descriptor(path: &Path, sddl: &str) -> Result<(), RepositoryError> {
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    // SAFETY: the string outlives the call, and the descriptor it allocates is
    // taken over by `LocalDescriptor` immediately below.
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            &HSTRING::from(sddl),
            SDDL_REVISION_1,
            &mut descriptor,
            None,
        )
    }
    .map_err(|error| fail("parse the security descriptor", Some(path), error))?;
    let descriptor = LocalDescriptor(descriptor);

    let mut dacl = std::ptr::null_mut();
    let mut present = BOOL(0);
    let mut defaulted = BOOL(0);
    // SAFETY: `descriptor` is a valid descriptor; the returned ACL points into
    // it and is used only while it is alive.
    unsafe { GetSecurityDescriptorDacl(descriptor.0, &mut present, &mut dacl, &mut defaulted) }
        .map_err(|error| fail("read the parsed DACL", Some(path), error))?;

    let mut owner = PSID::default();
    let mut owner_defaulted = BOOL(0);
    // SAFETY: as above -- the SID points into the live `descriptor`.
    unsafe { GetSecurityDescriptorOwner(descriptor.0, &mut owner, &mut owner_defaulted) }
        .map_err(|error| fail("read the parsed owner", Some(path), error))?;

    let wide = HSTRING::from(path.as_os_str().to_string_lossy().as_ref());
    // SAFETY: every pointer passed in outlives the call.
    let status = unsafe {
        SetNamedSecurityInfoW(
            &wide,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION
                | DACL_SECURITY_INFORMATION
                | PROTECTED_DACL_SECURITY_INFORMATION,
            Some(owner),
            None,
            Some(dacl),
            None,
        )
    };
    status
        .ok()
        .map_err(|error| fail("set the file's security descriptor", Some(path), error))
}

fn fail(
    operation: &'static str,
    path: Option<&Path>,
    error: windows::core::Error,
) -> RepositoryError {
    let error = match path {
        Some(path) => {
            let described = windows_error(operation, None, error);
            RepositoryError::new(format!("{described} on {}", path.display()))
        }
        None => windows_error(operation, None, error),
    };
    tracing::error!("{error}");
    error
}

/// Reads the owner and the DACL of `path` back as an SDDL string.
///
/// Only the tests need this: production sets the descriptor and never asks
/// what it became.
#[cfg(test)]
pub(crate) fn security_descriptor(path: &Path) -> Result<String, RepositoryError> {
    use windows::Win32::Security::Authorization::{
        ConvertSecurityDescriptorToStringSecurityDescriptorW, GetNamedSecurityInfoW,
    };

    let wide = HSTRING::from(path.as_os_str().to_string_lossy().as_ref());
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    // SAFETY: the path outlives the call; the descriptor it allocates is taken
    // over by `LocalDescriptor` below.
    let status = unsafe {
        GetNamedSecurityInfoW(
            &wide,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            None,
            None,
            None,
            None,
            &mut descriptor,
        )
    };
    status
        .ok()
        .map_err(|error| fail("read the file's security descriptor", Some(path), error))?;
    let descriptor = LocalDescriptor(descriptor);

    let mut text = PWSTR::null();
    // SAFETY: `descriptor` is alive for the duration of the call.
    unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            descriptor.0,
            SDDL_REVISION_1,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut text,
            None,
        )
    }
    .map_err(|error| fail("format the security descriptor", Some(path), error))?;
    Ok(LocalString(text).to_owned_string())
}

#[cfg(test)]
mod tests {
    use std::{
        fs::{self, File},
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::{read_public_key, restrict_to_owner, security_descriptor, write_key_pair};

    struct TempRoot(PathBuf);

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn temp_root(label: &str) -> TempRoot {
        static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "vmlord-vm-key-test-{label}-{}-{}",
            std::process::id(),
            NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("test root should be created");
        TempRoot(path)
    }

    #[test]
    fn a_restricted_file_is_reachable_by_system_administrators_and_the_owner_only() {
        let root = temp_root("restrict");
        let path = root.0.join("id_ed25519");
        File::create_new(&path).expect("the file should be created");

        restrict_to_owner(&path).expect("the DACL should be restricted");

        let descriptor = security_descriptor(&path).expect("the DACL should be read back");
        // `D:P` is what makes the list protected; without it the entries below
        // would sit on top of everything the storage root hands down.
        assert!(descriptor.contains("D:P"), "{descriptor}");
        assert!(descriptor.contains("(A;;FA;;;SY)"), "{descriptor}");
        assert!(descriptor.contains("(A;;FA;;;BA)"), "{descriptor}");
        assert!(
            !descriptor.contains(";ID;"),
            "no entry may be inherited any more: {descriptor}"
        );
        assert_eq!(
            descriptor.matches("(A;;FA;;;").count(),
            3,
            "SYSTEM, Administrators and the owner, and nobody else: {descriptor}"
        );
    }

    #[test]
    fn writing_a_pair_leaves_both_halves_in_the_vms_keys_directory() {
        let root = temp_root("write");
        let pair = vmlord_keys::generate("dev-linux").expect("a key pair should be generated");

        write_key_pair(&root.0, &pair).expect("the key pair should be written");

        let private = fs::read_to_string(crate::layout::ssh_key_path(&root.0))
            .expect("the private key should be on disk");
        assert_eq!(private, pair.private_openssh());
        assert_eq!(
            read_public_key(&root.0).expect("the public key should be readable"),
            Some(pair.public_openssh().to_string())
        );
    }

    #[test]
    fn the_private_key_is_written_with_a_restricted_dacl() {
        let root = temp_root("write-dacl");
        let pair = vmlord_keys::generate("dev-linux").expect("a key pair should be generated");

        write_key_pair(&root.0, &pair).expect("the key pair should be written");

        let descriptor = security_descriptor(&crate::layout::ssh_key_path(&root.0))
            .expect("the DACL should be read back");
        assert!(descriptor.contains("D:P"), "{descriptor}");
        assert!(!descriptor.contains(";ID;"), "{descriptor}");
    }

    /// A guest already holds the public half of the key it was given; handing
    /// the VM a new pair would leave the guest trusting a key the host no
    /// longer has. Re-keying is deleting the VM and creating it again.
    #[test]
    fn a_vm_that_already_has_a_key_is_never_given_another_one() {
        let root = temp_root("existing");
        let first = vmlord_keys::generate("dev-linux").expect("a key pair should be generated");
        let second = vmlord_keys::generate("dev-linux").expect("a key pair should be generated");
        write_key_pair(&root.0, &first).expect("the first key pair should be written");

        let message = write_key_pair(&root.0, &second)
            .expect_err("a second key pair must be refused")
            .to_string();

        assert!(message.contains("already has an SSH key"), "{message}");
        assert_eq!(
            read_public_key(&root.0).expect("the public key should be readable"),
            Some(first.public_openssh().to_string()),
            "the existing key must survive the refusal"
        );
    }

    /// A VM created with `ssh_enabled: false` has no key, and that is a state
    /// rather than a failure.
    #[test]
    fn a_vm_without_a_key_reports_no_key_rather_than_an_error() {
        let root = temp_root("keyless");

        assert_eq!(
            read_public_key(&root.0).expect("a keyless VM is not a failure"),
            None
        );
    }

    #[test]
    fn restricting_a_file_that_is_not_there_fails_with_the_path_in_the_message() {
        let root = temp_root("absent");
        let path = root.0.join("never-created");

        let message = restrict_to_owner(&path)
            .expect_err("a missing file cannot be restricted")
            .to_string();

        assert!(message.contains("never-created"), "{message}");
    }
}
