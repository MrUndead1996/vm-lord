use std::{path::Path, time::Duration};

use windows::{
    Win32::{
        Foundation::{
            ERROR_NOT_SUPPORTED, ERROR_TIMEOUT, HCS_E_SYSTEM_NOT_FOUND, HLOCAL, LocalFree,
        },
        System::HostComputeSystem::{
            HCS_OPERATION, HCS_SYSTEM, HcsCloseComputeSystem, HcsCloseOperation,
            HcsCreateComputeSystem, HcsCreateOperation, HcsEnumerateComputeSystems,
            HcsGetComputeSystemProperties, HcsGetServiceProperties, HcsGrantVmAccess,
            HcsOpenComputeSystem,
            HcsShutDownComputeSystem, HcsStartComputeSystem, HcsTerminateComputeSystem,
            HcsWaitForOperationResult,
        },
    },
    core::{HSTRING, PCWSTR, PWSTR},
};

use crate::error::windows_error;
use vmlord_core::RepositoryError;

/// Access mask granting full control over a compute system, used to reopen
/// a system this process created in order to roll it back.
pub(crate) const HCS_ACCESS_ALL: u32 = 0x1000_0000;

const ENUMERATE_TIMEOUT: Duration = Duration::from_secs(10);

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

    /// Asks HCS what state the compute system is in.
    ///
    /// A compute system existing is not the same as its VM running: creation
    /// leaves a `Created` system behind that has never executed anything, so
    /// the state has to be asked for rather than inferred from the system's
    /// presence.
    pub fn state(&self, timeout: Duration) -> Result<HcsSystemState, RepositoryError> {
        let operation = HcsOperation::new();
        let query = HSTRING::from(basic_properties_query());
        // SAFETY: `self.handle` and `operation.0` are valid owned handles, and
        // `query` outlives the call.
        unsafe { HcsGetComputeSystemProperties(self.handle, operation.0, &query) }.map_err(
            |error| {
                let error = windows_error("get compute system properties", Some(&self.id), error);
                log::error!("{error}");
                error
            },
        )?;

        let document = operation.wait_for_completion(timeout)?;
        log::debug!(
            "HCS reports these properties for compute system \"{}\": {document}",
            self.id
        );
        parse_system_state(&document)
            .inspect(|state| log::debug!("HCS compute system \"{}\" is {state:?}", self.id))
    }
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

/// Reads the `State` field out of an HCS compute-system properties document.
///
/// HCS nests the properties of some system kinds under `Properties`, so both
/// shapes are accepted before the document is declared stateless.
fn parse_system_state(document: &str) -> Result<HcsSystemState, RepositoryError> {
    let value: serde_json::Value = serde_json::from_str(document).map_err(|error| {
        RepositoryError::new(format!(
            "HCS compute system properties are not valid JSON: {error}"
        ))
    })?;
    let state = value
        .pointer("/State")
        .or_else(|| value.pointer("/Properties/State"))
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            RepositoryError::new(format!(
                "HCS compute system properties carry no \"State\": {document}"
            ))
        })?;

    Ok(match state {
        "Created" => HcsSystemState::Created,
        "Running" => HcsSystemState::Running,
        "Paused" => HcsSystemState::Paused,
        "Stopped" => HcsSystemState::Stopped,
        other => HcsSystemState::Other(other.to_owned()),
    })
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

/// The property query passed to `HcsGetComputeSystemProperties`.
///
/// A null query does return a document, but one without a `State` field, so
/// the basic property set has to be asked for explicitly. This is what the
/// legacy AppSandbox backend queried too (`hcs_vm.c`), which is the only
/// available statement of what HCS actually answers here.
fn basic_properties_query() -> &'static str {
    r#"{"PropertyTypes":["Basic"]}"#
}

/// The options document passed to `HcsShutDownComputeSystem`.
///
/// HCS parses the options as JSON and rejects a null pointer with
/// `HCS_E_INVALID_JSON` ("Invalid JSON document '$'"), unlike
/// `HcsStartComputeSystem` and `HcsTerminateComputeSystem`, which accept one.
/// An empty object requests the default shutdown behaviour.
fn shutdown_options() -> &'static str {
    "{}"
}

