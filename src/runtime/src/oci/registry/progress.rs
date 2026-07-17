use std::sync::Arc;
use std::time::{Duration, Instant};

const REPORT_INTERVAL_BYTES: u64 = 1024 * 1024;
const REPORT_INTERVAL_TIME: Duration = Duration::from_secs(5);

/// Current phase of a registry layer transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullProgressState {
    Downloading,
    Retrying,
    Reused,
    Complete,
}

/// Structured registry layer progress with actual transferred byte counts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullProgress {
    pub current_layer: usize,
    pub total_layers: usize,
    pub digest: String,
    pub downloaded_bytes: u64,
    pub total_bytes: u64,
    pub attempt: usize,
    pub max_attempts: usize,
    pub retry_delay_ms: Option<u64>,
    pub state: PullProgressState,
}

/// Callback for structured registry layer progress events.
pub type PullProgressEventFn = Arc<dyn Fn(PullProgress) + Send + Sync>;

pub(super) struct ProgressReporter {
    callback: Option<PullProgressEventFn>,
    current_layer: usize,
    total_layers: usize,
    digest: String,
    total_bytes: u64,
    max_attempts: usize,
    last_bytes: u64,
    last_report: Instant,
}

impl ProgressReporter {
    pub(super) fn new(
        callback: Option<PullProgressEventFn>,
        current_layer: usize,
        total_layers: usize,
        digest: String,
        total_bytes: u64,
        max_attempts: usize,
    ) -> Self {
        Self {
            callback,
            current_layer,
            total_layers,
            digest,
            total_bytes,
            max_attempts,
            last_bytes: 0,
            last_report: Instant::now(),
        }
    }

    pub(super) fn downloading(&mut self, downloaded_bytes: u64, attempt: usize, force: bool) {
        let now = Instant::now();
        if !force
            && downloaded_bytes.saturating_sub(self.last_bytes) < REPORT_INTERVAL_BYTES
            && now.duration_since(self.last_report) < REPORT_INTERVAL_TIME
        {
            return;
        }
        self.last_bytes = downloaded_bytes;
        self.last_report = now;
        self.emit(
            PullProgressState::Downloading,
            downloaded_bytes,
            attempt,
            None,
        );
    }

    pub(super) fn retrying(
        &mut self,
        downloaded_bytes: u64,
        next_attempt: usize,
        delay: Duration,
    ) {
        self.emit(
            PullProgressState::Retrying,
            downloaded_bytes,
            next_attempt,
            Some(duration_millis(delay)),
        );
    }

    pub(super) fn reused(&mut self) {
        self.emit(
            PullProgressState::Reused,
            self.total_bytes,
            0,
            None,
        );
    }

    pub(super) fn complete(&mut self, downloaded_bytes: u64, attempt: usize) {
        self.emit(
            PullProgressState::Complete,
            downloaded_bytes,
            attempt,
            None,
        );
    }

    fn emit(
        &self,
        state: PullProgressState,
        downloaded_bytes: u64,
        attempt: usize,
        retry_delay_ms: Option<u64>,
    ) {
        tracing::info!(
            layer = self.current_layer,
            layers = self.total_layers,
            digest = %self.digest,
            downloaded_bytes,
            total_bytes = self.total_bytes,
            attempt,
            max_attempts = self.max_attempts,
            ?state,
            "Registry layer transfer progress"
        );
        if let Some(callback) = &self.callback {
            callback(PullProgress {
                current_layer: self.current_layer,
                total_layers: self.total_layers,
                digest: self.digest.clone(),
                downloaded_bytes,
                total_bytes: self.total_bytes,
                attempt,
                max_attempts: self.max_attempts,
                retry_delay_ms,
                state,
            });
        }
    }
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}
