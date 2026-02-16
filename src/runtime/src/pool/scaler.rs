//! Pool autoscaler — adjusts `min_idle` based on acquire pressure.
//!
//! Monitors hit/miss rates over a sliding window and scales the pool's
//! `min_idle` target up or down to match demand.

use std::collections::VecDeque;
use std::time::Instant;

use a3s_box_core::config::ScalingPolicy;

/// Result of a scaling evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScaleDecision {
    /// Increase min_idle by the given amount.
    ScaleUp(usize),
    /// Decrease min_idle by the given amount.
    ScaleDown(usize),
    /// No change needed.
    Hold,
}

/// Sliding window of acquire events for miss rate calculation.
struct PressureWindow {
    /// Ring buffer of (timestamp, was_hit).
    events: VecDeque<(Instant, bool)>,
    /// Window duration.
    window: std::time::Duration,
}

impl PressureWindow {
    fn new(window_secs: u64) -> Self {
        Self {
            events: VecDeque::new(),
            window: std::time::Duration::from_secs(window_secs),
        }
    }

    /// Record an acquire event (hit = from pool, miss = on-demand boot).
    fn record(&mut self, hit: bool) {
        self.events.push_back((Instant::now(), hit));
        self.prune();
    }

    /// Record an event with a specific timestamp (for testing).
    #[cfg(test)]
    fn record_at(&mut self, at: Instant, hit: bool) {
        self.events.push_back((at, hit));
        self.prune();
    }

    /// Remove events outside the window.
    fn prune(&mut self) {
        let cutoff = Instant::now().checked_sub(self.window).unwrap_or_else(Instant::now);
        while let Some(&(ts, _)) = self.events.front() {
            if ts < cutoff {
                self.events.pop_front();
            } else {
                break;
            }
        }
    }

    /// Calculate the miss rate (0.0 to 1.0). Returns None if no events.
    fn miss_rate(&mut self) -> Option<f64> {
        self.prune();
        if self.events.is_empty() {
            return None;
        }
        let total = self.events.len() as f64;
        let misses = self.events.iter().filter(|(_, hit)| !hit).count() as f64;
        Some(misses / total)
    }

    /// Number of events in the window.
    fn len(&self) -> usize {
        self.events.len()
    }
}

/// Pool autoscaler that adjusts min_idle based on pressure signals.
pub struct PoolScaler {
    /// Scaling policy configuration.
    policy: ScalingPolicy,
    /// Sliding window of acquire events.
    window: PressureWindow,
    /// Last time a scaling decision was made.
    last_scale_at: Option<Instant>,
    /// Current dynamic min_idle value.
    current_min_idle: usize,
    /// Effective upper bound for min_idle.
    max_min_idle: usize,
}

impl PoolScaler {
    /// Create a new scaler with the given policy and initial min_idle.
    pub fn new(policy: ScalingPolicy, initial_min_idle: usize, max_size: usize) -> Self {
        let max_min_idle = if policy.max_min_idle > 0 {
            policy.max_min_idle.min(max_size)
        } else {
            max_size
        };

        Self {
            window: PressureWindow::new(policy.window_secs),
            policy,
            last_scale_at: None,
            current_min_idle: initial_min_idle,
            max_min_idle,
        }
    }

    /// Record an acquire event. `hit` = true if served from pool.
    pub fn record_acquire(&mut self, hit: bool) {
        self.window.record(hit);
    }

    /// Get the current dynamic min_idle value.
    pub fn current_min_idle(&self) -> usize {
        self.current_min_idle
    }

