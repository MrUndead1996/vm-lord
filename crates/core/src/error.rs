//! The error every repository operation fails with, and the context it keeps.
//!
//! The context is fields rather than prose because an error is usually
//! recorded far from where it was raised: by then the VM name and the Windows
//! code are the only things that let a reader find the operation in the log,
//! and a formatted sentence has already thrown them away.

use std::fmt;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepositoryError {
    message: String,
    vm: Option<String>,
    /// The Windows call that failed. `&'static str` because these are literals
    /// at every site, and an owned string would be an allocation for nothing.
    operation: Option<&'static str>,
    /// An HRESULT, unsigned. Win32 statuses are widened before they get here.
    ///
    /// A plain `u32` rather than a `windows` type: this crate does not depend
    /// on `windows`, and the conversion belongs in the layer that does.
    code: Option<u32>,
}

impl RepositoryError {
    /// An error with no Windows call behind it: a rejected request, a backend
    /// that does not support an operation, a picker that is not there.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            vm: None,
            operation: None,
            code: None,
        }
    }

    /// A failed Windows call.
    ///
    /// The code is positional and not optional, which is the point: an HRESULT
    /// a caller could forget is an HRESULT that gets forgotten.
    #[must_use]
    pub fn windows(
        operation: &'static str,
        vm: Option<&str>,
        code: u32,
        message: impl Into<String>,
    ) -> Self {
        Self {
            message: message.into(),
            vm: vm.map(ToString::to_string),
            operation: Some(operation),
            code: Some(code),
        }
    }

    /// The VM the failed operation was about, when it was about one.
    #[must_use]
    pub fn vm(&self) -> Option<&str> {
        self.vm.as_deref()
    }

    /// The Windows code, when Windows is what failed.
    #[must_use]
    pub fn code(&self) -> Option<u32> {
        self.code
    }
}

impl fmt::Display for RepositoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Some(operation) = self.operation else {
            return formatter.write_str(&self.message);
        };
        write!(formatter, "Windows API operation \"{operation}\"")?;
        if let Some(vm) = &self.vm {
            write!(formatter, " for VM \"{vm}\"")?;
        }
        write!(
            formatter,
            " failed (HRESULT 0x{:08X})",
            self.code.unwrap_or_default()
        )?;
        if !self.message.is_empty() {
            write!(formatter, ": {}", self.message)?;
        }
        Ok(())
    }
}

impl std::error::Error for RepositoryError {}

#[cfg(test)]
mod tests {
    use super::RepositoryError;

    #[test]
    fn a_windows_failure_reads_exactly_as_it_always_has() {
        // The text is load-bearing: a person reads it, and the platform layer
        // asserts it. Structuring the error must not restyle it.
        let error =
            RepositoryError::windows("open compute system", Some("dev-linux"), 0x8007_0005, "");

        assert_eq!(
            error.to_string(),
            "Windows API operation \"open compute system\" for VM \"dev-linux\" \
             failed (HRESULT 0x80070005)"
        );
    }

    #[test]
    fn windows_own_description_is_appended_when_there_is_one() {
        let error = RepositoryError::windows(
            "open compute system",
            Some("dev-linux"),
            0x8007_0005,
            "Access is denied.",
        );

        assert_eq!(
            error.to_string(),
            "Windows API operation \"open compute system\" for VM \"dev-linux\" \
             failed (HRESULT 0x80070005): Access is denied."
        );
    }

    #[test]
    fn a_failure_with_no_vm_names_none() {
        let error =
            RepositoryError::windows("open the host network service", None, 0x8007_0005, "");

        assert_eq!(
            error.to_string(),
            "Windows API operation \"open the host network service\" failed (HRESULT 0x80070005)"
        );
    }

    #[test]
    fn the_context_is_readable_as_fields_and_not_only_as_prose() {
        // The whole point: an error that surfaced three layers up can still be
        // recorded with its VM and its code as fields.
        let error =
            RepositoryError::windows("attach the endpoint", Some("dev-linux"), 0x803B_0014, "");

        assert_eq!(error.vm(), Some("dev-linux"));
        assert_eq!(error.code(), Some(0x803B_0014));
    }

    #[test]
    fn an_error_with_no_windows_behind_it_says_so_by_having_no_code() {
        let error = RepositoryError::new("a password must not be empty");

        assert_eq!(error.to_string(), "a password must not be empty");
        assert_eq!(error.code(), None);
        assert_eq!(error.vm(), None);
    }
}
