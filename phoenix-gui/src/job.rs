use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::Instant;

use phoenix_capture::backup::{run_backup, BackupOptions};
use phoenix_core::ProgressHandle;
use phoenix_restore::restore::{run_restore, verify_backup_with_progress, RestoreOptions};
use tracing::{error, info};
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
    Clone,
}

impl JobKind {
    pub fn cancelled_message(self) -> &'static str {
        match self {
            JobKind::Backup => "Backup cancelled",
            JobKind::Restore => "Restore cancelled",
            JobKind::Verify => "Verify cancelled",
            JobKind::Clone => "Clone cancelled",
        }
    }

    /// Present-tense headline shown at the top of the status modal while the
    /// job is running.
    pub fn title(self) -> &'static str {
        match self {
            JobKind::Backup => "Backing up",
            JobKind::Restore => "Restoring",
            JobKind::Verify => "Verifying backup",
            JobKind::Clone => "Cloning disk",
        }
    }

    /// Neutral noun headline shown at the top of the status modal after the
    /// job has finished (paired with the colored Close button).
    pub fn noun(self) -> &'static str {
        match self {
            JobKind::Backup => "Backup",
            JobKind::Restore => "Restore",
            JobKind::Verify => "Verify",
            JobKind::Clone => "Clone",
        }
    }
}

pub struct BackgroundJob {
    pub kind: JobKind,
    /// Whether this job is a backup running with verify-after-backup enabled.
    /// Drives the "Completed and verified." vs "Completed. Unverified."
    /// success screen in the status modal. Always false for non-backup jobs.
    pub verify_after: bool,
    /// The `.phnx` file a backup job writes, kept so the finished modal can
    /// offer an after-the-fact "Verify?" of it. None for non-backup jobs.
    pub output: Option<PathBuf>,
    pub progress: ProgressHandle,
    rx: mpsc::Receiver<JobResult>,
    /// Kept so we can `join()` the worker and recover its panic payload if
    /// the channel disconnects without a result (worker panicked before
    /// sending). Without this, a panicking worker would leave the modal
    /// spinning forever with no completion.
    handle: Option<thread::JoinHandle<()>>,
    /// Terminal result, cached the first time `poll` observes it. Once set,
    /// the channel is never read again — this is what makes `poll` and
    /// `is_running` safe to call any number of times per frame (the old
    /// design had both methods `try_recv`, so whichever ran second could
    /// swallow the one-shot completion message and hang the UI).
    finished: Option<JobResult>,
}

impl BackgroundJob {
    /// Advance the job: if the worker has produced a result (or died),
    /// cache and return it. Idempotent and cheap to call every frame.
    pub fn poll(&mut self) -> Option<JobResult> {
        if self.finished.is_none() {
            match self.rx.try_recv() {
                Ok(result) => self.finished = Some(result),
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    // The sender was dropped without a message: the worker
                    // thread panicked. Join it to surface the panic text.
                    let msg = match self.handle.take().map(|h| h.join()) {
                        Some(Err(payload)) => panic_message(&payload),
                        _ => "worker thread ended without producing a result".to_string(),
                    };
                    error!(target: "phoenix_gui::job", "{msg}");
                    self.finished = Some(Err(format!("Internal error: {msg}")));
                }
            }
        }
        self.finished.clone()
    }

    /// Pure read of the cached state — never touches the channel, so it
    /// can't race `poll`. Reflects the result observed by the most recent
    /// `poll` call (which the UI runs once per frame).
    pub fn is_running(&self) -> bool {
        self.finished.is_none()
    }
}

/// Extract a readable message from a thread panic payload.
fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        format!("worker thread panicked: {s}")
    } else if let Some(s) = payload.downcast_ref::<String>() {
        format!("worker thread panicked: {s}")
    } else {
        "worker thread panicked (non-string payload)".to_string()
    }
}

/// Spawn `f` on a worker thread, wiring up the result channel and join
/// handle. `f` returns the job's terminal [`JobResult`].
fn make_job<F>(kind: JobKind, progress: ProgressHandle, f: F) -> BackgroundJob
where
    F: FnOnce() -> JobResult + Send + 'static,
{
    let (tx, rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        let _ = tx.send(f());
    });
    BackgroundJob {
        kind,
        verify_after: false,
        output: None,
        progress,
        rx,
        handle: Some(handle),
        finished: None,
    }
}

