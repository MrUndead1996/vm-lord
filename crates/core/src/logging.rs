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
///
/// Records go to the log file and to standard output, which is what a program
/// started from a console wants.
pub fn initialize(settings: &AppSettings) -> Result<(), LoggingError> {
    install(settings, Console::Echo)
}

/// The same, for a process whose standard output is not a console.
///
/// `vmlord-display` is started by VMLord with a pipe as its standard output,
/// and that pipe carries length-prefixed launch messages. A log record written
/// there is not a stray line in a terminal: it is bytes in the middle of a
/// frame, and the reader on the other end takes the first four of them for a
/// length. Anything that writes to a pipe it did not frame belongs here.
pub fn initialize_without_console(settings: &AppSettings) -> Result<(), LoggingError> {
    install(settings, Console::Silent)
}

/// Whether records are echoed to standard output as well as written to file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Console {
    Echo,
    Silent,
}

fn install(settings: &AppSettings, console: Console) -> Result<(), LoggingError> {
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
        console,
        file: Mutex::new(file),
    }));

    log::set_logger(logger).map_err(LoggingError::AlreadyInitialized)?;
    log::set_max_level(level);
    Ok(())
}

/// Writes one record to the file, and to the console only when asked.
///
/// Its own function so that the one decision this logger makes can be tested
/// without installing a logger or owning a terminal.
fn emit(line: &str, console: Console, out: &mut impl Write, file: &mut impl Write) {
    if console == Console::Echo {
        let _ = writeln!(out, "{line}");
    }
    let _ = writeln!(file, "{line}");
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
    console: Console,
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
        if let Ok(mut file) = self.file.lock() {
            emit(&line, self.console, &mut io::stdout().lock(), &mut *file);
        } else if self.console == Console::Echo {
            let _ = writeln!(io::stdout().lock(), "{line}");
        }
    }

    fn flush(&self) {
        if self.console == Console::Echo {
            let _ = io::stdout().lock().flush();
        }
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

    use super::{Console, emit, level_filter};
    use crate::LogLevel;

    #[test]
    fn maps_configured_levels_to_log_filters() {
        assert_eq!(level_filter(LogLevel::Error), LevelFilter::Error);
        assert_eq!(level_filter(LogLevel::Warn), LevelFilter::Warn);
        assert_eq!(level_filter(LogLevel::Info), LevelFilter::Info);
        assert_eq!(level_filter(LogLevel::Debug), LevelFilter::Debug);
        assert_eq!(level_filter(LogLevel::Trace), LevelFilter::Trace);
    }

    #[test]
    fn a_process_whose_output_is_a_pipe_writes_no_record_into_it() {
        // `vmlord-display`'s standard output carries length-prefixed launch
        // messages. One log line in there is four bytes read as a length.
        let mut out = Vec::new();
        let mut file = Vec::new();

        emit("[INFO ] something happened", Console::Silent, &mut out, &mut file);

        assert!(out.is_empty(), "nothing may reach a pipe this logger did not frame");
        assert!(String::from_utf8_lossy(&file).contains("something happened"));
    }

    #[test]
    fn a_process_started_from_a_console_still_gets_its_records_there() {
        let mut out = Vec::new();
        let mut file = Vec::new();

        emit("[INFO ] something happened", Console::Echo, &mut out, &mut file);

        assert!(String::from_utf8_lossy(&out).contains("something happened"));
        assert!(String::from_utf8_lossy(&file).contains("something happened"));
    }
}
