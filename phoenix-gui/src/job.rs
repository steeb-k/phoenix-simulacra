use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;

use phoenix_capture::backup::{run_backup, BackupOptions};
use phoenix_core::ProgressHandle;
use phoenix_restore::restore::{run_restore, verify_backup_with_progress, RestoreOptions};
/// Result message for the status bar (success text or error).
pub type JobResult = Result<String, String>;

pub struct BackgroundJob {
    pub progress: ProgressHandle,
    rx: mpsc::Receiver<JobResult>,
}

impl BackgroundJob {
    pub fn poll(&self) -> Option<JobResult> {
        self.rx.try_recv().ok()
    }

    pub fn is_running(&self) -> bool {
        self.poll().is_none()
    }
}

pub fn spawn_backup(opts: BackupOptions) -> BackgroundJob {
    let (tx, rx) = mpsc::channel();
    let progress = opts.progress.clone().unwrap_or_default();
    let progress_worker = progress.clone();

    let output_display = opts.output.display().to_string();
    thread::spawn(move || {
        let result = run_backup(BackupOptions {
            progress: Some(progress_worker),
            ..opts
        })
        .map(|()| format!("Backup completed: {output_display}"))
        .map_err(|e| e.to_string());
        let _ = tx.send(result);
    });

    BackgroundJob { progress, rx }
}

pub fn spawn_restore(opts: RestoreOptions) -> BackgroundJob {
    let (tx, rx) = mpsc::channel();
    let progress = opts.progress.clone().unwrap_or_default();
    let progress_worker = progress.clone();

    thread::spawn(move || {
        let result = run_restore(RestoreOptions {
            progress: Some(progress_worker),
            ..opts
        })
        .map(|()| "Restore completed".to_string())
        .map_err(|e| e.to_string());
        let _ = tx.send(result);
    });

    BackgroundJob { progress, rx }
}

pub fn spawn_verify(path: PathBuf, quick: bool) -> BackgroundJob {
    let (tx, rx) = mpsc::channel();
    let progress = ProgressHandle::new();
    let progress_worker = progress.clone();

    thread::spawn(move || {
        let label = if quick { "Quick verify OK" } else { "Full verify OK" };
        let result = verify_backup_with_progress(&path, quick, Some(progress_worker))
            .map(|()| label.to_string())
            .map_err(|e| e.to_string());
        let _ = tx.send(result);
    });

    BackgroundJob { progress, rx }
}