pub fn spawn_backup(opts: BackupOptions) -> BackgroundJob {
    let progress = opts.progress.clone().unwrap_or_default();
    let progress_worker = progress.clone();

    let verify_after = opts.verify_after;
    let output_path = opts.output.clone();
    let output_for_job = opts.output.clone();
    let output_display = output_path.display().to_string();
    let disk_index = opts.disk_index;
    let partitions = opts.partition_indices.clone();
    let mut job = make_job(JobKind::Backup, progress, move || {
        // Anchor-point logs: every backup run leaves a clear START / END
        // pair in the log so an operator scrolling a multi-run log can
        // locate "the run that produced C-Backup-N.phnx" by output path
        // alone. Before this, a successful backup with the default log
        // filter could finish without a single line being written.
        info!(
            target: "phoenix_gui::job",
            output = %output_display,
            disk_index,
            partitions = ?partitions,
            "spawn_backup: starting backup"
        );
        let started = Instant::now();
        let result = run_backup(BackupOptions {
            progress: Some(progress_worker.clone()),
            ..opts
        });
        let elapsed = started.elapsed();

        // If the worker bailed because the user cancelled, the partial
        // `.phnx` on disk has no manifest footer and is unopenable. Best-
        // effort delete it so the user's backup folder doesn't accumulate
        // half-finished files after a cancel. We deliberately don't fail
        // the result if removal fails — the cancel is still the headline
        // outcome.
        if result.is_err() && progress_worker.is_cancelled() {
            let _ = std::fs::remove_file(&output_path);
        }

        match &result {
            Ok(()) => {
                let size = std::fs::metadata(&output_path)
                    .map(|m| m.len())
                    .unwrap_or(0);
                info!(
                    target: "phoenix_gui::job",
                    output = %output_display,
                    bytes_on_disk = size,
                    elapsed_ms = elapsed.as_millis() as u64,
                    "spawn_backup: completed"
                );
            }
            Err(e) => {
                error!(
                    target: "phoenix_gui::job",
                    output = %output_display,
                    elapsed_ms = elapsed.as_millis() as u64,
                    cancelled = progress_worker.is_cancelled(),
                    error = %e,
                    "spawn_backup: failed"
                );
            }
        }

        result
            .map(|()| format!("Backup completed: {output_display}"))
            .map_err(|e| e.to_string())
    });
    job.verify_after = verify_after;
    job.output = Some(output_for_job);
    job
}

pub fn spawn_restore(opts: RestoreOptions) -> BackgroundJob {
    let progress = opts.progress.clone().unwrap_or_default();
    let progress_worker = progress.clone();

    make_job(JobKind::Restore, progress, move || {
        run_restore(RestoreOptions {
            progress: Some(progress_worker),
            ..opts
        })
        .map(|s| {
            let mut msg = "Restore completed".to_string();
            msg.push_str(&conversion_note(
                s.partitions_converted,
                s.conversion_bootable,
            ));
            msg
        })
        .map_err(|e| e.to_string())
    })
}

pub fn spawn_verify(path: PathBuf, quick: bool) -> BackgroundJob {
    let progress = ProgressHandle::new();
    let progress_worker = progress.clone();

    make_job(JobKind::Verify, progress, move || {
        let label = if quick {
            "Quick verify OK"
        } else {
            "Full verify OK"
        };
        let display = path.display().to_string();
        verify_backup_with_progress(&path, quick, Some(progress_worker))
            .map(|()| format!("{label}: {display}"))
            .map_err(|e| e.to_string())
    })
}

pub fn spawn_clone(opts: phoenix_clone::CloneOptions) -> BackgroundJob {
    let progress = opts.progress.clone().unwrap_or_default();
    let progress_worker = progress.clone();
    make_job(JobKind::Clone, progress, move || {
        phoenix_clone::run_clone(phoenix_clone::CloneOptions {
            progress: Some(progress_worker),
            ..opts
        })
        .map(|s| {
            format!(
                "Clone complete: {} partition(s) copied, {} resized{}",
                s.partitions_cloned,
                s.partitions_resized,
                conversion_note(s.partitions_converted, s.conversion_bootable),
            )
        })
        .map_err(|e| e.to_string())
    })
}

/// A short results-line suffix describing a 4Kn → 512e conversion (Alert G), or
/// empty when none ran (`bootable` is `None`).
fn conversion_note(converted: u32, bootable: Option<bool>) -> String {
    match bootable {
        None => String::new(),
        Some(true) => format!(
            " — converted {converted} partition(s) 4Kn→512e; ESP converted, should be bootable"
        ),
        Some(false) => format!(
            " — converted {converted} partition(s) 4Kn→512e; ESP not converted, will not boot"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panicking_worker_surfaces_error_instead_of_hanging() {
        let mut job = make_job(JobKind::Verify, ProgressHandle::new(), || {
            panic!("boom in worker");
        });
        // Spin briefly until the panic propagates to a cached terminal Err.
        let mut result = None;
        for _ in 0..2000 {
            if let Some(r) = job.poll() {
                result = Some(r);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let r = result.expect("panicking worker must produce a terminal result");
        assert!(r.is_err(), "expected an error result, got {r:?}");
        assert!(
            !job.is_running(),
            "job must not report running after finishing"
        );
    }

    #[test]
    fn successful_worker_result_is_cached_and_idempotent() {
        let mut job = make_job(JobKind::Verify, ProgressHandle::new(), || {
            Ok("done".to_string())
        });
        let mut first = None;
        for _ in 0..2000 {
            if let Some(r) = job.poll() {
                first = Some(r);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(first, Some(Ok("done".to_string())));
        // Polling again returns the same cached value, never Empty/None.
        assert_eq!(job.poll(), Some(Ok("done".to_string())));
    }
}
