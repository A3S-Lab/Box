//! Per-container cgroup v2 (memory + cpu limits) for the guest.
//!
//! The CRI `LinuxContainerResources` limits are enforced inside the guest by
//! placing the container — and, crucially, every main, exec, and PTY process —
//! in one cgroup v2 with `memory.max` and/or `cpu.max` set. Each process joins the
//! cgroup from its pre-exec hook (writing its own PID to `cgroup.procs` before
//! exec), so workers it forks immediately cannot escape the limit. When memory
//! is exceeded the kernel OOM killer selects a process from the workload while
//! `memory.events`'s `oom_kill` counter lets us report the exit reason as
//! `OOMKilled`. The workload is deliberately not an indivisible OOM group: an
//! exec allocation failure must not kill an otherwise healthy long-lived main
//! process.
//!
//! This is Linux-only and best-effort for legacy MicroVM environments. The host
//! Sandbox R17 gate separately fails closed unless the exact workload cgroup is
//! observable and behaviorally enforces every advertised limit.

#![cfg(target_os = "linux")]

use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};

use tracing::{debug, warn};

const CGROUP_ROOT: &str = "/sys/fs/cgroup";
const CONTROL_CGROUP_NAME: &str = "a3s-control";
static CGROUP_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Default)]
struct CgroupLimits {
    memory_max: Option<u64>,
    memory_low: Option<u64>,
    memory_swap_max: Option<i64>,
    cpu_quota: Option<i64>,
    cpu_period: Option<u64>,
    cpu_shares: Option<u64>,
    pids_max: Option<u64>,
}

/// Ensure cgroup v2 is mounted at `/sys/fs/cgroup` with the `memory`, `cpu`, and
/// `pids` controllers delegated to child cgroups. Idempotent; returns `false`
/// only if cgroup v2 cannot be mounted or exposes no controllers at all. Each
/// individual limit (memory.max / cpu.max / pids.max) is best-effort in
/// `ContainerCgroup::create_for_main`, so a missing controller degrades to
/// "that limit is not enforced" rather than failing the launch.
pub fn ensure_cgroup2_ready() -> bool {
    let controllers_path = format!("{CGROUP_ROOT}/cgroup.controllers");
    if std::fs::metadata(&controllers_path).is_err() {
        // Not mounted yet — mount the unified hierarchy.
        use nix::mount::{mount, MsFlags};
        let _ = std::fs::create_dir_all(CGROUP_ROOT);
        if let Err(error) = mount(
            Some("cgroup2"),
            CGROUP_ROOT,
            Some("cgroup2"),
            MsFlags::empty(),
            None::<&str>,
        ) {
            // A concurrent caller may have mounted it between our check and this
            // mount (EBUSY): only treat it as a failure if the hierarchy still
            // isn't there, so one of two racing execs doesn't skip enforcement.
            if std::fs::metadata(&controllers_path).is_err() {
                warn!(error = %error, "cgroup: failed to mount cgroup2");
                return false;
            }
        }
    }

    let available = match std::fs::read_to_string(&controllers_path) {
        Ok(controllers) => controllers,
        Err(error) => {
            warn!(error = %error, "cgroup: cannot read cgroup.controllers");
            return false;
        }
    };
    // A microVM sees the real hierarchy root, where controllers can be enabled
    // while PID 1 remains in place. A host Sandbox instead sees its OCI leaf as
    // the cgroup-namespace root. The no-internal-process rule requires moving
    // trusted guest-init into a management child before that leaf can delegate
    // controllers to the workload child. Keep that topology here as the single
    // setup path for main, exec, and PTY workloads.
    if !enable_subtree_controllers(CGROUP_ROOT, &available) {
        if !move_control_plane_to_child(CGROUP_ROOT)
            || !enable_subtree_controllers(CGROUP_ROOT, &available)
        {
            warn!("cgroup: failed to prepare a delegated workload hierarchy");
            return false;
        }
    }

    if available.split_whitespace().next().is_none() {
        warn!("cgroup: no v2 controllers available in cgroup.controllers");
        return false;
    }
    true
}

