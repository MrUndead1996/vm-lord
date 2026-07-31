use windows::{
    Win32::System::HostComputeSystem::{
        HCS_OPERATION, HCS_SYSTEM, HcsCloseComputeSystem, HcsCloseOperation, HcsCreateOperation,
        HcsOpenComputeSystem,
    },
    core::HSTRING,
};

use crate::error::windows_error;

/// An owned HCS asynchronous-operation handle.
pub struct HcsOperation(HCS_OPERATION);

impl HcsOperation {
    /// Creates an HCS operation without a completion callback.
    #[must_use]
    pub fn new() -> Self {
        // SAFETY: A null context and no callback are supported by HCS. The returned
        // handle is closed by this wrapper.
        Self(unsafe { HcsCreateOperation(None, None) })
    }
}

impl Default for HcsOperation {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for HcsOperation {
    fn drop(&mut self) {
        // SAFETY: This wrapper exclusively owns the HCS operation handle.
        unsafe { HcsCloseOperation(self.0) };
    }
}

/// An owned HCS compute-system handle.
pub struct HcsSystem(HCS_SYSTEM);

impl HcsSystem {
    /// Opens an existing compute system by its stable VM identifier.
    pub fn open(
        vm_name: &str,
        requested_access: u32,
    ) -> Result<Self, vmlord_core::RepositoryError> {
        let hcs_name = HSTRING::from(vm_name);
        // SAFETY: `hcs_name` remains valid for the duration of the call. A successful
        // handle is transferred to this wrapper and closed by `Drop`.
        let handle = unsafe { HcsOpenComputeSystem(&hcs_name, requested_access) }
            .map_err(|error| windows_error("open compute system", Some(vm_name), error))?;
        Ok(Self(handle))
    }
}

impl Drop for HcsSystem {
    fn drop(&mut self) {
        // SAFETY: This wrapper exclusively owns the HCS system handle.
        unsafe { HcsCloseComputeSystem(self.0) };
    }
}
