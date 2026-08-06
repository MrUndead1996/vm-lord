use windows::{
    Win32::{
        Foundation::{HLOCAL, LocalFree},
        System::HostComputeSystem::{
            HCS_OPERATION, HCS_SYSTEM, HcsCloseComputeSystem, HcsCloseOperation,
            HcsCreateOperation, HcsGetServiceProperties, HcsOpenComputeSystem,
        },
    },
    core::{HSTRING, PCWSTR, PWSTR},
};

use crate::error::windows_error;
use vmlord_core::RepositoryError;

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

/// An owned, HCS-allocated wide string, freed with `LocalFree` on drop.
struct HcsAllocatedString(PWSTR);

impl HcsAllocatedString {
    fn new(raw: PWSTR) -> Result<Self, RepositoryError> {
        if raw.is_null() {
            return Err(RepositoryError::new("HCS returned a null result string"));
        }
        Ok(Self(raw))
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

/// Safe, idempotent access to Host Compute Service availability.
pub struct HcsClient {
    initialized: bool,
    #[cfg(test)]
    probe: Option<Box<dyn Fn() -> Result<String, RepositoryError>>>,
}

impl HcsClient {
    /// Creates a client that has not yet probed HCS availability.
    #[must_use]
    pub fn new() -> Self {
        Self {
            initialized: false,
            #[cfg(test)]
            probe: None,
        }
    }

    #[cfg(test)]
    fn with_probe(probe: impl Fn() -> Result<String, RepositoryError> + 'static) -> Self {
        Self {
            initialized: false,
            probe: Some(Box::new(probe)),
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
}

impl Default for HcsClient {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use vmlord_core::RepositoryError;

    use super::{HcsClient, hcs_service_properties_query, parse_service_result};

    #[test]
    fn service_properties_query_is_null() {
        assert!(hcs_service_properties_query().is_null());
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
