use std::{
    fmt::{self, Write as _},
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::PathBuf,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

use tracing::{
    Event, Level, Metadata, Subscriber,
    field::{Field, Visit},
    level_filters::LevelFilter,
    span::{Attributes, Id},
    subscriber::SetGlobalDefaultError,
};
use tracing_log::log_tracer::SetLoggerError;
use tracing_subscriber::{Layer, layer::Context, layer::SubscriberExt as _, registry::LookupSpan};

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
pub(crate) enum Console {
    Echo,
    Silent,
}

fn install(settings: &AppSettings, console: Console) -> Result<(), LoggingError> {
    let layer = record_layer(settings, console)?;
    install_bridge(settings)?;
    tracing::subscriber::set_global_default(tracing_subscriber::registry().with(layer))
        .map_err(LoggingError::AlreadyInitialized)?;
    Ok(())
}

/// Brings up logging and the diagnostics panel together.
///
/// The panel is the reason a sink comes back: the caller hands it to the
/// application, which reads it on every refresh. `vmlord-com1` and
/// `vmlord-display` call `initialize` instead -- neither has a panel to show a
/// record in.
///
/// # Errors
///
/// [`LoggingError`] when the log directory or file cannot be opened, or when a
/// subscriber is already installed.
pub fn initialize_with_diagnostics(
    settings: &AppSettings,
) -> Result<crate::diagnostics::DiagnosticsSink, LoggingError> {
    let sink = crate::diagnostics::DiagnosticsSink::new();
    let layer = record_layer(settings, Console::Echo)?;
    install_bridge(settings)?;
    tracing::subscriber::set_global_default(
        tracing_subscriber::registry()
            .with(layer)
            .with(crate::diagnostics::DiagnosticsLayer::new(sink.clone())),
    )
    .map_err(LoggingError::AlreadyInitialized)?;
    Ok(sink)
}

/// Opens the log file and builds the layer that writes to it.
///
/// Separate from installation so that a process which also wants diagnostics
/// can compose the two layers rather than repeat the file handling.
fn record_layer(settings: &AppSettings, console: Console) -> Result<RecordLayer, LoggingError> {
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

    Ok(RecordLayer {
        level: level_filter(settings.log_level),
        console,
        file: Mutex::new(file),
    })
}

/// Points the `log` crate at the subscriber.
///
/// Dependencies -- `eframe` among them -- write through `log`, and without this
/// their records stop reaching the file. `LogTracer` decides what to forward
/// from `log`'s own maximum level, so the two have to be told the same thing.
fn install_bridge(settings: &AppSettings) -> Result<(), LoggingError> {
    tracing_log::LogTracer::init().map_err(LoggingError::LogBridge)?;
    log::set_max_level(log_level_filter(settings.log_level));
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

/// One record's line, stamp first, fields last.
///
/// Its own function for the same reason `emit` is: what a line looks like is
/// worth a test, and installing a subscriber to read one back is not. `fields`
/// arrives already rendered and already carrying its leading space, or empty.
fn compose(stamp: &str, level: Level, target: &str, message: &str, fields: &str) -> String {
    format!(
        "[{stamp}] [{:<5}] {target}: {message}{fields}",
        level.as_str()
    )
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
pub(crate) fn timestamp(now: SystemTime) -> String {
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
        LogLevel::Error => LevelFilter::ERROR,
        LogLevel::Warn => LevelFilter::WARN,
        LogLevel::Info => LevelFilter::INFO,
        LogLevel::Debug => LevelFilter::DEBUG,
        LogLevel::Trace => LevelFilter::TRACE,
    }
}

/// The same setting, in the units the `log` bridge is configured in.
fn log_level_filter(level: LogLevel) -> log::LevelFilter {
    match level {
        LogLevel::Error => log::LevelFilter::Error,
        LogLevel::Warn => log::LevelFilter::Warn,
        LogLevel::Info => log::LevelFilter::Info,
        LogLevel::Debug => log::LevelFilter::Debug,
        LogLevel::Trace => log::LevelFilter::Trace,
    }
}

/// The layer that writes records to the log file, and to a console when there
/// is one to write to.
pub(crate) struct RecordLayer {
    level: LevelFilter,
    console: Console,
    file: Mutex<File>,
}

impl<S> Layer<S> for RecordLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn enabled(&self, metadata: &Metadata<'_>, _: Context<'_, S>) -> bool {
        *metadata.level() <= self.level
    }

    fn on_new_span(&self, attributes: &Attributes<'_>, id: &Id, context: Context<'_, S>) {
        // Rendered once, when the span opens, rather than on every event
        // inside it: an operation's fields do not change while it runs.
        let mut visitor = RecordVisitor::default();
        attributes.record(&mut visitor);
        if let Some(span) = context.span(id) {
            span.extensions_mut().insert(SpanFields(visitor.fields));
        }
    }

    fn on_event(&self, event: &Event<'_>, context: Context<'_, S>) {
        if *event.metadata().level() > self.level {
            return;
        }

        let mut visitor = RecordVisitor::default();
        event.record(&mut visitor);

        // The operation's own fields come before the event's: a reader who
        // scans down a column wants the VM in the same place on every line.
        let mut fields = String::new();
        if let Some(scope) = context.event_scope(event) {
            for span in scope.from_root() {
                if let Some(rendered) = span.extensions().get::<SpanFields>() {
                    fields.push_str(&rendered.0);
                }
            }
        }
        fields.push_str(&visitor.fields);

        let line = compose(
            &timestamp(SystemTime::now()),
            *event.metadata().level(),
            event.metadata().target(),
            &visitor.message,
            &fields,
        );
        if let Ok(mut file) = self.file.lock() {
            emit(&line, self.console, &mut io::stdout().lock(), &mut *file);
        } else if self.console == Console::Echo {
            let _ = writeln!(io::stdout().lock(), "{line}");
        }
    }
}

/// A span's fields, rendered when it opened.
struct SpanFields(String);

/// Pulls a record's message and its remaining fields apart.
///
/// `tracing` carries the message as a field named `message`, so the two have to
/// be separated here rather than in a format string. Everything else is
/// rendered `name=value` with a leading space, so `compose` can append the lot
/// without knowing whether there were any.
#[derive(Default)]
struct RecordVisitor {
    message: String,
    fields: String,
}

impl Visit for RecordVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        } else {
            let _ = write!(self.fields, " {}={value}", field.name());
        }
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        // A code reads as hex or it reads as nothing: `0x803B0014` is
        // searchable and `2151546900` is not.
        if field.name() == "code" {
            let _ = write!(self.fields, " code=0x{value:08X}");
        } else {
            let _ = write!(self.fields, " {}={value}", field.name());
        }
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        let _ = write!(self.fields, " {}={value}", field.name());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        // A field written `%value` arrives here too, wrapped so that its
        // `Debug` prints the `Display` text unquoted.
        if field.name() == "message" {
            self.message = format!("{value:?}");
        } else {
            let _ = write!(self.fields, " {}={value:?}", field.name());
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
    AlreadyInitialized(SetGlobalDefaultError),
    LogBridge(SetLoggerError),
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
            Self::LogBridge(source) => {
                write!(
                    formatter,
                    "the log-to-tracing bridge is already installed: {source}"
                )
            }
        }
    }
}

