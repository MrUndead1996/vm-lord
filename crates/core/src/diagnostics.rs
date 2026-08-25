//! The stream a person reads, taken from the stream the file records.
//!
//! An event marked with a `diagnostic` field is meant for the user; the layer
//! below collects those and nothing else. Everything about a record other than
//! its text is a field, so the panel can select on it rather than parse prose.

use std::{
    collections::VecDeque,
    str::FromStr,
    sync::{Arc, Mutex},
    time::SystemTime,
};

use tracing::{
    Event, Subscriber,
    field::{Field, Visit},
    span::{Attributes, Id},
};
use tracing_subscriber::{Layer, layer::Context, registry::LookupSpan};

/// How many records the panel keeps. Older ones fall off the front: a session
/// that ran all day should not carry its morning in memory.
const CAPACITY: usize = 100;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub level: DiagnosticLevel,
    pub subsystem: Subsystem,
    pub vm: Option<String>,
    /// A Windows error code, unsigned, shown as `0x{:08X}`. Win32 statuses are
    /// widened to HRESULTs before they get here, so one field is enough.
    pub code: Option<u32>,
    /// Stamped by the layer rather than the call site: when a thing was
    /// observed is a property of the observation, and taking it here means it
    /// cannot be forgotten.
    pub at: SystemTime,
    pub message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiagnosticLevel {
    Info,
    Warning,
    Error,
}

/// Which part of VMLord an event came from.
///
/// A field rather than a guess from the wording of a message: this is what
/// makes HCS, HNS, networking, GPU, display and provisioning one stream instead
/// of six conventions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Subsystem {
    Hcs,
    Hns,
    Network,
    Gpu,
    Display,
    Provisioning,
    Image,
    App,
}

impl Subsystem {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hcs => "Hcs",
            Self::Hns => "Hns",
            Self::Network => "Network",
            Self::Gpu => "Gpu",
            Self::Display => "Display",
            Self::Provisioning => "Provisioning",
            Self::Image => "Image",
            Self::App => "App",
        }
    }
}

impl FromStr for Subsystem {
    type Err = UnknownSubsystem;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "Hcs" => Ok(Self::Hcs),
            "Hns" => Ok(Self::Hns),
            "Network" => Ok(Self::Network),
            "Gpu" => Ok(Self::Gpu),
            "Display" => Ok(Self::Display),
            "Provisioning" => Ok(Self::Provisioning),
            "Image" => Ok(Self::Image),
            "App" => Ok(Self::App),
            _ => Err(UnknownSubsystem),
        }
    }
}

/// What `Subsystem::from_str` answers to a name it does not know.
///
/// Only reachable if a field was written by something other than the
/// `diagnostic!` macro, which is why the layer treats it as "no subsystem"
/// rather than as a reason to drop the record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnknownSubsystem;

/// Where marked records wait for the UI to read them.
///
/// Shared and interior-mutable because the threads that produce them -- a
/// build, a start, a display session -- are not the thread that shows them.
#[derive(Clone, Default)]
pub struct DiagnosticsSink(Arc<Mutex<VecDeque<Diagnostic>>>);

impl DiagnosticsSink {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn push(&self, record: Diagnostic) {
        let mut records = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if records.len() == CAPACITY {
            records.pop_front();
        }
        records.push_back(record);
    }

    /// Everything recorded since the last read, oldest first.
    #[must_use]
    pub fn take(&self) -> Vec<Diagnostic> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .drain(..)
            .collect()
    }
}

/// Collects the events that are addressed to a person.
pub struct DiagnosticsLayer {
    sink: DiagnosticsSink,
}

impl DiagnosticsLayer {
    #[must_use]
    pub fn new(sink: DiagnosticsSink) -> Self {
        Self { sink }
    }
}

