//! Live resource resize for running Box backends.
//!
//! Tier 1 (provisioned vCPU count and memory size) is immutable for a running
//! Box. MicroVMs cannot hot-resize these libkrun settings, and the public Box
//! lifecycle keeps the same stop/recreate contract across backends.
//!
//! Tier 2 (cgroup-based limits): MicroVMs write the guest workload cgroup via
//! the exec channel. Host Sandboxes send one complete resource contract through
//! the exact-generation A3S OCI SDK.

use a3s_box_core::config::{BoxConfig, ResourceLimits};
use a3s_box_core::error::{BoxError, Result};

/// A resource update request.
///
/// Fields set to `None` are left unchanged.
#[derive(Debug, Clone, Default)]
pub struct ResourceUpdate {
    /// vCPU count change (Tier 1 — will be rejected).
    pub vcpus: Option<u32>,
    /// Memory in MiB change (Tier 1 — will be rejected).
    pub memory_mb: Option<u32>,
    /// Cgroup-based limits (Tier 2 — applied by the selected backend).
    pub limits: ResourceLimits,
}

/// Result of a resize attempt.
#[derive(Debug)]
pub struct ResizeResult {
    /// Fields that were successfully applied.
    pub applied: Vec<String>,
    /// Fields that were rejected with reasons.
    pub rejected: Vec<(String, String)>,
}

impl ResourceUpdate {
    /// Check if any Tier 1 (immutable) fields are requested.
    pub fn has_tier1_changes(&self) -> bool {
        self.vcpus.is_some() || self.memory_mb.is_some()
    }

    /// Check if any Tier 2 (cgroup) fields are requested.
    pub fn has_tier2_changes(&self) -> bool {
        self.limits.cpu_shares.is_some()
            || self.limits.cpu_quota.is_some()
            || self.limits.cpu_period.is_some()
            || self.limits.memory_reservation.is_some()
            || self.limits.memory_swap.is_some()
            || self.limits.pids_limit.is_some()
            || self.limits.cpuset_cpus.is_some()
    }

    /// Merge every explicitly requested value into a complete Box config.
    ///
    /// Sandbox live updates use the resulting full snapshot because the A3S
    /// OCI update contract replaces `linux.resources` atomically rather than
    /// applying a sequence of partial cgroup writes.
    pub fn apply_to_config(&self, config: &mut BoxConfig) {
        if let Some(vcpus) = self.vcpus {
            config.resources.vcpus = vcpus;
        }
        if let Some(memory_mb) = self.memory_mb {
            config.resources.memory_mb = memory_mb;
        }
        self.apply_to_limits(&mut config.resource_limits);
    }

    /// Merge every explicitly requested Tier 2 value into existing limits.
    pub fn apply_to_limits(&self, limits: &mut ResourceLimits) {
        if let Some(value) = self.limits.memory_reservation {
            limits.memory_reservation = Some(value);
        }
        if let Some(value) = self.limits.memory_swap {
            limits.memory_swap = Some(value);
        }
        if let Some(value) = self.limits.pids_limit {
            limits.pids_limit = Some(value);
        }
        if let Some(value) = self.limits.cpu_shares {
            limits.cpu_shares = Some(value);
        }
        if let Some(value) = self.limits.cpu_quota {
            limits.cpu_quota = Some(value);
        }
        if let Some(value) = self.limits.cpu_period {
            limits.cpu_period = Some(value);
        }
        if let Some(value) = self.limits.cpuset_cpus.as_ref() {
            limits.cpuset_cpus = Some(value.clone());
        }
    }

