use super::*;
use a3s_box_core::event::EventEmitter;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};

#[test]
fn native_snapshot_launch_shape_is_explicit_and_platform_gated() {
    assert!(validate_snapshot_launch_shape(false, false, false).is_ok());

    if native_snapshot_fork_supported() {
        assert!(validate_snapshot_launch_shape(true, true, false).is_ok());
        assert!(validate_snapshot_launch_shape(true, false, true).is_ok());
        for invalid in [
            (true, false, false),
            (false, true, false),
            (false, false, true),
            (true, true, true),
        ] {
            assert!(validate_snapshot_launch_shape(invalid.0, invalid.1, invalid.2).is_err());
        }
    } else {
        let error = validate_snapshot_launch_shape(true, true, false)
            .unwrap_err()
            .to_string();
        assert!(error.contains("Linux x86_64 KVM"), "{error}");
    }
}

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
#[tokio::test]
async fn unsupported_native_snapshot_fails_before_layout_side_effects() {
    let temporary = tempfile::tempdir().unwrap();
    let box_id = "unsupported-snapshot".to_string();
    let config = BoxConfig {
        snapshot_mem_file: Some(temporary.path().join("memory").display().to_string()),
        snapshot_sock: Some(temporary.path().join("trigger.sock").display().to_string()),
        ..BoxConfig::default()
    };
    let mut vm = VmManager::with_box_id(config, EventEmitter::new(8), box_id.clone());
    vm.home_dir = temporary.path().to_path_buf();

    let error = vm.boot().await.unwrap_err().to_string();

    assert!(error.contains("Linux x86_64 KVM"), "{error}");
    assert!(!temporary.path().join("boxes").join(box_id).exists());
    assert!(!temporary.path().join("memory").exists());
}

struct RecordingHandler {
    stopped: Arc<AtomicBool>,
}

impl VmHandler for RecordingHandler {
    fn stop(&mut self, _signal: i32, _timeout_ms: u64) -> Result<()> {
        self.stopped.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn metrics(&self) -> crate::vmm::VmMetrics {
        crate::vmm::VmMetrics::default()
    }

    fn is_running(&self) -> bool {
        true
    }

    fn has_exited(&self) -> bool {
        false
    }

    fn pid(&self) -> u32 {
        42
    }
}

#[cfg(unix)]
struct GuestStopHandler {
    exited: Arc<AtomicBool>,
    backend_finalized: Arc<AtomicBool>,
}

#[cfg(unix)]
impl VmHandler for GuestStopHandler {
    fn stop(&mut self, _signal: i32, _timeout_ms: u64) -> Result<()> {
        self.backend_finalized.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn metrics(&self) -> crate::vmm::VmMetrics {
        crate::vmm::VmMetrics::default()
    }

    fn is_running(&self) -> bool {
        !self.exited.load(Ordering::SeqCst)
    }

    fn has_exited(&self) -> bool {
        self.exited.load(Ordering::SeqCst)
    }

    fn pid(&self) -> u32 {
        42
    }

    fn exit_code(&self) -> Option<i32> {
        self.exited.load(Ordering::SeqCst).then_some(0)
    }

    fn try_wait_exit(&mut self) -> Result<Option<i32>> {
        Ok(self.exited.load(Ordering::SeqCst).then_some(0))
    }
}

struct ExitStateHandler {
    exited: bool,
}

impl VmHandler for ExitStateHandler {
    fn stop(&mut self, _signal: i32, _timeout_ms: u64) -> Result<()> {
        Ok(())
    }

    fn metrics(&self) -> crate::vmm::VmMetrics {
        crate::vmm::VmMetrics::default()
    }

    fn is_running(&self) -> bool {
        !self.exited
    }

    fn has_exited(&self) -> bool {
        self.exited
    }

    fn pid(&self) -> u32 {
        42
    }
}

struct CompletedHandler {
    code: i32,
}

impl VmHandler for CompletedHandler {
    fn stop(&mut self, _signal: i32, _timeout_ms: u64) -> Result<()> {
        Ok(())
    }

