use std::{path::Path, time::Duration};

use windows::{
    Win32::{
        Foundation::{
            ERROR_NOT_SUPPORTED, ERROR_TIMEOUT, HCS_E_SYSTEM_NOT_FOUND, HLOCAL, LocalFree,
        },
        System::HostComputeSystem::{
            HCS_OPERATION, HCS_SYSTEM, HcsCloseComputeSystem, HcsCloseOperation,
            HcsCreateComputeSystem, HcsCreateOperation, HcsEnumerateComputeSystems,
            HcsGetServiceProperties, HcsGrantVmAccess, HcsModifyComputeSystem,
            HcsOpenComputeSystem, HcsShutDownComputeSystem, HcsStartComputeSystem,
            HcsTerminateComputeSystem, HcsWaitForOperationResult,
        },
    },
    core::{HSTRING, PCWSTR, PWSTR},
};

use uuid::Uuid;

use crate::{
    error::windows_error, hcn_endpoint::HCN_E_ENDPOINT_ALREADY_ATTACHED, hcs_config::adapter_key,
};
use vmlord_core::RepositoryError;

/// Access mask granting full control over a compute system, used to reopen
/// a system this process created in order to roll it back.
pub(crate) const HCS_ACCESS_ALL: u32 = 0x1000_0000;

const ENUMERATE_TIMEOUT: Duration = Duration::from_secs(10);

/// A hot-detach needs nothing from the guest -- HCS removes the device itself
/// -- so the bound only guards against a wedged Host Compute Service.
const DETACH_TIMEOUT: Duration = Duration::from_secs(30);

fn timeout_milliseconds(timeout: Duration) -> Result<u32, RepositoryError> {
    u32::try_from(timeout.as_millis()).map_err(|_| {
        RepositoryError::new("HCS operation timeout exceeds the maximum supported duration")
    })
}

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

    /// Waits up to `timeout` for the operation to complete, returning its
    /// result document (empty if HCS returned none).
    pub fn wait_for_completion(self, timeout: Duration) -> Result<String, RepositoryError> {
        self.wait(timeout)
            .map_err(|error| wait_failure(timeout, error))
    }

    /// Waits like [`HcsOperation::wait_for_completion`], but surfaces the raw
    /// Windows failure so callers can classify an operation-specific HRESULT
    /// before it is flattened into a message.
    fn wait(self, timeout: Duration) -> Result<String, WaitFailure> {
        let timeout_ms = timeout_milliseconds(timeout).map_err(WaitFailure::Timeout)?;
        let mut result = PWSTR::null();
        // SAFETY: `self.0` is an owned HCS operation handle valid for this call.
        // On success HCS writes a possibly-null result pointer into `result`,
        // which is immediately transferred to `HcsAllocatedString` for ownership.
        let native_result =
            unsafe { HcsWaitForOperationResult(self.0, timeout_ms, Some(&mut result)) };
        let document = HcsAllocatedString::from_optional(result);

        native_result.map_err(WaitFailure::Windows)?;

        match document {
            Some(document) => document.into_string(),
            None => Ok(String::new()),
        }
        .map_err(WaitFailure::Timeout)
    }
}

/// A failure from [`HcsOperation::wait`], retaining the Windows error so a
/// caller can match on its HRESULT.
enum WaitFailure {
    Windows(windows::core::Error),
    /// A failure that never carries a meaningful HRESULT (an unrepresentable
    /// timeout, or a malformed result document).
    Timeout(RepositoryError),
}

/// The information HCS returned when it refused a modify operation.
///
/// The result document is deliberately left unparsed. Its schema varies by
/// resource and HCS version, but it is the service's only host-specific
/// explanation of why a best-effort GPU assignment did not happen.
pub(crate) struct HcsModifyFailure {
    pub(crate) hresult: u32,
    pub(crate) result_detail: Option<String>,
}

impl HcsModifyFailure {
    #[cfg(test)]
    pub(crate) fn new(hresult: u32, result_detail: Option<String>) -> Self {
        Self {
            hresult,
            result_detail,
        }
    }
}

/// Renders a wait failure the way every caller that does not classify the
/// HRESULT itself reports it.
fn wait_failure(timeout: Duration, failure: WaitFailure) -> RepositoryError {
    match failure {
        WaitFailure::Timeout(error) => error,
        WaitFailure::Windows(error) if error.code() == ERROR_TIMEOUT.to_hresult() => {
            RepositoryError::new(format!(
                "HCS operation timed out after {} ms",
                timeout.as_millis()
            ))
        }
        WaitFailure::Windows(error) => windows_error("wait for HCS operation result", None, error),
    }
}

/// Why a compute system could not be created or started.
///
/// One cause is worth separating from every other: an endpoint HNS still has
/// attached to a compute system that no longer exists cannot be attached again,
/// and no retry of the same start fixes it -- only replacing the endpoint does.
pub enum HcsStartFailure {
    /// HNS reported `HCN_E_ENDPOINT_ALREADY_ATTACHED`.
    EndpointBusy(RepositoryError),
    Failed(RepositoryError),
}

impl HcsStartFailure {
    /// The failure as the repository boundary reports it, whatever its cause.
    #[must_use]
    pub fn into_error(self) -> RepositoryError {
        match self {
            Self::EndpointBusy(error) | Self::Failed(error) => error,
        }
    }
}

/// Classifies a call HCS refused outright.
fn call_failure(operation: &str, id: &str, error: windows::core::Error) -> HcsStartFailure {
    let endpoint_busy = error.code() == HCN_E_ENDPOINT_ALREADY_ATTACHED;
    let error = windows_error(operation, Some(id), error);
    log::error!("{error}");
    if endpoint_busy {
        HcsStartFailure::EndpointBusy(error)
    } else {
        HcsStartFailure::Failed(error)
    }
}

