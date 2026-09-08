//! The one place a log file crosses into Windows Shell to be opened.

use std::{
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
};

use vmlord_core::RepositoryError;
use windows::{
    Win32::{
        Foundation::CloseHandle,
        UI::{
            Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW},
            WindowsAndMessaging::SW_SHOWNORMAL,
        },
    },
    core::PCWSTR,
};

use crate::error::windows_error;

/// Opens a log file with whatever application the user's system associates
/// with it.
///
/// The caller supplies the path this run's logging layer returned when it
/// opened the file. This boundary rejects spellings that cannot name an
/// existing regular file, then delegates the choice of editor to Windows
/// Shell rather than to an external process.
///
/// # Errors
///
/// Returns [`RepositoryError`] if the path cannot name a regular absolute
/// file, or if Windows refuses the open.
pub fn open_log_file(path: &Path) -> Result<(), RepositoryError> {
    let log_path = canonical_log_path(path)?;

    let file = wide_path(&log_path);
    let verb = wide("open");

    let mut shell_execute = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: PCWSTR(verb.as_ptr()),
        lpFile: PCWSTR(file.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };

    // `ShellExecuteExW` reads the strings during this call and fills
    // `hProcess` only when the association it launched created a process.
    // Every pointer above remains valid for that call because its backing
    // vectors stay in scope.
    unsafe { ShellExecuteExW(&mut shell_execute) }
        .map_err(|error| windows_error("open log file", None, error))?;

    if !shell_execute.hProcess.is_invalid() {
        // The process outlives this handoff. Closing our handle does not wait
        // for it and releases the only resource this boundary owns.
        if let Err(error) = unsafe { CloseHandle(shell_execute.hProcess) } {
            tracing::warn!(
                "the log file opened, but its process handle could not be closed: {error}"
            );
        }
    }

    Ok(())
}

fn canonical_log_path(path: &Path) -> Result<PathBuf, RepositoryError> {
    if !path.is_absolute() {
        return Err(RepositoryError::new(format!(
            "the log file path is not absolute: {}",
            path.display()
        )));
    }

    if path.as_os_str().encode_wide().any(|unit| unit == 0) {
        return Err(RepositoryError::new(
            "the log file path contains an embedded NUL character",
        ));
    }

    let canonical = std::fs::canonicalize(path).map_err(|error| {
        RepositoryError::new(format!(
            "the log file path cannot be canonicalized ({}): {error}",
            path.display()
        ))
    })?;

    if !canonical.is_file() {
        return Err(RepositoryError::new(format!(
            "the log file is not a regular file: {}",
            canonical.display()
        )));
    }

    Ok(canonical)
}

fn wide_path(path: &Path) -> Vec<u16> {
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path, path::PathBuf};

    use uuid::Uuid;

    use super::canonical_log_path;

    struct Fixture {
        directory: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let directory =
                std::env::temp_dir().join(format!("vmlord-log-opener-{}", Uuid::new_v4()));
            fs::create_dir_all(&directory).expect("test fixture directory should be created");
            Self { directory }
        }

        fn path(&self, name: &str) -> PathBuf {
            self.directory.join(name)
        }

        fn write(&self, name: &str) -> PathBuf {
            let path = self.path(name);
            fs::write(&path, b"log fixture").expect("test fixture file should be written");
            path
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.directory);
        }
    }

    #[test]
    fn a_relative_log_path_is_refused() {
        let error = canonical_log_path(Path::new("vmlord-20260908.log"))
            .expect_err("a log path must be absolute");

        assert!(error.to_string().contains("not absolute"), "{error}");
    }

    #[test]
    fn a_missing_log_file_is_refused() {
        let fixture = Fixture::new();

        let error = canonical_log_path(&fixture.path("vmlord-missing.log"))
            .expect_err("a log that was never written cannot be opened");

        assert!(
            error.to_string().contains("cannot be canonicalized"),
            "{error}"
        );
    }

    #[test]
    fn a_directory_is_refused() {
        let fixture = Fixture::new();
        let directory = fixture.path("vmlord-20260908.log");
        fs::create_dir(&directory).expect("test fixture directory should be created");

        let error = canonical_log_path(&directory).expect_err("a directory is not a log file");

        assert!(error.to_string().contains("not a regular file"), "{error}");
    }

    #[test]
    fn a_regular_log_file_is_canonicalized() {
        let fixture = Fixture::new();
        let log = fixture.write("vmlord-20260908-101500-000.log");

        let canonical = canonical_log_path(&log).expect("a regular log file is accepted");

        assert_eq!(
            canonical,
            fs::canonicalize(&log).expect("test fixture path should canonicalize")
        );
        assert!(canonical.is_file());
    }
}