impl std::error::Error for LoggingError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::AlreadyInitialized(_) | Self::LogBridge(_) => None,
            Self::MissingParent { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use tracing::{Level, level_filters::LevelFilter};

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
            Level::INFO,
            "vmlord_display_viewer::status",
            "the display session is Running",
            "",
        );

        assert_eq!(
            line,
            "[2024-02-29T01:01:01.123Z] [INFO ] \
             vmlord_display_viewer::status: the display session is Running"
        );
    }

    #[test]
    fn maps_configured_levels_to_log_filters() {
        assert_eq!(level_filter(LogLevel::Error), LevelFilter::ERROR);
        assert_eq!(level_filter(LogLevel::Warn), LevelFilter::WARN);
        assert_eq!(level_filter(LogLevel::Info), LevelFilter::INFO);
        assert_eq!(level_filter(LogLevel::Debug), LevelFilter::DEBUG);
        assert_eq!(level_filter(LogLevel::Trace), LevelFilter::TRACE);
    }

    #[test]
    fn a_records_fields_follow_its_message() {
        // Fields are what makes a record selectable. They go after the message
        // so that a line still reads as a sentence first.
        let line = compose(
            "2024-02-29T01:01:01.123Z",
            Level::WARN,
            "vmlord_platform::repository",
            "the endpoint could not be attached",
            " vm=dev-linux code=0x803B0014",
        );

        assert_eq!(
            line,
            "[2024-02-29T01:01:01.123Z] [WARN ] \
             vmlord_platform::repository: the endpoint could not be attached \
             vm=dev-linux code=0x803B0014"
        );
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