/// Classifies an operation HCS accepted and then failed.
fn operation_failure(
    operation: &str,
    id: &str,
    timeout: Duration,
    failure: WaitFailure,
) -> HcsStartFailure {
    match failure {
        WaitFailure::Windows(error) if error.code() == HCN_E_ENDPOINT_ALREADY_ATTACHED => {
            let error = windows_error(operation, Some(id), error);
            log::error!("{error}");
            HcsStartFailure::EndpointBusy(error)
        }
        failure => {
            let error = wait_failure(timeout, failure);
            log::error!("the {operation} of \"{id}\" failed: {error}");
            HcsStartFailure::Failed(error)
        }
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
pub struct HcsSystem {
    handle: HCS_SYSTEM,
    id: String,
}

impl HcsSystem {
    /// Opens an existing compute system by its stable VM identifier.
    pub fn open(
        vm_name: &str,
        requested_access: u32,
    ) -> Result<Self, vmlord_core::RepositoryError> {
        Self::try_open(vm_name, requested_access)
            .map_err(|error| windows_error("open compute system", Some(vm_name), error))
    }

    /// Opens an existing compute system, reporting `Ok(None)` when HCS does not
    /// know it.
    ///
    /// A compute system exists only while it is created or running: HCS
    /// destroys it once it exits, whether the guest powered off or
    /// [`HcsSystem::terminate`] stopped it. A VM that VMLord still knows is
    /// therefore routinely absent here, which is a fact about its state rather
    /// than an error; callers that can rebuild the system from its stored
    /// configuration use this instead of [`HcsSystem::open`].
    pub fn open_if_present(
        vm_name: &str,
        requested_access: u32,
    ) -> Result<Option<Self>, RepositoryError> {
        match Self::try_open(vm_name, requested_access) {
            Ok(system) => Ok(Some(system)),
            Err(error) if error.code() == HCS_E_SYSTEM_NOT_FOUND => {
                log::debug!("HCS does not know compute system \"{vm_name}\"");
                Ok(None)
            }
            Err(error) => Err(windows_error("open compute system", Some(vm_name), error)),
        }
    }

    fn try_open(vm_name: &str, requested_access: u32) -> Result<Self, windows::core::Error> {
        let hcs_name = HSTRING::from(vm_name);
        // SAFETY: `hcs_name` remains valid for the duration of the call. A successful
        // handle is transferred to this wrapper and closed by `Drop`.
        let handle = unsafe { HcsOpenComputeSystem(&hcs_name, requested_access) }?;
        Ok(Self {
            handle,
            id: vm_name.to_owned(),
        })
    }

    /// Starts the compute system, returning the pending start operation.
    ///
    /// The caller must keep this handle alive until the returned operation
    /// completes, and must have granted the VM access to every file its
    /// configuration attaches (see [`HcsClient::grant_vm_access`]); otherwise
    /// the start fails with `ERROR_ACCESS_DENIED`.
    pub fn start(&self) -> Result<HcsOperation, RepositoryError> {
        log::debug!("starting HCS compute system \"{}\"", self.id);
        let operation = HcsOperation::new();
        // SAFETY: `self.handle` and `operation.0` are valid owned handles for
        // the duration of this call. Null options are accepted here: a start
        // takes its parameters from the compute system's own configuration,
        // unlike `HcsShutDownComputeSystem`, which rejects null options with
        // `HCS_E_INVALID_JSON`.
        unsafe { HcsStartComputeSystem(self.handle, operation.0, PCWSTR::null()) }.map_err(
            |error| {
                let error = windows_error("start compute system", Some(&self.id), error);
                log::error!("{error}");
                error
            },
        )?;
        Ok(operation)
    }

    /// Starts the compute system and waits up to `timeout`, saying whether the
    /// start failed because the VM's endpoint is still attached elsewhere.
    ///
    /// This is what [`crate::VmStartPipeline`] uses: an occupied endpoint is
    /// the one failure it can recover from, and it can only recognise it here,
    /// where the raw HRESULT is still available.
    pub fn start_and_wait(&self, timeout: Duration) -> Result<(), HcsStartFailure> {
        log::debug!("starting HCS compute system \"{}\"", self.id);
        let operation = HcsOperation::new();
        // SAFETY: `self.handle` and `operation.0` are valid owned handles for
        // the duration of this call. Null options are accepted here: a start
        // takes its parameters from the compute system's own configuration.
        unsafe { HcsStartComputeSystem(self.handle, operation.0, PCWSTR::null()) }
            .map_err(|error| call_failure("start compute system", &self.id, error))?;

        operation
            .wait(timeout)
            .map(|_document| ())
            .map_err(|failure| {
                operation_failure("start compute system", &self.id, timeout, failure)
            })
    }

    /// Asks the guest to shut down gracefully, returning the pending
    /// shutdown operation.
    ///
    /// The operation completes once HCS has accepted and delivered the
    /// request, not once the guest has finished powering off; callers that
    /// need the latter must watch for the system's exit separately.
    ///
    /// A guest without integration services (or one refusing the request)
    /// never powers off, so this is not a substitute for
    /// [`HcsSystem::terminate`].
    pub fn shutdown(&self) -> Result<HcsOperation, RepositoryError> {
        log::debug!("shutting down HCS compute system \"{}\"", self.id);
        let operation = HcsOperation::new();
        let options = HSTRING::from(shutdown_options());
        // SAFETY: `self.handle` and `operation.0` are valid owned handles for
        // the duration of this call, and `options` outlives it.
        unsafe { HcsShutDownComputeSystem(self.handle, operation.0, &options) }.map_err(
            |error| {
                let error = windows_error("shut down compute system", Some(&self.id), error);
                log::error!("{error}");
                error
            },
        )?;
        Ok(operation)
    }

    /// Requests a graceful shutdown and waits up to `timeout` for HCS to
    /// report the request's outcome.
    ///
    /// `ERROR_NOT_SUPPORTED` is reported as its own error: HCS accepted the
    /// request but has no way to deliver it, which no retry fixes and which
    /// only a forced stop can work around.
    pub fn shutdown_and_wait(&self, timeout: Duration) -> Result<(), RepositoryError> {
        match self.shutdown()?.wait(timeout) {
            Ok(_document) => Ok(()),
            Err(WaitFailure::Windows(error))
                if error.code() == ERROR_NOT_SUPPORTED.to_hresult() =>
            {
                let error = unsupported_shutdown_error(&self.id, error.code().0 as u32);
                log::error!("{error}");
                Err(error)
            }
            Err(failure) => {
                let error = wait_failure(timeout, failure);
                log::error!(
                    "the shutdown of HCS compute system \"{}\" failed: {error}",
                    self.id
                );
                Err(error)
            }
        }
    }

    /// Terminates the compute system, e.g. to roll back a failed creation.
    ///
    /// Termination stops the VM's execution immediately, without involving the
    /// guest, and HCS then destroys the compute system: a terminated system is
    /// gone, and reopening it fails with `HCS_E_SYSTEM_NOT_FOUND`. Nothing on
    /// disk is touched, so the VM can be re-created from its stored
    /// configuration and started again -- which is what
    /// [`crate::VmStartPipeline`] does.
    pub fn terminate(&self) -> Result<HcsOperation, RepositoryError> {
        log::debug!("terminating HCS compute system \"{}\"", self.id);
        let operation = HcsOperation::new();
        // SAFETY: `self.handle` and `operation.0` are valid owned handles for
        // the duration of this call. Null options match
        // `HcsTerminateComputeSystem`'s legacy AppSandbox usage; unlike
        // `HcsShutDownComputeSystem`, it does not require a JSON options body.
        unsafe { HcsTerminateComputeSystem(self.handle, operation.0, PCWSTR::null()) }.map_err(
            |error| {
                let error = windows_error("terminate compute system", Some(&self.id), error);
                log::error!("{error}");
                error
            },
        )?;
        Ok(operation)
    }

    /// Terminates the compute system and waits up to `timeout` for HCS to
    /// report the outcome.
    ///
    /// Unlike [`HcsSystem::shutdown_and_wait`], completion means the VM has
    /// actually stopped: termination needs nothing from the guest.
    pub fn terminate_and_wait(&self, timeout: Duration) -> Result<(), RepositoryError> {
        self.terminate()?
            .wait_for_completion(timeout)
            .map(|_document| ())
            .inspect_err(|error| {
                log::error!(
                    "the termination of HCS compute system \"{}\" failed: {error}",
                    self.id
                );
            })
    }

    /// Hot-detaches the network adapter keyed by `endpoint_id` from this
    /// running compute system and waits for HCS to report the outcome.
    ///
    /// HNS keeps an endpoint attached to the compute system it was handed to
    /// even after HCS destroys that system, so a VM terminated with its adapter
    /// still in place leaves the endpoint occupied: the next start fails with
    /// `HCN_E_ENDPOINT_ALREADY_ATTACHED`. Detaching before the VM stops is what
    /// keeps the endpoint -- and therefore the guest's address -- reusable.
    pub fn remove_network_adapter(&self, endpoint_id: Uuid) -> Result<(), RepositoryError> {
        log::debug!(
            "detaching the adapter of endpoint {endpoint_id} from HCS compute system \"{}\"",
            self.id
        );
        self.modify(&detach_adapter_document(endpoint_id), DETACH_TIMEOUT)
            .map(|_document| ())
            .map_err(|failure| {
                RepositoryError::new(format!(
                    "modify compute system \"{}\" failed with HRESULT 0x{:08X}: {}",
                    self.id,
                    failure.hresult,
                    failure
                        .result_detail
                        .as_deref()
                        .unwrap_or("no HCS result detail")
                ))
            })
            .inspect_err(|error| {
                log::error!(
                    "detaching the adapter of HCS compute system \"{}\" failed: {error}",
                    self.id
                );
            })
    }

    /// Modifies this compute system and waits for HCS to report the outcome.
    ///
    /// The call is safe for platform services: this wrapper owns the operation
    /// handle and preserves both the failing HRESULT and HCS's optional result
    /// document for callers whose work is best effort.
    pub(crate) fn modify(
        &self,
        document: &str,
        timeout: Duration,
    ) -> Result<String, HcsModifyFailure> {
        let operation = HcsOperation::new();
        let document = HSTRING::from(document);
        // SAFETY: `self.handle` and `operation.0` are valid owned handles for
        // the duration of this call, and `document` outlives it. A null
        // identity asks HCS to act as the calling process, which is what every
        // other call in this module does.
        unsafe { HcsModifyComputeSystem(self.handle, operation.0, &document, None) }.map_err(
            |error| HcsModifyFailure {
                hresult: error.code().0 as u32,
                result_detail: None,
            },
        )?;

        let timeout_ms = timeout_milliseconds(timeout).map_err(|error| HcsModifyFailure {
            hresult: ERROR_TIMEOUT.to_hresult().0 as u32,
            result_detail: Some(error.to_string()),
        })?;
        let mut result = PWSTR::null();
        // SAFETY: `operation.0` is an owned HCS operation handle valid for this
        // call. A non-null result is an HCS allocation transferred immediately
        // to `HcsAllocatedString`, which frees it on drop.
        let native_result =
            unsafe { HcsWaitForOperationResult(operation.0, timeout_ms, Some(&mut result)) };
        let native_hresult = native_result
            .as_ref()
            .err()
            .map_or(0, |error| error.code().0 as u32);
        let result_detail = HcsAllocatedString::from_optional(result)
            .map(HcsAllocatedString::into_string)
            .transpose()
            .map_err(|error| HcsModifyFailure {
                hresult: native_hresult,
                result_detail: Some(error.to_string()),
            })?;

        match native_result {
            Ok(()) => Ok(result_detail.unwrap_or_default()),
            Err(error) => Err(HcsModifyFailure {
                hresult: error.code().0 as u32,
                result_detail,
            }),
        }
    }

    /// The raw compute-system handle, for registering an event callback on it.
    ///
    /// Non-owning: this `HcsSystem` still closes the handle in `Drop`, so
    /// anything holding the returned value must not outlive it.
    pub(crate) fn raw_handle(&self) -> HCS_SYSTEM {
        self.handle
    }
}

/// One compute system HCS currently reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HcsSystemSummary {
    pub id: String,
    /// The GUID Hyper-V runs this compute system as, when it reported one.
    ///
    /// A different identifier from `id`, which is the name VMLord gave the
    /// system: the runtime id is what a partition is called on the outside --
    /// an HvSocket address names it, and nothing else about a compute system
    /// does. It changes on every start, so it is read here rather than
    /// recorded.
    ///
    /// `None` means HCS reported no `RuntimeId`, or one that is not a GUID.
    /// Nothing that needs it can be done for such a system, and dropping the
    /// whole entry over it would lose a VM from the list.
    pub runtime_id: Option<Uuid>,
    /// The state HCS reported, or `None` when the entry carried none.
    ///
    /// HCS omits `State` for a compute system that has been created but never
    /// started, and reports it for one that runs, so an entry without a state
    /// is not an unknown -- see [`HcsSystemState::from_enumeration`].
    pub state: Option<HcsSystemState>,
}

/// The state HCS reports for a compute system.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HcsSystemState {
    /// The system exists but has never been started.
    Created,
    /// The system is executing.
    Running,
    /// The system is paused.
    Paused,
    /// The system has stopped but has not been destroyed yet.
    Stopped,
    /// A state this VMLord does not know; carried verbatim so callers can log
    /// it rather than silently treat it as one of the states above.
    Other(String),
}

