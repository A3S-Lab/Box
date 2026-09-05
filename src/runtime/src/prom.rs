//! Prometheus metrics for the A3S Box runtime.
//!
//! Provides pre-registered metrics for VM lifecycle, exec operations,
//! image management, and warm pool monitoring.
//!
//! # Usage
//!
//! ```rust,no_run
//! use a3s_box_runtime::prom::RuntimeMetrics;
//!
//! let metrics = RuntimeMetrics::new();
//! metrics.vm_boot_duration.observe(0.195); // 195ms boot
//! metrics.vm_count.with_label_values(&["ready"]).inc();
//! ```

use prometheus::{
    Error as PrometheusError, GaugeVec, Histogram, HistogramOpts, HistogramVec, IntCounter,
    IntGauge, IntGaugeVec, Opts, Registry,
};

/// Pre-registered Prometheus metrics for the Box runtime.
#[derive(Clone)]
pub struct RuntimeMetrics {
    /// Prometheus registry holding all metrics.
    pub registry: Registry,

    // -- VM lifecycle --
    /// VM boot duration in seconds.
    pub vm_boot_duration: Histogram,
    /// VM boot duration split by a stable lifecycle phase.
    ///
    /// The `phase` label is intentionally bounded to runtime-owned values such
    /// as `layout`, `prepare`, `launch`, and `readiness`; callers must not use
    /// box IDs, image references, or other unbounded values as labels.
    pub vm_boot_phase_duration: HistogramVec,
    /// Number of VMs by state (created, ready, busy, compacting, stopped).
    pub vm_count: IntGaugeVec,
    /// Total VMs created since process start.
    pub vm_created_total: IntCounter,
    /// Total VMs destroyed since process start.
    pub vm_destroyed_total: IntCounter,

    // -- VM resources --
    /// VM CPU usage percentage (per VM, labeled by box_id).
    pub vm_cpu_percent: GaugeVec,
    /// VM memory usage in bytes (per VM, labeled by box_id).
    pub vm_memory_bytes: GaugeVec,

    // -- Exec operations --
    /// Total exec commands executed.
    pub exec_total: IntCounter,
    /// Exec command duration in seconds.
    pub exec_duration: Histogram,
    /// Exec commands that failed (non-zero exit or error).
    pub exec_errors_total: IntCounter,

    // -- Image operations --
    /// Total image pulls.
    pub image_pull_total: IntCounter,
    /// Image pull duration in seconds.
    pub image_pull_duration: Histogram,
    /// Total image builds.
    pub image_build_total: IntCounter,
    /// Rootfs cache hits.
    pub rootfs_cache_hits: IntCounter,
    /// Rootfs cache misses.
    pub rootfs_cache_misses: IntCounter,

    // -- Warm pool --
    /// Current warm pool size (idle VMs).
    pub warm_pool_size: IntGauge,
    /// Warm pool capacity (max_size).
    pub warm_pool_capacity: IntGauge,
    /// Total VMs allocated from warm pool.
    pub warm_pool_hits: IntCounter,
    /// Total VMs created fresh (warm pool miss).
    pub warm_pool_misses: IntCounter,
}

impl RuntimeMetrics {
    /// Create and register all metrics with a new registry.
    pub fn new() -> Self {
        Self::try_new().expect("static RuntimeMetrics descriptors should be valid")
    }

    /// Try to create and register all metrics with a new registry.
    pub fn try_new() -> Result<Self, PrometheusError> {
        let registry = Registry::new();
        Self::try_with_registry(registry)
    }

    /// Create and register all metrics with an existing registry.
    pub fn with_registry(registry: Registry) -> Self {
        Self::try_with_registry(registry)
            .expect("static RuntimeMetrics descriptors should not conflict")
    }

