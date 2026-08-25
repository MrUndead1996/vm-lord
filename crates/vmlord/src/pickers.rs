//! The Windows file dialogs the application layer asks for.
//!
//! They live in the composition root rather than in `platform`: the traits they
//! implement belong to `app`, and `platform` depends on the domain crate alone.
//! Nothing here touches a Windows API directly -- `rfd` opens the common
//! dialogs -- so no layering rule is bent to keep them here.

use vmlord_app::{ImagePicker, SettingsPathPicker};
use vmlord_core::RepositoryError;

pub struct WindowsImagePicker;

impl WindowsImagePicker {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Default for WindowsImagePicker {
    fn default() -> Self {
        Self::new()
    }
}

impl ImagePicker for WindowsImagePicker {
    fn pick_iso_image(&mut self) -> Result<Option<String>, RepositoryError> {
        Ok(rfd::FileDialog::new()
            .set_title("Select Linux VM image")
            .add_filter("VM images", &["iso", "vhdx"])
            .pick_file()
            .map(|path| path.to_string_lossy().into_owned()))
    }
}

pub struct WindowsSettingsPathPicker;

impl WindowsSettingsPathPicker {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Default for WindowsSettingsPathPicker {
    fn default() -> Self {
        Self::new()
    }
}

impl SettingsPathPicker for WindowsSettingsPathPicker {
    fn pick_vm_storage_directory(&mut self) -> Result<Option<String>, RepositoryError> {
        Ok(rfd::FileDialog::new()
            .set_title("Select VM storage directory")
            .pick_folder()
            .map(|path| path.to_string_lossy().into_owned()))
    }

    fn pick_log_file(&mut self) -> Result<Option<String>, RepositoryError> {
        Ok(rfd::FileDialog::new()
            .set_title("Select log file")
            .set_file_name("vmlord.log")
            .save_file()
            .map(|path| path.to_string_lossy().into_owned()))
    }
}