impl HcsSystemState {
    /// Interprets what an enumeration entry said about a compute system's
    /// state.
    ///
    /// A missing state means [`HcsSystemState::Created`]. HCS writes `State`
    /// only once a compute system has run: a VM created and never started is
    /// enumerated with an `Id`, a `SystemType`, an `Owner` and a `RuntimeId`
    /// and nothing else, while a running one carries `"State": "Running"`.
    /// This is the only signal that separates the two, because a created
    /// system also refuses `HcsGetComputeSystemProperties`.
    #[must_use]
    pub fn from_enumeration(reported: Option<Self>) -> Self {
        reported.unwrap_or(Self::Created)
    }
}

fn parse_system_state(state: &str) -> HcsSystemState {
    match state {
        "Created" => HcsSystemState::Created,
        "Running" => HcsSystemState::Running,
        "Paused" => HcsSystemState::Paused,
        "Stopped" => HcsSystemState::Stopped,
        other => HcsSystemState::Other(other.to_owned()),
    }
}

impl Drop for HcsSystem {
    fn drop(&mut self) {
        // SAFETY: This wrapper exclusively owns the HCS system handle.
        unsafe { HcsCloseComputeSystem(self.handle) };
    }
}

/// An owned, HCS-allocated wide string, freed with `LocalFree` on drop.
struct HcsAllocatedString(PWSTR);