/// Reports a shutdown HCS accepted but cannot deliver.
///
/// A VM whose guest exposes no shutdown channel HCS can use -- one booted from
/// installer media, for instance -- fails the shutdown *operation* with
/// `ERROR_NOT_SUPPORTED` even though the call itself and its options document
/// were accepted. No retry helps, so the message points at the only remaining
/// way to stop the VM.
fn unsupported_shutdown_error(id: &str, hresult: u32) -> RepositoryError {
    RepositoryError::new(format!(
        "HCS does not support a graceful shutdown of compute system \"{id}\" \
         (HRESULT 0x{hresult:08X}, ERROR_NOT_SUPPORTED); the guest exposes no \
         shutdown channel HCS can use, so only a forced stop can stop it"
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
fn parse_enumerate_result(document: &str) -> Result<Vec<String>, RepositoryError> {
    if document.trim().is_empty() {
        return Ok(Vec::new());
    }

    let entries: Vec<serde_json::Value> = serde_json::from_str(document).map_err(|error| {
        RepositoryError::new(format!("HCS enumeration returned malformed JSON: {error}"))
    })?;

    Ok(entries
        .into_iter()
        .filter_map(|entry| {
            entry
                .get("Id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
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
            let error = windows_error("grant VM access", Some(id), error);
            log::error!("{error}");
            error
        })
    }

    /// Lists the ids of every HCS compute system currently visible to this
    /// process.
    pub fn enumerate_system_ids(&self) -> Result<Vec<String>, RepositoryError> {
        log::debug!("enumerating HCS compute systems");
        let document = self.enumerate_document().inspect_err(|error| {
            log::error!("failed to enumerate HCS compute systems: {error}");
        })?;
        let ids = parse_enumerate_result(&document)?;
        log::debug!("enumerated {} HCS compute system(s)", ids.len());
        Ok(ids)
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
        HcsClient, HcsSystemState, basic_properties_query, hcs_service_properties_query,
        parse_enumerate_result, parse_service_result, parse_system_state, shutdown_options,
        unsupported_shutdown_error,
    };

    #[test]
    fn the_basic_property_query_is_a_valid_json_document() {
        // A null query answers without a "State" field, so this document is
        // what makes the state readable at all; it must stay parsable.
        let query: serde_json::Value = serde_json::from_str(basic_properties_query())
            .expect("the property query must be valid JSON");

        assert_eq!(query["PropertyTypes"], serde_json::json!(["Basic"]));
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
            let document = format!(r#"{{"Id":"vmlord-1","State":"{reported}"}}"#);

            assert_eq!(parse_system_state(&document).unwrap(), expected);
        }
    }

    #[test]
    fn system_state_rejects_a_document_without_a_state() {
        assert!(parse_system_state(r#"{"Id":"vmlord-1"}"#).is_err());
        assert!(parse_system_state("not json").is_err());
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
        assert_eq!(parse_enumerate_result("").unwrap(), Vec::<String>::new());
        assert_eq!(parse_enumerate_result("[]").unwrap(), Vec::<String>::new());
    }

    #[test]
    fn enumerate_result_extracts_every_id() {
        let document = r#"[{"Id":"vmlord-1","State":"Running"},{"Id":"vmlord-2","State":"Off"}]"#;

        assert_eq!(
            parse_enumerate_result(document).unwrap(),
            vec!["vmlord-1".to_string(), "vmlord-2".to_string()]
        );
    }

    #[test]
    fn enumerate_result_skips_entries_without_an_id() {
        let document = r#"[{"State":"Running"},{"Id":"vmlord-2"}]"#;

        assert_eq!(
            parse_enumerate_result(document).unwrap(),
            vec!["vmlord-2".to_string()]
        );
    }

    #[test]
    fn enumerate_result_rejects_malformed_json() {
        assert!(parse_enumerate_result("not json").is_err());
    }

    #[test]
    fn enumerate_system_ids_returns_the_probes_parsed_ids() {
        let client = HcsClient::with_enumerate_probe(|| {
            Ok(r#"[{"Id":"vmlord-1"},{"Id":"vmlord-2"}]"#.to_string())
        });

        assert_eq!(
            client.enumerate_system_ids().unwrap(),
            vec!["vmlord-1".to_string(), "vmlord-2".to_string()]
        );
    }

    #[test]
    fn enumerate_system_ids_propagates_a_probe_error() {
        let client =
            HcsClient::with_enumerate_probe(|| Err(RepositoryError::new("HCS unavailable")));

        let error = client.enumerate_system_ids().unwrap_err();

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