    /// Stable names for Tier 2 fields carried by this request.
    pub fn tier2_change_names(&self) -> Vec<&'static str> {
        let mut names = Vec::new();
        if self.limits.memory_reservation.is_some() {
            names.push("memory_reservation");
        }
        if self.limits.memory_swap.is_some() {
            names.push("memory_swap");
        }
        if self.limits.pids_limit.is_some() {
            names.push("pids_limit");
        }
        if self.limits.cpu_shares.is_some() {
            names.push("cpu_shares");
        }
        if self.limits.cpu_quota.is_some() {
            names.push("cpu_quota");
        }
        if self.limits.cpu_period.is_some() {
            names.push("cpu_period");
        }
        if self.limits.cpuset_cpus.is_some() {
            names.push("cpuset_cpus");
        }
        names
    }

    /// Build shell commands to apply Tier 2 cgroup changes inside a MicroVM guest.
    ///
    /// Host Sandbox updates must use the A3S OCI SDK and never call this method.
    /// For a MicroVM, the resize exec runs in the guest root cgroup, so each
    /// command resolves the per-container `box-<pid>-<seq>` slice at runtime.
    /// A bare `/sys/fs/cgroup/<file>` write would hit the root cgroup and silently
    /// leave the container's limits unchanged.
    pub fn build_microvm_cgroup_commands(&self) -> Vec<String> {
        let mut cmds = Vec::new();

        // cpu.max: "$QUOTA $PERIOD" (or "max $PERIOD" for unlimited)
        if self.limits.cpu_quota.is_some() || self.limits.cpu_period.is_some() {
            let quota = self
                .limits
                .cpu_quota
                .map(|q| {
                    if q < 0 {
                        "max".to_string()
                    } else {
                        q.to_string()
                    }
                })
                .unwrap_or_else(|| "max".to_string());
            let period = self.limits.cpu_period.unwrap_or(100_000);
            cmds.push(cgroup_write_cmd("cpu.max", &format!("{quota} {period}")));
        }

        // cpu.weight: 1-10000 (maps from Docker's cpu-shares 2-262144)
        if let Some(shares) = self.limits.cpu_shares {
            // Docker shares (2-262144) → cgroup v2 weight (1-10000), runc's
            // mapping. Clamp shares into range FIRST (so the `* 9999` cannot
            // overflow for absurd inputs near u64::MAX) and clamp the final
            // result to [1, 10000] (the bare `1 + …` can reach 10001). Mirrors
            // the guest `cgroup::shares_to_weight`.
            let shares = shares.clamp(2, 262_144);
            let weight = (1 + ((shares - 2) * 9999) / 262_142).clamp(1, 10_000);
            cmds.push(cgroup_write_cmd("cpu.weight", &weight.to_string()));
        }

        // memory.low (soft limit / reservation)
        if let Some(reservation) = self.limits.memory_reservation {
            cmds.push(cgroup_write_cmd("memory.low", &reservation.to_string()));
        }

        // memory.swap.max
        if let Some(swap) = self.limits.memory_swap {
            let val = if swap < 0 {
                "max".to_string()
            } else {
                swap.to_string()
            };
            cmds.push(cgroup_write_cmd("memory.swap.max", &val));
        }

        // pids.max
        if let Some(pids) = self.limits.pids_limit {
            cmds.push(cgroup_write_cmd("pids.max", &pids.to_string()));
        }

        // cpuset.cpus — only emit a known-good value. `validate_update` already
        // rejects malformed cpusets, but guard here too since the value is
        // interpolated into the resize shell command: a stray quote/`$`/`;`
        // could otherwise break out of `echo '…'` and run arbitrary shell in the
        // guest.
        if let Some(ref cpuset) = self.limits.cpuset_cpus {
            if is_valid_cpuset(cpuset) {
                cmds.push(cgroup_write_cmd("cpuset.cpus", cpuset));
            } else {
                tracing::warn!(cpuset = %cpuset, "Skipping malformed cpuset.cpus value");
            }
        }

        cmds
    }
}

/// Validate a cgroup `cpuset.cpus` value: a comma-separated list of CPU indices
/// and ranges, e.g. `0`, `0,2,4`, `0-3`, `0-1,4-7`. Only ASCII digits, `,` and
/// `-` are allowed, so no shell metacharacter can survive — the kernel rejects
/// anything else anyway. Surrounding whitespace per element is tolerated.
fn is_valid_cpuset(cpuset: &str) -> bool {
    let cpuset = cpuset.trim();
    if cpuset.is_empty() {
        return false;
    }
    cpuset.split(',').all(|element| {
        let element = element.trim();
        match element.split_once('-') {
            Some((lo, hi)) => parse_cpu_index(lo)
                .zip(parse_cpu_index(hi))
                .is_some_and(|(lo, hi)| lo <= hi),
            None => parse_cpu_index(element).is_some(),
        }
    })
}

fn parse_cpu_index(value: &str) -> Option<u32> {
    (!value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| value.parse().ok())
        .flatten()
}

