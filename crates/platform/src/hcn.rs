use std::ptr;

use windows::{
    Win32::System::HostComputeNetwork::{HcnCloseNetwork, HcnOpenNetwork},
    core::GUID,
};

use crate::error::windows_error;

/// An owned HCN network handle used by the Windows Host Network Service.
pub struct HcnNetwork(*mut core::ffi::c_void);

impl HcnNetwork {
    /// Opens an existing HCN network by its canonical 128-bit identifier.
    pub fn open(id: u128) -> Result<Self, vmlord_core::RepositoryError> {
        let id = GUID::from_u128(id);
        let mut network = ptr::null_mut();
        // SAFETY: `id` and the output pointer are valid for the call. On success HCN
        // transfers ownership of the returned handle to this wrapper.
        unsafe { HcnOpenNetwork(&id, &mut network, None) }
            .map_err(|error| windows_error("open HCN network", None, error))?;
        Ok(Self(network))
    }
}

impl Drop for HcnNetwork {
    fn drop(&mut self) {
        // SAFETY: This wrapper exclusively owns a handle returned by HCN.
        let _ = unsafe { HcnCloseNetwork(self.0) };
    }
}