impl HcsAllocatedString {
    fn new(raw: PWSTR) -> Result<Self, RepositoryError> {
        if raw.is_null() {
            return Err(RepositoryError::new("HCS returned a null result string"));
        }
        Ok(Self(raw))
    }

    /// Wraps `raw` for ownership if non-null, otherwise returns `None`.
    fn from_optional(raw: PWSTR) -> Option<Self> {
        (!raw.is_null()).then_some(Self(raw))
    }

    fn into_string(self) -> Result<String, RepositoryError> {
        // SAFETY: `self.0` is a non-null pointer to a null-terminated UTF-16 buffer
        // exclusively owned by this wrapper for the duration of this call.
        unsafe { self.0.to_string() }.map_err(|error| {
            RepositoryError::new(format!("HCS result was not valid UTF-16: {error}"))
        })
    }
}

impl Drop for HcsAllocatedString {
    fn drop(&mut self) {
        // SAFETY: `self.0` originates from an HCS allocation exclusively owned by
        // this wrapper and is freed exactly once here.
        unsafe { LocalFree(Some(HLOCAL(self.0.as_ptr().cast()))) };
    }
}

/// Requests default HCS service properties rather than a specific query document.
///
/// HCS treats a non-null query pointer as a property-query JSON document, even
/// when the string is empty, and rejects it as invalid JSON. Passing a null
/// pointer requests the default service properties instead.
fn hcs_service_properties_query() -> PCWSTR {
    PCWSTR::null()
}

/// The options document passed to `HcsShutDownComputeSystem`.
///
/// HCS parses the options as JSON and rejects a null pointer with
/// `HCS_E_INVALID_JSON` ("Invalid JSON document '$'"), unlike
/// `HcsStartComputeSystem` and `HcsTerminateComputeSystem`, which accept one.
///
/// The mechanism has to be named. HCS knows two -- `GuestConnection`, the
/// hvsocket channel a utility VM's in-guest agent serves, and
/// `IntegrationService`, the VMBus channel an ordinary guest's own drivers
/// answer -- and an empty document left the choice to HCS, which reached for
/// the guest connection VMLord's VMs do not have and failed the operation with
/// `ERROR_NOT_SUPPORTED`. An Ubuntu cloud image serves the other one out of the
/// box, through `hv_util`, so that is the one to ask for (#70).
///
/// `Force` stays false: this is the request a guest is allowed to take its time
/// over, and refusing it is the guest's right. Stopping a VM that will not go
/// is what `HcsTerminateComputeSystem` is for.
fn shutdown_options() -> &'static str {
    concat!(
        r#"{"Mechanism":"IntegrationService","Type":"Shutdown","Force":false,"#,
        r#""Reason":"VMLord was asked to stop this VM"}"#
    )
}

/// The document asking HCS to hot-detach the adapter keyed by `endpoint_id`.
///
/// `RequestType: "Remove"` against the adapter's own resource path: HCS takes
/// the device out of the running VM, and HNS releases the endpoint it was
/// attached to.
fn detach_adapter_document(endpoint_id: Uuid) -> String {
    format!(
        r#"{{"ResourcePath":"VirtualMachine/Devices/NetworkAdapters/{}","RequestType":"Remove"}}"#,
        adapter_key(endpoint_id)
    )
}

