use std::{
    fmt,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::PathBuf,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

use log::{Level, LevelFilter, Log, Metadata, Record, SetLoggerError};

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

/// One record's line, stamp first.
///
/// Its own function for the same reason `emit` is: what a line looks like is
/// worth a test, and installing a logger to read one back is not.
fn compose(stamp: &str, level: Level, target: &str, message: &fmt::Arguments<'_>) -> String {
    format!("[{stamp}] [{level:<5}] {target}: {message}")
}

/// The instant a record was written, as `1970-01-01T00:00:00.000Z`.
///
/// UTC, and said so with the `Z`. A local stamp would need a timezone database
/// or a platform call -- the second of which means `unsafe` in a crate that has
/// neither -- and it would read differently on either side of a DST boundary,
/// which is exactly when a log is being read to work out what happened. One
/// clock also makes the host's log line up with a guest's `journalctl` by
/// nothing more than an offset.
///
/// Milliseconds, because the thresholds this log has to be able to settle are
/// stated in them.
fn timestamp(now: SystemTime) -> String {
    let since = now.duration_since(UNIX_EPOCH).unwrap_or_default();
    let seconds = since.as_secs();
    let (days, time) = (seconds / 86_400, seconds % 86_400);
    let (year, month, day) = civil_from_days(days as i64);

    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        time / 3_600,
        (time % 3_600) / 60,
        time % 60,
        since.subsec_millis()
    )
}

/// The civil date `days` after 1970-01-01, by Howard Hinnant's algorithm.
///
/// Written out rather than approximated because the leap rule is the whole
/// difficulty: a year is 365 days except when it is 366, which is every fourth
/// except every hundredth except every four-hundredth. The algorithm shifts the
/// era to start in March so that the leap day lands at the end of it and the
/// month lengths become a single linear formula.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = (shifted - era * 146_097) as u64;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;

    let day = (day_of_year - (153 * shifted_month + 2) / 5 + 1) as u32;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    } as u32;
    let year = year_of_era as i64 + era * 400;

    (if month <= 2 { year + 1 } else { year }, month, day)
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

        let line = compose(
            &timestamp(SystemTime::now()),
            record.level(),
            record.target(),
            record.args(),
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
    use std::time::{Duration, UNIX_EPOCH};

    use log::{Level, LevelFilter};

    use super::{Console, compose, emit, level_filter, timestamp};
    use crate::LogLevel;

    #[test]
    fn the_epoch_is_spelled_as_the_day_it_was() {
        assert_eq!(timestamp(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
    }

    #[test]
    fn a_leap_day_is_the_twenty_ninth_and_not_the_first_of_march() {
        // 2024-02-29T00:00:00Z. The day the naive "365 days a year" arithmetic
        // gets wrong, which is the whole reason this conversion is written out
        // rather than approximated.
        let leap = UNIX_EPOCH + Duration::from_secs(1_709_164_800);

        assert_eq!(timestamp(leap), "2024-02-29T00:00:00.000Z");
    }

    #[test]
    fn the_time_of_day_is_carried_down_to_the_millisecond() {
        // A millisecond is the resolution the latency thresholds of task #128
        // are stated in; a whole second would not settle "under 100 ms".
        let moment = UNIX_EPOCH + Duration::from_millis(1_709_164_800_000 + 3_661_123);

        assert_eq!(timestamp(moment), "2024-02-29T01:01:01.123Z");
    }

    #[test]
    fn a_record_carries_its_stamp_before_anything_else() {
        let line = compose(
            "2024-02-29T01:01:01.123Z",
            Level::Info,
            "vmlord_display_viewer::status",
            &format_args!("the display session is Running"),
        );

        assert_eq!(
            line,
            "[2024-02-29T01:01:01.123Z] [INFO ] \
             vmlord_display_viewer::status: the display session is Running"
        );
    }

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

        emit(
            "[INFO ] something happened",
            Console::Silent,
            &mut out,
            &mut file,
        );

        assert!(
            out.is_empty(),
            "nothing may reach a pipe this logger did not frame"
        );
        assert!(String::from_utf8_lossy(&file).contains("something happened"));
    }

    #[test]
    fn a_process_started_from_a_console_still_gets_its_records_there() {
        let mut out = Vec::new();
        let mut file = Vec::new();

        emit(
            "[INFO ] something happened",
            Console::Echo,
            &mut out,
            &mut file,
        );

        assert!(String::from_utf8_lossy(&out).contains("something happened"));
        assert!(String::from_utf8_lossy(&file).contains("something happened"));
    }
}
