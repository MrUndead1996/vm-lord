//! The AppSandbox import workflow, as the application owns it.
//!
//! Everything a person edits before an import is accepted lives here rather
//! than in the UI or the platform: which discovered VM they picked and what
//! they want the copy called. The platform is told once, at submission, and
//! answers only in the domain terms of [`vmlord_core::appsandbox`] -- no
//! AppSandbox path, and no private key, ever reaches this layer to be shown.

use vmlord_core::{
    AppSandboxCompatibility, AppSandboxImportRequest, AppSandboxSourceId, AppSandboxVmCandidate,
    IncompleteAppSandboxImport, RepositoryError,
};

/// What the import dialog is looking at, between one discovery and the next.
#[derive(Default)]
pub struct ImportWorkflow {
    candidates: Vec<AppSandboxVmCandidate>,
    selected: Option<AppSandboxSourceId>,
    destination_name: String,
    incomplete: Vec<IncompleteAppSandboxImport>,
}

impl ImportWorkflow {
    /// Every VM the last discovery found, importable or not.
    ///
    /// The incompatible ones are kept deliberately: a person looking for a VM
    /// that is missing from the list needs to see it there with its reason, not
    /// to wonder whether VMLord saw it at all.
    #[must_use]
    pub fn candidates(&self) -> &[AppSandboxVmCandidate] {
        &self.candidates
    }

    /// The imports retained on disk for an explicit retry or discard.
    #[must_use]
    pub fn incomplete(&self) -> &[IncompleteAppSandboxImport] {
        &self.incomplete
    }

    /// The candidate the person is looking at, if the last discovery still has
    /// it.
    #[must_use]
    pub fn selected(&self) -> Option<&AppSandboxVmCandidate> {
        let selected = self.selected.as_ref()?;
        self.candidates
            .iter()
            .find(|candidate| &candidate.source_id == selected)
    }

    /// Picks a candidate and offers its own name for the copy.
    ///
    /// The name is only a starting point: the source keeps its name whatever
    /// happens here, so the copy may be called anything the user likes, and
    /// they will have to rename it when they already have a VM called that.
    pub fn select(&mut self, source_id: &AppSandboxSourceId) -> Result<(), RepositoryError> {
        let candidate = self
            .candidates
            .iter()
            .find(|candidate| &candidate.source_id == source_id)
            .ok_or_else(|| {
                RepositoryError::new("that AppSandbox VM is not in the discovered list")
            })?;
        self.destination_name.clone_from(&candidate.name);
        self.selected = Some(source_id.clone());
        Ok(())
    }

    /// What the copy is to be called.
    #[must_use]
    pub fn destination_name(&self) -> &str {
        &self.destination_name
    }

    pub fn set_destination_name(&mut self, name: impl Into<String>) {
        self.destination_name = name.into();
    }

    /// The request the current selection and name make, or why they make none.
    ///
    /// Refused here rather than by the platform where it can be: a name nobody
    /// could use and a source nobody chose are mistakes the person can see and
    /// correct, and the answer belongs in the return value of the call that
    /// made it.
    pub fn request(&self) -> Result<AppSandboxImportRequest, RepositoryError> {
        let candidate = self
            .selected()
            .ok_or_else(|| RepositoryError::new("no AppSandbox VM has been chosen to import"))?;
        if candidate.compatibility != AppSandboxCompatibility::Compatible {
            return Err(RepositoryError::new(format!(
                "the AppSandbox VM \"{}\" cannot be imported",
                candidate.name
            )));
        }
        let request = AppSandboxImportRequest {
            source_id: candidate.source_id.clone(),
            destination_name: self.destination_name.clone(),
        };
        request.validate()?;
        Ok(request)
    }

    /// Replaces the discovered list, keeping a selection the new list still
    /// has.
    ///
    /// A discovery that no longer sees the chosen VM clears the choice rather
    /// than keeping a dangling one: the identity is resolved by the platform
    /// through its latest snapshot, so a stale choice could only be refused.
    pub(crate) fn replace_candidates(&mut self, candidates: Vec<AppSandboxVmCandidate>) {
        if let Some(selected) = &self.selected
            && !candidates
                .iter()
                .any(|candidate| &candidate.source_id == selected)
        {
            self.selected = None;
            self.destination_name.clear();
        }
        self.candidates = candidates;
    }

    pub(crate) fn replace_incomplete(&mut self, incomplete: Vec<IncompleteAppSandboxImport>) {
        self.incomplete = incomplete;
    }
}