/// Reports a shutdown HCS accepted but cannot deliver.
///
/// The shutdown *operation* fails with `ERROR_NOT_SUPPORTED` -- the call itself
/// and its options document having been accepted -- when the compute system
/// offers its guest no shutdown integration service to carry the request.
///
/// Since #70 every VM VMLord creates is given one, so this now means a VM built
/// from a configuration written before that: `config.json` is what a start
/// re-creates the compute system from, and a document without a `Services`
/// section keeps producing a VM that cannot be asked to stop. Re-creating the
/// VM is what fixes it; no retry does, so the message names the only way to
/// stop this one now.
fn unsupported_shutdown_error(id: &str, hresult: u32) -> RepositoryError {
    RepositoryError::new(format!(
        "HCS cannot deliver a graceful shutdown to compute system \"{id}\" \
         (HRESULT 0x{hresult:08X}, ERROR_NOT_SUPPORTED); the VM offers its guest \
         no shutdown service, which is how VMLord built VMs before #70, so only \
         a forced stop can stop it"
    ))
}

/// Validates an HCS service-properties result document.
///
/// Service-properties JSON has no stable schema; availability is represented
/// by any valid, non-error result document, so only malformed JSON and an
/// explicit `/Error/Message` are rejected.
fn parse_service_result(document: &str) -> Result<(), RepositoryError> {
    let value: serde_json::Value = serde_json::from_str(document).map_err(|error| {
        RepositoryError::new(format!(
            "HCS service query returned malformed JSON: {error}"
        ))
    })?;

    if let Some(message) = value
        .pointer("/Error/Message")
        .and_then(|value| value.as_str())
    {
        return Err(RepositoryError::new(format!(
            "HCS service query reported an error: {message}"
        )));
    }

    Ok(())
}

/// Queries the live Host Compute Service for its default service properties.
fn query_hcs_service_properties() -> Result<String, RepositoryError> {
    // SAFETY: The null query requests default service properties. The returned
    // pointer, once non-null, is transferred to `HcsAllocatedString` for ownership.
    let raw = unsafe { HcsGetServiceProperties(hcs_service_properties_query()) }
        .map_err(|error| windows_error("query HCS service properties", None, error))?;

    HcsAllocatedString::new(raw)?.into_string()
}

/// Whether the Host Compute Service is answering right now.
///
/// The query and its parse belong together: "the service replied" and "the
/// reply was not an error" are one question, and a caller outside this module
/// has no business holding half of it.
pub(crate) fn service_available() -> Result<(), RepositoryError> {
    parse_service_result(&query_hcs_service_properties()?)
}

/// Enumerates every HCS compute system visible to this process.
///
/// A null query (as opposed to an empty JSON document, which HCS rejects)
/// requests every compute system, matching the legacy AppSandbox backend's
/// `hcs_vm.c` enumeration usage.
fn query_hcs_enumerate_systems() -> Result<String, RepositoryError> {
    let operation = HcsOperation::new();
    // SAFETY: `operation.0` is a valid, owned operation handle for the
    // duration of this call; the null query is a `PCWSTR`, satisfying
    // `HcsEnumerateComputeSystems`'s `Param<PCWSTR>` bound.
    unsafe { HcsEnumerateComputeSystems(PCWSTR::null(), operation.0) }
        .map_err(|error| windows_error("enumerate compute systems", None, error))?;

    operation.wait_for_completion(ENUMERATE_TIMEOUT)
}

/// Extracts each compute system's `Id` from an `HcsEnumerateComputeSystems`
/// result document.
///
/// HCS's enumeration schema is not stable across versions beyond the `Id`
/// field every entry carries, so entries without one are skipped rather than
/// treated as a parse failure.
fn parse_enumerate_result(document: &str) -> Result<Vec<HcsSystemSummary>, RepositoryError> {
    if document.trim().is_empty() {
        return Ok(Vec::new());
    }

    let entries: Vec<serde_json::Value> = serde_json::from_str(document).map_err(|error| {
        RepositoryError::new(format!("HCS enumeration returned malformed JSON: {error}"))
    })?;

    Ok(entries
        .into_iter()
        .filter_map(|entry| {
            let id = entry
                .get("Id")
                .and_then(serde_json::Value::as_str)?
                .to_owned();
            let state = entry
                .get("State")
                .and_then(serde_json::Value::as_str)
                .map(parse_system_state);
            let runtime_id = entry
                .get("RuntimeId")
                .and_then(serde_json::Value::as_str)
                .and_then(|runtime_id| Uuid::parse_str(runtime_id).ok());
            Some(HcsSystemSummary {
                id,
                state,
                runtime_id,
            })
        })
        .collect())
}

/// Safe, idempotent access to Host Compute Service availability.
pub struct HcsClient {
    initialized: bool,
    #[cfg(test)]
    probe: Option<Box<dyn Fn() -> Result<String, RepositoryError>>>,
    #[cfg(test)]
    enumerate_probe: Option<Box<dyn Fn() -> Result<String, RepositoryError>>>,
}

impl HcsClient {
    /// Creates a client that has not yet probed HCS availability.
    #[must_use]
    pub fn new() -> Self {
        Self {
            initialized: false,
            #[cfg(test)]
            probe: None,
            #[cfg(test)]
            enumerate_probe: None,
        }
    }

    #[cfg(test)]
    fn with_probe(probe: impl Fn() -> Result<String, RepositoryError> + 'static) -> Self {
        Self {
            initialized: false,
            probe: Some(Box::new(probe)),
            enumerate_probe: None,
        }
    }

    #[cfg(test)]
    fn with_enumerate_probe(probe: impl Fn() -> Result<String, RepositoryError> + 'static) -> Self {
        Self {
            initialized: false,
            probe: None,
            enumerate_probe: Some(Box::new(probe)),
        }
    }

