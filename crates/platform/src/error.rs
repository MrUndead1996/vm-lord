use vmlord_core::RepositoryError;
use windows::core::Error;

/// Converts a Windows HRESULT to an error suitable for the repository boundary.
#[must_use]
pub fn hresult_to_repository_error(
    operation: &str,
    vm_name: Option<&str>,
    hresult: i32,
) -> RepositoryError {
    let target = vm_name
        .map(|name| format!(" for VM \"{name}\""))
        .unwrap_or_default();

    RepositoryError::new(format!(
        "Windows API operation \"{operation}\"{target} failed (HRESULT 0x{:08X})",
        hresult as u32
    ))
}

/// Converts a `windows-rs` error while retaining the failed operation and VM.
///
/// Windows' own description of the HRESULT is included: an HCS error code
/// alone ("0x8037010D") names nothing a reader can act on, and looking one up
/// takes a table that is not in this repository.
#[must_use]
pub(crate) fn windows_error(
    operation: &str,
    vm_name: Option<&str>,
    error: Error,
) -> RepositoryError {
    let described = hresult_to_repository_error(operation, vm_name, error.code().0);
    let message = error.message();
    if message.is_empty() {
        return described;
    }
    RepositoryError::new(format!("{described}: {message}"))
}

#[cfg(test)]
mod tests {

    use super::hresult_to_repository_error;

    #[test]
    fn includes_operation_vm_name_and_hresult() {
        let error = hresult_to_repository_error(
            "open compute system",
            Some("dev-linux"),
            0x8007_0005_u32 as i32,
        );

        assert_eq!(
            error.to_string(),
            "Windows API operation \"open compute system\" for VM \"dev-linux\" failed (HRESULT 0x80070005)"
        );
    }

    #[test]
    fn a_windows_error_carries_the_systems_own_description() {
        let error = super::windows_error(
            "open compute system",
            Some("dev-linux"),
            windows::core::Error::from_hresult(windows::core::HRESULT(0x8007_0005_u32 as i32)),
        );

        let message = error.to_string();
        assert!(message.contains("0x80070005"), "{message}");
        assert!(
            message.len() > "Windows API operation \"open compute system\" for VM \"dev-linux\" failed (HRESULT 0x80070005)".len(),
            "the HRESULT alone names nothing actionable; Windows' description must be appended: {message}"
        );
    }
}
