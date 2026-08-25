use std::{
    env, fmt, fs, io,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

const APPLICATION_DIRECTORY: &str = "VMLord";
const SETTINGS_FILE_NAME: &str = "settings.toml";
const DEFAULT_VM_DIRECTORY: &str = "vms";
const DEFAULT_LOG_DIRECTORY: &str = "logs";
const DEFAULT_LOG_FILE_NAME: &str = "vmlord.log";
const DEFAULT_IMAGE_DIRECTORY: &str = "images";

/// Persistent settings that configure application-wide behavior.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppSettings {
    /// Directory used to persist VM data.
    pub vm_storage_path: PathBuf,
    /// UI language.
    pub language: Language,
    /// Destination for application log records.
    pub log_file_path: PathBuf,
    pub log_level: LogLevel,
    /// Directory holding distribution images downloaded from the internet.
    ///
    /// `serde(default)` leaves this empty for a `settings.toml` written before
    /// the field existed; `load_or_create` fills it in. That keeps existing
    /// configurations loading without a migration, the same way `endpoint_id`
    /// and `network_mode` are handled in `VmComputeSystemMapping`.
    #[serde(default)]
    pub image_cache_path: PathBuf,
    /// Timeouts for the readiness wait that ends a VM's creation.
    ///
    /// Last in the struct on purpose: TOML demands that every value precede
    /// every table, and this field is written as a table.
    #[serde(default)]
    pub guest_readiness: GuestReadinessTimeouts,
}

/// How long each phase of waiting for a freshly created guest may take.
///
/// Settings rather than constants because the numbers describe the user's
/// network and hardware, not VMLord: the first boot of a cloud image installs
/// the packages the seed asked for, and ten minutes of that is ordinary on a
/// slow link.
///
/// `#[serde(default)]` on the struct fills in a field an older `settings.toml`
/// does not have, and the same attribute on the field in [`AppSettings`] fills
/// in the whole group -- the treatment `image_cache_path` already gets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GuestReadinessTimeouts {
    /// Waiting for HNS to give the VM's endpoint an address.
    pub address_secs: u64,
    /// Waiting for port 22 to open once the address exists.
    pub ssh_port_secs: u64,
    /// Waiting for `cloud-init status --wait` to return.
    pub cloud_init_secs: u64,
    /// One SSH connection attempt, passed to the client as `ConnectTimeout`.
    pub connect_timeout_secs: u64,
}

