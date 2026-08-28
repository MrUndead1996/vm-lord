//! Application-owned orchestration for verified application updates.

use std::{
    fmt,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, TryRecvError},
    },
    thread,
};

use vmlord_core::{DownloadPhase, ProgressPublisher, ValidatedUpdate};

/// A release that passed core validation, with the notes a person reviews.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AvailableUpdate {
    pub validated: ValidatedUpdate,
    pub release_notes: String,
}

/// What an application-update operation currently looks like to a client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdateState {
    Idle,
    Checking,
    Available(AvailableUpdate),
    Downloading {
        update: AvailableUpdate,
        progress: Option<DownloadPhase>,
    },
    Ready {
        update: AvailableUpdate,
        installer: PathBuf,
        installing: bool,
    },
    Failed {
        message: String,
    },
}

/// The composition-root boundary for network retrieval and Windows launch.
pub trait UpdateRuntime: Send + Sync {
    fn check(&self) -> Result<Option<AvailableUpdate>, String>;

    fn download(
        &self,
        update: &AvailableUpdate,
        progress: ProgressPublisher<DownloadPhase>,
        cancel: Arc<AtomicBool>,
    ) -> Result<PathBuf, String>;

    fn launch(&self, installer: &Path) -> Result<(), String>;
}

/// A request that cannot be accepted in the state's current phase.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdateActionError {
    RuntimeUnavailable,
    OperationInProgress,
    UpdateAlreadyAvailable,
    NoAvailableUpdate,
    NoReadyInstaller,
    AlreadyInstalling,
    Launch(String),
}

impl fmt::Display for UpdateActionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RuntimeUnavailable => formatter.write_str("the update service is unavailable"),
            Self::OperationInProgress => {
                formatter.write_str("an update operation is already running")
            }
            Self::UpdateAlreadyAvailable => {
                formatter.write_str("an application update is already available")
            }
            Self::NoAvailableUpdate => formatter.write_str("there is no update ready to download"),
            Self::NoReadyInstaller => {
                formatter.write_str("there is no verified installer ready to launch")
            }
            Self::AlreadyInstalling => {
                formatter.write_str("the verified installer is already being launched")
            }
            Self::Launch(error) => write!(
                formatter,
                "failed to launch the verified installer: {error}"
            ),
        }
    }
}

impl std::error::Error for UpdateActionError {}

pub(crate) struct UpdateManager {
    runtime: Option<Arc<dyn UpdateRuntime>>,
    state: UpdateState,
    receiver: Option<Receiver<UpdateEvent>>,
    cancel: Option<Arc<AtomicBool>>,
    download_progress: Option<ProgressPublisher<DownloadPhase>>,
}

impl Default for UpdateManager {
    fn default() -> Self {
        Self {
            runtime: None,
            state: UpdateState::Idle,
            receiver: None,
            cancel: None,
            download_progress: None,
        }
    }
}

impl UpdateManager {
    pub(crate) fn set_runtime(&mut self, runtime: Arc<dyn UpdateRuntime>) {
        self.runtime = Some(runtime);
    }

    pub(crate) fn has_runtime(&self) -> bool {
        self.runtime.is_some()
    }

    pub(crate) fn state(&self) -> &UpdateState {
        &self.state
    }

    pub(crate) fn start_check(&mut self, automatic: bool) -> Result<(), UpdateActionError> {
        self.require_no_operation()?;
        if matches!(self.state, UpdateState::Available(_)) {
            return Err(UpdateActionError::UpdateAlreadyAvailable);
        }
        let runtime = Arc::clone(self.runtime()?);
        self.state = UpdateState::Checking;

        self.start_worker(
            "vmlord-update-check",
            move |sender| {
                let result = runtime.check();
                let _ = sender.send(UpdateEvent::Check { result, automatic });
            },
            UpdateState::Idle,
        )
    }

    pub(crate) fn download(&mut self) -> Result<(), UpdateActionError> {
        self.require_no_operation()?;
        let UpdateState::Available(update) = &self.state else {
            return Err(UpdateActionError::NoAvailableUpdate);
        };
        let update = update.clone();
        let runtime = Arc::clone(self.runtime()?);
        let progress = ProgressPublisher::default();
        let cancel = Arc::new(AtomicBool::new(false));
        let fallback = update.clone();
        self.state = UpdateState::Downloading {
            update: update.clone(),
            progress: None,
        };
        self.cancel = Some(Arc::clone(&cancel));
        self.download_progress = Some(progress.clone());

        self.start_worker(
            "vmlord-update-download",
            move |sender| {
                let result = runtime.download(&update, progress, Arc::clone(&cancel));
                let cancelled = cancel.load(Ordering::Relaxed);
                let _ = sender.send(UpdateEvent::Download {
                    update,
                    result,
                    cancelled,
                });
            },
            UpdateState::Available(fallback),
        )
    }

    pub(crate) fn cancel(&mut self) -> Result<(), UpdateActionError> {
        let (UpdateState::Downloading { .. }, Some(cancel)) = (&self.state, &self.cancel) else {
            return Err(UpdateActionError::NoAvailableUpdate);
        };
        cancel.store(true, Ordering::Relaxed);
        tracing::info!("cancelling application update download");
        Ok(())
    }