    /// Probes Host Compute Service availability, if not already confirmed.
    ///
    /// Subsequent calls after a successful probe are no-ops.
    pub fn initialize(&mut self) -> Result<(), RepositoryError> {
        if self.initialized {
            log::debug!("HCS client already initialized; skipping availability probe");
            return Ok(());
        }

        log::debug!("probing Host Compute Service availability");
        let document = self.probe_service_properties().inspect_err(|error| {
            log::error!("Host Compute Service is unavailable: {error}");
        })?;

        parse_service_result(&document).inspect_err(|error| {
            log::error!("Host Compute Service returned an invalid result: {error}");
        })?;

        self.initialized = true;
        log::info!("Host Compute Service is available");
        Ok(())
    }

    /// Returns whether a previous [`HcsClient::initialize`] call succeeded.
    #[must_use]
    pub fn is_initialized(&self) -> bool {
        self.initialized
    }

    /// Creates a compute system from `configuration` and returns its owned
    /// system and (not-yet-awaited) creation-operation handles.
    ///
    /// `configuration` must set `ShouldTerminateOnLastHandleClosed` to
    /// `false` (see the HCS configuration builder); otherwise HCS destroys
    /// even a never-started system as soon as the returned operation's
    /// handle closes.
    pub fn create_system(
        &self,
        id: &str,
        configuration: &str,
    ) -> Result<(HcsSystem, HcsOperation), RepositoryError> {
        log::debug!("creating HCS compute system \"{id}\"");
        let operation = HcsOperation::new();
        let hcs_id = HSTRING::from(id);
        let hcs_configuration = HSTRING::from(configuration);
        // SAFETY: `hcs_id` and `hcs_configuration` remain valid for the duration
        // of the call. On success the returned system handle is transferred to
        // `HcsSystem` for ownership.
        let handle =
            unsafe { HcsCreateComputeSystem(&hcs_id, &hcs_configuration, operation.0, None) }
                .map_err(|error| {
                    let error = windows_error("create compute system", Some(id), error);
                    log::error!("{error}");
                    error
                })?;

        Ok((
            HcsSystem {
                handle,
                id: id.to_owned(),
            },
            operation,
        ))
    }

    /// Creates a compute system and waits up to `timeout` for the creation to
    /// complete, saying whether it failed because the VM's endpoint is still
    /// attached elsewhere.
    ///
    /// Unlike [`HcsClient::create_system`], the creation is awaited here rather
    /// than handed back: the returned system is one the caller can start, and
    /// keeping the handle alive across the wait is this method's business.
    pub fn create_system_and_wait(
        &self,
        id: &str,
        configuration: &str,
        timeout: Duration,
    ) -> Result<HcsSystem, HcsStartFailure> {
        log::debug!("creating HCS compute system \"{id}\"");
        let operation = HcsOperation::new();
        let hcs_id = HSTRING::from(id);
        let hcs_configuration = HSTRING::from(configuration);
        // SAFETY: `hcs_id` and `hcs_configuration` remain valid for the
        // duration of the call. On success the returned system handle is
        // transferred to `HcsSystem` for ownership.
        let handle =
            unsafe { HcsCreateComputeSystem(&hcs_id, &hcs_configuration, operation.0, None) }
                .map_err(|error| call_failure("create compute system", id, error))?;
        let system = HcsSystem {
            handle,
            id: id.to_owned(),
        };

        operation
            .wait(timeout)
            .map_err(|failure| operation_failure("create compute system", id, timeout, failure))?;

        Ok(system)
    }

    /// Grants the VM's worker process access to a file (a VHD/VHDX or an
    /// attached ISO) it must open when it starts.
    ///
    /// Hyper-V opens VM-owned files under the VM's own
    /// `NT VIRTUAL MACHINE\<id>` security principal, not the creating user's
    /// token: without this call, starting the VM fails with
    /// `ERROR_ACCESS_DENIED` even though the file was just created
    /// successfully by an elevated process.
    pub fn grant_vm_access(&self, id: &str, path: &Path) -> Result<(), RepositoryError> {
        log::debug!(
            "granting HCS compute system \"{id}\" access to {}",
            path.display()
        );
        let hcs_id = HSTRING::from(id);
        // `HSTRING` has no `From<&OsStr>`; UTF-16 surrogates in paths are
        // extremely rare, so a lossy conversion is acceptable here.
        let wide_path = HSTRING::from(path.as_os_str().to_string_lossy().as_ref());
        // SAFETY: `hcs_id` and `wide_path` remain valid for the duration of the call.
        unsafe { HcsGrantVmAccess(&hcs_id, &wide_path) }.map_err(|error| {
            // Returned rather than logged at error: whether a refusal matters
            // is the caller's to say. It is fatal for the files a compute
            // system attaches, which Hyper-V opens as the VM itself, and
            // expected for the GPU shares under `System32`, which no grant can
            // cover and none of which needs one.
            let error = windows_error("grant VM access", Some(id), error);
            log::debug!("{error}");
            error
        })
    }

    /// Lists every HCS compute system currently visible to this process,
    /// with the state HCS reports for it.
    ///
    /// The enumeration carries the state, which is why VMLord reads it from
    /// here rather than querying each system's properties: a compute system
    /// that has been created but never started refuses a property query
    /// outright, and that is precisely the state worth distinguishing.
    pub fn enumerate_systems(&self) -> Result<Vec<HcsSystemSummary>, RepositoryError> {
        log::debug!("enumerating HCS compute systems");
        let document = self.enumerate_document().inspect_err(|error| {
            log::error!("failed to enumerate HCS compute systems: {error}");
        })?;
        log::debug!("HCS enumeration returned: {document}");
        let systems = parse_enumerate_result(&document)?;
        log::debug!("enumerated {} HCS compute system(s)", systems.len());
        Ok(systems)
    }

    #[cfg(not(test))]
    fn probe_service_properties(&self) -> Result<String, RepositoryError> {
        query_hcs_service_properties()
    }

    #[cfg(test)]
    fn probe_service_properties(&self) -> Result<String, RepositoryError> {
        match &self.probe {
            Some(probe) => probe(),
            None => query_hcs_service_properties(),
        }
    }