    fn metrics(&self) -> crate::vmm::VmMetrics {
        crate::vmm::VmMetrics::default()
    }

    fn is_running(&self) -> bool {
        false
    }

    fn has_exited(&self) -> bool {
        true
    }

    fn pid(&self) -> u32 {
        42
    }

    fn exit_code(&self) -> Option<i32> {
        Some(self.code)
    }

    fn try_wait_exit(&mut self) -> Result<Option<i32>> {
        Ok(Some(self.code))
    }
}

#[cfg(not(target_os = "windows"))]
struct PersistedExitOnCompletionHandler {
    provider_code: i32,
    persisted_code: i32,
    exit_path: PathBuf,
}

#[cfg(not(target_os = "windows"))]
impl VmHandler for PersistedExitOnCompletionHandler {
    fn stop(&mut self, _signal: i32, _timeout_ms: u64) -> Result<()> {
        Ok(())
    }

    fn metrics(&self) -> crate::vmm::VmMetrics {
        crate::vmm::VmMetrics::default()
    }

    fn is_running(&self) -> bool {
        false
    }

    fn has_exited(&self) -> bool {
        true
    }

    fn pid(&self) -> u32 {
        42
    }

    fn exit_code(&self) -> Option<i32> {
        Some(self.provider_code)
    }

    fn try_wait_exit(&mut self) -> Result<Option<i32>> {
        std::fs::write(&self.exit_path, format!("{}\n", self.persisted_code))?;
        Ok(Some(self.provider_code))
    }
}

struct CompletionCollectedByStopHandler {
    code: i32,
    collected: bool,
}

impl VmHandler for CompletionCollectedByStopHandler {
    fn stop(&mut self, _signal: i32, _timeout_ms: u64) -> Result<()> {
        self.collected = true;
        Ok(())
    }

    fn metrics(&self) -> crate::vmm::VmMetrics {
        crate::vmm::VmMetrics::default()
    }

    fn is_running(&self) -> bool {
        false
    }

    fn has_exited(&self) -> bool {
        true
    }

    fn pid(&self) -> u32 {
        42
    }

    fn exit_code(&self) -> Option<i32> {
        self.collected.then_some(self.code)
    }

    fn try_wait_exit(&mut self) -> Result<Option<i32>> {
        Ok(None)
    }
}

struct DelayedCompletionHandler {
    polls: Arc<AtomicUsize>,
    available_after: usize,
}

impl VmHandler for DelayedCompletionHandler {
    fn stop(&mut self, _signal: i32, _timeout_ms: u64) -> Result<()> {
        Ok(())
    }

    fn metrics(&self) -> crate::vmm::VmMetrics {
        crate::vmm::VmMetrics::default()
    }

    fn is_running(&self) -> bool {
        false
    }

    fn has_exited(&self) -> bool {
        true
    }

    fn pid(&self) -> u32 {
        42
    }

    fn exit_code(&self) -> Option<i32> {
        (self.polls.load(Ordering::SeqCst) > self.available_after).then_some(0)
    }

    fn try_wait_exit(&mut self) -> Result<Option<i32>> {
        let poll = self.polls.fetch_add(1, Ordering::SeqCst);
        Ok((poll >= self.available_after).then_some(0))
    }
}

/// A handler whose `stop` always fails — models a wedged VM that won't halt.
struct FailingHandler;

impl VmHandler for FailingHandler {
    fn stop(&mut self, _signal: i32, _timeout_ms: u64) -> Result<()> {
        Err(BoxError::StateError("simulated stop failure".to_string()))
    }

    fn metrics(&self) -> crate::vmm::VmMetrics {
        crate::vmm::VmMetrics::default()
    }

    fn is_running(&self) -> bool {
        true
    }

    fn pid(&self) -> u32 {
        42
    }
}

// Regression: a handler-stop failure must still run the host teardown
// (overlay unmount, socket + box dirs). Pre-fix, destroy_with_options
// returned early on the stop error and leaked the box directory on every
// wedged stop.
#[path = "tests/lifecycle.rs"]
mod lifecycle;
#[path = "tests/platform.rs"]
mod platform;
