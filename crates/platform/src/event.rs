use std::time::Duration;

use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT},
        System::Threading::{CreateEventW, SetEvent, WaitForSingleObject},
    },
    core::Error,
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

    use super::{EventWaitResult, WindowsEvent};

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