    #[cfg(not(test))]
    fn enumerate_document(&self) -> Result<String, RepositoryError> {
        query_hcs_enumerate_systems()
    }

    #[cfg(test)]
    fn enumerate_document(&self) -> Result<String, RepositoryError> {
        match &self.enumerate_probe {
            Some(probe) => probe(),
            None => query_hcs_enumerate_systems(),
        }
    }
}

impl Default for HcsClient {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use vmlord_core::RepositoryError;

    use super::{
        HcsClient, HcsModifyFailure, HcsStartFailure, HcsSystemState, HcsSystemSummary,
        call_failure, detach_adapter_document, hcs_service_properties_query,
        parse_enumerate_result, parse_service_result, parse_system_state, shutdown_options,
        unsupported_shutdown_error,
    };

    #[test]
    fn an_occupied_endpoint_is_classified_apart_from_every_other_failure() {
        // The start retries only this one code; misclassifying it either loses
        // the recovery or retries a start that will never succeed.
        let busy = call_failure(
            "start compute system",
            "vmlord-dev",
            windows::core::Error::from_hresult(windows::core::HRESULT(0x803B_0014_u32 as i32)),
        );

        assert!(matches!(busy, HcsStartFailure::EndpointBusy(_)));
        let message = busy.into_error().to_string();
        assert!(message.contains("0x803B0014"), "{message}");
        assert!(message.contains("vmlord-dev"), "{message}");
    }

    #[test]
    fn any_other_hresult_is_an_ordinary_failure() {
        let denied = call_failure(
            "start compute system",
            "vmlord-dev",
            windows::core::Error::from_hresult(windows::core::HRESULT(0x8007_0005_u32 as i32)),
        );

        assert!(matches!(denied, HcsStartFailure::Failed(_)));
        assert!(denied.into_error().to_string().contains("0x80070005"));
    }

