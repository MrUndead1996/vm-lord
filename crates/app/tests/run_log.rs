use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use vmlord_app::{LogFileOpener, WorkspaceApp, unavailable_repository};
use vmlord_core::RepositoryError;

struct FakeOpener {
    result: Result<(), String>,
    requested: Mutex<Vec<PathBuf>>,
}

impl LogFileOpener for FakeOpener {
    fn open_log_file(&self, path: &Path) -> Result<(), RepositoryError> {
        self.requested.lock().unwrap().push(path.to_path_buf());
        self.result.clone().map_err(RepositoryError::new)
    }
}

fn application(opener: Arc<FakeOpener>) -> WorkspaceApp {
    WorkspaceApp::new(unavailable_repository("not needed by run log tests")).with_run_log(
        PathBuf::from(r"C:\Logs\vmlord-20260908-101500-000.log"),
        opener,
    )
}

#[test]
fn the_run_log_is_the_path_the_composition_root_recorded() {
    let app = application(Arc::new(FakeOpener {
        result: Ok(()),
        requested: Mutex::new(Vec::new()),
    }));

    assert_eq!(
        app.run_log_path(),
        Some(Path::new(r"C:\Logs\vmlord-20260908-101500-000.log"))
    );
}

#[test]
fn an_app_without_a_recorded_run_log_has_nothing_to_report() {
    let app = WorkspaceApp::new(unavailable_repository("not needed by run log tests"));

    assert_eq!(app.run_log_path(), None);
}

#[test]
fn opening_the_run_log_hands_the_recorded_path_to_the_opener() {
    let opener = Arc::new(FakeOpener {
        result: Ok(()),
        requested: Mutex::new(Vec::new()),
    });
    let mut app = application(Arc::clone(&opener));

    app.open_run_log().expect("an opened log file succeeds");

    assert_eq!(
        opener.requested.lock().unwrap().as_slice(),
        [PathBuf::from(r"C:\Logs\vmlord-20260908-101500-000.log")]
    );
}

#[test]
fn a_refused_open_is_an_error_for_the_panel_to_report() {
    let mut app = application(Arc::new(FakeOpener {
        result: Err("the log file is not a regular file".to_owned()),
        requested: Mutex::new(Vec::new()),
    }));

    let error = app
        .open_run_log()
        .expect_err("a refused open must not read as success");

    assert!(error.to_string().contains("not a regular file"), "{error}");
}