fn enable_subtree_controllers(cgroup_root: &str, available: &str) -> bool {
    let subtree = format!("{cgroup_root}/cgroup.subtree_control");
    let current = std::fs::read_to_string(&subtree).unwrap_or_default();
    for controller in ["memory", "cpu", "pids"] {
        if available
            .split_whitespace()
            .any(|value| value == controller)
            && !current.split_whitespace().any(|value| value == controller)
        {
            if let Err(error) = write_cgroup_file(&subtree, &format!("+{controller}")) {
                debug!(error = %error, controller, "cgroup: controller delegation needs an empty parent");
            }
        }
    }

    let enabled = std::fs::read_to_string(&subtree).unwrap_or_default();
    ["memory", "cpu", "pids"].into_iter().all(|controller| {
        !available
            .split_whitespace()
            .any(|value| value == controller)
            || enabled.split_whitespace().any(|value| value == controller)
    })
}

fn move_control_plane_to_child(cgroup_root: &str) -> bool {
    let control = format!("{cgroup_root}/{CONTROL_CGROUP_NAME}");
    match std::fs::create_dir(&control) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            if !std::fs::symlink_metadata(&control)
                .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
            {
                warn!(
                    path = control,
                    "cgroup: control path is not a real directory"
                );
                return false;
            }
        }
        Err(error) => {
            warn!(error = %error, path = control, "cgroup: failed to create control cgroup");
            return false;
        }
    }

    let procs = format!("{control}/cgroup.procs");
    if let Err(error) = write_cgroup_file(&procs, &std::process::id().to_string()) {
        warn!(error = %error, path = procs, "cgroup: failed to isolate the control plane");
        return false;
    }
    debug!(
        path = control,
        "cgroup: moved control plane out of workload parent"
    );
    true
}

fn write_cgroup_file(path: &str, value: &str) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new().write(true).open(path)?;
    file.write_all(value.as_bytes())
}

/// Map a CRI `cpu_shares` value (cgroup v1 range [2, 262144], default 1024) to a
/// cgroup v2 `cpu.weight` (range [1, 10000]), using runc's conversion.
fn shares_to_weight(shares: u64) -> u64 {
    let shares = shares.clamp(2, 262_144);
    (1 + ((shares - 2) * 9999) / 262_142).clamp(1, 10_000)
}

/// A per-container cgroup v2 (memory + cpu limits). Dropping it removes the
/// cgroup directory.
pub struct ContainerCgroup {
    path: String,
}

impl ContainerCgroup {
    /// Create the cgroup for a long-lived container main process.
    ///
    /// This retains an otherwise-unlimited cgroup so every later exec/PTY joins
    /// the same aggregate budget and `container-update` always has one safe,
    /// unambiguous target.
    #[allow(clippy::too_many_arguments)]
    pub fn create_for_main(
        memory_max: Option<u64>,
        memory_low: Option<u64>,
        memory_swap_max: Option<i64>,
        cpu_quota: Option<i64>,
        cpu_period: Option<u64>,
        cpu_shares: Option<u64>,
        pids_max: Option<u64>,
    ) -> Option<Self> {
        if !ensure_cgroup2_ready() {
            return None;
        }
        Self::create_in_ready_hierarchy(
            CGROUP_ROOT,
            CgroupLimits {
                memory_max,
                memory_low,
                memory_swap_max,
                cpu_quota,
                cpu_period,
                cpu_shares,
                pids_max,
            },
        )
    }

