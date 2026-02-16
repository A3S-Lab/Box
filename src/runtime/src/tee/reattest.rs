//! Periodic re-attestation for long-running TEE workloads.
//!
//! Verifies the TEE's attestation report at configurable intervals to detect
//! runtime compromise. If re-attestation fails, the configured action is taken
//! (log warning, emit event, or stop the VM).

use std::time::{Duration, Instant};

use a3s_box_core::error::{BoxError, Result};
use serde::{Deserialize, Serialize};

/// Configuration for periodic re-attestation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReattestConfig {
    /// Whether periodic re-attestation is enabled
    pub enabled: bool,
    /// Interval between re-attestation checks in seconds (default: 300 = 5 min)
    #[serde(default = "default_interval")]
    pub interval_secs: u64,
    /// Maximum consecutive failures before taking action (default: 3)
    #[serde(default = "default_max_failures")]
    pub max_failures: u32,
    /// Action to take on persistent failure
    #[serde(default)]
    pub failure_action: FailureAction,
    /// Grace period after boot before first check, in seconds (default: 60)
    #[serde(default = "default_grace_period")]
    pub grace_period_secs: u64,
}

fn default_interval() -> u64 {
    300
}

fn default_max_failures() -> u32 {
    3
}

fn default_grace_period() -> u64 {
    60
}

impl Default for ReattestConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_secs: 300,
            max_failures: 3,
            failure_action: FailureAction::default(),
            grace_period_secs: 60,
        }
    }
}

/// Action to take when re-attestation persistently fails.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum FailureAction {
    /// Log a warning but continue running (default)
    #[default]
    Warn,
    /// Emit an event for external monitoring systems
    Event,
    /// Stop the VM immediately
    Stop,
}

/// Tracks the state of periodic re-attestation for a single VM.
pub struct ReattestState {
    config: ReattestConfig,
    /// When the VM was started (for grace period)
    boot_time: Instant,
    /// When the last successful attestation occurred
    last_success: Option<Instant>,
    /// When the last check was attempted
    last_check: Option<Instant>,
    /// Consecutive failure count
    consecutive_failures: u32,
    /// Total successful checks
    total_successes: u64,
    /// Total failed checks
    total_failures: u64,
}

impl ReattestState {
    /// Create a new re-attestation state tracker.
    pub fn new(config: ReattestConfig) -> Self {
        Self {
            config,
            boot_time: Instant::now(),
            last_success: None,
            last_check: None,
            consecutive_failures: 0,
            total_successes: 0,
            total_failures: 0,
        }
    }

    /// Check if a re-attestation check is due now.
    pub fn is_check_due(&self) -> bool {
        if !self.config.enabled {
            return false;
        }

        // Respect grace period after boot
        if self.boot_time.elapsed() < Duration::from_secs(self.config.grace_period_secs) {
            return false;
        }

        match self.last_check {
            None => true, // Never checked yet
            Some(last) => last.elapsed() >= Duration::from_secs(self.config.interval_secs),
        }
    }

    /// Record a successful re-attestation.
    pub fn record_success(&mut self) {
        let now = Instant::now();
        self.last_success = Some(now);
        self.last_check = Some(now);
        self.consecutive_failures = 0;
        self.total_successes += 1;
    }

    /// Record a failed re-attestation. Returns the action to take.
    pub fn record_failure(&mut self) -> FailureAction {
        let now = Instant::now();
        self.last_check = Some(now);
        self.consecutive_failures += 1;
        self.total_failures += 1;

        if self.consecutive_failures >= self.config.max_failures {
            self.config.failure_action
        } else {
            FailureAction::Warn
        }
    }

    /// Get the number of consecutive failures.
    pub fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures
    }

    /// Get the total number of successful checks.
    pub fn total_successes(&self) -> u64 {
        self.total_successes
    }

    /// Get the total number of failed checks.
    pub fn total_failures(&self) -> u64 {
        self.total_failures
    }

    /// Get the time since last successful attestation.
    pub fn time_since_last_success(&self) -> Option<Duration> {
        self.last_success.map(|t| t.elapsed())
    }

    /// Get the time since last check (success or failure).
    pub fn time_since_last_check(&self) -> Option<Duration> {
        self.last_check.map(|t| t.elapsed())
    }

    /// Whether the VM has ever been successfully attested.
    pub fn has_attested(&self) -> bool {
        self.last_success.is_some()
    }

    /// Whether the failure threshold has been exceeded.
    pub fn is_failed(&self) -> bool {
        self.consecutive_failures >= self.config.max_failures
    }

    /// Get the configured failure action.
    pub fn failure_action(&self) -> FailureAction {
        self.config.failure_action
    }

    /// Get a summary of the re-attestation state.
    pub fn summary(&self) -> ReattestSummary {
        ReattestSummary {
            enabled: self.config.enabled,
            interval_secs: self.config.interval_secs,
            consecutive_failures: self.consecutive_failures,
            max_failures: self.config.max_failures,
            total_successes: self.total_successes,
            total_failures: self.total_failures,
            is_failed: self.is_failed(),
            failure_action: self.config.failure_action,
        }
    }
}

