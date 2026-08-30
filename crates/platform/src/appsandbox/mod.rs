mod bootstrap;
mod bundle;
mod config;
mod conversion;
mod copy;
mod discovery;
mod journal;
mod pipeline;
mod source;
mod verify;
mod worker;

#[allow(unused_imports)] // The import pipeline consumes the bootstrap in the next task.
pub(crate) use bootstrap::{BootstrapRequest, BootstrapVm, ImportBootstrapPipeline};
#[allow(unused_imports)] // The import pipeline runs the conversion in the next task.
pub(crate) use bundle::ConversionBundle;
#[allow(unused_imports)] // The import pipeline runs the conversion in the next task.
pub(crate) use conversion::{
    ConversionCommand, ConversionReport, ConversionRequest, ConversionRunner, GuestIdentity,
    SecretText,
};
#[cfg(test)]
pub(crate) use discovery::FileSystem;
pub(crate) use discovery::{Discovery, DiscoveryResult};
#[allow(unused_imports)] // Later import stages consume this private platform facade.
pub(crate) use journal::{
    BootstrapSshFacts, ConversionStep, ImportJournal, ImportJournalDetails, ImportResources,
    JournalStage, SourceFingerprint,
};
#[allow(unused_imports)] // Repository wiring consumes the assembled pipeline in Task 9.
pub(crate) use pipeline::{ImportPipeline, ImportSubject};
#[cfg(test)]
pub(crate) use source::SourceFileIdentity;
pub(crate) use source::ValidatedSource;
#[allow(unused_imports)] // Repository wiring consumes verification in Task 9.
pub(crate) use verify::{Verification, VerificationRequest};
#[allow(unused_imports)] // Repository wiring consumes worker outcomes in Task 9.
pub(crate) use worker::{ImportWorker, ImportWorkerActions, ImportWorkerOutcome};