    fn create_in_ready_hierarchy(cgroup_root: &str, limits: CgroupLimits) -> Option<Self> {
        let want_memory = limits.memory_max.is_some_and(|value| value > 0);
        let want_memory_low = limits.memory_low.is_some_and(|value| value > 0);
        // memory.swap.max accepts a byte value or -1 (unlimited); any explicit
        // value means the limit was requested.
        let want_memory_swap = limits.memory_swap_max.is_some();
        let want_cpu = limits.cpu_quota.is_some_and(|value| value > 0);
        let want_weight = limits.cpu_shares.is_some_and(|value| value > 0);
        let want_pids = limits.pids_max.is_some_and(|value| value > 0);
        let seq = CGROUP_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = format!("{cgroup_root}/box-{}-{}", std::process::id(), seq);
        if let Err(error) = std::fs::create_dir(&path) {
            warn!(error = %error, path, "cgroup: failed to create container cgroup");
            return None;
        }
        if want_memory {
            let limit = limits.memory_max.unwrap_or(0);
            if let Err(error) = write_cgroup_file(&format!("{path}/memory.max"), &limit.to_string())
            {
                warn!(error = %error, "cgroup: failed to set memory.max");
                let _ = std::fs::remove_dir(&path);
                return None;
            }
            // Keep the aggregate workload cgroup divisible during OOM. The
            // kernel can then kill the allocating exec/worker without also
            // killing a healthy long-lived main process. This explicit write
            // guards against a non-default template while remaining
            // best-effort on older kernels.
            let _ = write_cgroup_file(&format!("{path}/memory.oom.group"), "0");
        }
        if want_memory_low {
            // memory.low = best-effort soft reservation (--memory-reservation):
            // the kernel reclaims from this cgroup only after unprotected memory
            // is exhausted. Non-fatal so other limits still apply.
            let low = limits.memory_low.unwrap_or(0);
            if let Err(error) = write_cgroup_file(&format!("{path}/memory.low"), &low.to_string()) {
                warn!(error = %error, low, "cgroup: failed to set memory.low");
            }
        }
        if want_memory_swap {
            // memory.swap.max (--memory-swap): a byte cap, or "max" for -1
            // (unlimited swap). Non-fatal.
            let value = match limits.memory_swap_max {
                Some(v) if v < 0 => "max".to_string(),
                Some(v) => v.to_string(),
                None => "max".to_string(),
            };
            if let Err(error) = write_cgroup_file(&format!("{path}/memory.swap.max"), &value) {
                warn!(error = %error, value, "cgroup: failed to set memory.swap.max");
            }
        }
        if want_cpu {
            // cgroup v2 `cpu.max` = "<quota_us> <period_us>"; CRI defaults the
            // period to 100ms when unset.
            let period = limits
                .cpu_period
                .filter(|value| *value > 0)
                .unwrap_or(100_000);
            let value = format!("{} {}", limits.cpu_quota.unwrap_or(0), period);
            // Non-fatal: keep the cgroup so any memory limit still applies.
            if let Err(error) = write_cgroup_file(&format!("{path}/cpu.max"), &value) {
                warn!(error = %error, value, "cgroup: failed to set cpu.max");
            }
        }
        if want_weight {
            let weight = shares_to_weight(limits.cpu_shares.unwrap_or(1024));
            if let Err(error) =
                write_cgroup_file(&format!("{path}/cpu.weight"), &weight.to_string())
            {
                warn!(error = %error, weight, "cgroup: failed to set cpu.weight");
            }
        }
        if want_pids {
            // cgroup v2 `pids.max` caps the number of processes/threads in the
            // cgroup; a fork past the limit fails with EAGAIN.
            let limit = limits.pids_max.unwrap_or(0);
            if let Err(error) = write_cgroup_file(&format!("{path}/pids.max"), &limit.to_string()) {
                warn!(error = %error, limit, "cgroup: failed to set pids.max");
            }
        }
        debug!(
            path,
            memory_max = ?limits.memory_max,
            cpu_quota = ?limits.cpu_quota,
            cpu_shares = ?limits.cpu_shares,
            pids_max = ?limits.pids_max,
            "cgroup: created container cgroup"
        );
        Some(Self { path })
    }

    /// Path to this cgroup's `cgroup.procs` (where a process writes its own PID
    /// to join). Used by the pre-exec hook so the container — and every process
    /// it forks — is in the cgroup from birth (a parent-side join after spawn
    /// races with workers the container forks immediately).
    pub fn procs_path(&self) -> String {
        format!("{}/cgroup.procs", self.path)
    }
}

impl Drop for ContainerCgroup {
    fn drop(&mut self) {
        // Safe only once the cgroup is empty (the container has been reaped).
        if let Err(error) = std::fs::remove_dir(&self.path) {
            debug!(error = %error, path = %self.path, "cgroup: cleanup rmdir failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{shares_to_weight, CgroupLimits, ContainerCgroup};

    #[test]
    fn main_container_keeps_an_empty_cgroup_for_future_live_updates() {
        let limits = CgroupLimits::default();
        let root = tempfile::tempdir().unwrap();
        let cgroup =
            ContainerCgroup::create_in_ready_hierarchy(root.path().to_str().unwrap(), limits)
                .expect("main container cgroup");
        let path = cgroup.path.clone();
        assert!(std::path::Path::new(&path).is_dir());
        drop(cgroup);
        assert!(!std::path::Path::new(&path).exists());
    }

    #[test]
    fn test_shares_to_weight_mapping() {
        // Endpoints + the cgroup v1 default map to the runc-equivalent weights.
        assert_eq!(shares_to_weight(2), 1);
        assert_eq!(shares_to_weight(262_144), 10_000);
        assert_eq!(shares_to_weight(1024), 39); // runc's mapping for the default
                                                // Out-of-range inputs are clamped, never panic / overflow.
        assert_eq!(shares_to_weight(0), 1);
        assert_eq!(shares_to_weight(u64::MAX), 10_000);
    }
}
