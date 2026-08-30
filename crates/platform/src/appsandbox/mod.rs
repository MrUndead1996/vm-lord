mod config;
mod discovery;
mod journal;
mod source;

#[cfg(test)]
pub(crate) use discovery::FileSystem;
pub(crate) use discovery::{Discovery, DiscoveryResult};
#[allow(unused_imports)] // Later import stages consume this private platform facade.
pub(crate) use journal::{
    BootstrapSshFacts, ConversionStep, ImportJournal, ImportJournalDetails, ImportResources,
    JournalStage, SourceFingerprint,
};
pub(crate) use source::ValidatedSource;