/// Serializable summary of re-attestation state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReattestSummary {
    pub enabled: bool,
    pub interval_secs: u64,
    pub consecutive_failures: u32,
    pub max_failures: u32,
    pub total_successes: u64,
    pub total_failures: u64,
    pub is_failed: bool,
    pub failure_action: FailureAction,
}

/// Validate a re-attestation configuration.
pub fn validate_config(config: &ReattestConfig) -> Result<()> {
    if config.enabled && config.interval_secs == 0 {
        return Err(BoxError::Other(
            "Re-attestation interval must be > 0 seconds".to_string(),
        ));
    }
    if config.enabled && config.max_failures == 0 {
        return Err(BoxError::Other(
            "Re-attestation max_failures must be > 0".to_string(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reattest_config_default() {
        let config = ReattestConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.interval_secs, 300);
        assert_eq!(config.max_failures, 3);
        assert_eq!(config.failure_action, FailureAction::Warn);
        assert_eq!(config.grace_period_secs, 60);
    }

    #[test]
    fn test_reattest_config_serde_roundtrip() {
        let config = ReattestConfig {
            enabled: true,
            interval_secs: 120,
            max_failures: 5,
            failure_action: FailureAction::Stop,
            grace_period_secs: 30,
        };
        let json = serde_json::to_string(&config).unwrap();
        let parsed: ReattestConfig = serde_json::from_str(&json).unwrap();
        assert!(parsed.enabled);
        assert_eq!(parsed.interval_secs, 120);
        assert_eq!(parsed.max_failures, 5);
        assert_eq!(parsed.failure_action, FailureAction::Stop);
        assert_eq!(parsed.grace_period_secs, 30);
    }

    #[test]
    fn test_failure_action_default() {
        assert_eq!(FailureAction::default(), FailureAction::Warn);
    }

    #[test]
    fn test_reattest_state_disabled() {
        let config = ReattestConfig::default(); // enabled: false
        let state = ReattestState::new(config);
        assert!(!state.is_check_due());
    }

    #[test]
    fn test_reattest_state_grace_period() {
        let config = ReattestConfig {
            enabled: true,
            grace_period_secs: 3600, // 1 hour — won't expire during test
            ..Default::default()
        };
        let state = ReattestState::new(config);
        assert!(!state.is_check_due());
    }

    #[test]
    fn test_reattest_state_first_check_due() {
        let config = ReattestConfig {
            enabled: true,
            grace_period_secs: 0, // No grace period
            ..Default::default()
        };
        let state = ReattestState::new(config);
        assert!(state.is_check_due());
    }

    #[test]
    fn test_reattest_state_record_success() {
        let config = ReattestConfig {
            enabled: true,
            grace_period_secs: 0,
            ..Default::default()
        };
        let mut state = ReattestState::new(config);

        state.record_success();
        assert!(state.has_attested());
        assert_eq!(state.consecutive_failures(), 0);
        assert_eq!(state.total_successes(), 1);
        assert_eq!(state.total_failures(), 0);
        assert!(!state.is_failed());
    }

    #[test]
    fn test_reattest_state_record_failure_under_threshold() {
        let config = ReattestConfig {
            enabled: true,
            grace_period_secs: 0,
            max_failures: 3,
            failure_action: FailureAction::Stop,
            ..Default::default()
        };
        let mut state = ReattestState::new(config);

        // First failure — under threshold, should warn
        let action = state.record_failure();
        assert_eq!(action, FailureAction::Warn);
        assert_eq!(state.consecutive_failures(), 1);
        assert!(!state.is_failed());
    }

    #[test]
    fn test_reattest_state_record_failure_at_threshold() {
        let config = ReattestConfig {
            enabled: true,
            grace_period_secs: 0,
            max_failures: 2,
            failure_action: FailureAction::Stop,
            ..Default::default()
        };
        let mut state = ReattestState::new(config);

        state.record_failure(); // 1
        let action = state.record_failure(); // 2 — at threshold
        assert_eq!(action, FailureAction::Stop);
        assert!(state.is_failed());
        assert_eq!(state.total_failures(), 2);
    }

    #[test]
    fn test_reattest_state_success_resets_failures() {
        let config = ReattestConfig {
            enabled: true,
            grace_period_secs: 0,
            max_failures: 3,
            ..Default::default()
        };
        let mut state = ReattestState::new(config);

        state.record_failure();
        state.record_failure();
        assert_eq!(state.consecutive_failures(), 2);

        state.record_success();
        assert_eq!(state.consecutive_failures(), 0);
        assert!(!state.is_failed());
        // Total counts are preserved
        assert_eq!(state.total_failures(), 2);
        assert_eq!(state.total_successes(), 1);
    }

    #[test]
    fn test_reattest_state_not_due_after_recent_check() {
        let config = ReattestConfig {
            enabled: true,
            grace_period_secs: 0,
            interval_secs: 3600, // 1 hour
            ..Default::default()
        };
        let mut state = ReattestState::new(config);

        assert!(state.is_check_due()); // First check
        state.record_success();
        assert!(!state.is_check_due()); // Just checked
    }

    #[test]
    fn test_reattest_state_summary() {
        let config = ReattestConfig {
            enabled: true,
            interval_secs: 120,
            max_failures: 5,
            failure_action: FailureAction::Event,
            grace_period_secs: 0,
        };
        let mut state = ReattestState::new(config);
        state.record_success();
        state.record_failure();

        let summary = state.summary();
        assert!(summary.enabled);
        assert_eq!(summary.interval_secs, 120);
        assert_eq!(summary.consecutive_failures, 1);
        assert_eq!(summary.max_failures, 5);
        assert_eq!(summary.total_successes, 1);
        assert_eq!(summary.total_failures, 1);
        assert!(!summary.is_failed);
        assert_eq!(summary.failure_action, FailureAction::Event);
    }

    #[test]
    fn test_reattest_summary_serde() {
        let summary = ReattestSummary {
            enabled: true,
            interval_secs: 300,
            consecutive_failures: 2,
            max_failures: 3,
            total_successes: 10,
            total_failures: 2,
            is_failed: false,
            failure_action: FailureAction::Warn,
        };
        let json = serde_json::to_string(&summary).unwrap();
        let parsed: ReattestSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.consecutive_failures, 2);
        assert_eq!(parsed.total_successes, 10);
    }

    #[test]
    fn test_validate_config_valid() {
        let config = ReattestConfig {
            enabled: true,
            interval_secs: 60,
            max_failures: 3,
            ..Default::default()
        };
        assert!(validate_config(&config).is_ok());
    }

    #[test]
    fn test_validate_config_disabled_always_valid() {
        let config = ReattestConfig {
            enabled: false,
            interval_secs: 0, // Would be invalid if enabled
            max_failures: 0,
            ..Default::default()
        };
        assert!(validate_config(&config).is_ok());
    }

    #[test]
    fn test_validate_config_zero_interval() {
        let config = ReattestConfig {
            enabled: true,
            interval_secs: 0,
            max_failures: 3,
            ..Default::default()
        };
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn test_validate_config_zero_max_failures() {
        let config = ReattestConfig {
            enabled: true,
            interval_secs: 60,
            max_failures: 0,
            ..Default::default()
        };
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn test_time_since_last_success_none() {
        let config = ReattestConfig::default();
        let state = ReattestState::new(config);
        assert!(state.time_since_last_success().is_none());
    }

    #[test]
    fn test_time_since_last_check_none() {
        let config = ReattestConfig::default();
        let state = ReattestState::new(config);
        assert!(state.time_since_last_check().is_none());
    }

    #[test]
    fn test_time_since_last_success_some() {
        let config = ReattestConfig {
            enabled: true,
            grace_period_secs: 0,
            ..Default::default()
        };
        let mut state = ReattestState::new(config);
        state.record_success();
        let elapsed = state.time_since_last_success().unwrap();
        assert!(elapsed < Duration::from_secs(1));
    }

    #[test]
    fn test_failure_action_event() {
        let config = ReattestConfig {
            enabled: true,
            grace_period_secs: 0,
            max_failures: 1,
            failure_action: FailureAction::Event,
            ..Default::default()
        };
        let mut state = ReattestState::new(config);
        let action = state.record_failure();
        assert_eq!(action, FailureAction::Event);
    }
}