/// Build a `sh` command that writes `value` to cgroup v2 control file `file` in
/// the container's per-container cgroup slice.
///
/// The resize exec runs in the guest root cgroup and this exec channel carries
/// no container id, so the command resolves the slice at runtime: when there is
/// exactly one `box-*` slice (every CLI box and single-container pod) it writes
/// there. Otherwise it FAILS (exit 1) — it must never fall back to writing the
/// bare root cgroup, which either errors (e.g. root has no `cpu.max`) or applies
/// the limit to the whole root hierarchy instead of the container, while the CLI
/// still reported success. A non-zero exit surfaces as a visible warning at the
/// call site (container_update) instead of a silent mis-apply.
fn cgroup_write_cmd(file: &str, value: &str) -> String {
    format!(
        "d=\"\"; n=0; for x in /sys/fs/cgroup/box-*/; do [ -d \"$x\" ] && {{ d=\"$x\"; n=$((n+1)); }}; done; [ \"$n\" = 1 ] || {{ echo \"a3s-resize: cannot resolve a unique per-container cgroup ($n box-* slices) to set {file}\" >&2; exit 1; }}; echo '{value}' > \"${{d}}{file}\""
    )
}

/// Validate a resource update request.
///
/// Returns `Err` if immutable Tier 1 provisioning changes are requested.
/// Returns `Ok(())` if only Tier 2 changes or no changes.
pub fn validate_update(update: &ResourceUpdate) -> Result<()> {
    if let Some(vcpus) = update.vcpus {
        return Err(BoxError::ResizeError(format!(
            "Cannot change provisioned vCPU count to {} on a running Box. Stop and recreate \
             the Box with the desired CPU count.",
            vcpus
        )));
    }
    if let Some(memory_mb) = update.memory_mb {
        return Err(BoxError::ResizeError(format!(
            "Cannot change provisioned memory to {}MB on a running Box. Stop and recreate \
             the Box with the desired memory size.",
            memory_mb
        )));
    }
    validate_update_values(update)
}