    /// Try to create and register all metrics with an existing registry.
    pub fn try_with_registry(registry: Registry) -> Result<Self, PrometheusError> {
        // VM lifecycle
        let vm_boot_duration = Histogram::with_opts(
            HistogramOpts::new(
                "a3s_box_vm_boot_duration_seconds",
                "VM boot duration in seconds",
            )
            .buckets(vec![0.05, 0.1, 0.15, 0.2, 0.3, 0.5, 1.0, 2.0, 5.0, 10.0]),
        )?;

        let vm_boot_phase_duration = HistogramVec::new(
            HistogramOpts::new(
                "a3s_box_vm_boot_phase_duration_seconds",
                "VM boot duration in seconds by lifecycle phase",
            )
            .buckets(vec![
                0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0, 30.0, 60.0,
            ]),
            &["phase"],
        )?;

        let vm_count = IntGaugeVec::new(
            Opts::new("a3s_box_vm_count", "Number of VMs by state"),
            &["state"],
        )?;

        let vm_created_total = IntCounter::new("a3s_box_vm_created_total", "Total VMs created")?;

        let vm_destroyed_total =
            IntCounter::new("a3s_box_vm_destroyed_total", "Total VMs destroyed")?;

        // VM resources
        let vm_cpu_percent = GaugeVec::new(
            Opts::new("a3s_box_vm_cpu_percent", "VM CPU usage percentage"),
            &["box_id"],
        )?;

        let vm_memory_bytes = GaugeVec::new(
            Opts::new("a3s_box_vm_memory_bytes", "VM memory usage in bytes"),
            &["box_id"],
        )?;

        // Exec operations
        let exec_total = IntCounter::new("a3s_box_exec_total", "Total exec commands executed")?;

        let exec_duration = Histogram::with_opts(
            HistogramOpts::new(
                "a3s_box_exec_duration_seconds",
                "Exec command duration in seconds",
            )
            .buckets(vec![0.01, 0.05, 0.1, 0.5, 1.0, 5.0, 10.0, 30.0, 60.0]),
        )?;

        let exec_errors_total =
            IntCounter::new("a3s_box_exec_errors_total", "Total failed exec commands")?;

        // Image operations
        let image_pull_total = IntCounter::new("a3s_box_image_pull_total", "Total image pulls")?;

        let image_pull_duration = Histogram::with_opts(
            HistogramOpts::new(
                "a3s_box_image_pull_duration_seconds",
                "Image pull duration in seconds",
            )
            .buckets(vec![0.5, 1.0, 2.0, 5.0, 10.0, 30.0, 60.0, 120.0]),
        )?;

        let image_build_total = IntCounter::new("a3s_box_image_build_total", "Total image builds")?;

        let rootfs_cache_hits =
            IntCounter::new("a3s_box_rootfs_cache_hits_total", "Rootfs cache hits")?;

        let rootfs_cache_misses =
            IntCounter::new("a3s_box_rootfs_cache_misses_total", "Rootfs cache misses")?;

        // Warm pool
        let warm_pool_size = IntGauge::new(
            "a3s_box_warm_pool_size",
            "Current warm pool size (idle VMs)",
        )?;

        let warm_pool_capacity =
            IntGauge::new("a3s_box_warm_pool_capacity", "Warm pool max capacity")?;

        let warm_pool_hits = IntCounter::new(
            "a3s_box_warm_pool_hits_total",
            "VMs allocated from warm pool",
        )?;

        let warm_pool_misses = IntCounter::new(
            "a3s_box_warm_pool_misses_total",
            "VMs created fresh (warm pool miss)",
        )?;

        // Register all metrics
        registry.register(Box::new(vm_boot_duration.clone()))?;
        registry.register(Box::new(vm_boot_phase_duration.clone()))?;
        registry.register(Box::new(vm_count.clone()))?;
        registry.register(Box::new(vm_created_total.clone()))?;
        registry.register(Box::new(vm_destroyed_total.clone()))?;
        registry.register(Box::new(vm_cpu_percent.clone()))?;
        registry.register(Box::new(vm_memory_bytes.clone()))?;
        registry.register(Box::new(exec_total.clone()))?;
        registry.register(Box::new(exec_duration.clone()))?;
        registry.register(Box::new(exec_errors_total.clone()))?;
        registry.register(Box::new(image_pull_total.clone()))?;
        registry.register(Box::new(image_pull_duration.clone()))?;
        registry.register(Box::new(image_build_total.clone()))?;
        registry.register(Box::new(rootfs_cache_hits.clone()))?;
        registry.register(Box::new(rootfs_cache_misses.clone()))?;
        registry.register(Box::new(warm_pool_size.clone()))?;
        registry.register(Box::new(warm_pool_capacity.clone()))?;
        registry.register(Box::new(warm_pool_hits.clone()))?;
        registry.register(Box::new(warm_pool_misses.clone()))?;

        Ok(Self {
            registry,
            vm_boot_duration,
            vm_boot_phase_duration,
            vm_count,
            vm_created_total,
            vm_destroyed_total,
            vm_cpu_percent,
            vm_memory_bytes,
            exec_total,
            exec_duration,
            exec_errors_total,
            image_pull_total,
            image_pull_duration,
            image_build_total,
            rootfs_cache_hits,
            rootfs_cache_misses,
            warm_pool_size,
            warm_pool_capacity,
            warm_pool_hits,
            warm_pool_misses,
        })
    }