    /// Evaluate pressure and return a scaling decision.
    ///
    /// Respects cooldown period between decisions.
    pub fn evaluate(&mut self) -> ScaleDecision {
        // Check cooldown
        if let Some(last) = self.last_scale_at {
            if last.elapsed().as_secs() < self.policy.cooldown_secs {
                return ScaleDecision::Hold;
            }
        }

        // Need at least a few events to make a decision
        if self.window.len() < 3 {
            return ScaleDecision::Hold;
        }

        let miss_rate = match self.window.miss_rate() {
            Some(rate) => rate,
            None => return ScaleDecision::Hold,
        };

        let decision = if miss_rate > self.policy.scale_up_threshold {
            // High miss rate → scale up
            let new_min = (self.current_min_idle + 1).min(self.max_min_idle);
            if new_min > self.current_min_idle {
                self.current_min_idle = new_min;
                self.last_scale_at = Some(Instant::now());
                ScaleDecision::ScaleUp(1)
            } else {
                ScaleDecision::Hold // Already at max
            }
        } else if miss_rate < self.policy.scale_down_threshold {
            // Low miss rate → scale down
            let new_min = self.current_min_idle.saturating_sub(1).max(1);
            if new_min < self.current_min_idle {
                self.current_min_idle = new_min;
                self.last_scale_at = Some(Instant::now());
                ScaleDecision::ScaleDown(1)
            } else {
                ScaleDecision::Hold // Already at floor
            }
        } else {
            ScaleDecision::Hold
        };

        decision
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_policy() -> ScalingPolicy {
        ScalingPolicy {
            enabled: true,
            scale_up_threshold: 0.3,
            scale_down_threshold: 0.05,
            max_min_idle: 0, // use max_size
            cooldown_secs: 60,
            window_secs: 120,
        }
    }

    // --- PressureWindow tests ---

    #[test]
    fn test_window_empty_miss_rate_is_none() {
        let mut w = PressureWindow::new(120);
        assert_eq!(w.miss_rate(), None);
        assert_eq!(w.len(), 0);
    }

    #[test]
    fn test_window_all_hits() {
        let mut w = PressureWindow::new(120);
        for _ in 0..10 {
            w.record(true);
        }
        assert_eq!(w.miss_rate(), Some(0.0));
    }

    #[test]
    fn test_window_all_misses() {
        let mut w = PressureWindow::new(120);
        for _ in 0..10 {
            w.record(false);
        }
        assert_eq!(w.miss_rate(), Some(1.0));
    }

    #[test]
    fn test_window_mixed_events() {
        let mut w = PressureWindow::new(120);
        // 3 misses, 7 hits → 30% miss rate
        for _ in 0..3 {
            w.record(false);
        }
        for _ in 0..7 {
            w.record(true);
        }
        let rate = w.miss_rate().unwrap();
        assert!((rate - 0.3).abs() < 0.001);
    }

    #[test]
    fn test_window_prunes_old_events() {
        // Window of 1 second
        let mut w = PressureWindow::new(1);
        let old = Instant::now() - std::time::Duration::from_secs(5);
        w.record_at(old, false);
        w.record_at(old, false);
        // These old events should be pruned
        w.record(true);
        assert_eq!(w.len(), 1);
        assert_eq!(w.miss_rate(), Some(0.0));
    }

    #[test]
    fn test_window_record_at() {
        let mut w = PressureWindow::new(300);
        let now = Instant::now();
        w.record_at(now, true);
        w.record_at(now, false);
        assert_eq!(w.len(), 2);
        assert_eq!(w.miss_rate(), Some(0.5));
    }

    // --- ScaleDecision tests ---

    #[test]
    fn test_scale_decision_equality() {
        assert_eq!(ScaleDecision::Hold, ScaleDecision::Hold);
        assert_eq!(ScaleDecision::ScaleUp(1), ScaleDecision::ScaleUp(1));
        assert_eq!(ScaleDecision::ScaleDown(1), ScaleDecision::ScaleDown(1));
        assert_ne!(ScaleDecision::ScaleUp(1), ScaleDecision::ScaleDown(1));
    }

    #[test]
    fn test_scale_decision_debug() {
        let d = ScaleDecision::ScaleUp(2);
        assert!(format!("{:?}", d).contains("ScaleUp"));
    }

    // --- PoolScaler tests ---

    #[test]
    fn test_scaler_initial_min_idle() {
        let scaler = PoolScaler::new(default_policy(), 2, 10);
        assert_eq!(scaler.current_min_idle(), 2);
    }

    #[test]
    fn test_scaler_max_min_idle_defaults_to_max_size() {
        let scaler = PoolScaler::new(default_policy(), 2, 10);
        assert_eq!(scaler.max_min_idle, 10);
    }

    #[test]
    fn test_scaler_max_min_idle_capped_by_max_size() {
        let mut policy = default_policy();
        policy.max_min_idle = 20; // higher than max_size
        let scaler = PoolScaler::new(policy, 2, 10);
        assert_eq!(scaler.max_min_idle, 10); // capped to max_size
    }

    #[test]
    fn test_scaler_max_min_idle_explicit() {
        let mut policy = default_policy();
        policy.max_min_idle = 5;
        let scaler = PoolScaler::new(policy, 2, 10);
        assert_eq!(scaler.max_min_idle, 5);
    }

    #[test]
    fn test_scaler_hold_with_few_events() {
        let mut scaler = PoolScaler::new(default_policy(), 2, 10);
        scaler.record_acquire(false);
        scaler.record_acquire(false);
        // Only 2 events, need at least 3
        assert_eq!(scaler.evaluate(), ScaleDecision::Hold);
    }

    #[test]
    fn test_scaler_scale_up_on_high_miss_rate() {
        let mut policy = default_policy();
        policy.cooldown_secs = 0; // disable cooldown for test
        let mut scaler = PoolScaler::new(policy, 2, 10);

        // 4 misses, 1 hit → 80% miss rate > 30% threshold
        for _ in 0..4 {
            scaler.record_acquire(false);
        }
        scaler.record_acquire(true);

        assert_eq!(scaler.evaluate(), ScaleDecision::ScaleUp(1));
        assert_eq!(scaler.current_min_idle(), 3);
    }

    #[test]
    fn test_scaler_scale_down_on_low_miss_rate() {
        let mut policy = default_policy();
        policy.cooldown_secs = 0;
        let mut scaler = PoolScaler::new(policy, 3, 10);

        // 20 hits, 0 misses → 0% miss rate < 5% threshold
        for _ in 0..20 {
            scaler.record_acquire(true);
        }

        assert_eq!(scaler.evaluate(), ScaleDecision::ScaleDown(1));
        assert_eq!(scaler.current_min_idle(), 2);
    }

    #[test]
    fn test_scaler_hold_in_normal_range() {
        let mut policy = default_policy();
        policy.cooldown_secs = 0;
        let mut scaler = PoolScaler::new(policy, 3, 10);

        // 10 hits, 1 miss → 9% miss rate (between 5% and 30%)
        for _ in 0..10 {
            scaler.record_acquire(true);
        }
        scaler.record_acquire(false);

        assert_eq!(scaler.evaluate(), ScaleDecision::Hold);
        assert_eq!(scaler.current_min_idle(), 3);
    }

    #[test]
    fn test_scaler_wont_exceed_max_min_idle() {
        let mut policy = default_policy();
        policy.cooldown_secs = 0;
        policy.max_min_idle = 3;
        let mut scaler = PoolScaler::new(policy, 3, 10);

        // All misses but already at max_min_idle
        for _ in 0..5 {
            scaler.record_acquire(false);
        }

        assert_eq!(scaler.evaluate(), ScaleDecision::Hold);
        assert_eq!(scaler.current_min_idle(), 3);
    }

    #[test]
    fn test_scaler_wont_go_below_one() {
        let mut policy = default_policy();
        policy.cooldown_secs = 0;
        let mut scaler = PoolScaler::new(policy, 1, 10);

        // All hits, but already at floor of 1
        for _ in 0..10 {
            scaler.record_acquire(true);
        }

        assert_eq!(scaler.evaluate(), ScaleDecision::Hold);
        assert_eq!(scaler.current_min_idle(), 1);
    }

    #[test]
    fn test_scaler_cooldown_prevents_rapid_scaling() {
        let policy = default_policy(); // cooldown = 60s
        let mut scaler = PoolScaler::new(policy, 2, 10);

        // First evaluation: scale up
        for _ in 0..5 {
            scaler.record_acquire(false);
        }
        assert_eq!(scaler.evaluate(), ScaleDecision::ScaleUp(1));

        // Second evaluation immediately: should hold due to cooldown
        for _ in 0..5 {
            scaler.record_acquire(false);
        }
        assert_eq!(scaler.evaluate(), ScaleDecision::Hold);
    }

    #[test]
    fn test_scaler_successive_scale_ups() {
        let mut policy = default_policy();
        policy.cooldown_secs = 0;
        let mut scaler = PoolScaler::new(policy, 1, 5);

        // Scale up multiple times
        for _ in 0..3 {
            for _ in 0..5 {
                scaler.record_acquire(false);
            }
            let decision = scaler.evaluate();
            assert_eq!(decision, ScaleDecision::ScaleUp(1));
        }
        assert_eq!(scaler.current_min_idle(), 4);
    }

    #[test]
    fn test_scaler_no_events_holds() {
        let mut policy = default_policy();
        policy.cooldown_secs = 0;
        let mut scaler = PoolScaler::new(policy, 2, 10);
        assert_eq!(scaler.evaluate(), ScaleDecision::Hold);
    }

    // --- ScalingPolicy config tests ---

    #[test]
    fn test_scaling_policy_default() {
        let policy = ScalingPolicy::default();
        assert!(!policy.enabled);
        assert!((policy.scale_up_threshold - 0.3).abs() < 0.001);
        assert!((policy.scale_down_threshold - 0.05).abs() < 0.001);
        assert_eq!(policy.max_min_idle, 0);
        assert_eq!(policy.cooldown_secs, 60);
        assert_eq!(policy.window_secs, 120);
    }

    #[test]
    fn test_scaling_policy_serde_roundtrip() {
        let policy = ScalingPolicy {
            enabled: true,
            scale_up_threshold: 0.4,
            scale_down_threshold: 0.1,
            max_min_idle: 8,
            cooldown_secs: 30,
            window_secs: 60,
        };
        let json = serde_json::to_string(&policy).unwrap();
        let parsed: ScalingPolicy = serde_json::from_str(&json).unwrap();
        assert!(parsed.enabled);
        assert!((parsed.scale_up_threshold - 0.4).abs() < 0.001);
        assert!((parsed.scale_down_threshold - 0.1).abs() < 0.001);
        assert_eq!(parsed.max_min_idle, 8);
        assert_eq!(parsed.cooldown_secs, 30);
        assert_eq!(parsed.window_secs, 60);
    }

    #[test]
    fn test_scaling_policy_deserialize_with_defaults() {
        let json = r#"{"enabled": true}"#;
        let policy: ScalingPolicy = serde_json::from_str(json).unwrap();
        assert!(policy.enabled);
        assert!((policy.scale_up_threshold - 0.3).abs() < 0.001);
        assert_eq!(policy.cooldown_secs, 60);
        assert_eq!(policy.window_secs, 120);
    }
}
