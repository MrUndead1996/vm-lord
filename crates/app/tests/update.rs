use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use semver::Version;
use vmlord_app::{
    AvailableUpdate, UpdateRuntime, UpdateState, WorkspaceApp, unavailable_repository,
};
use vmlord_core::{DownloadPhase, InstallerAsset, ProgressPublisher, ValidatedUpdate};

#[derive(Clone)]
enum DownloadResult {
    Installer(PathBuf),
    Error(String),
    WaitForCancellation,
}

struct FakeRuntime {
    check: Mutex<Result<Option<AvailableUpdate>, String>>,
    download: Mutex<DownloadResult>,
    launched: AtomicBool,
}

impl UpdateRuntime for FakeRuntime {
    fn check(&self) -> Result<Option<AvailableUpdate>, String> {
        self.check.lock().unwrap().clone()
    }

    fn download(
        &self,
        _update: &AvailableUpdate,
        progress: ProgressPublisher<DownloadPhase>,
        cancel: Arc<AtomicBool>,
    ) -> Result<PathBuf, String> {
        progress.publish(DownloadPhase::Downloading {
            downloaded: 1,
            total: Some(2),
        });
        match self.download.lock().unwrap().clone() {
            DownloadResult::Installer(path) => Ok(path),
            DownloadResult::Error(error) => Err(error),
            DownloadResult::WaitForCancellation => {
                while !cancel.load(Ordering::Relaxed) {
                    thread::yield_now();
                }
                Err("cancelled".to_owned())
            }
        }
    }

    fn launch(&self, _installer: &Path) -> Result<(), String> {
        self.launched.store(true, Ordering::Relaxed);
        Ok(())
    }
}

fn available_update() -> AvailableUpdate {
    AvailableUpdate {
        validated: ValidatedUpdate {
            version: Version::new(0, 2, 0),
            installer: InstallerAsset {
                url: "https://github.com/MrUndead1996/vm-lord/releases/download/v0.2.0/VMLord-0.2.0-x86_64-setup.exe".to_owned(),
                size: 2,
                sha256: "a".repeat(64),
            },
        },
        release_notes: "A verified release.".to_owned(),
    }
}

fn application(runtime: Arc<FakeRuntime>) -> WorkspaceApp {
    WorkspaceApp::new(unavailable_repository("not needed by update tests"))
        .with_update_runtime(runtime)
}

fn wait_until(app: &mut WorkspaceApp, condition: impl Fn(&UpdateState) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        app.poll_update();
        if condition(app.update_state()) {
            return;
        }
        assert!(Instant::now() < deadline, "update operation did not finish");
        thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn newer_release_becomes_available() {
    let runtime = Arc::new(FakeRuntime {
        check: Mutex::new(Ok(Some(available_update()))),
        download: Mutex::new(DownloadResult::Installer(PathBuf::from("installer.exe"))),
        launched: AtomicBool::new(false),
    });
    let mut app = application(runtime);

    app.check_for_updates().unwrap();
    wait_until(&mut app, |state| matches!(state, UpdateState::Available(_)));

    assert!(
        matches!(app.update_state(), UpdateState::Available(update) if update.validated.version == Version::new(0, 2, 0))
    );
}

#[test]
fn concurrent_check_is_refused_without_losing_the_available_update() {
    let runtime = Arc::new(FakeRuntime {
        check: Mutex::new(Ok(Some(available_update()))),
        download: Mutex::new(DownloadResult::Installer(PathBuf::from("installer.exe"))),
        launched: AtomicBool::new(false),
    });
    let mut app = application(runtime);
    app.check_for_updates().unwrap();
    wait_until(&mut app, |state| matches!(state, UpdateState::Available(_)));

    assert!(app.check_for_updates().is_err());
    assert!(matches!(app.update_state(), UpdateState::Available(_)));
}

#[test]
fn corrupt_download_becomes_failed() {
    let runtime = Arc::new(FakeRuntime {
        check: Mutex::new(Ok(Some(available_update()))),
        download: Mutex::new(DownloadResult::Error(
            "installer checksum mismatch".to_owned(),
        )),
        launched: AtomicBool::new(false),
    });
    let mut app = application(runtime);
    app.check_for_updates().unwrap();
    wait_until(&mut app, |state| matches!(state, UpdateState::Available(_)));

    app.download_update().unwrap();
    wait_until(&mut app, |state| {
        matches!(state, UpdateState::Failed { .. })
    });

    assert!(
        matches!(app.update_state(), UpdateState::Failed { message } if message.contains("checksum mismatch"))
    );
}

#[test]
fn cancellation_returns_to_the_available_release() {
    let runtime = Arc::new(FakeRuntime {
        check: Mutex::new(Ok(Some(available_update()))),
        download: Mutex::new(DownloadResult::WaitForCancellation),
        launched: AtomicBool::new(false),
    });
    let mut app = application(runtime);
    app.check_for_updates().unwrap();
    wait_until(&mut app, |state| matches!(state, UpdateState::Available(_)));
    app.download_update().unwrap();
    wait_until(&mut app, |state| {
        matches!(state, UpdateState::Downloading { .. })
    });

    app.cancel_update().unwrap();
    wait_until(&mut app, |state| matches!(state, UpdateState::Available(_)));

    assert!(matches!(app.update_state(), UpdateState::Available(_)));
}

#[test]
fn successful_launch_marks_the_installer_in_progress_before_requesting_exit() {
    let runtime = Arc::new(FakeRuntime {
        check: Mutex::new(Ok(Some(available_update()))),
        download: Mutex::new(DownloadResult::Installer(PathBuf::from("installer.exe"))),
        launched: AtomicBool::new(false),
    });
    let mut app = application(Arc::clone(&runtime));
    app.check_for_updates().unwrap();
    wait_until(&mut app, |state| matches!(state, UpdateState::Available(_)));
    app.download_update().unwrap();
    wait_until(&mut app, |state| matches!(state, UpdateState::Ready { .. }));

    assert!(app.install_update().unwrap());
    assert!(runtime.launched.load(Ordering::Relaxed));
    assert!(matches!(
        app.update_state(),
        UpdateState::Ready {
            installing: true,
            ..
        }
    ));
}

#[test]
fn first_run_is_the_value_injected_by_the_composition_root() {
    let app = WorkspaceApp::new(unavailable_repository("not needed by update tests"))
        .with_first_run(true);

    assert!(app.first_run());
}