impl Default for GuestReadinessTimeouts {
    fn default() -> Self {
        Self {
            address_secs: 90,
            ssh_port_secs: 300,
            cloud_init_secs: 1200,
            connect_timeout_secs: 10,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Language {
    #[serde(rename = "en-US")]
    #[default]
    EnUs,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Error,
    Warn,
    #[default]
    Info,
    Debug,
    Trace,
}

/// File-backed settings repository.
#[derive(Clone, Debug)]
pub struct SettingsStore {
    config_path: PathBuf,
}

impl SettingsStore {
    /// Creates a store for a specific TOML configuration file.
    #[must_use]
    pub fn new(config_path: impl Into<PathBuf>) -> Self {
        Self {
            config_path: config_path.into(),
        }
    }

    /// Creates a store in `%LOCALAPPDATA%\\VMLord\\settings.toml`.
    pub fn for_current_user() -> Result<Self, SettingsError> {
        let local_app_data =
            env::var_os("LOCALAPPDATA").ok_or(SettingsError::LocalAppDataUnavailable)?;
        Ok(Self::new(
            PathBuf::from(local_app_data)
                .join(APPLICATION_DIRECTORY)
                .join(SETTINGS_FILE_NAME),
        ))
    }

    #[must_use]
    pub fn config_path(&self) -> &Path {
        &self.config_path
    }

    /// Loads settings from disk, creating the default configuration if absent.
    pub fn load_or_create(&self) -> Result<AppSettings, SettingsError> {
        match fs::read_to_string(&self.config_path) {
            Ok(contents) => {
                let mut settings: AppSettings =
                    toml::from_str(&contents).map_err(|source| SettingsError::Parse {
                        path: self.config_path.clone(),
                        source: Box::new(source),
                    })?;
                if settings.image_cache_path.as_os_str().is_empty() {
                    settings.image_cache_path =
                        self.config_directory()?.join(DEFAULT_IMAGE_DIRECTORY);
                    tracing::debug!(
                        "settings carried no image cache path; defaulting to {}",
                        settings.image_cache_path.display()
                    );
                }
                Ok(settings)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let settings = self.default_settings()?;
                self.save(&settings)?;
                Ok(settings)
            }
            Err(source) => Err(SettingsError::Io {
                operation: "read settings",
                path: self.config_path.clone(),
                source,
            }),
        }
    }

    /// Persists settings as TOML and creates their parent directories when needed.
    pub fn save(&self, settings: &AppSettings) -> Result<(), SettingsError> {
        let config_directory = self.config_directory()?;
        fs::create_dir_all(config_directory).map_err(|source| SettingsError::Io {
            operation: "create configuration directory",
            path: config_directory.to_path_buf(),
            source,
        })?;
        fs::create_dir_all(&settings.vm_storage_path).map_err(|source| SettingsError::Io {
            operation: "create VM storage directory",
            path: settings.vm_storage_path.clone(),
            source,
        })?;
        fs::create_dir_all(&settings.image_cache_path).map_err(|source| SettingsError::Io {
            operation: "create image cache directory",
            path: settings.image_cache_path.clone(),
            source,
        })?;
        let log_directory =
            settings
                .log_file_path
                .parent()
                .ok_or_else(|| SettingsError::MissingParent {
                    path: settings.log_file_path.clone(),
                })?;
        fs::create_dir_all(log_directory).map_err(|source| SettingsError::Io {
            operation: "create log directory",
            path: log_directory.to_path_buf(),
            source,
        })?;

        let contents = toml::to_string_pretty(settings).map_err(SettingsError::Serialize)?;
        fs::write(&self.config_path, contents).map_err(|source| SettingsError::Io {
            operation: "write settings",
            path: self.config_path.clone(),
            source,
        })
    }

    fn default_settings(&self) -> Result<AppSettings, SettingsError> {
        let config_directory = self.config_directory()?;
        Ok(AppSettings {
            vm_storage_path: config_directory.join(DEFAULT_VM_DIRECTORY),
            language: Language::EnUs,
            log_file_path: config_directory
                .join(DEFAULT_LOG_DIRECTORY)
                .join(DEFAULT_LOG_FILE_NAME),
            log_level: LogLevel::Info,
            image_cache_path: config_directory.join(DEFAULT_IMAGE_DIRECTORY),
            guest_readiness: GuestReadinessTimeouts::default(),
        })
    }

    fn config_directory(&self) -> Result<&Path, SettingsError> {
        self.config_path
            .parent()
            .ok_or_else(|| SettingsError::MissingParent {
                path: self.config_path.clone(),
            })
    }
}

#[derive(Debug)]
pub enum SettingsError {
    LocalAppDataUnavailable,
    MissingParent {
        path: PathBuf,
    },
    Io {
        operation: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    Parse {
        path: PathBuf,
        source: Box<toml::de::Error>,
    },
    Serialize(toml::ser::Error),
}

impl fmt::Display for SettingsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LocalAppDataUnavailable => formatter.write_str(
                "LOCALAPPDATA is not set; unable to determine the VMLord configuration directory",
            ),
            Self::MissingParent { path } => {
                write!(
                    formatter,
                    "settings path has no parent directory: {}",
                    path.display()
                )
            }
            Self::Io {
                operation,
                path,
                source,
            } => write!(
                formatter,
                "failed to {operation} at {}: {source}",
                path.display()
            ),
            Self::Parse { path, source } => {
                write!(
                    formatter,
                    "failed to parse settings at {}: {source}",
                    path.display()
                )
            }
            Self::Serialize(source) => write!(formatter, "failed to serialize settings: {source}"),
        }
    }
}

impl std::error::Error for SettingsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Parse { source, .. } => Some(source),
            Self::Serialize(source) => Some(source),
            Self::LocalAppDataUnavailable | Self::MissingParent { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::{
        AppSettings, GuestReadinessTimeouts, Language, LogLevel, SettingsError, SettingsStore,
    };

    fn temporary_directory() -> std::path::PathBuf {
        let unique_id = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("vmlord-settings-test-{unique_id}"))
    }

    #[test]
    fn settings_errors_remain_compact() {
        assert!(std::mem::size_of::<SettingsError>() <= 64);
    }

