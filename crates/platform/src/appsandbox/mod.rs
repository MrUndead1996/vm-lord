mod bootstrap;
mod config;
mod copy;
mod discovery;
mod journal;
mod source;

#[allow(unused_imports)] // The import pipeline consumes the bootstrap in the next task.
pub(crate) use bootstrap::{BootstrapRequest, BootstrapVm, ImportBootstrapPipeline};
#[cfg(test)]
pub(crate) use discovery::FileSystem;
pub(crate) use discovery::{Discovery, DiscoveryResult};
#[allow(unused_imports)] // Later import stages consume this private platform facade.
pub(crate) use journal::{
    BootstrapSshFacts, ConversionStep, ImportJournal, ImportJournalDetails, ImportResources,
    JournalStage, SourceFingerprint,
};
#[cfg(test)]
pub(crate) use source::SourceFileIdentity;
pub(crate) use source::ValidatedSource;
