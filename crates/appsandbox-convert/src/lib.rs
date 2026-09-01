//! Turning a copied AppSandbox Linux guest into a VMLord guest, with nothing
//! running from the disk it is on.
//!
//! The conversion is a function over a mounted filesystem root: it knows
//! nothing of VHDX, of Windows or of how the root came to be mounted. That is
//! what lets the same code run under WSL today and inside a service VM later,
//! and what lets every one of its tests run against a directory tree.

mod input;
mod root;

use std::fmt;

pub use input::{Conversion, SshDropIns};

/// A refusal, or a step that could not be completed.
pub struct ConvertError(String);

impl ConvertError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        let error = Self(message.into());
        tracing::error!("{error}");
        error
    }
}

impl fmt::Display for ConvertError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl fmt::Debug for ConvertError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

impl std::error::Error for ConvertError {}

mod facts;
#[cfg(test)]
mod fixture;
mod install;
mod remove;
mod verify;

pub use verify::verify;
