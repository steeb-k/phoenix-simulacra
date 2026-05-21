use std::sync::{Arc, Mutex};

/// Thread-safe progress snapshot for GUI or CLI.
#[derive(Clone, Debug, Default)]
pub struct ProgressSnapshot {
    pub active: bool,
    pub phase: String,
    pub detail: String,
    pub current: u64,
    pub total: u64,
}

impl ProgressSnapshot {
    pub fn fraction(&self) -> f32 {
        if !self.active || self.total == 0 {
            return 0.0;
        }
        (self.current as f64 / self.total as f64).min(1.0) as f32
    }
}

#[derive(Clone, Default)]
pub struct ProgressHandle {
    inner: Arc<Mutex<ProgressSnapshot>>,
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
}
