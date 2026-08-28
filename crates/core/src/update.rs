use std::fmt;

use semver::Version;
use serde::{Deserialize, Serialize};

const SUPPORTED_SCHEMA: u32 = 1;
const MAX_INSTALLER_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const RELEASE_DOWNLOAD_PREFIX: &str = "https://github.com/MrUndead1996/vm-lord/releases/download/";

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReleaseManifest {
    pub schema: u32,
    pub version: Version,
    pub installer: InstallerAsset,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct InstallerAsset {
    pub url: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidatedUpdate {
    pub version: Version,
    pub installer: InstallerAsset,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdateManifestError {
    Schema,
    InstallerHash,
    InstallerSize,
    InstallerUrl,
}

impl fmt::Display for UpdateManifestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::Schema => "unsupported release manifest schema",
            Self::InstallerHash => "invalid installer SHA-256",
            Self::InstallerSize => "invalid installer size",
            Self::InstallerUrl => "invalid installer URL",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for UpdateManifestError {}

impl ReleaseManifest {
    /// Validates the release data before any installer download is considered.
    pub fn validate(
        &self,
        current: &Version,
    ) -> Result<Option<ValidatedUpdate>, UpdateManifestError> {
        if self.schema != SUPPORTED_SCHEMA {
            return Err(UpdateManifestError::Schema);
        }
        if !is_sha256(&self.installer.sha256) {
            return Err(UpdateManifestError::InstallerHash);
        }
        if self.installer.size == 0 || self.installer.size > MAX_INSTALLER_BYTES {
            return Err(UpdateManifestError::InstallerSize);
        }
        if !is_release_installer_url(&self.installer.url, &self.version) {
            return Err(UpdateManifestError::InstallerUrl);
        }
        if self.version <= *current {
            return Ok(None);
        }

        Ok(Some(ValidatedUpdate {
            version: self.version.clone(),
            installer: self.installer.clone(),
        }))
    }
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn is_release_installer_url(url: &str, version: &Version) -> bool {
    let prefix = format!("{RELEASE_DOWNLOAD_PREFIX}v{version}/");
    let Some(asset_name) = url.strip_prefix(&prefix) else {
        return false;
    };

    !asset_name.is_empty() && !asset_name.contains(['/', '?', '#'])
}

#[cfg(test)]
mod tests {
    use semver::Version;

    use super::{InstallerAsset, ReleaseManifest, UpdateManifestError, MAX_INSTALLER_BYTES};

    fn manifest() -> ReleaseManifest {
        ReleaseManifest {
            schema: 1,
            version: Version::new(0, 2, 0),
            installer: InstallerAsset {
                url: "https://github.com/MrUndead1996/vm-lord/releases/download/v0.2.0/VMLord-0.2.0-x86_64-setup.exe".to_owned(),
                size: 1,
                sha256: "a".repeat(64),
            },
        }
    }

    fn manifest_with_url(url: &str) -> ReleaseManifest {
        let mut manifest = manifest();
        manifest.installer.url = url.to_owned();
        manifest
    }

    #[test]
    fn schema_one_accepts_a_newer_release() {
        let manifest = manifest();

        let update = manifest.validate(&Version::new(0, 1, 0)).unwrap();

        assert_eq!(update.unwrap().version, Version::new(0, 2, 0));
    }

    #[test]
    fn unknown_schema_is_refused() {
        let mut manifest = manifest();
        manifest.schema = 2;

        assert!(matches!(
            manifest.validate(&Version::new(0, 1, 0)),
            Err(UpdateManifestError::Schema)
        ));
    }

    #[test]
    fn equal_version_has_no_update() {
        assert_eq!(manifest().validate(&Version::new(0, 2, 0)).unwrap(), None);
    }

    #[test]
    fn older_version_has_no_update() {
        assert_eq!(manifest().validate(&Version::new(0, 3, 0)).unwrap(), None);
    }

    #[test]
    fn malformed_sha256_is_refused() {
        for sha256 in ["a".repeat(63), "A".repeat(64), "g".repeat(64)] {
            let mut manifest = manifest();
            manifest.installer.sha256 = sha256;

            assert!(matches!(
                manifest.validate(&Version::new(0, 1, 0)),
                Err(UpdateManifestError::InstallerHash)
            ));
        }
    }

    #[test]
    fn zero_sized_installer_is_refused() {
        let mut manifest = manifest();
        manifest.installer.size = 0;

        assert!(matches!(
            manifest.validate(&Version::new(0, 1, 0)),
            Err(UpdateManifestError::InstallerSize)
        ));
    }

    #[test]
    fn installer_larger_than_the_download_limit_is_refused() {
        let mut manifest = manifest();
        manifest.installer.size = MAX_INSTALLER_BYTES + 1;

        assert!(matches!(
            manifest.validate(&Version::new(0, 1, 0)),
            Err(UpdateManifestError::InstallerSize)
        ));
    }

    #[test]
    fn non_https_installer_is_refused() {
        let manifest = manifest_with_url(
            "http://github.com/MrUndead1996/vm-lord/releases/download/v0.2.0/setup.exe",
        );

        assert!(matches!(
            manifest.validate(&Version::new(0, 1, 0)),
            Err(UpdateManifestError::InstallerUrl)
        ));
    }

    #[test]
    fn unexpected_github_host_is_refused() {
        let manifest = manifest_with_url(
            "https://api.github.com/MrUndead1996/vm-lord/releases/download/v0.2.0/setup.exe",
        );

        assert!(matches!(
            manifest.validate(&Version::new(0, 1, 0)),
            Err(UpdateManifestError::InstallerUrl)
        ));
    }

    #[test]
    fn another_repository_cannot_supply_an_installer() {
        let manifest = manifest_with_url(
            "https://github.com/attacker/vm-lord/releases/download/v0.2.0/setup.exe",
        );
        assert!(matches!(
            manifest.validate(&Version::new(0, 1, 0)),
            Err(UpdateManifestError::InstallerUrl)
        ));
    }

    #[test]
    fn release_path_must_name_the_manifest_version() {
        let manifest = manifest_with_url(
            "https://github.com/MrUndead1996/vm-lord/releases/download/v0.2.1/setup.exe",
        );

        assert!(matches!(
            manifest.validate(&Version::new(0, 1, 0)),
            Err(UpdateManifestError::InstallerUrl)
        ));
    }
}