impl<S> Layer<S> for DiagnosticsLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attributes: &Attributes<'_>, id: &Id, context: Context<'_, S>) {
        let mut visitor = DiagnosticVisitor::default();
        attributes.record(&mut visitor);
        if let (Some(vm), Some(span)) = (visitor.vm, context.span(id)) {
            span.extensions_mut().insert(SpanVm(vm));
        }
    }

    fn on_event(&self, event: &Event<'_>, context: Context<'_, S>) {
        let mut visitor = DiagnosticVisitor::default();
        event.record(&mut visitor);
        if !visitor.marked {
            return;
        }

        // The event's own VM wins: a refresh may report on one VM from inside
        // another's span, and being relabelled by its surroundings would be a
        // lie.
        let vm = visitor.vm.or_else(|| {
            context.event_scope(event)?.from_root().find_map(|span| {
                span.extensions()
                    .get::<SpanVm>()
                    .map(|found| found.0.clone())
            })
        });

        let level = match *event.metadata().level() {
            tracing::Level::ERROR => DiagnosticLevel::Error,
            tracing::Level::WARN => DiagnosticLevel::Warning,
            _ => DiagnosticLevel::Info,
        };
        self.sink.push(Diagnostic {
            level,
            subsystem: visitor.subsystem.unwrap_or(Subsystem::App),
            vm,
            code: visitor.code,
            at: SystemTime::now(),
            message: visitor.message,
        });
    }
}

/// The VM an operation is about, kept on the span so that events inside it do
/// not have to repeat it.
struct SpanVm(String);

/// Reads the fields the `diagnostic!` macro writes, by name and by type.
#[derive(Default)]
struct DiagnosticVisitor {
    marked: bool,
    subsystem: Option<Subsystem>,
    vm: Option<String>,
    code: Option<u32>,
    message: String,
}

impl Visit for DiagnosticVisitor {
    fn record_bool(&mut self, field: &Field, value: bool) {
        if field.name() == "diagnostic" {
            self.marked = value;
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "subsystem" => self.subsystem = value.parse().ok(),
            "vm" => self.vm = Some(value.to_string()),
            "message" => self.message = value.to_string(),
            _ => {}
        }
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        if field.name() == "code" {
            self.code = u32::try_from(value).ok();
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        // The message arrives here whenever it interpolates anything.
        if field.name() == "message" {
            self.message = format!("{value:?}");
        }
    }
}

/// Records an event that is addressed to a person.
///
/// A macro rather than a function so the marker field cannot be misspelled: a
/// `diagnostc = true` would silently fail to show in the UI and would break no
/// test. Ordinary `info!` and `warn!` stay plain `tracing`; this is only for
/// what the user is meant to read.
///
/// The level is a literal so the `tracing` macro is chosen at compile time,
/// which keeps the record's level and the event's level one decision.
#[macro_export]
macro_rules! diagnostic {
    ($level:ident, $subsystem:expr, vm = $vm:expr, code = $code:expr, $($rest:tt)*) => {
        $crate::__diagnostic_emit!(
            $level,
            diagnostic = true,
            subsystem = $subsystem.as_str(),
            vm = $vm,
            code = u64::from($code),
            $($rest)*
        )
    };
    ($level:ident, $subsystem:expr, vm = $vm:expr, $($rest:tt)*) => {
        $crate::__diagnostic_emit!(
            $level,
            diagnostic = true,
            subsystem = $subsystem.as_str(),
            vm = $vm,
            $($rest)*
        )
    };
    ($level:ident, $subsystem:expr, code = $code:expr, $($rest:tt)*) => {
        $crate::__diagnostic_emit!(
            $level,
            diagnostic = true,
            subsystem = $subsystem.as_str(),
            code = u64::from($code),
            $($rest)*
        )
    };
    ($level:ident, $subsystem:expr, $($rest:tt)*) => {
        $crate::__diagnostic_emit!(
            $level,
            diagnostic = true,
            subsystem = $subsystem.as_str(),
            $($rest)*
        )
    };
}

/// Turns a `DiagnosticLevel` spelled as a literal into the matching `tracing`
/// macro. Separate only so the four forms above do not each repeat it.
#[doc(hidden)]
#[macro_export]
macro_rules! __diagnostic_emit {
    (Info, $($rest:tt)*) => { ::tracing::info!($($rest)*) };
    (Warning, $($rest:tt)*) => { ::tracing::warn!($($rest)*) };
    (Error, $($rest:tt)*) => { ::tracing::error!($($rest)*) };
}

#[cfg(test)]
mod tests {
    use tracing_subscriber::layer::SubscriberExt as _;