    /// Record one bounded VM boot phase in seconds.
    pub fn record_vm_boot_phase(&self, phase: &str, duration_secs: f64) {
        self.vm_boot_phase_duration
            .with_label_values(&[phase])
            .observe(duration_secs);
    }

    /// Remove resource gauges for a VM that no longer exists.
    ///
    /// The gauges are labelled by box ID for point-in-time inspection. Keeping
    /// every historical ID in a long-lived daemon would make the registry grow
    /// without bound, so lifecycle teardown must delete both label sets.
    pub fn remove_vm_resource_metrics(&self, box_id: &str) {
        if let Err(error) = self.vm_cpu_percent.remove_label_values(&[box_id]) {
            tracing::debug!(%box_id, %error, "VM CPU metric labels were already absent");
        }
        if let Err(error) = self.vm_memory_bytes.remove_label_values(&[box_id]) {
            tracing::debug!(%box_id, %error, "VM memory metric labels were already absent");
        }
    }

    /// Encode all metrics in Prometheus text exposition format.
    pub fn encode(&self) -> String {
        use prometheus::Encoder;
        let encoder = prometheus::TextEncoder::new();
        let metric_families = self.registry.gather();
        let mut buffer = Vec::new();
        encoder
            .encode(&metric_families, &mut buffer)
            .expect("encode");
        String::from_utf8(buffer).expect("utf8")
    }
}

/// Drop-based timer for recording a boot phase even when the phase returns an
/// error. The metrics handle is cloned so the timer never borrows a
/// [`VmManager`] across an await point.
pub(crate) struct BootPhaseTimer {
    metrics: Option<RuntimeMetrics>,
    phase: &'static str,
    started: std::time::Instant,
}

impl BootPhaseTimer {
    pub(crate) fn new(metrics: Option<RuntimeMetrics>, phase: &'static str) -> Self {
        Self {
            metrics,
            phase,
            started: std::time::Instant::now(),
        }
    }
}

impl Drop for BootPhaseTimer {
    fn drop(&mut self) {
        if let Some(metrics) = &self.metrics {
            metrics.record_vm_boot_phase(self.phase, self.started.elapsed().as_secs_f64());
        }
    }
}

impl Default for RuntimeMetrics {
    fn default() -> Self {
        Self::new()
    }
}

impl a3s_box_core::traits::MetricsCollector for RuntimeMetrics {
    fn record_vm_boot(&self, duration_secs: f64) {
        self.vm_boot_duration.observe(duration_secs);
    }

    fn inc_vm_state(&self, state: &str) {
        self.vm_count.with_label_values(&[state]).inc();
    }

    fn dec_vm_state(&self, state: &str) {
        self.vm_count.with_label_values(&[state]).dec();
    }

    fn inc_vm_created(&self) {
        self.vm_created_total.inc();
    }

    fn inc_vm_destroyed(&self) {
        self.vm_destroyed_total.inc();
    }

    fn record_exec(&self, duration_secs: f64, success: bool) {
        self.exec_total.inc();
        self.exec_duration.observe(duration_secs);
        if !success {
            self.exec_errors_total.inc();
        }
    }

    fn inc_cache_hit(&self) {
        self.rootfs_cache_hits.inc();
    }

