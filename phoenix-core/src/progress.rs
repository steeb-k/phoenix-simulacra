use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Human-readable duration for log lines and perf summaries:
/// `"2h 14m 05s"`, `"8m 32s"`, `"42.7s"`.
pub fn format_elapsed(secs: f64) -> String {
    let whole = secs.max(0.0) as u64;
    if whole >= 3600 {
        format!(
            "{}h {:02}m {:02}s",
            whole / 3600,
            (whole % 3600) / 60,
            whole % 60
        )
    } else if whole >= 60 {
        format!("{}m {:02}s", whole / 60, whole % 60)
    } else {
        format!("{:.1}s", secs.max(0.0))
    }
}

/// `"412.5 GB at 187 MB/s"` — volume plus effective throughput. Falls back
/// to volume alone when the elapsed time is too small to divide by.
pub fn format_rate(bytes: u64, secs: f64) -> String {
    if secs <= 0.0 {
        return format!("{:.1} GB", bytes as f64 / 1e9);
    }
    format!(
        "{:.1} GB at {:.0} MB/s",
        bytes as f64 / 1e9,
        bytes as f64 / 1e6 / secs
    )
}

/// Compact human-readable size for step labels: `"476.8 GB"`, `"15.7 GB"`,
/// `"16 MB"`, `"512 KB"`. One decimal place, with a trailing `.0` trimmed so
/// round sizes read cleanly. Binary units (1024-based) labelled GB/MB/KB to
/// match the rest of the app's readouts.
pub fn format_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    const TB: f64 = GB * 1024.0;
    let b = bytes as f64;
    let (val, unit) = if b >= TB {
        (b / TB, "TB")
    } else if b >= GB {
        (b / GB, "GB")
    } else if b >= MB {
        (b / MB, "MB")
    } else if b >= KB {
        (b / KB, "KB")
    } else {
        return format!("{bytes} B");
    };
    // One decimal, but drop a trailing ".0" so "16.0 GB" reads as "16 GB".
    let rounded = (val * 10.0).round() / 10.0;
    if (rounded.fract()).abs() < f64::EPSILON {
        format!("{:.0} {unit}", rounded)
    } else {
        format!("{:.1} {unit}", rounded)
    }
}

/// Thread-safe progress snapshot for GUI or CLI.
#[derive(Clone, Debug, Default)]
pub struct ProgressSnapshot {
    pub active: bool,
    pub phase: String,
    pub detail: String,
    pub current: u64,
    pub total: u64,
    /// Full, ordered list of step labels for the whole operation, declared
    /// up front by the worker so the GUI can render upcoming steps (grayed
    /// out) ahead of time. Empty when the operation doesn't publish a plan
    /// (the GUI then falls back to the live `phase` string alone).
    pub steps: Vec<String>,
    /// Index into `steps` of the currently-active step.
    pub current_step: usize,
}

impl ProgressSnapshot {
    pub fn fraction(&self) -> f32 {
        if !self.active || self.total == 0 {
            return 0.0;
        }
        (self.current as f64 / self.total as f64).min(1.0) as f32
    }
}

/// Progress + cancellation handle shared between the worker thread and the
/// GUI (or any other observer). The cancel flag rides along on the same
/// handle so every operation that already accepts an `Option<ProgressHandle>`
/// — backup, restore, verify, future clone — automatically gets a uniform
/// cancellation point without growing extra parameters in its signature.
#[derive(Clone, Default)]
pub struct ProgressHandle {
    inner: Arc<Mutex<ProgressSnapshot>>,
    cancel: Arc<AtomicBool>,
}

impl ProgressHandle {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn begin(&self, total: u64, phase: impl Into<String>) {
        let mut s = self.inner.lock().expect("progress lock");
        s.active = true;
        s.phase = phase.into();
        s.detail.clear();
        s.current = 0;
        s.total = total;
    }

    pub fn set_phase(&self, phase: impl Into<String>) {
        self.inner.lock().expect("progress lock").phase = phase.into();
    }

    /// Declare the full ordered list of step labels for the operation. The
    /// worker calls this once up front (before `begin`) so the GUI can show
    /// upcoming steps grayed out. Resets `current_step` to 0.
    pub fn set_steps(&self, steps: Vec<String>) {
        let mut s = self.inner.lock().expect("progress lock");
        s.steps = steps;
        s.current_step = 0;
    }

    /// Mark `idx` as the active step and mirror its label into `phase` so
    /// code (and log output) that still keys off `phase` keeps working.
    pub fn set_step(&self, idx: usize) {
        let mut s = self.inner.lock().expect("progress lock");
        s.current_step = idx;
        if let Some(label) = s.steps.get(idx).cloned() {
            s.phase = label;
        }
    }

    pub fn set_detail(&self, detail: impl Into<String>) {
        self.inner.lock().expect("progress lock").detail = detail.into();
    }

    pub fn set(&self, current: u64, detail: impl Into<String>) {
        let mut s = self.inner.lock().expect("progress lock");
        s.current = current;
        s.detail = detail.into();
    }

    pub fn bump(&self, delta: u64) {
        let mut s = self.inner.lock().expect("progress lock");
        s.current = s.current.saturating_add(delta);
    }

    pub fn end(&self) {
        let mut s = self.inner.lock().expect("progress lock");
        s.active = false;
        if s.total > 0 {
            s.current = s.total;
        }
    }

    pub fn snapshot(&self) -> ProgressSnapshot {
        self.inner.lock().expect("progress lock").clone()
    }

    /// Signal the worker that the operation should abort at the next
    /// chunk boundary. Cheap, lock-free, idempotent.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    /// Read the cancel flag. Workers call this at chunk-loop granularity
    /// to keep the wakeup latency bounded by a single chunk's worth of I/O.
    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    /// Clear the cancel flag. Useful when reusing a `ProgressHandle`
    /// across multiple sequential operations (we currently always create
    /// a fresh one per worker, but exposing this avoids surprises if a
    /// future caller decides to share).
    pub fn reset_cancel(&self) {
        self.cancel.store(false, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::format_size;

    #[test]
    fn format_size_trims_and_scales() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(16 * 1024 * 1024), "16 MB");
        assert_eq!(format_size(1024 * 1024 * 1024), "1 GB");
        // 476.8 GB — the reference screenshot's largest partition.
        let n = (476.8f64 * 1024.0 * 1024.0 * 1024.0) as u64;
        assert_eq!(format_size(n), "476.8 GB");
        // Round MB drops the decimal; a fractional GB keeps one place.
        assert_eq!(format_size(1536 * 1024 * 1024), "1.5 GB");
    }
}