    /// Launches synchronously because the platform boundary only creates the
    /// installer process. `true` tells the UI it may now request window exit.
    pub(crate) fn install(&mut self) -> Result<bool, UpdateActionError> {
        let (installer, installing) = match &self.state {
            UpdateState::Ready {
                installer,
                installing,
                ..
            } => (installer.clone(), *installing),
            _ => return Err(UpdateActionError::NoReadyInstaller),
        };
        if installing {
            return Err(UpdateActionError::AlreadyInstalling);
        }

        let runtime = Arc::clone(self.runtime()?);
        if let UpdateState::Ready { installing, .. } = &mut self.state {
            *installing = true;
        }
        match runtime.launch(&installer) {
            Ok(()) => {
                tracing::info!(installer = %installer.display(), "verified update installer launched");
                Ok(true)
            }
            Err(error) => {
                if let UpdateState::Ready { installing, .. } = &mut self.state {
                    *installing = false;
                }
                Err(UpdateActionError::Launch(error))
            }
        }
    }

    /// Applies every completed worker result and returns failures that belong
    /// in the diagnostics history. The caller owns diagnostics so this module
    /// remains independently testable and has no UI dependency.
    pub(crate) fn poll(&mut self) -> Vec<UpdateFailure> {
        self.snapshot_download_progress();

        let mut failures = Vec::new();
        loop {
            let event = match self.receiver.as_ref().map(Receiver::try_recv) {
                Some(Ok(event)) => event,
                Some(Err(TryRecvError::Empty)) | None => break,
                Some(Err(TryRecvError::Disconnected)) => {
                    self.receiver = None;
                    self.cancel = None;
                    self.download_progress = None;
                    self.state = UpdateState::Failed {
                        message: "the update worker stopped without reporting a result".to_owned(),
                    };
                    failures.push(UpdateFailure::new(
                        "application update",
                        "the update worker stopped without reporting a result".to_owned(),
                        false,
                    ));
                    break;
                }
            };
            self.receiver = None;
            self.cancel = None;
            self.download_progress = None;
            match event {
                UpdateEvent::Check { result, automatic } => match result {
                    Ok(Some(update)) => self.state = UpdateState::Available(update),
                    Ok(None) => self.state = UpdateState::Idle,
                    Err(message) => {
                        self.state = UpdateState::Failed {
                            message: message.clone(),
                        };
                        failures.push(UpdateFailure::new(
                            "check for application updates",
                            message,
                            automatic,
                        ));
                    }
                },
                UpdateEvent::Download {
                    update,
                    result,
                    cancelled,
                } => match result {
                    Ok(installer) => {
                        self.state = UpdateState::Ready {
                            update,
                            installer,
                            installing: false,
                        };
                    }
                    Err(_) if cancelled => self.state = UpdateState::Available(update),
                    Err(message) => {
                        self.state = UpdateState::Failed {
                            message: message.clone(),
                        };
                        failures.push(UpdateFailure::new(
                            "download the application update",
                            message,
                            false,
                        ));
                    }
                },
            }
        }
        failures
    }

    fn snapshot_download_progress(&mut self) {
        // The publisher is held by the worker. Its latest value is deliberately
        // copied into the presentation state only while the worker is active.
        // Keeping a stale byte count after a terminal event would read as a
        // download still running.
        let Some(progress) = self
            .download_progress
            .as_ref()
            .and_then(ProgressPublisher::snapshot)
        else {
            return;
        };
        if let UpdateState::Downloading {
            progress: current, ..
        } = &mut self.state
        {
            *current = Some(progress);
        }
    }

    fn runtime(&self) -> Result<&Arc<dyn UpdateRuntime>, UpdateActionError> {
        self.runtime
            .as_ref()
            .ok_or(UpdateActionError::RuntimeUnavailable)
    }

    fn require_no_operation(&self) -> Result<(), UpdateActionError> {
        if self.receiver.is_some() {
            Err(UpdateActionError::OperationInProgress)
        } else {
            Ok(())
        }
    }

    fn start_worker(
        &mut self,
        name: &str,
        operation: impl FnOnce(mpsc::Sender<UpdateEvent>) + Send + 'static,
        fallback: UpdateState,
    ) -> Result<(), UpdateActionError> {
        let (sender, receiver) = mpsc::channel();
        if let Err(error) = thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || operation(sender))
        {
            self.state = fallback;
            self.cancel = None;
            return Err(UpdateActionError::Launch(format!(
                "could not start update worker: {error}"
            )));
        }
        self.receiver = Some(receiver);
        Ok(())
    }
}

pub(crate) struct UpdateFailure {
    pub(crate) action: &'static str,
    pub(crate) message: String,
    pub(crate) automatic: bool,
}

impl UpdateFailure {
    fn new(action: &'static str, message: String, automatic: bool) -> Self {
        Self {
            action,
            message,
            automatic,
        }
    }
}

enum UpdateEvent {
    Check {
        result: Result<Option<AvailableUpdate>, String>,
        automatic: bool,
    },
    Download {
        update: AvailableUpdate,
        result: Result<PathBuf, String>,
        cancelled: bool,
    },
}
