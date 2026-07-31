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
#[must_use]
pub(crate) fn windows_error(
    operation: &str,
    vm_name: Option<&str>,
    error: Error,
) -> RepositoryError {
    hresult_to_repository_error(operation, vm_name, error.code().0)
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
}
