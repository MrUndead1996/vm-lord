//! One file inside a payload, as the payload's own manifest declares it.
//!
//! Shared because every payload's manifest says the same three things about a
//! file, and because the rule for what a path inside an archive may look like
//! is a rule about archives rather than about what they carry.

use serde::{Deserialize, Serialize};

use crate::{PayloadError, Sha256Digest};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedFile {
    path: String,
    size: u64,
    sha256: Sha256Digest,
}

impl PreparedFile {
    /// A declared file, for the tools that write a manifest rather than read
    /// one.
    #[must_use]
    pub fn new(path: String, size: u64, sha256: Sha256Digest) -> Self {
        Self { path, size, sha256 }
    }

    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    #[must_use]
    pub fn size(&self) -> u64 {
        self.size
    }

    #[must_use]
    pub fn sha256(&self) -> &Sha256Digest {
        &self.sha256
    }
}

/// Refuses a path a payload must never declare.
///
/// Relative, forward-slashed, with nothing that could climb out of the
/// directory it is expanded into -- and never `payload.json` itself, which is
/// the document making the claim and cannot be one of the files it claims.
///
/// # Errors
///
/// [`PayloadError::InvalidManifest`] naming the path that was refused.
pub fn validate_path(path: &str) -> Result<(), PayloadError> {
    if path.is_empty()
        || path.contains('\\')
        || path.contains('\0')
        || path.starts_with('/')
        || path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        || path == "payload.json"
    {
        return Err(PayloadError::InvalidManifest(format!(
            "unsafe prepared-file path: {path}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::validate_path;

    #[test]
    fn a_path_that_could_leave_the_payload_is_refused() {
        for path in [
            "",
            "/etc/passwd",
            "content/../../etc/passwd",
            "content\\drm\\Kbuild",
            "content//drm",
            "payload.json",
        ] {
            assert!(validate_path(path).is_err(), "accepted {path}");
        }
    }

    #[test]
    fn an_ordinary_relative_path_is_accepted() {
        assert!(validate_path("content/drm/vmlord_drm.c").is_ok());
    }
}
