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

/// Persistent settings that configure application-wide behavior.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppSettings {
    /// Directory used to persist VM data.
    pub vm_storage_path: PathBuf,
    /// BCP 47 language tag used by application clients.
    pub language: String,
    /// Destination for application log records.
    pub log_file_path: PathBuf,
    pub log_level: LogLevel,
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
            Ok(contents) => toml::from_str(&contents).map_err(|source| SettingsError::Parse {
                path: self.config_path.clone(),
                source,
            }),
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
            language: "en-US".into(),
            log_file_path: config_directory
                .join(DEFAULT_LOG_DIRECTORY)
                .join(DEFAULT_LOG_FILE_NAME),
            log_level: LogLevel::Info,
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
        source: toml::de::Error,
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

    use super::{AppSettings, LogLevel, SettingsStore};

    fn temporary_directory() -> std::path::PathBuf {
        let unique_id = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("vmlord-settings-test-{unique_id}"))
    }

    #[test]
    fn load_or_create_writes_defaults_and_required_directories() {
        let directory = temporary_directory();
        let config_path = directory.join("settings.toml");
        let store = SettingsStore::new(&config_path);

        let settings = store.load_or_create().unwrap();

        assert_eq!(settings.vm_storage_path, directory.join("vms"));
        assert_eq!(settings.language, "en-US");
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
    fn save_and_load_preserves_custom_settings() {
        let directory = temporary_directory();
        let store = SettingsStore::new(directory.join("settings.toml"));
        let settings = AppSettings {
            vm_storage_path: directory.join("virtual-machines"),
            language: "ru-RU".into(),
            log_file_path: directory.join("diagnostics").join("application.log"),
            log_level: LogLevel::Debug,
        };

        store.save(&settings).unwrap();

        assert_eq!(store.load_or_create().unwrap(), settings);
        fs::remove_dir_all(directory).unwrap();
    }
}
