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

/// A subscriber that keeps every record, for the tests that assert what is not
/// in them.
///
/// Scoped rather than global: `capture` installs it for the duration of one
/// closure, so two tests running side by side cannot read each other's
/// records. The old global logger could only be installed once per process,
/// which meant every test shared one buffer.
#[cfg(test)]
pub mod capture {
    use std::{
        fmt,
        sync::{Arc, Mutex},
    };

    use tracing::{
        Event, Subscriber,
        field::{Field, Visit},
    };
    use tracing_subscriber::{Layer, layer::Context, layer::SubscriberExt as _};

    /// Runs `body` with every record it writes captured, and hands back what
    /// it returned along with the records, joined.
    pub fn capture<T>(body: impl FnOnce() -> T) -> (T, String) {
        let records = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(Capture(Arc::clone(&records)));
        let value = tracing::subscriber::with_default(subscriber, body);
        let text = records
            .lock()
            .expect("no test panics while holding the records")
            .join("\n");
        (value, text)
    }

    struct Capture(Arc<Mutex<Vec<String>>>);

    impl<S: Subscriber> Layer<S> for Capture {
        fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
            let mut message = String::new();
            event.record(&mut MessageVisitor(&mut message));
            self.0
                .lock()
                .expect("no test panics while holding the records")
                .push(message);
        }
    }

    /// Keeps the record's text and discards its fields: these tests assert
    /// what a message does not contain.
    struct MessageVisitor<'a>(&'a mut String);

    impl Visit for MessageVisitor<'_> {
        fn record_str(&mut self, field: &Field, value: &str) {
            if field.name() == "message" {
                *self.0 = value.to_string();
            }
        }

        fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
            if field.name() == "message" {
                *self.0 = format!("{value:?}");
            }
        }
    }
}