    #[test]
    fn load_or_create_writes_defaults_and_required_directories() {
        let directory = temporary_directory();
        let config_path = directory.join("settings.toml");
        let store = SettingsStore::new(&config_path);

        let settings = store.load_or_create().unwrap();

        assert_eq!(settings.vm_storage_path, directory.join("vms"));
        assert_eq!(settings.language, Language::EnUs);
        assert_eq!(
            settings.log_file_path,
            directory.join("logs").join("vmlord.log")
        );
        assert_eq!(settings.log_level, LogLevel::Info);
        assert!(config_path.is_file());
        assert!(settings.vm_storage_path.is_dir());
        assert!(directory.join("logs").is_dir());
        assert_eq!(store.load_or_create().unwrap(), settings);

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn defaults_put_the_image_cache_next_to_the_other_application_directories() {
        let directory = temporary_directory();
        let store = SettingsStore::new(directory.join("settings.toml"));

        let settings = store.load_or_create().unwrap();

        assert_eq!(settings.image_cache_path, directory.join("images"));
        assert!(settings.image_cache_path.is_dir());

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn settings_written_before_the_image_cache_existed_still_load() {
        let directory = temporary_directory();
        fs::create_dir_all(&directory).unwrap();
        let config_path = directory.join("settings.toml");
        fs::write(
            &config_path,
            format!(
                "vm_storage_path = {vms:?}\n\
                 language = \"en-US\"\n\
                 log_file_path = {log:?}\n\
                 log_level = \"info\"\n",
                vms = directory.join("vms").display().to_string(),
                log = directory.join("vmlord.log").display().to_string(),
            ),
        )
        .unwrap();

        let settings = SettingsStore::new(&config_path).load_or_create().unwrap();

        assert_eq!(
            settings.image_cache_path,
            directory.join("images"),
            "an existing settings.toml must keep loading without a migration"
        );

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn an_explicit_image_cache_path_is_preserved() {
        let directory = temporary_directory();
        let store = SettingsStore::new(directory.join("settings.toml"));
        let settings = AppSettings {
            vm_storage_path: directory.join("vms"),
            language: Language::EnUs,
            log_file_path: directory.join("logs").join("vmlord.log"),
            log_level: LogLevel::Info,
            image_cache_path: directory.join("elsewhere").join("images"),
            guest_readiness: GuestReadinessTimeouts::default(),
        };

        store.save(&settings).unwrap();

        assert_eq!(store.load_or_create().unwrap(), settings);
        assert!(settings.image_cache_path.is_dir());

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn save_and_load_preserves_custom_settings() {
        let directory = temporary_directory();
        let store = SettingsStore::new(directory.join("settings.toml"));
        let settings = AppSettings {
            vm_storage_path: directory.join("virtual-machines"),
            language: Language::EnUs,
            log_file_path: directory.join("diagnostics").join("application.log"),
            log_level: LogLevel::Debug,
            image_cache_path: directory.join("images"),
            guest_readiness: GuestReadinessTimeouts::default(),
        };

        store.save(&settings).unwrap();

        assert_eq!(store.load_or_create().unwrap(), settings);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn readiness_timeouts_have_defaults_and_survive_a_round_trip() {
        let defaults = GuestReadinessTimeouts::default();

        assert_eq!(defaults.address_secs, 90);
        assert_eq!(defaults.ssh_port_secs, 300);
        assert_eq!(defaults.cloud_init_secs, 1200);
        assert_eq!(defaults.connect_timeout_secs, 10);

        let directory = temporary_directory();
        let store = SettingsStore::new(directory.join("settings.toml"));
        let mut settings = store.load_or_create().unwrap();
        assert_eq!(settings.guest_readiness, defaults);

        settings.guest_readiness.cloud_init_secs = 60;
        store.save(&settings).unwrap();

        assert_eq!(
            store
                .load_or_create()
                .unwrap()
                .guest_readiness
                .cloud_init_secs,
            60
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn settings_written_before_the_readiness_timeouts_existed_still_load() {
        // `#[serde(default)]`, as with `image_cache_path`: an older file loads
        // without a migration and gets the default timeouts.
        let directory = temporary_directory();
        fs::create_dir_all(&directory).unwrap();
        let config_path = directory.join("settings.toml");
        fs::write(
            &config_path,
            format!(
                "vm_storage_path = {vms:?}\n\
                 language = \"en-US\"\n\
                 log_file_path = {log:?}\n\
                 log_level = \"info\"\n",
                vms = directory.join("vms").display().to_string(),
                log = directory.join("vmlord.log").display().to_string(),
            ),
        )
        .unwrap();

        let settings = SettingsStore::new(&config_path).load_or_create().unwrap();

        assert_eq!(settings.guest_readiness, GuestReadinessTimeouts::default());

        fs::remove_dir_all(directory).unwrap();
    }
}