    fn inc_cache_miss(&self) {
        self.rootfs_cache_misses.inc();
    }
}

impl std::fmt::Debug for RuntimeMetrics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeMetrics").finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use a3s_box_core::traits::MetricsCollector;

    #[test]
    fn test_metrics_creation() {
        let m = RuntimeMetrics::new();
        assert_eq!(m.vm_created_total.get(), 0);
        assert_eq!(m.vm_destroyed_total.get(), 0);
        assert_eq!(m.exec_total.get(), 0);
    }

    #[test]
    fn test_vm_boot_duration_observe() {
        let m = RuntimeMetrics::new();
        m.vm_boot_duration.observe(0.195);
        m.vm_boot_duration.observe(0.210);
        assert_eq!(m.vm_boot_duration.get_sample_count(), 2);
    }

    #[test]
    fn test_vm_boot_phase_duration_observe_and_encode() {
        let m = RuntimeMetrics::new();
        m.record_vm_boot_phase("layout", 0.012);
        m.record_vm_boot_phase("layout", 0.018);
        m.record_vm_boot_phase("readiness", 0.125);

        assert_eq!(
            m.vm_boot_phase_duration
                .with_label_values(&["layout"])
                .get_sample_count(),
            2
        );
        assert_eq!(
            m.vm_boot_phase_duration
                .with_label_values(&["readiness"])
                .get_sample_count(),
            1
        );
        let output = m.encode();
        assert!(output.contains("a3s_box_vm_boot_phase_duration_seconds"));
        assert!(output.contains("phase=\"layout\""));
    }

    #[test]
    fn test_boot_phase_timer_records_on_drop() {
        let m = RuntimeMetrics::new();
        {
            let _timer = BootPhaseTimer::new(Some(m.clone()), "launch");
        }
        assert_eq!(
            m.vm_boot_phase_duration
                .with_label_values(&["launch"])
                .get_sample_count(),
            1
        );
    }

    #[test]
    fn test_vm_count_by_state() {
        let m = RuntimeMetrics::new();
        m.vm_count.with_label_values(&["ready"]).set(3);
        m.vm_count.with_label_values(&["busy"]).set(1);
        assert_eq!(m.vm_count.with_label_values(&["ready"]).get(), 3);
        assert_eq!(m.vm_count.with_label_values(&["busy"]).get(), 1);
        assert_eq!(m.vm_count.with_label_values(&["stopped"]).get(), 0);
    }

    #[test]
    fn test_vm_created_destroyed_counters() {
        let m = RuntimeMetrics::new();
        m.vm_created_total.inc();
        m.vm_created_total.inc();
        m.vm_destroyed_total.inc();
        assert_eq!(m.vm_created_total.get(), 2);
        assert_eq!(m.vm_destroyed_total.get(), 1);
    }

    #[test]
    fn test_vm_resource_gauges() {
        let m = RuntimeMetrics::new();
        m.vm_cpu_percent.with_label_values(&["box-123"]).set(45.5);
        m.vm_memory_bytes
            .with_label_values(&["box-123"])
            .set(256.0 * 1024.0 * 1024.0);
        assert_eq!(m.vm_cpu_percent.with_label_values(&["box-123"]).get(), 45.5);
    }

    #[test]
    fn test_remove_vm_resource_metrics_drops_dynamic_labels() {
        let m = RuntimeMetrics::new();
        m.vm_cpu_percent
            .with_label_values(&["box-ephemeral"])
            .set(1.0);
        m.vm_memory_bytes
            .with_label_values(&["box-ephemeral"])
            .set(2.0);

        m.remove_vm_resource_metrics("box-ephemeral");

        let encoded = m.encode();
        assert!(!encoded.contains("box-ephemeral"));
    }

    #[test]
    fn test_exec_metrics() {
        let m = RuntimeMetrics::new();
        m.exec_total.inc();
        m.exec_duration.observe(0.05);
        m.exec_errors_total.inc();
        assert_eq!(m.exec_total.get(), 1);
        assert_eq!(m.exec_errors_total.get(), 1);
        assert_eq!(m.exec_duration.get_sample_count(), 1);
    }

    #[test]
    fn test_image_metrics() {
        let m = RuntimeMetrics::new();
        m.image_pull_total.inc();
        m.image_pull_duration.observe(3.5);
        m.image_build_total.inc();
        m.rootfs_cache_hits.inc();
        m.rootfs_cache_misses.inc();
        m.rootfs_cache_misses.inc();
        assert_eq!(m.image_pull_total.get(), 1);
        assert_eq!(m.rootfs_cache_hits.get(), 1);
        assert_eq!(m.rootfs_cache_misses.get(), 2);
    }

    #[test]
    fn test_warm_pool_metrics() {
        let m = RuntimeMetrics::new();
        m.warm_pool_capacity.set(10);
        m.warm_pool_size.set(5);
        m.warm_pool_hits.inc();
        m.warm_pool_misses.inc();
        assert_eq!(m.warm_pool_capacity.get(), 10);
        assert_eq!(m.warm_pool_size.get(), 5);
        assert_eq!(m.warm_pool_hits.get(), 1);
        assert_eq!(m.warm_pool_misses.get(), 1);
    }

    #[test]
    fn test_encode_prometheus_format() {
        let m = RuntimeMetrics::new();
        m.vm_created_total.inc();
        m.exec_total.inc();
        let output = m.encode();
        assert!(output.contains("a3s_box_vm_created_total 1"));
        assert!(output.contains("a3s_box_exec_total 1"));
        assert!(output.contains("# HELP"));
        assert!(output.contains("# TYPE"));
    }

    #[test]
    fn test_metrics_clone() {
        let m = RuntimeMetrics::new();
        m.vm_created_total.inc();
        let m2 = m.clone();
        // Cloned metrics share the same underlying counters
        assert_eq!(m2.vm_created_total.get(), 1);
        m.vm_created_total.inc();
        assert_eq!(m2.vm_created_total.get(), 2);
    }

    #[test]
    fn test_metrics_default() {
        let m = RuntimeMetrics::default();
        assert_eq!(m.vm_created_total.get(), 0);
    }

    #[test]
    fn test_try_with_registry_reports_duplicate_registration() {
        let registry = Registry::new();
        let _first = RuntimeMetrics::try_with_registry(registry.clone()).unwrap();
        let second = RuntimeMetrics::try_with_registry(registry);
        assert!(second.is_err());
    }

    #[test]
    fn test_metrics_collector_trait_updates_registered_metrics() {
        let m = RuntimeMetrics::new();

        MetricsCollector::record_vm_boot(&m, 0.25);
        MetricsCollector::inc_vm_state(&m, "ready");
        MetricsCollector::inc_vm_created(&m);
        MetricsCollector::inc_vm_destroyed(&m);
        MetricsCollector::record_exec(&m, 0.05, false);
        MetricsCollector::inc_cache_hit(&m);
        MetricsCollector::inc_cache_miss(&m);
        MetricsCollector::dec_vm_state(&m, "ready");

        assert_eq!(m.vm_boot_duration.get_sample_count(), 1);
        assert_eq!(m.vm_count.with_label_values(&["ready"]).get(), 0);
        assert_eq!(m.vm_created_total.get(), 1);
        assert_eq!(m.vm_destroyed_total.get(), 1);
        assert_eq!(m.exec_total.get(), 1);
        assert_eq!(m.exec_errors_total.get(), 1);
        assert_eq!(m.exec_duration.get_sample_count(), 1);
        assert_eq!(m.rootfs_cache_hits.get(), 1);
        assert_eq!(m.rootfs_cache_misses.get(), 1);
    }

    #[test]
    fn test_metrics_collector_trait_does_not_count_successful_exec_as_error() {
        let m = RuntimeMetrics::new();

        MetricsCollector::record_exec(&m, 0.01, true);

        assert_eq!(m.exec_total.get(), 1);
        assert_eq!(m.exec_errors_total.get(), 0);
        assert_eq!(m.exec_duration.get_sample_count(), 1);
    }

    #[test]
    fn test_runtime_metrics_debug_is_stable_and_compact() {
        let m = RuntimeMetrics::new();

        assert_eq!(format!("{m:?}"), "RuntimeMetrics");
    }
}
