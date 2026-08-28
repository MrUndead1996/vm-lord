//! The one place a verified update installer crosses into Windows process launch.

use std::{
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
};

use vmlord_core::RepositoryError;
use windows::{
    core::PCWSTR,
    Win32::{
        Foundation::CloseHandle,
        UI::{
            Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW},
            WindowsAndMessaging::SW_SHOWNORMAL,
        },
    },
};

use crate::error::windows_error;

const CLOSE_APPLICATIONS: &str = "/CLOSEAPPLICATIONS";
const RESTART_APPLICATIONS: &str = "/RESTARTAPPLICATIONS";

/// A verified installer ready for Inno Setup to run.
///
/// `elevated` records whether the installation being replaced is all-users.
/// A current-user installation must not cause a UAC prompt, so only the
/// all-users case uses Shell's `runas` verb.
#[derive(Clone, Debug)]
pub struct InstallerLaunch {
    pub path: PathBuf,
    pub elevated: bool,
}

impl InstallerLaunch {
    /// Makes the current-user launch request for `path`.
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            elevated: false,
        }
    }

    /// The Inno Setup arguments that close this copy before replacing it.
    ///
    /// They deliberately do not select an installation directory or scope:
    /// the existing installation's Inno Setup configuration remains the
    /// authority for both.
    #[must_use]
    pub fn arguments(&self) -> [&'static str; 2] {
        [CLOSE_APPLICATIONS, RESTART_APPLICATIONS]
    }
}

/// Starts a verified installer and returns as soon as Windows creates it.
///
/// The caller supplies the canonical download path after integrity validation.
/// This boundary rejects path spellings that are unsafe or cannot name the
/// expected executable, then delegates process creation to Windows Shell.
///
/// # Errors
///
/// Returns [`RepositoryError`] if the path cannot name a regular absolute
/// `.exe` file, or if Windows refuses to create the installer process.
pub fn launch_installer(request: &InstallerLaunch) -> Result<(), RepositoryError> {
    let installer_path = canonical_installer_path(&request.path)?;

    let installer = wide_path(&installer_path);
    let arguments = wide_arguments(request.arguments());
    let runas = request.elevated.then(|| wide("runas"));

    let mut shell_execute = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: runas
            .as_ref()
            .map_or(PCWSTR::null(), |value| PCWSTR(value.as_ptr())),
        lpFile: PCWSTR(installer.as_ptr()),
        lpParameters: PCWSTR(arguments.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };

    // `ShellExecuteExW` reads the strings during this call and fills `hProcess`
    // only after it has created the installer process. Every pointer above
    // remains valid for that call because its backing vectors stay in scope.
    unsafe { ShellExecuteExW(&mut shell_execute) }
        .map_err(|error| windows_error("launch installer", None, error))?;

    if !shell_execute.hProcess.is_invalid() {
        // The process outlives this handoff. Closing our handle does not wait
        // for it and releases the only resource this boundary owns.
        if let Err(error) = unsafe { CloseHandle(shell_execute.hProcess) } {
            tracing::warn!(
                "the update installer started, but its process handle could not be closed: {error}"
            );
        }
    }

    Ok(())
}

fn canonical_installer_path(path: &Path) -> Result<PathBuf, RepositoryError> {
    if !path.is_absolute() {
        return Err(RepositoryError::new(format!(
            "the update installer path is not absolute: {}",
            path.display()
        )));
    }

    if has_alternate_data_stream(path) {
        return Err(RepositoryError::new(format!(
            "the update installer path names an alternate data stream: {}",
            path.display()
        )));
    }

    if path.as_os_str().encode_wide().any(|unit| unit == 0) {
        return Err(RepositoryError::new(
            "the update installer path contains an embedded NUL character",
        ));
    }

    if !path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("exe"))
    {
        return Err(RepositoryError::new(format!(
            "the update installer is not an .exe file: {}",
            path.display()
        )));
    }

    let canonical = std::fs::canonicalize(path).map_err(|error| {
        RepositoryError::new(format!(
            "the update installer path cannot be canonicalized ({}): {error}",
            path.display()
        ))
    })?;

    if !canonical.is_file() {
        return Err(RepositoryError::new(format!(
            "the update installer is not a regular file: {}",
            canonical.display()
        )));
    }

    Ok(canonical)
}

fn has_alternate_data_stream(path: &Path) -> bool {
    let path = path.as_os_str().to_string_lossy();
    let allowed_drive_colon = match path.as_bytes() {
        [drive, b':', ..] if drive.is_ascii_alphabetic() => Some(1),
        [b'\\', b'\\', b'?', b'\\', drive, b':', ..] if drive.is_ascii_alphabetic() => Some(5),
        _ => None,
    };

    path.bytes()
        .enumerate()
        .any(|(index, byte)| byte == b':' && Some(index) != allowed_drive_colon)
}

fn wide_path(path: &Path) -> Vec<u16> {
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn wide_arguments(arguments: [&str; 2]) -> Vec<u16> {
    wide(&arguments.join(" "))
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::InstallerLaunch;

    // This catches an update becoming a silent installer invocation, or one
    // that overrides the install scope selected by the existing installation.
    #[test]
    fn an_update_waits_for_vmlord_to_exit_without_becoming_silent() {
        let request = InstallerLaunch::new(PathBuf::from(r"C:\Temp\VMLord-0.2.0-setup.exe"));

        assert_eq!(
            request.arguments(),
            ["/CLOSEAPPLICATIONS", "/RESTARTAPPLICATIONS"]
        );
    }
}
