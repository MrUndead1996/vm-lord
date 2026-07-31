use std::{
    fmt,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::PathBuf,
    sync::Mutex,
};

use log::{LevelFilter, Log, Metadata, Record, SetLoggerError};

use crate::{AppSettings, LogLevel};

/// Initializes the application-wide logger using the configured output file and level.
pub fn initialize(settings: &AppSettings) -> Result<(), LoggingError> {
    let log_directory =
        settings
            .log_file_path
            .parent()
            .ok_or_else(|| LoggingError::MissingParent {
                path: settings.log_file_path.clone(),
            })?;
    fs::create_dir_all(log_directory).map_err(|source| LoggingError::Io {
        operation: "create log directory",
        path: log_directory.to_path_buf(),
        source,
    })?;

    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&settings.log_file_path)
        .map_err(|source| LoggingError::Io {
            operation: "open log file",
            path: settings.log_file_path.clone(),
            source,
        })?;
    let level = level_filter(settings.log_level);
    let logger = Box::leak(Box::new(ApplicationLogger {
        level,
        file: Mutex::new(file),
    }));

    log::set_logger(logger).map_err(LoggingError::AlreadyInitialized)?;
    log::set_max_level(level);
    Ok(())
}

fn level_filter(level: LogLevel) -> LevelFilter {
    match level {
        LogLevel::Error => LevelFilter::Error,
        LogLevel::Warn => LevelFilter::Warn,
        LogLevel::Info => LevelFilter::Info,
        LogLevel::Debug => LevelFilter::Debug,
        LogLevel::Trace => LevelFilter::Trace,
    }
}

struct ApplicationLogger {
    level: LevelFilter,
    file: Mutex<File>,
}

impl Log for ApplicationLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() <= self.level
    }

    fn log(&self, record: &Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }

        let line = format!(
            "[{:<5}] {}: {}",
            record.level(),
            record.target(),
            record.args()
        );
        let _ = writeln!(io::stdout().lock(), "{line}");
        if let Ok(mut file) = self.file.lock() {
            let _ = writeln!(file, "{line}");
        }
    }

    fn flush(&self) {
        let _ = io::stdout().lock().flush();
        if let Ok(mut file) = self.file.lock() {
            let _ = file.flush();
        }
    }
}

#[derive(Debug)]
pub enum LoggingError {
    MissingParent {
        path: PathBuf,
    },
    Io {
        operation: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    AlreadyInitialized(SetLoggerError),
}

impl fmt::Display for LoggingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingParent { path } => {
                write!(
                    formatter,
                    "log file path has no parent directory: {}",
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
            Self::AlreadyInitialized(source) => {
                write!(
                    formatter,
                    "application logger is already initialized: {source}"
                )
            }
        }
    }
}

impl std::error::Error for LoggingError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::AlreadyInitialized(_) => None,
            Self::MissingParent { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use log::LevelFilter;

    use super::level_filter;
    use crate::LogLevel;

    #[test]
    fn maps_configured_levels_to_log_filters() {
        assert_eq!(level_filter(LogLevel::Error), LevelFilter::Error);
        assert_eq!(level_filter(LogLevel::Warn), LevelFilter::Warn);
        assert_eq!(level_filter(LogLevel::Info), LevelFilter::Info);
        assert_eq!(level_filter(LogLevel::Debug), LevelFilter::Debug);
        assert_eq!(level_filter(LogLevel::Trace), LevelFilter::Trace);
    }
}
