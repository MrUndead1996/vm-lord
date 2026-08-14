//! GPU-PV assignment for an already running HCS compute system.

use std::time::Duration;

use vmlord_core::{GpuFailure, GpuMode, GpuStatusCode};

use crate::hcs::{HcsModifyFailure, HcsSystem};

/// Bounds an assignment operation that HCS accepted but did not complete.
const GPU_ASSIGNMENT_TIMEOUT: Duration = Duration::from_secs(30);

/// Applies a VM's desired GPU-PV mode to a running compute system.
///
/// The caller decides how to report a failure: GPU assignment is best effort,
/// so this service returns a [`GpuFailure`] instead of changing VM lifecycle.
#[derive(Default)]
pub struct GpuAssignmentService;

impl GpuAssignmentService {
    /// Applies `mode` once. `None` does nothing because it asks HCS for no GPU.
    pub fn assign(&self, system: &HcsSystem, mode: GpuMode) -> Result<(), GpuFailure> {
        let Some(document) = assignment_document(mode)? else {
            return Ok(());
        };

        system
            .modify(&document, GPU_ASSIGNMENT_TIMEOUT)
            .map(|_result| ())
            .map_err(assignment_failure)
    }
}

/// Builds the HCS GPU-resource update for a supported desired mode.
pub(crate) fn assignment_document(mode: GpuMode) -> Result<Option<String>, GpuFailure> {
    let assignment_mode = match mode {
        GpuMode::None => return Ok(None),
        GpuMode::Default => "Default",
        GpuMode::Mirror => "Mirror",
        GpuMode::Unknown(value) => {
            return Err(GpuFailure::new(
                GpuStatusCode::AssignmentFailed,
                format!("GPU mode {value} is not supported by this build"),
            ));
        }
    };

    serde_json::to_string(&serde_json::json!({
        "ResourcePath": "VirtualMachine/ComputeTopology/Gpu",
        "RequestType": "Update",
        "Settings": { "AssignmentMode": assignment_mode },
    }))
    .map(Some)
    .map_err(|error| {
        GpuFailure::new(
            GpuStatusCode::AssignmentFailed,
            format!("failed to serialize the HCS GPU assignment request: {error}"),
        )
    })
}

/// Renders a failed HCS assignment without interpreting its version-specific
/// result document.
pub(crate) fn assignment_failure(failure: HcsModifyFailure) -> GpuFailure {
    let detail = failure
        .result_detail
        .filter(|detail| !detail.is_empty())
        .map(|detail| format!("; HCS result detail: {detail}"))
        .unwrap_or_default();
    GpuFailure::new(
        GpuStatusCode::AssignmentFailed,
        format!(
            "HCS GPU assignment failed with HRESULT 0x{:08X}{detail}",
            failure.hresult
        ),
    )
}

#[cfg(test)]
mod tests {
    use vmlord_core::{GpuMode, GpuStatusCode};

    use super::{assignment_document, assignment_failure};
    use crate::hcs::HcsModifyFailure;

    #[test]
    fn default_mode_updates_the_gpu_resource() {
        // A wrong path or request type is accepted by neither the HCS GPU
        // contract nor a host that has to attach the default adapter.
        let document = assignment_document(GpuMode::Default).unwrap().unwrap();
        let value: serde_json::Value = serde_json::from_str(&document).unwrap();

        assert_eq!(value["ResourcePath"], "VirtualMachine/ComputeTopology/Gpu");
        assert_eq!(value["RequestType"], "Update");
        assert_eq!(value["Settings"]["AssignmentMode"], "Default");
    }

    #[test]
    fn mirror_mode_updates_the_gpu_resource() {
        // Mirror is not the legacy TryAll spelling: HCS receives this exact
        // mode to attach every present and future GPU.
        let document = assignment_document(GpuMode::Mirror).unwrap().unwrap();
        let value: serde_json::Value = serde_json::from_str(&document).unwrap();

        assert_eq!(value["Settings"]["AssignmentMode"], "Mirror");
    }

    #[test]
    fn none_mode_needs_no_hcs_request() {
        assert_eq!(assignment_document(GpuMode::None).unwrap(), None);
    }

    #[test]
    fn hcs_failure_includes_hresult_and_result_detail() {
        let failure = assignment_failure(HcsModifyFailure::new(
            0x8037_010D,
            Some(r#"{"Error":"GPU unavailable"}"#.into()),
        ));

        assert_eq!(failure.code, GpuStatusCode::AssignmentFailed);
        assert!(failure.message.contains("HRESULT 0x8037010D"));
        assert!(failure.message.contains("GPU unavailable"));
    }
}
