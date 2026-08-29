use std::path::PathBuf;

/// Paths and source identity resolved by discovery and owned by the platform.
///
/// This type deliberately has no `Debug` implementation: it contains the
/// AppSandbox private-key path and must never cross into the application or UI
/// layers. A later import resolves an opaque source ID only through the latest
/// discovery snapshot and revalidates these observations before copying.
pub(crate) struct ValidatedSource {
    pub(crate) config_path: PathBuf,
    pub(crate) vm_ordinal: usize,
    pub(crate) source_disk: PathBuf,
    pub(crate) private_key: PathBuf,
}