    #[test]
    fn modify_failure_keeps_the_hresult_and_result_detail() {
        // GPU-PV failures are best effort, so the caller cannot surface them by
        // failing the start; this diagnostic is the only evidence it has.
        let failure = HcsModifyFailure::new(0x8037_010D, Some(r#"{"Error":"bad GPU"}"#.into()));

        assert_eq!(failure.hresult, 0x8037_010D);
        assert_eq!(
            failure.result_detail.as_deref(),
            Some(r#"{"Error":"bad GPU"}"#)
        );
    }

    #[test]
    fn the_detach_document_removes_the_adapter_keyed_by_the_endpoint() {
        // The resource path has to spell the adapter exactly the way the stored
        // configuration keys it, or HCS removes nothing and reports success.
        let endpoint_id = uuid::Uuid::from_u128(0x3f2b_0c11_5c78_4c1b_9e2f_3a8b_7d4c_6e50);

        let document: serde_json::Value =
            serde_json::from_str(&detach_adapter_document(endpoint_id)).unwrap();

        assert_eq!(
            document["ResourcePath"],
            "VirtualMachine/Devices/NetworkAdapters/3F2B0C11-5C78-4C1B-9E2F-3A8B7D4C6E50"
        );
        assert_eq!(document["RequestType"], "Remove");
    }

    #[test]
    fn the_detach_path_uses_the_configurations_own_adapter_key() {
        let endpoint_id = uuid::Uuid::new_v4();

        let document: serde_json::Value =
            serde_json::from_str(&detach_adapter_document(endpoint_id)).unwrap();

        assert!(
            document["ResourcePath"]
                .as_str()
                .unwrap()
                .ends_with(&crate::hcs_config::adapter_key(endpoint_id)),
            "{document}"
        );
    }

    #[test]
    fn system_state_maps_every_state_hcs_reports() {
        for (reported, expected) in [
            ("Created", HcsSystemState::Created),
            ("Running", HcsSystemState::Running),
            ("Paused", HcsSystemState::Paused),
            ("Stopped", HcsSystemState::Stopped),
            (
                "SavedAsTemplate",
                HcsSystemState::Other("SavedAsTemplate".into()),
            ),
        ] {
            assert_eq!(parse_system_state(reported), expected);
        }
    }

    /// Verbatim output of a live Hyper-V host running one started VM (WSL) and
    /// one VMLord VM that had just been created and never started. It is the
    /// evidence that HCS writes `State` only once a compute system has run.
    const LIVE_ENUMERATION: &str = r#"[
        {"Id":"8636363D-C5F9-49AA-B507-3B83F98C0D14","SystemType":"VirtualMachine",
         "Owner":"WSL","RuntimeId":"8636363d-c5f9-49aa-b507-3b83f98c0d14","State":"Running"},
        {"Id":"vmlord-b961b64484554b6289e8e70d6e38f181","SystemType":"VirtualMachine",
         "Owner":"VMLord","RuntimeId":"a811a3d9-78e5-5a7d-ba56-4b799c99f150"}
    ]"#;

    #[test]
    fn a_live_enumeration_states_the_running_system_and_omits_the_created_one() {
        let systems = parse_enumerate_result(LIVE_ENUMERATION).unwrap();

        assert_eq!(
            systems,
            vec![
                HcsSystemSummary {
                    id: "8636363D-C5F9-49AA-B507-3B83F98C0D14".into(),
                    state: Some(HcsSystemState::Running),
                    runtime_id: Some(
                        uuid::Uuid::parse_str("8636363d-c5f9-49aa-b507-3b83f98c0d14").unwrap()
                    ),
                },
                HcsSystemSummary {
                    id: "vmlord-b961b64484554b6289e8e70d6e38f181".into(),
                    state: None,
                    runtime_id: Some(
                        uuid::Uuid::parse_str("a811a3d9-78e5-5a7d-ba56-4b799c99f150").unwrap()
                    ),
                },
            ]
        );
    }

    #[test]
    fn a_system_enumerated_without_a_state_has_never_started() {
        assert_eq!(
            HcsSystemState::from_enumeration(None),
            HcsSystemState::Created
        );
        assert_eq!(
            HcsSystemState::from_enumeration(Some(HcsSystemState::Running)),
            HcsSystemState::Running
        );
    }

    #[test]
    fn service_properties_query_is_null() {
        assert!(hcs_service_properties_query().is_null());
    }

    #[test]
    fn shutdown_options_are_a_valid_json_document() {
        // A null options pointer makes `HcsShutDownComputeSystem` fail with
        // `HCS_E_INVALID_JSON`, so the options must stay a parsable document.
        let options = shutdown_options();

        assert!(!options.is_empty());
        assert!(serde_json::from_str::<serde_json::Value>(options).is_ok());
    }

    #[test]
    fn a_shutdown_asks_the_guests_own_integration_service_to_do_it() {
        // #70: left to itself HCS reaches for a guest connection -- the
        // hvsocket agent a utility VM runs and VMLord's VMs do not -- and fails
        // the operation. The VMBus service an ordinary Linux guest answers
        // through `hv_util` has to be named.
        let options: serde_json::Value = serde_json::from_str(shutdown_options()).unwrap();

        assert_eq!(options["Mechanism"], "IntegrationService");
        assert_eq!(options["Type"], "Shutdown");
        // A graceful stop the guest may take its time over: forcing one is
        // `HcsTerminateComputeSystem`'s job, and it is a separate action on
        // purpose.
        assert_eq!(options["Force"], false);
    }

    #[test]
    fn an_unsupported_shutdown_names_the_system_and_points_at_a_forced_stop() {
        let error = unsupported_shutdown_error("vmlord-dev", 0x8007_0032);

        assert!(error.to_string().contains("vmlord-dev"));
        assert!(error.to_string().contains("0x80070032"));
        assert!(error.to_string().contains("forced stop"));
    }

    #[test]
    fn accepts_a_valid_service_result_without_properties() {
        assert!(parse_service_result(r#"{"Capabilities":[]}"#).is_ok());
    }

    #[test]
    fn rejects_an_explicit_error_message() {
        let error = parse_service_result(r#"{"Error":{"Message":"boom"}}"#).unwrap_err();
        assert!(error.to_string().contains("boom"));
    }

    #[test]
    fn rejects_malformed_json() {
        assert!(parse_service_result("not json").is_err());
    }

    #[test]
    fn new_client_is_not_initialized() {
        assert!(!HcsClient::new().is_initialized());
    }

    #[test]
    fn initialize_succeeds_when_the_probe_reports_availability() {
        let mut client = HcsClient::with_probe(|| Ok(r#"{"Capabilities":[]}"#.to_string()));

        client
            .initialize()
            .expect("probe result should be accepted");

        assert!(client.is_initialized());
    }

    #[test]
    fn initialize_fails_and_stays_uninitialized_when_the_probe_errors() {
        let mut client = HcsClient::with_probe(|| Err(RepositoryError::new("service unavailable")));

        let error = client.initialize().unwrap_err();

        assert!(error.to_string().contains("service unavailable"));
        assert!(!client.is_initialized());
    }

    #[test]
    fn initialize_rejects_a_probe_result_reporting_an_error() {
        let mut client =
            HcsClient::with_probe(|| Ok(r#"{"Error":{"Message":"boom"}}"#.to_string()));

        let error = client.initialize().unwrap_err();

        assert!(error.to_string().contains("boom"));
        assert!(!client.is_initialized());
    }

    #[test]
    fn enumerate_result_is_empty_for_an_empty_document() {
        assert_eq!(
            parse_enumerate_result("").unwrap(),
            Vec::<HcsSystemSummary>::new()
        );
        assert_eq!(
            parse_enumerate_result("[]").unwrap(),
            Vec::<HcsSystemSummary>::new()
        );
    }

    #[test]
    fn enumerate_result_skips_entries_without_an_id() {
        let document = r#"[{"State":"Running"},{"Id":"vmlord-2","State":"Running"}]"#;

        assert_eq!(
            parse_enumerate_result(document).unwrap(),
            vec![HcsSystemSummary {
                id: "vmlord-2".into(),
                state: Some(HcsSystemState::Running),
                runtime_id: None,
            }]
        );
    }

    #[test]
    fn a_runtime_id_that_is_not_a_guid_leaves_the_system_listed_without_one() {
        // The id is what a listing needs; the runtime id is what an HvSocket
        // address needs, and losing the VM from the list over it would be far
        // worse than listing it with nothing to connect to.
        let document = r#"[{"Id":"vmlord-1","State":"Running","RuntimeId":"not-a-guid"}]"#;

        let systems = parse_enumerate_result(document).unwrap();

        assert_eq!(systems[0].id, "vmlord-1");
        assert_eq!(systems[0].runtime_id, None);
    }

    #[test]
    fn enumerate_result_rejects_malformed_json() {
        assert!(parse_enumerate_result("not json").is_err());
    }

    #[test]
    fn enumerate_systems_returns_the_probes_parsed_systems() {
        let client = HcsClient::with_enumerate_probe(|| {
            Ok(r#"[{"Id":"vmlord-1","State":"Running"}]"#.to_string())
        });

        assert_eq!(
            client.enumerate_systems().unwrap(),
            vec![HcsSystemSummary {
                id: "vmlord-1".into(),
                state: Some(HcsSystemState::Running),
                runtime_id: None,
            }]
        );
    }

    #[test]
    fn enumerate_systems_propagates_a_probe_error() {
        let client =
            HcsClient::with_enumerate_probe(|| Err(RepositoryError::new("HCS unavailable")));

        let error = client.enumerate_systems().unwrap_err();

        assert!(error.to_string().contains("HCS unavailable"));
    }

    #[test]
    fn initialize_does_not_probe_again_after_success() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };

        let calls = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&calls);
        let mut client = HcsClient::with_probe(move || {
            counted.fetch_add(1, Ordering::SeqCst);
            Ok(r#"{"Capabilities":[]}"#.to_string())
        });

        client.initialize().expect("first probe should succeed");
        client.initialize().expect("second call should be a no-op");

        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