/// Validate resource values independently from whether they can be changed on
/// a running Box.
///
/// Callers that persist limits for a stopped Box must still call this function:
/// lifecycle state only controls hot-resize support, not input validity.
pub fn validate_update_values(update: &ResourceUpdate) -> Result<()> {
    // Reject a malformed cpuset before it can be persisted or interpolated into
    // the resize shell command (cgroup `cpuset.cpus` accepts only indices/ranges).
    if let Some(ref cpuset) = update.limits.cpuset_cpus {
        if !is_valid_cpuset(cpuset) {
            return Err(BoxError::ResizeError(format!(
                "Invalid cpuset.cpus value {cpuset:?}: expected a comma-separated list of CPU \
                 indices in the range 0..={} or ascending ranges such as \"0-3\" or \"0,2,4\".",
                u32::MAX
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_update_has_no_changes() {
        let update = ResourceUpdate::default();
        assert!(!update.has_tier1_changes());
        assert!(!update.has_tier2_changes());
        assert!(update.build_microvm_cgroup_commands().is_empty());
        assert!(update.tier2_change_names().is_empty());
    }

    #[test]
    fn update_merges_only_explicit_values_into_a_complete_config() {
        let mut config = BoxConfig::default();
        config.resources.vcpus = 2;
        config.resources.memory_mb = 512;
        config.resource_limits.cpu_quota = Some(20_000);
        config.resource_limits.pids_limit = Some(64);
        let update = ResourceUpdate {
            limits: ResourceLimits {
                cpu_shares: Some(512),
                pids_limit: Some(96),
                ..Default::default()
            },
            ..Default::default()
        };

        update.apply_to_config(&mut config);

        assert_eq!(config.resources.vcpus, 2);
        assert_eq!(config.resources.memory_mb, 512);
        assert_eq!(config.resource_limits.cpu_quota, Some(20_000));
        assert_eq!(config.resource_limits.cpu_shares, Some(512));
        assert_eq!(config.resource_limits.pids_limit, Some(96));
        assert_eq!(update.tier2_change_names(), ["pids_limit", "cpu_shares"]);
    }

    #[test]
    fn test_tier1_vcpus_detected() {
        let update = ResourceUpdate {
            vcpus: Some(4),
            ..Default::default()
        };
        assert!(update.has_tier1_changes());
        assert!(!update.has_tier2_changes());
    }

    #[test]
    fn test_tier1_memory_detected() {
        let update = ResourceUpdate {
            memory_mb: Some(2048),
            ..Default::default()
        };
        assert!(update.has_tier1_changes());
    }

    #[test]
    fn test_validate_rejects_vcpu_change() {
        let update = ResourceUpdate {
            vcpus: Some(8),
            ..Default::default()
        };
        let err = validate_update(&update).unwrap_err();
        assert!(err.to_string().contains("vCPU count"));
        assert!(err.to_string().contains("running Box"));
        assert!(err.to_string().contains("Stop and recreate"));
    }

    #[test]
    fn test_validate_rejects_memory_change() {
        let update = ResourceUpdate {
            memory_mb: Some(4096),
            ..Default::default()
        };
        let err = validate_update(&update).unwrap_err();
        assert!(err.to_string().contains("memory"));
        assert!(err.to_string().contains("running Box"));
        assert!(err.to_string().contains("Stop and recreate"));
    }

    #[test]
    fn test_validate_allows_tier2_only() {
        let update = ResourceUpdate {
            limits: ResourceLimits {
                cpu_shares: Some(512),
                pids_limit: Some(100),
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(validate_update(&update).is_ok());
    }

    #[test]
    fn test_cpu_max_command() {
        let update = ResourceUpdate {
            limits: ResourceLimits {
                cpu_quota: Some(50000),
                cpu_period: Some(100000),
                ..Default::default()
            },
            ..Default::default()
        };
        let cmds = update.build_microvm_cgroup_commands();
        assert_eq!(cmds.len(), 1);
        assert!(cmds[0].contains("50000 100000"));
        assert!(cmds[0].contains("cpu.max"));
    }

    #[test]
    fn test_cpu_max_unlimited_quota() {
        let update = ResourceUpdate {
            limits: ResourceLimits {
                cpu_quota: Some(-1),
                ..Default::default()
            },
            ..Default::default()
        };
        let cmds = update.build_microvm_cgroup_commands();
        assert_eq!(cmds.len(), 1);
        assert!(cmds[0].contains("max 100000"));
    }

    #[test]
    fn test_cpu_weight_conversion() {
        let update = ResourceUpdate {
            limits: ResourceLimits {
                cpu_shares: Some(1024),
                ..Default::default()
            },
            ..Default::default()
        };
        let cmds = update.build_microvm_cgroup_commands();
        assert_eq!(cmds.len(), 1);
        assert!(cmds[0].contains("cpu.weight"));
    }

    #[test]
    fn test_cpu_weight_minimum() {
        let update = ResourceUpdate {
            limits: ResourceLimits {
                cpu_shares: Some(2),
                ..Default::default()
            },
            ..Default::default()
        };
        let cmds = update.build_microvm_cgroup_commands();
        assert!(cmds[0].contains("'1'"));
    }

    #[test]
    fn test_memory_reservation_command() {
        let update = ResourceUpdate {
            limits: ResourceLimits {
                memory_reservation: Some(536870912), // 512MB
                ..Default::default()
            },
            ..Default::default()
        };
        let cmds = update.build_microvm_cgroup_commands();
        assert_eq!(cmds.len(), 1);
        assert!(cmds[0].contains("536870912"));
        assert!(cmds[0].contains("memory.low"));
    }

    #[test]
    fn test_memory_swap_unlimited() {
        let update = ResourceUpdate {
            limits: ResourceLimits {
                memory_swap: Some(-1),
                ..Default::default()
            },
            ..Default::default()
        };
        let cmds = update.build_microvm_cgroup_commands();
        assert!(cmds[0].contains("'max'"));
        assert!(cmds[0].contains("memory.swap.max"));
    }

    #[test]
    fn test_pids_max_command() {
        let update = ResourceUpdate {
            limits: ResourceLimits {
                pids_limit: Some(256),
                ..Default::default()
            },
            ..Default::default()
        };
        let cmds = update.build_microvm_cgroup_commands();
        assert!(cmds[0].contains("256"));
        assert!(cmds[0].contains("pids.max"));
    }

    #[test]
    fn test_cpuset_command() {
        let update = ResourceUpdate {
            limits: ResourceLimits {
                cpuset_cpus: Some("0,1,3".to_string()),
                ..Default::default()
            },
            ..Default::default()
        };
        let cmds = update.build_microvm_cgroup_commands();
        assert!(cmds[0].contains("0,1,3"));
        assert!(cmds[0].contains("cpuset.cpus"));
    }

    #[test]
    fn test_cpuset_valid_forms_accepted() {
        for ok in ["0", "0,1,3", "0-3", "0-1,4-7", " 0 , 2 ", "4294967295"] {
            assert!(is_valid_cpuset(ok), "{ok:?} should be valid");
        }
    }

    #[test]
    fn test_cpuset_injection_rejected() {
        // Shell-injection payloads and other malformed values must be rejected so
        // they never reach `echo '…'` in the resize command.
        for bad in [
            "",
            "0'$(id >>/tmp/pwned)",
            "0; rm -rf /",
            "0`whoami`",
            "0\nmalicious",
            "all",
            "0-",
            "-3",
            "3-1",
            "4294967296",
        ] {
            assert!(!is_valid_cpuset(bad), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn test_validate_rejects_malformed_cpuset() {
        let update = ResourceUpdate {
            limits: ResourceLimits {
                cpuset_cpus: Some("0'$(id)".to_string()),
                ..Default::default()
            },
            ..Default::default()
        };
        let err = validate_update(&update).unwrap_err();
        assert!(err.to_string().contains("cpuset"));
        // And the dangerous value never makes it into a shell command.
        assert!(update.build_microvm_cgroup_commands().is_empty());
    }

    #[test]
    fn test_value_validation_rejects_reversed_cpuset_without_hot_resize() {
        let update = ResourceUpdate {
            limits: ResourceLimits {
                cpuset_cpus: Some("7-3".to_string()),
                ..Default::default()
            },
            ..Default::default()
        };

        let err = validate_update_values(&update).unwrap_err();
        assert!(err.to_string().contains("ascending ranges"));
    }

    #[test]
    fn test_cpu_weight_clamped_for_oversized_shares() {
        // Absurd shares must not overflow the `* 9999` nor exceed cgroup's max
        // weight of 10000.
        let update = ResourceUpdate {
            limits: ResourceLimits {
                cpu_shares: Some(u64::MAX),
                ..Default::default()
            },
            ..Default::default()
        };
        let cmds = update.build_microvm_cgroup_commands();
        assert!(cmds[0].contains("'10000'"), "got {}", cmds[0]);
    }

    #[test]
    fn test_multiple_tier2_commands() {
        let update = ResourceUpdate {
            limits: ResourceLimits {
                cpu_shares: Some(512),
                pids_limit: Some(100),
                memory_reservation: Some(268435456),
                ..Default::default()
            },
            ..Default::default()
        };
        let cmds = update.build_microvm_cgroup_commands();
        assert_eq!(cmds.len(), 3);
    }

    #[test]
    fn test_cgroup_commands_target_per_container_slice() {
        let update = ResourceUpdate {
            limits: ResourceLimits {
                pids_limit: Some(50),
                ..Default::default()
            },
            ..Default::default()
        };
        let cmds = update.build_microvm_cgroup_commands();
        assert_eq!(cmds.len(), 1);
        // Must resolve the per-container `box-*` slice, not write a bare root path.
        assert!(cmds[0].contains("/sys/fs/cgroup/box-*"), "got {}", cmds[0]);
        assert!(cmds[0].contains("pids.max"));
        assert!(cmds[0].contains("'50'"));
    }

    #[test]
    fn test_cgroup_command_fails_instead_of_writing_root() {
        // When the per-container slice can't be uniquely resolved the command
        // must exit non-zero (surfacing a warning at the call site), NOT fall
        // back to writing the bare root cgroup, which mis-applies the limit to
        // the whole hierarchy while the CLI reports success.
        let update = ResourceUpdate {
            limits: ResourceLimits {
                cpu_quota: Some(50_000),
                cpu_period: Some(100_000),
                ..Default::default()
            },
            ..Default::default()
        };
        let cmds = update.build_microvm_cgroup_commands();
        assert_eq!(cmds.len(), 1);
        assert!(cmds[0].contains("exit 1"), "must fail loudly: {}", cmds[0]);
        // The dangerous root-cgroup fallback must be gone: no assignment that
        // points the write target `d` at the bare root.
        assert!(
            !cmds[0].contains("d=\"/sys/fs/cgroup/\""),
            "must not fall back to the root cgroup: {}",
            cmds[0]
        );
    }

    #[test]
    fn test_resize_result_structure() {
        let result = ResizeResult {
            applied: vec!["cpu.weight".to_string()],
            rejected: vec![("vcpus".to_string(), "not supported".to_string())],
        };
        assert_eq!(result.applied.len(), 1);
        assert_eq!(result.rejected.len(), 1);
    }
}
