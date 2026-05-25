use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;

use phoenix_capture::backup::{run_backup, BackupOptions};
use phoenix_core::ProgressHandle;
use phoenix_restore::restore::{run_restore, verify_backup_with_progress, RestoreOptions};
/// Result message for the status bar (success text or error).
pub type JobResult = Result<String, String>;

/// What kind of long-running operation a [`BackgroundJob`] is performing.
/// Lets the GUI tailor the "cancelled" status text per page even if the
/// user has navigated away from the page that kicked the job off.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobKind {
    Backup,
    Restore,
    Verify,
}

impl JobKind {
    pub fn cancelled_message(self) -> &'static str {
        match self {
            JobKind::Backup => "Backup cancelled",
            JobKind::Restore => "Restore cancelled",
            JobKind::Verify => "Verify cancelled",
        }
    }
}

pub struct BackgroundJob {
    pub kind: JobKind,
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

    let output_path = opts.output.clone();
    let output_display = output_path.display().to_string();
    thread::spawn(move || {
        let result = run_backup(BackupOptions {
            progress: Some(progress_worker.clone()),
            ..opts
        });

        // If the worker bailed because the user cancelled, the partial
        // `.phnx` on disk has no manifest footer and is unopenable. Best-
        // effort delete it so the user's backup folder doesn't accumulate
        // half-finished files after a cancel. We deliberately don't fail
        // the result if removal fails — the cancel is still the headline
        // outcome.
        if result.is_err() && progress_worker.is_cancelled() {
            let _ = std::fs::remove_file(&output_path);
        }

        let result = result
            .map(|()| format!("Backup completed: {output_display}"))
            .map_err(|e| e.to_string());
        let _ = tx.send(result);
    });

    BackgroundJob {
        kind: JobKind::Backup,
        progress,
        rx,
    }
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

    BackgroundJob {
        kind: JobKind::Restore,
        progress,
        rx,
    }
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

    BackgroundJob {
        kind: JobKind::Verify,
        progress,
        rx,
    }
}
