//! Where this process writes what it did, and what it must never write.
//!
//! # The redaction rule
//!
//! Decoded pixels, cursor bitmaps and the codec payloads they came from never
//! reach a log record. What may: sizes, sequences, generations, geometry,
//! session states, error codes and the text of an `Error` record. There is no
//! screenshot feature in this build, and adding one would need a warning to the
//! user before it wrote anything -- the rule is stated here so that nobody adds
//! one quietly.

/// Brings the application log up, so that a viewer's story lands in the same
/// file as VMLord's -- and nowhere near its standard output, which is the
/// launch pipe.
///
/// A viewer that cannot log still shows a desktop: a failure here is reported
/// to standard error and nothing else. Losing the log is not worth losing the
/// session.
pub fn initialize() {
    let settings =
        vmlord_core::SettingsStore::for_current_user().and_then(|store| store.load_or_create());

    match settings {
        Ok(settings) => {
            // Never to standard output: that pipe carries framed launch
            // messages, and a log line written into it is read as a length.
            if let Err(error) = vmlord_core::initialize_logging_without_console(&settings) {
                eprintln!("VMLord Display: the log could not be opened: {error}");
            }
        }
        Err(error) => eprintln!("VMLord Display: settings could not be read: {error}"),
    }
}

/// A logger that keeps every record, for the tests that assert what is not in
/// them.
#[cfg(test)]
pub mod capture {
    use std::sync::{Mutex, OnceLock};

    use log::{Level, LevelFilter, Log, Metadata, Record};

    static RECORDS: OnceLock<Mutex<Vec<String>>> = OnceLock::new();

    struct Capture;

    impl Log for Capture {
        fn enabled(&self, _: &Metadata<'_>) -> bool {
            true
        }

        fn log(&self, record: &Record<'_>) {
            if record.level() <= Level::Trace {
                records()
                    .lock()
                    .expect("no test panics while holding the log")
                    .push(record.args().to_string());
            }
        }

        fn flush(&self) {}
    }

    fn records() -> &'static Mutex<Vec<String>> {
        RECORDS.get_or_init(|| Mutex::new(Vec::new()))
    }

    /// Installs the capturing logger. Safe to call from every test.
    pub fn install() {
        static INSTALLED: OnceLock<()> = OnceLock::new();
        INSTALLED.get_or_init(|| {
            let _ = log::set_logger(&Capture);
            log::set_max_level(LevelFilter::Trace);
        });
    }

    /// Everything logged so far, joined.
    pub fn text() -> String {
        records()
            .lock()
            .expect("no test panics while holding the log")
            .join("\n")
    }
}