    use super::{Diagnostic, DiagnosticLevel, DiagnosticsLayer, DiagnosticsSink, Subsystem};

    fn collect(body: impl FnOnce()) -> Vec<Diagnostic> {
        let sink = DiagnosticsSink::new();
        let subscriber = tracing_subscriber::registry().with(DiagnosticsLayer::new(sink.clone()));
        tracing::subscriber::with_default(subscriber, body);
        sink.take()
    }

    #[test]
    fn a_marked_event_reaches_the_panel() {
        let records = collect(|| {
            crate::diagnostic!(
                Warning,
                Subsystem::Gpu,
                vm = "dev-linux",
                "GPU-PV assignment was refused"
            );
        });

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].level, DiagnosticLevel::Warning);
        assert_eq!(records[0].subsystem, Subsystem::Gpu);
        assert_eq!(records[0].vm.as_deref(), Some("dev-linux"));
        assert_eq!(records[0].message, "GPU-PV assignment was refused");
    }

    #[test]
    fn an_unmarked_event_does_not() {
        // A warning that an image cache did not warm belongs in the file and
        // not in the user's face. The marker, not the level, is what decides.
        let records = collect(|| {
            tracing::warn!("the image cache could not be warmed");
        });

        assert!(records.is_empty(), "{records:?}");
    }

    #[test]
    fn a_windows_code_survives_as_a_number() {
        let records = collect(|| {
            crate::diagnostic!(
                Error,
                Subsystem::Hcs,
                vm = "dev-linux",
                code = 0x803B_0014_u32,
                "the endpoint was already attached"
            );
        });

        assert_eq!(records[0].code, Some(0x803B_0014));
    }

    #[test]
    fn the_sink_keeps_the_hundred_most_recent_records() {
        let records = collect(|| {
            for index in 0..120 {
                crate::diagnostic!(Info, Subsystem::App, "record {index}");
            }
        });

        assert_eq!(records.len(), 100);
        assert_eq!(records[0].message, "record 20");
        assert_eq!(records[99].message, "record 119");
    }

    #[test]
    fn a_subsystem_survives_the_round_trip_through_a_field() {
        // The layer reads the subsystem back by parsing the string the macro
        // wrote. If the two ever disagree the record lands in the wrong place
        // silently, so they are tested against each other.
        for subsystem in [
            Subsystem::Hcs,
            Subsystem::Hns,
            Subsystem::Network,
            Subsystem::Gpu,
            Subsystem::Display,
            Subsystem::Provisioning,
            Subsystem::Image,
            Subsystem::App,
        ] {
            assert_eq!(subsystem.as_str().parse(), Ok(subsystem));
        }
    }

    #[test]
    fn a_record_takes_the_vm_from_the_operation_it_happened_inside() {
        // The point of the span: an event deep inside `start_vm` should not
        // have to name the VM, and should not be able to lose it.
        let records = collect(|| {
            let span = tracing::info_span!("start_vm", vm = "dev-linux");
            let _entered = span.enter();
            crate::diagnostic!(Error, Subsystem::Hcs, "the compute system refused to start");
        });

        assert_eq!(records[0].vm.as_deref(), Some("dev-linux"));
    }

    #[test]
    fn a_record_that_names_its_own_vm_keeps_it() {
        // A refresh that reports on one VM from inside another's span must not
        // be relabelled by its surroundings.
        let records = collect(|| {
            let span = tracing::info_span!("refresh", vm = "outer");
            let _entered = span.enter();
            crate::diagnostic!(
                Warning,
                Subsystem::Display,
                vm = "inner",
                "the window closed"
            );
        });

        assert_eq!(records[0].vm.as_deref(), Some("inner"));
    }

    #[test]
    fn taking_the_records_empties_the_sink() {
        let sink = DiagnosticsSink::new();
        let subscriber = tracing_subscriber::registry().with(DiagnosticsLayer::new(sink.clone()));
        tracing::subscriber::with_default(subscriber, || {
            crate::diagnostic!(Info, Subsystem::App, "settings saved");
        });

        assert_eq!(sink.take().len(), 1);
        assert!(sink.take().is_empty(), "a record is shown once");
    }
}
