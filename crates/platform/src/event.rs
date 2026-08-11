use std::time::Duration;

use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT},
        System::Threading::{
            CreateEventW, EVENT_MODIFY_STATE, OpenEventW, SYNCHRONIZATION_SYNCHRONIZE, SetEvent,
            WaitForSingleObject,
        },
    },
    core::{Error, HSTRING},
};

use crate::error::windows_error;

/// Result of waiting for a Windows event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventWaitResult {
    Signaled,
    TimedOut,
}

/// An owned Windows event handle that is closed when dropped.
pub struct WindowsEvent(HANDLE);

impl WindowsEvent {
    /// Creates an unnamed event.
    pub fn new(
        manual_reset: bool,
        initially_signaled: bool,
    ) -> Result<Self, vmlord_core::RepositoryError> {
        // SAFETY: No security descriptor or name is supplied; the returned handle is
        // owned by this wrapper and closed in `Drop`.
        let handle = unsafe { CreateEventW(None, manual_reset, initially_signaled, None) }
            .map_err(|error| windows_error("create event", None, error))?;
        Ok(Self(handle))
    }

    /// Creates an event under `name`, so that another process can open it.
    ///
    /// The name is expected to live in the `Local\` namespace: the helper runs
    /// in the same session as VMLord, and a global name would let any session
    /// on the machine cancel a reader.
    pub fn create_named(
        name: &str,
        manual_reset: bool,
        initially_signaled: bool,
    ) -> Result<Self, vmlord_core::RepositoryError> {
        let name = HSTRING::from(name);
        // SAFETY: `name` outlives the call, and the returned handle is owned by
        // this wrapper and closed in `Drop`.
        let handle = unsafe { CreateEventW(None, manual_reset, initially_signaled, &name) }
            .map_err(|error| windows_error("create named event", None, error))?;
        Ok(Self(handle))
    }

    /// Opens an event another process created under `name`.
    pub fn open(name: &str) -> Result<Self, vmlord_core::RepositoryError> {
        let name = HSTRING::from(name);
        // SAFETY: `name` outlives the call, and the returned handle is owned by
        // this wrapper and closed in `Drop`.
        let handle = unsafe {
            OpenEventW(
                EVENT_MODIFY_STATE | SYNCHRONIZATION_SYNCHRONIZE,
                false,
                &name,
            )
        }
        .map_err(|error| windows_error("open named event", None, error))?;
        Ok(Self(handle))
    }

    /// Signals the event.
    pub fn signal(&self) -> Result<(), vmlord_core::RepositoryError> {
        // SAFETY: `self.0` is a valid event handle for the lifetime of `self`.
        unsafe { SetEvent(self.0) }.map_err(|error| windows_error("signal event", None, error))
    }

    /// Waits for the event for at most `timeout`.
    pub fn wait(&self, timeout: Duration) -> Result<EventWaitResult, vmlord_core::RepositoryError> {
        let timeout_ms = timeout.as_millis().min(u128::from(u32::MAX)) as u32;
        // SAFETY: `self.0` is a valid event handle for the lifetime of `self`.
        match unsafe { WaitForSingleObject(self.0, timeout_ms) } {
            WAIT_OBJECT_0 => Ok(EventWaitResult::Signaled),
            WAIT_TIMEOUT => Ok(EventWaitResult::TimedOut),
            WAIT_FAILED => Err(windows_error("wait for event", None, Error::from_win32())),
            result => Err(vmlord_core::RepositoryError::new(format!(
                "Windows API operation \"wait for event\" returned unexpected status {}",
                result.0
            ))),
        }
    }

    /// Reports whether the event is signaled right now, without blocking.
    pub fn is_signaled(&self) -> Result<bool, vmlord_core::RepositoryError> {
        Ok(matches!(
            self.wait(Duration::ZERO)?,
            EventWaitResult::Signaled
        ))
    }

    /// The raw handle, for the few places that have to wait on an event
    /// alongside other kernel objects.
    pub(crate) fn raw_handle(&self) -> HANDLE {
        self.0
    }
}

impl Drop for WindowsEvent {
    fn drop(&mut self) {
        // SAFETY: This wrapper owns the handle and only drops it once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use uuid::Uuid;

    use super::{EventWaitResult, WindowsEvent};

    #[test]
    fn a_named_event_can_be_opened_by_another_owner() {
        // The COM1 helper runs in its own process: the parent creates the
        // events, the helper opens them by name, and both see one state.
        let name = format!(r"Local\VMLord.Test.{}", Uuid::new_v4());
        let created = WindowsEvent::create_named(&name, true, false).unwrap();
        let opened = WindowsEvent::open(&name).unwrap();

        assert!(!opened.is_signaled().unwrap());
        created.signal().unwrap();
        assert!(opened.is_signaled().unwrap());
    }

    #[test]
    fn opening_an_event_nobody_created_fails() {
        let name = format!(r"Local\VMLord.Test.Missing.{}", Uuid::new_v4());

        assert!(WindowsEvent::open(&name).is_err());
    }

    #[test]
    fn event_can_be_signaled_and_waited() {
        let event = WindowsEvent::new(false, false).expect("event should be created");
        assert_eq!(
            event.wait(Duration::ZERO).unwrap(),
            EventWaitResult::TimedOut
        );

        event.signal().unwrap();
        assert_eq!(
            event.wait(Duration::ZERO).unwrap(),
            EventWaitResult::Signaled
        );
    }
}
