//! Generated OCI specification for the certified Sandbox backend.

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

use a3s_box_core::config::BoxConfig;
use a3s_box_core::error::{BoxError, Result};
use oci_spec::runtime::{
    Arch, Capabilities, Capability, LinuxBuilder, LinuxCapabilitiesBuilder, LinuxCpuBuilder,
    LinuxDeviceBuilder, LinuxDeviceCgroupBuilder, LinuxDeviceType, LinuxIdMappingBuilder,
    LinuxMemoryBuilder, LinuxNamespaceBuilder, LinuxNamespaceType, LinuxPidsBuilder,
    LinuxResourcesBuilder, LinuxSeccompAction, LinuxSeccompArgBuilder, LinuxSeccompBuilder,
    LinuxSeccompOperator, LinuxSyscallBuilder, Mount, MountBuilder, ProcessBuilder, RootBuilder,
    Spec, SpecBuilder, UserBuilder,
};

use super::capability::{IdMapping, SandboxIdMappingPlan};

/// OCI annotation schema for generated A3S Sandbox bundles.
pub const SANDBOX_BUNDLE_SCHEMA: &str = "a3s.box.sandbox-bundle.v1";
/// Baseline process count enforced even when the caller omits `--pids-limit`.
pub const DEFAULT_SANDBOX_PIDS_LIMIT: i64 = 4096;
const DEFAULT_CPU_PERIOD_US: u64 = 100_000;
const DEFAULT_TMPFS_SIZE: &str = "67108864";
const LINUX_EPERM: u32 = 1;
const LINUX_ENOSYS: u32 = 38;
const LINUX_CLONE_NAMESPACE_MASK: u64 =
    0x0002_0000 | 0x0200_0000 | 0x0400_0000 | 0x0800_0000 | 0x1000_0000 | 0x2000_0000 | 0x4000_0000;

/// A host path deliberately exposed inside the Sandbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxMount {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub read_only: bool,
}

/// A generated tmpfs mount with a bounded byte size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxTmpfs {
    pub destination: PathBuf,
    pub size_bytes: u64,
}

/// Cgroup values compiled from `BoxConfig`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxResources {
    pub memory_limit: i64,
    pub memory_reservation: Option<i64>,
    pub memory_swap: Option<i64>,
    pub cpu_shares: Option<u64>,
    pub cpu_quota: i64,
    pub cpu_period: u64,
    pub cpuset_cpus: Option<String>,
    pub pids_limit: i64,
}

impl SandboxResources {
    /// Convert public resource intent into strict OCI cgroup controls.
    pub fn from_box_config(config: &BoxConfig) -> Result<Self> {
        if config.resources.memory_mb == 0 {
            return Err(BoxError::ConfigError(
                "Sandbox memory limit must be greater than zero".to_string(),
            ));
        }
        if config.resources.vcpus == 0 {
            return Err(BoxError::ConfigError(
                "Sandbox CPU limit must be greater than zero".to_string(),
            ));
        }

        let memory_limit = i64::from(config.resources.memory_mb)
            .checked_mul(1024 * 1024)
            .ok_or_else(|| {
                BoxError::ConfigError("Sandbox memory limit overflows i64".to_string())
            })?;
        let memory_reservation = config
            .resource_limits
            .memory_reservation
            .map(|value| {
                i64::try_from(value).map_err(|_| {
                    BoxError::ConfigError("Sandbox memory reservation overflows i64".to_string())
                })
            })
            .transpose()?;
        if memory_reservation.is_some_and(|reservation| reservation > memory_limit) {
            return Err(BoxError::ConfigError(
                "Sandbox memory reservation cannot exceed the hard memory limit".to_string(),
            ));
        }
        let memory_swap = config.resource_limits.memory_swap;
        if memory_swap.is_some_and(|swap| swap != -1 && swap < memory_limit) {
            return Err(BoxError::ConfigError(
                "Sandbox memory+swap limit cannot be below the hard memory limit".to_string(),
            ));
        }

        let cpu_period = config
            .resource_limits
            .cpu_period
            .unwrap_or(DEFAULT_CPU_PERIOD_US);
        if cpu_period == 0 {
            return Err(BoxError::ConfigError(
                "Sandbox CPU period must be greater than zero".to_string(),
            ));
        }
        let cpu_quota = match config.resource_limits.cpu_quota {
            Some(quota) if quota > 0 => quota,
            Some(_) => {
                return Err(BoxError::ConfigError(
                    "Sandbox CPU quota must be greater than zero".to_string(),
                ))
            }
            None => i64::from(config.resources.vcpus)
                .checked_mul(i64::try_from(cpu_period).map_err(|_| {
                    BoxError::ConfigError("Sandbox CPU period overflows i64".to_string())
                })?)
                .ok_or_else(|| {
                    BoxError::ConfigError("Sandbox CPU quota overflows i64".to_string())
                })?,
        };

        if config
            .resource_limits
            .cpu_shares
            .is_some_and(|shares| !(2..=262_144).contains(&shares))
        {
            return Err(BoxError::ConfigError(
                "Sandbox CPU shares must be between 2 and 262144".to_string(),
            ));
        }
        if let Some(cpuset) = config.resource_limits.cpuset_cpus.as_deref() {
            validate_cpuset(cpuset)?;
        }

        let pids_limit_u64 = config
            .resource_limits
            .pids_limit
            .unwrap_or(DEFAULT_SANDBOX_PIDS_LIMIT as u64);
        let pids_limit = i64::try_from(pids_limit_u64)
            .map_err(|_| BoxError::ConfigError("Sandbox PID limit overflows i64".to_string()))?;
        if pids_limit <= 0 {
            return Err(BoxError::ConfigError(
                "Sandbox PID limit must be greater than zero".to_string(),
            ));
        }

        Ok(Self {
            memory_limit,
            memory_reservation,
            memory_swap,
            cpu_shares: config.resource_limits.cpu_shares,
            cpu_quota,
            cpu_period,
            cpuset_cpus: config.resource_limits.cpuset_cpus.clone(),
            pids_limit,
        })
    }
}

/// Backend-neutral inputs already validated and resolved by the runtime.
#[derive(Debug, Clone)]
pub struct SandboxBundleSpec {
    pub box_id: String,
    pub rootfs_path: PathBuf,
    pub rootfs_read_only: bool,
    pub hostname: String,
    pub init_environment: Vec<(String, String)>,
    pub mounts: Vec<SandboxMount>,
    pub tmpfs: Vec<SandboxTmpfs>,
    pub id_mappings: SandboxIdMappingPlan,
    pub resources: SandboxResources,
    pub requested_capabilities: Vec<String>,
    pub execution_plan_digest: String,
    pub runtime_digest: String,
}

/// Compile a complete OCI config. Arbitrary caller-provided OCI JSON is never
/// accepted by this backend.
pub fn compile_oci_spec(input: &SandboxBundleSpec) -> Result<Spec> {
    validate_box_id(&input.box_id)?;
    validate_rootfs_path(&input.rootfs_path)?;
    validate_hostname(&input.hostname)?;
    validate_digest("execution plan", &input.execution_plan_digest)?;
    validate_digest("runtime", &input.runtime_digest)?;
    validate_id_mapping_plan(&input.id_mappings)?;

    let process = ProcessBuilder::default()
        .terminal(false)
        .user(
            UserBuilder::default()
                .uid(0u32)
                .gid(0u32)
                .build()
                .map_err(oci_error)?,
        )
        .args(vec!["/sbin/init".to_string()])
        .env(compile_environment(&input.init_environment)?)
        .cwd(PathBuf::from("/"))
        .capabilities(compile_capabilities(&input.requested_capabilities)?)
        .no_new_privileges(true)
        .build()
        .map_err(oci_error)?;

    let linux = LinuxBuilder::default()
        .uid_mappings(compile_id_mappings(&input.id_mappings.uid_mappings)?)
        .gid_mappings(compile_id_mappings(&input.id_mappings.gid_mappings)?)
        .namespaces(compile_namespaces()?)
        .resources(compile_resources(&input.resources)?)
        .cgroups_path(PathBuf::from(format!("a3s-box/{}", input.box_id)))
        .devices(compile_devices()?)
        .seccomp(compile_seccomp()?)
        .rootfs_propagation("private".to_string())
        .masked_paths(masked_paths())
        .readonly_paths(readonly_paths())
        .build()
        .map_err(oci_error)?;

    let mut annotations = HashMap::new();
    annotations.insert(
        "com.a3s.box.sandbox.schema".to_string(),
        SANDBOX_BUNDLE_SCHEMA.to_string(),
    );
    annotations.insert(
        "com.a3s.box.execution-plan.digest".to_string(),
        input.execution_plan_digest.clone(),
    );
    annotations.insert(
        "com.a3s.box.runtime.digest".to_string(),
        input.runtime_digest.clone(),
    );
    annotations.insert(
        "com.a3s.box.isolation-class".to_string(),
        "shared-kernel".to_string(),
    );

    SpecBuilder::default()
        .version("1.1.0".to_string())
        .root(
            RootBuilder::default()
                .path(input.rootfs_path.clone())
                .readonly(input.rootfs_read_only)
                .build()
                .map_err(oci_error)?,
        )
        .mounts(compile_mounts(&input.mounts, &input.tmpfs)?)
        .process(process)
        .hostname(input.hostname.clone())
        .annotations(annotations)
        .linux(linux)
        .build()
        .map_err(oci_error)
}

fn compile_environment(environment: &[(String, String)]) -> Result<Vec<String>> {
    let mut values = std::collections::BTreeMap::new();
    for (key, value) in environment {
        if key.is_empty() || key.contains(['=', '\0']) || value.contains('\0') {
            return Err(BoxError::ConfigError(format!(
                "Invalid Sandbox process environment key {key:?}"
            )));
        }
        values.insert(key.clone(), value.clone());
    }
    values.entry("PATH".to_string()).or_insert_with(|| {
        "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string()
    });
    values.insert("A3S_BOOTSTRAP_MODE".to_string(), "host-sandbox".to_string());
    values.insert("A3S_EXEC_LISTENER_FD".to_string(), "3".to_string());
    values.insert("A3S_PTY_LISTENER_FD".to_string(), "4".to_string());
    values.insert("A3S_INIT_LOG_FD".to_string(), "5".to_string());

    Ok(values
        .into_iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect())
}

fn compile_capabilities(requested: &[String]) -> Result<oci_spec::runtime::LinuxCapabilities> {
    // PID 1 needs these only for metadata replay, loopback setup, and dropping
    // to the configured workload user. guest-init narrows the child process to
    // the user-requested set before exec.
    let mut bounding: Capabilities = [
        Capability::Chown,
        Capability::DacOverride,
        Capability::Fowner,
        Capability::Fsetid,
        Capability::Kill,
        Capability::NetAdmin,
        Capability::NetBindService,
        Capability::Setgid,
        Capability::Setpcap,
        Capability::Setuid,
        Capability::SysChroot,
    ]
    .into_iter()
    .collect();

    for capability in requested {
        bounding.insert(parse_allowed_capability(capability)?);
    }

    LinuxCapabilitiesBuilder::default()
        .bounding(bounding.clone())
        .effective(bounding.clone())
        .permitted(bounding)
        .inheritable(HashSet::<Capability>::new())
        .ambient(HashSet::<Capability>::new())
        .build()
        .map_err(oci_error)
}

fn parse_allowed_capability(value: &str) -> Result<Capability> {
    let normalized = value
        .trim()
        .to_ascii_uppercase()
        .strip_prefix("CAP_")
        .map(ToString::to_string)
        .unwrap_or_else(|| value.trim().to_ascii_uppercase());
    let capability = match normalized.as_str() {
        "AUDIT_WRITE" => Capability::AuditWrite,
        "CHOWN" => Capability::Chown,
        "DAC_OVERRIDE" => Capability::DacOverride,
        "FOWNER" => Capability::Fowner,
        "FSETID" => Capability::Fsetid,
        "KILL" => Capability::Kill,
        "MKNOD" => Capability::Mknod,
        "NET_BIND_SERVICE" => Capability::NetBindService,
        "SETFCAP" => Capability::Setfcap,
        "SETGID" => Capability::Setgid,
        "SETPCAP" => Capability::Setpcap,
        "SETUID" => Capability::Setuid,
        "SYS_CHROOT" => Capability::SysChroot,
        _ => {
            return Err(BoxError::ConfigError(format!(
                "Sandbox capability {value:?} is outside the allowlist"
            )))
        }
    };
    Ok(capability)
}

fn compile_id_mappings(mappings: &[IdMapping]) -> Result<Vec<oci_spec::runtime::LinuxIdMapping>> {
    mappings
        .iter()
        .map(|mapping| {
            LinuxIdMappingBuilder::default()
                .container_id(mapping.container_id)
                .host_id(mapping.host_id)
                .size(mapping.size)
                .build()
                .map_err(oci_error)
        })
        .collect()
}

fn compile_namespaces() -> Result<Vec<oci_spec::runtime::LinuxNamespace>> {
    [
        LinuxNamespaceType::User,
        LinuxNamespaceType::Mount,
        LinuxNamespaceType::Pid,
        LinuxNamespaceType::Ipc,
        LinuxNamespaceType::Uts,
        LinuxNamespaceType::Network,
        LinuxNamespaceType::Cgroup,
    ]
    .into_iter()
    .map(|typ| {
        LinuxNamespaceBuilder::default()
            .typ(typ)
            .build()
            .map_err(oci_error)
    })
    .collect()
}

fn compile_resources(resources: &SandboxResources) -> Result<oci_spec::runtime::LinuxResources> {
    let mut memory = LinuxMemoryBuilder::default().limit(resources.memory_limit);
    if let Some(reservation) = resources.memory_reservation {
        memory = memory.reservation(reservation);
    }
    if let Some(swap) = resources.memory_swap {
        memory = memory.swap(swap);
    }

    let mut cpu = LinuxCpuBuilder::default()
        .quota(resources.cpu_quota)
        .period(resources.cpu_period);
    if let Some(shares) = resources.cpu_shares {
        cpu = cpu.shares(shares);
    }
    if let Some(cpuset) = resources.cpuset_cpus.as_ref() {
        cpu = cpu.cpus(cpuset.clone());
    }

    let mut device_rules = vec![LinuxDeviceCgroupBuilder::default()
        .allow(false)
        .access("rwm".to_string())
        .build()
        .map_err(oci_error)?];
    for device in minimal_device_numbers() {
        device_rules.push(
            LinuxDeviceCgroupBuilder::default()
                .allow(true)
                .typ(LinuxDeviceType::C)
                .major(device.1)
                .minor(device.2)
                .access("rwm".to_string())
                .build()
                .map_err(oci_error)?,
        );
    }

    LinuxResourcesBuilder::default()
        .devices(device_rules)
        .memory(memory.build().map_err(oci_error)?)
        .cpu(cpu.build().map_err(oci_error)?)
        .pids(
            LinuxPidsBuilder::default()
                .limit(resources.pids_limit)
                .build()
                .map_err(oci_error)?,
        )
        .build()
        .map_err(oci_error)
}

fn compile_devices() -> Result<Vec<oci_spec::runtime::LinuxDevice>> {
    minimal_device_numbers()
        .iter()
        .map(|(path, major, minor)| {
            LinuxDeviceBuilder::default()
                .path(PathBuf::from(path))
                .typ(LinuxDeviceType::C)
                .major(*major)
                .minor(*minor)
                .file_mode(0o666u32)
                .uid(0u32)
                .gid(0u32)
                .build()
                .map_err(oci_error)
        })
        .collect()
}

fn minimal_device_numbers() -> &'static [(&'static str, i64, i64)] {
    &[
        ("/dev/null", 1, 3),
        ("/dev/zero", 1, 5),
        ("/dev/full", 1, 7),
        ("/dev/random", 1, 8),
        ("/dev/urandom", 1, 9),
        ("/dev/tty", 5, 0),
    ]
}

fn compile_mounts(user_mounts: &[SandboxMount], user_tmpfs: &[SandboxTmpfs]) -> Result<Vec<Mount>> {
    let mut mounts = vec![
        mount("/proc", "proc", "proc", &["nosuid", "noexec", "nodev"])?,
        mount(
            "/dev",
            "tmpfs",
            "tmpfs",
            &[
                "nosuid",
                "strictatime",
                "mode=755",
                &format!("size={DEFAULT_TMPFS_SIZE}"),
            ],
        )?,
        mount(
            "/dev/pts",
            "devpts",
            "devpts",
            &[
                "nosuid",
                "noexec",
                "newinstance",
                "ptmxmode=0666",
                "mode=0620",
                "gid=5",
            ],
        )?,
        mount(
            "/dev/shm",
            "tmpfs",
            "shm",
            &[
                "nosuid",
                "noexec",
                "nodev",
                "mode=1777",
                &format!("size={DEFAULT_TMPFS_SIZE}"),
            ],
        )?,
        mount(
            "/dev/mqueue",
            "mqueue",
            "mqueue",
            &["nosuid", "noexec", "nodev"],
        )?,
        mount(
            "/sys",
            "sysfs",
            "sysfs",
            &["nosuid", "noexec", "nodev", "ro"],
        )?,
        mount(
            "/sys/fs/cgroup",
            "cgroup",
            "cgroup",
            &["nosuid", "noexec", "nodev", "relatime", "ro"],
        )?,
        mount(
            "/tmp",
            "tmpfs",
            "tmpfs",
            &[
                "nosuid",
                "nodev",
                "mode=1777",
                &format!("size={DEFAULT_TMPFS_SIZE}"),
            ],
        )?,
        mount(
            "/run",
            "tmpfs",
            "tmpfs",
            &[
                "nosuid",
                "nodev",
                "mode=755",
                &format!("size={DEFAULT_TMPFS_SIZE}"),
            ],
        )?,
    ];

    let mut destinations: HashSet<PathBuf> = mounts
        .iter()
        .map(|entry| entry.destination().clone())
        .collect();
    for user_mount in user_mounts {
        validate_user_mount(user_mount)?;
        if !destinations.insert(user_mount.destination.clone()) {
            return Err(BoxError::ConfigError(format!(
                "Duplicate Sandbox mount destination {}",
                user_mount.destination.display()
            )));
        }
        let mut options = vec![
            "rbind".to_string(),
            "rprivate".to_string(),
            "nosuid".to_string(),
            "nodev".to_string(),
        ];
        options.push(if user_mount.read_only { "ro" } else { "rw" }.to_string());
        mounts.push(
            MountBuilder::default()
                .destination(user_mount.destination.clone())
                .typ("bind".to_string())
                .source(user_mount.source.clone())
                .options(options)
                .build()
                .map_err(oci_error)?,
        );
    }

    for tmpfs in user_tmpfs {
        validate_absolute_normalized(&tmpfs.destination, "tmpfs destination")?;
        if tmpfs.size_bytes == 0 {
            return Err(BoxError::ConfigError(format!(
                "Sandbox tmpfs {} must have a non-zero size",
                tmpfs.destination.display()
            )));
        }
        if path_is_or_below(&tmpfs.destination, Path::new("/proc"))
            || path_is_or_below(&tmpfs.destination, Path::new("/sys"))
            || path_is_or_below(&tmpfs.destination, Path::new("/dev"))
            || path_is_or_below(&tmpfs.destination, Path::new("/run/a3s-box"))
            || tmpfs.destination == Path::new("/")
        {
            return Err(BoxError::ConfigError(format!(
                "Sandbox tmpfs destination {} is protected",
                tmpfs.destination.display()
            )));
        }
        // A user size for /tmp or /run replaces the generated default rather
        // than creating two mounts at the same destination.
        if let Some(index) = mounts
            .iter()
            .position(|mount| mount.destination() == &tmpfs.destination)
        {
            if !matches!(tmpfs.destination.to_str(), Some("/tmp" | "/run")) {
                return Err(BoxError::ConfigError(format!(
                    "Duplicate Sandbox mount destination {}",
                    tmpfs.destination.display()
                )));
            }
            mounts.remove(index);
            destinations.remove(&tmpfs.destination);
        }
        if !destinations.insert(tmpfs.destination.clone()) {
            return Err(BoxError::ConfigError(format!(
                "Duplicate Sandbox mount destination {}",
                tmpfs.destination.display()
            )));
        }
        mounts.push(
            MountBuilder::default()
                .destination(tmpfs.destination.clone())
                .typ("tmpfs".to_string())
                .source(PathBuf::from("tmpfs"))
                .options(vec![
                    "nosuid".to_string(),
                    "nodev".to_string(),
                    "mode=1777".to_string(),
                    format!("size={}", tmpfs.size_bytes),
                ])
                .build()
                .map_err(oci_error)?,
        );
    }

    Ok(mounts)
}

fn mount(destination: &str, typ: &str, source: &str, options: &[&str]) -> Result<Mount> {
    MountBuilder::default()
        .destination(PathBuf::from(destination))
        .typ(typ.to_string())
        .source(PathBuf::from(source))
        .options(
            options
                .iter()
                .map(|value| (*value).to_string())
                .collect::<Vec<_>>(),
        )
        .build()
        .map_err(oci_error)
}

fn compile_seccomp() -> Result<oci_spec::runtime::LinuxSeccomp> {
    let allowed = LinuxSyscallBuilder::default()
        .names(
            ALLOWED_SYSCALLS
                .iter()
                .map(|name| (*name).to_string())
                .collect::<Vec<_>>(),
        )
        .action(LinuxSeccompAction::ScmpActAllow)
        .build()
        .map_err(oci_error)?;

    // clone is needed for threads/processes, but namespace creation stays
    // denied. clone3 deliberately returns ENOSYS so libc falls back to clone.
    let clone = LinuxSyscallBuilder::default()
        .names(vec!["clone".to_string()])
        .action(LinuxSeccompAction::ScmpActAllow)
        .args(vec![LinuxSeccompArgBuilder::default()
            .index(0usize)
            .value(0u64)
            .value_two(LINUX_CLONE_NAMESPACE_MASK)
            .op(LinuxSeccompOperator::ScmpCmpMaskedEq)
            .build()
            .map_err(oci_error)?])
        .build()
        .map_err(oci_error)?;
    let clone3 = LinuxSyscallBuilder::default()
        .names(vec!["clone3".to_string()])
        .action(LinuxSeccompAction::ScmpActErrno)
        .errno_ret(LINUX_ENOSYS)
        .build()
        .map_err(oci_error)?;

    LinuxSeccompBuilder::default()
        .default_action(LinuxSeccompAction::ScmpActErrno)
        .default_errno_ret(LINUX_EPERM)
        .architectures(vec![certified_seccomp_architecture()?])
        .syscalls(vec![allowed, clone, clone3])
        .build()
        .map_err(oci_error)
}

fn certified_seccomp_architecture() -> Result<Arch> {
    match std::env::consts::ARCH {
        "x86_64" => Ok(Arch::ScmpArchX86_64),
        "aarch64" => Ok(Arch::ScmpArchAarch64),
        architecture => Err(BoxError::ConfigError(format!(
            "Sandbox seccomp is not certified for architecture {architecture}"
        ))),
    }
}

fn validate_id_mapping_plan(plan: &SandboxIdMappingPlan) -> Result<()> {
    validate_mapping_set(&plan.uid_mappings, plan.maximum_container_uid, "UID")?;
    validate_mapping_set(&plan.gid_mappings, plan.maximum_container_gid, "GID")?;
    if plan
        .uid_mappings
        .iter()
        .any(|mapping| mapping.container_id == 0 && mapping.host_id == 0)
        || plan
            .gid_mappings
            .iter()
            .any(|mapping| mapping.container_id == 0 && mapping.host_id == 0)
    {
        return Err(BoxError::ConfigError(
            "Sandbox container root must not map to host root".to_string(),
        ));
    }
    Ok(())
}

fn validate_mapping_set(mappings: &[IdMapping], maximum: u32, kind: &str) -> Result<()> {
    if mappings.is_empty() || mappings[0].container_id != 0 {
        return Err(BoxError::ConfigError(format!(
            "Sandbox {kind} mappings must start at container ID 0"
        )));
    }
    let mut next = 0u32;
    let mut host_ranges = Vec::new();
    for mapping in mappings {
        if mapping.size == 0 || mapping.container_id != next {
            return Err(BoxError::ConfigError(format!(
                "Sandbox {kind} mappings must be contiguous and non-empty"
            )));
        }
        next = next.checked_add(mapping.size).ok_or_else(|| {
            BoxError::ConfigError(format!("Sandbox {kind} container mapping overflows"))
        })?;
        let host_end = mapping.host_id.checked_add(mapping.size).ok_or_else(|| {
            BoxError::ConfigError(format!("Sandbox {kind} host mapping overflows"))
        })?;
        if host_ranges
            .iter()
            .any(|(start, end)| mapping.host_id < *end && *start < host_end)
        {
            return Err(BoxError::ConfigError(format!(
                "Sandbox {kind} host mappings overlap"
            )));
        }
        host_ranges.push((mapping.host_id, host_end));
    }
    if next <= maximum {
        return Err(BoxError::ConfigError(format!(
            "Sandbox {kind} mappings do not cover container ID {maximum}"
        )));
    }
    Ok(())
}

fn validate_user_mount(mount: &SandboxMount) -> Result<()> {
    validate_absolute_normalized(&mount.source, "mount source")?;
    validate_absolute_normalized(&mount.destination, "mount destination")?;

    const PROTECTED_SOURCES: &[&str] = &[
        "/", "/boot", "/dev", "/etc", "/proc", "/run", "/sys", "/var/run",
    ];
    if PROTECTED_SOURCES
        .iter()
        .any(|protected| path_is_or_below(&mount.source, Path::new(protected)))
    {
        return Err(BoxError::ConfigError(format!(
            "Sandbox mount source {} is protected",
            mount.source.display()
        )));
    }

    const PROTECTED_DESTINATIONS: &[&str] = &[
        "/dev",
        "/proc",
        "/run/a3s-box",
        "/sbin/init",
        "/sys",
        "/.a3s-box-env",
        "/.a3s_image_metadata_v1.json",
        "/.a3s_rootfs_metadata_v1.json",
    ];
    if mount.destination == Path::new("/")
        || PROTECTED_DESTINATIONS.iter().any(|protected| {
            let protected = Path::new(protected);
            path_is_or_below(&mount.destination, protected)
                || (mount.destination != Path::new("/")
                    && protected.starts_with(&mount.destination))
        })
    {
        return Err(BoxError::ConfigError(format!(
            "Sandbox mount destination {} is protected",
            mount.destination.display()
        )));
    }
    Ok(())
}

fn validate_rootfs_path(path: &Path) -> Result<()> {
    validate_absolute_normalized(path, "rootfs path")?;
    if path == Path::new("/") {
        return Err(BoxError::ConfigError(
            "Host root cannot be used as a Sandbox rootfs".to_string(),
        ));
    }
    Ok(())
}

fn validate_absolute_normalized(path: &Path, label: &str) -> Result<()> {
    if !path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::CurDir | Component::ParentDir | Component::Prefix(_)
            )
        })
    {
        return Err(BoxError::ConfigError(format!(
            "Sandbox {label} must be an absolute normalized path: {}",
            path.display()
        )));
    }
    Ok(())
}

fn path_is_or_below(path: &Path, protected: &Path) -> bool {
    path == protected || (protected != Path::new("/") && path.starts_with(protected))
}

fn validate_box_id(box_id: &str) -> Result<()> {
    if box_id.is_empty()
        || box_id.len() > 128
        || !box_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(BoxError::ConfigError(format!(
            "Invalid Sandbox box ID {box_id:?}"
        )));
    }
    Ok(())
}

fn validate_hostname(hostname: &str) -> Result<()> {
    if hostname.is_empty()
        || hostname.len() > 253
        || hostname.split('.').any(|label| {
            label.is_empty()
                || label.len() > 63
                || label.starts_with('-')
                || label.ends_with('-')
                || !label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
    {
        return Err(BoxError::ConfigError(format!(
            "Invalid Sandbox hostname {hostname:?}"
        )));
    }
    Ok(())
}

fn validate_digest(label: &str, digest: &str) -> Result<()> {
    let Some(hex) = digest.strip_prefix("sha256:") else {
        return Err(BoxError::ConfigError(format!(
            "Sandbox {label} digest must use sha256"
        )));
    };
    if hex.len() != 64 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(BoxError::ConfigError(format!(
            "Invalid Sandbox {label} digest"
        )));
    }
    Ok(())
}

fn validate_cpuset(value: &str) -> Result<()> {
    let first = value.as_bytes().first().copied();
    let last = value.as_bytes().last().copied();
    if value.is_empty()
        || matches!(first, Some(b',' | b'-'))
        || matches!(last, Some(b',' | b'-'))
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b',' | b'-'))
    {
        return Err(BoxError::ConfigError(format!(
            "Invalid Sandbox cpuset {value:?}"
        )));
    }
    Ok(())
}

fn masked_paths() -> Vec<String> {
    [
        "/proc/acpi",
        "/proc/asound",
        "/proc/kcore",
        "/proc/keys",
        "/proc/latency_stats",
        "/proc/sched_debug",
        "/proc/scsi",
        "/proc/timer_list",
        "/proc/timer_stats",
        "/sys/devices/virtual/powercap",
        "/sys/firmware",
    ]
    .into_iter()
    .map(ToString::to_string)
    .collect()
}

fn readonly_paths() -> Vec<String> {
    [
        "/proc/bus",
        "/proc/fs",
        "/proc/irq",
        "/proc/sys",
        "/proc/sysrq-trigger",
    ]
    .into_iter()
    .map(ToString::to_string)
    .collect()
}

fn oci_error(error: impl std::fmt::Display) -> BoxError {
    BoxError::ConfigError(format!("Failed to compile Sandbox OCI spec: {error}"))
}

// Default-deny profile for guest-init and general code execution. Namespace,
// mount, kernel-module, BPF, keyring, perf, io_uring, userfaultfd, reboot, and
// host-control syscalls are intentionally absent.
const ALLOWED_SYSCALLS: &[&str] = &[
    "accept",
    "accept4",
    "access",
    "arch_prctl",
    "bind",
    "brk",
    "capget",
    "capset",
    "chdir",
    "chmod",
    "chown",
    "clock_getres",
    "clock_gettime",
    "clock_nanosleep",
    "close",
    "close_range",
    "connect",
    "copy_file_range",
    "creat",
    "dup",
    "dup2",
    "dup3",
    "epoll_create",
    "epoll_create1",
    "epoll_ctl",
    "epoll_pwait",
    "epoll_pwait2",
    "epoll_wait",
    "eventfd",
    "eventfd2",
    "execve",
    "execveat",
    "exit",
    "exit_group",
    "faccessat",
    "faccessat2",
    "fadvise64",
    "fallocate",
    "fchdir",
    "fchmod",
    "fchmodat",
    "fchown",
    "fchownat",
    "fcntl",
    "fdatasync",
    "fgetxattr",
    "flistxattr",
    "flock",
    "fork",
    "fremovexattr",
    "fsetxattr",
    "fstat",
    "fstatfs",
    "fsync",
    "ftruncate",
    "futex",
    "futex_waitv",
    "getcwd",
    "getdents",
    "getdents64",
    "getegid",
    "geteuid",
    "getgid",
    "getgroups",
    "getitimer",
    "getpeername",
    "getpgid",
    "getpgrp",
    "getpid",
    "getppid",
    "getpriority",
    "getrandom",
    "getresgid",
    "getresuid",
    "getrlimit",
    "get_robust_list",
    "getrusage",
    "getsid",
    "getsockname",
    "getsockopt",
    "gettid",
    "gettimeofday",
    "getuid",
    "getxattr",
    "inotify_add_watch",
    "inotify_init",
    "inotify_init1",
    "inotify_rm_watch",
    "ioctl",
    "ioprio_get",
    "ioprio_set",
    "kill",
    "lchown",
    "lgetxattr",
    "link",
    "linkat",
    "listen",
    "listxattr",
    "llistxattr",
    "lremovexattr",
    "lseek",
    "lsetxattr",
    "lstat",
    "madvise",
    "membarrier",
    "memfd_create",
    "mincore",
    "mkdir",
    "mkdirat",
    "mlock",
    "mlock2",
    "mlockall",
    "mmap",
    "mprotect",
    "mremap",
    "msync",
    "munlock",
    "munlockall",
    "munmap",
    "nanosleep",
    "newfstatat",
    "open",
    "openat",
    "openat2",
    "pause",
    "pidfd_open",
    "pidfd_send_signal",
    "pipe",
    "pipe2",
    "poll",
    "ppoll",
    "prctl",
    "pread64",
    "preadv",
    "preadv2",
    "prlimit64",
    "process_madvise",
    "process_vm_readv",
    "process_vm_writev",
    "pselect6",
    "pwrite64",
    "pwritev",
    "pwritev2",
    "read",
    "readahead",
    "readlink",
    "readlinkat",
    "readv",
    "recvfrom",
    "recvmmsg",
    "recvmsg",
    "rename",
    "renameat",
    "renameat2",
    "restart_syscall",
    "rseq",
    "rt_sigaction",
    "rt_sigpending",
    "rt_sigprocmask",
    "rt_sigqueueinfo",
    "rt_sigreturn",
    "rt_sigsuspend",
    "rt_sigtimedwait",
    "rt_tgsigqueueinfo",
    "sched_getaffinity",
    "sched_getattr",
    "sched_getparam",
    "sched_getscheduler",
    "sched_get_priority_max",
    "sched_get_priority_min",
    "sched_setaffinity",
    "sched_setattr",
    "sched_setparam",
    "sched_setscheduler",
    "sched_yield",
    "seccomp",
    "select",
    "semctl",
    "semget",
    "semop",
    "semtimedop",
    "sendfile",
    "sendmmsg",
    "sendmsg",
    "sendto",
    "set_robust_list",
    "set_tid_address",
    "setfsgid",
    "setfsuid",
    "setgid",
    "setgroups",
    "setitimer",
    "setpgid",
    "setpriority",
    "setregid",
    "setresgid",
    "setresuid",
    "setreuid",
    "setrlimit",
    "setsid",
    "setsockopt",
    "setuid",
    "shutdown",
    "sigaltstack",
    "signalfd",
    "signalfd4",
    "socket",
    "socketpair",
    "splice",
    "stat",
    "statfs",
    "statx",
    "symlink",
    "symlinkat",
    "sync",
    "sync_file_range",
    "syncfs",
    "sysinfo",
    "tee",
    "tgkill",
    "time",
    "timer_create",
    "timer_delete",
    "timer_getoverrun",
    "timer_gettime",
    "timer_settime",
    "timerfd_create",
    "timerfd_gettime",
    "timerfd_settime",
    "times",
    "tkill",
    "truncate",
    "umask",
    "uname",
    "unlink",
    "unlinkat",
    "utime",
    "utimensat",
    "utimes",
    "vfork",
    "vmsplice",
    "wait4",
    "waitid",
    "write",
    "writev",
];

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn sample_input() -> SandboxBundleSpec {
        SandboxBundleSpec {
            box_id: "box-123".to_string(),
            rootfs_path: PathBuf::from("/var/lib/a3s/boxes/box-123/rootfs"),
            rootfs_read_only: false,
            hostname: "box-123".to_string(),
            init_environment: vec![
                ("PATH".to_string(), "/bin".to_string()),
                (
                    "A3S_BOOTSTRAP_MODE".to_string(),
                    "attacker-value".to_string(),
                ),
            ],
            mounts: vec![SandboxMount {
                source: PathBuf::from("/srv/a3s/workspaces/box-123"),
                destination: PathBuf::from("/workspace"),
                read_only: false,
            }],
            tmpfs: Vec::new(),
            id_mappings: SandboxIdMappingPlan {
                uid_mappings: vec![IdMapping {
                    container_id: 0,
                    host_id: 100000,
                    size: 65536,
                }],
                gid_mappings: vec![IdMapping {
                    container_id: 0,
                    host_id: 200000,
                    size: 65536,
                }],
                maximum_container_uid: 65535,
                maximum_container_gid: 65535,
            },
            resources: SandboxResources {
                memory_limit: 512 * 1024 * 1024,
                memory_reservation: Some(256 * 1024 * 1024),
                memory_swap: Some(1024 * 1024 * 1024),
                cpu_shares: Some(1024),
                cpu_quota: 200000,
                cpu_period: 100000,
                cpuset_cpus: Some("0-1".to_string()),
                pids_limit: 512,
            },
            requested_capabilities: Vec::new(),
            execution_plan_digest: format!("sha256:{}", "a".repeat(64)),
            runtime_digest: format!("sha256:{}", "b".repeat(64)),
        }
    }

    fn as_json(spec: &Spec) -> Value {
        serde_json::to_value(spec).unwrap()
    }

    #[test]
    fn compiler_emits_every_mandatory_isolation_control() {
        let value = as_json(&compile_oci_spec(&sample_input()).unwrap());
        let namespaces: HashSet<_> = value["linux"]["namespaces"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["type"].as_str().unwrap())
            .collect();
        for required in ["user", "mount", "pid", "ipc", "uts", "network", "cgroup"] {
            assert!(namespaces.contains(required), "missing {required}");
        }
        assert_eq!(value["process"]["args"], serde_json::json!(["/sbin/init"]));
        assert_eq!(value["process"]["noNewPrivileges"], true);
        assert_eq!(value["linux"]["seccomp"]["defaultAction"], "SCMP_ACT_ERRNO");
        let expected_seccomp_architecture = match std::env::consts::ARCH {
            "x86_64" => "SCMP_ARCH_X86_64",
            "aarch64" => "SCMP_ARCH_AARCH64",
            architecture => panic!("unexpected test architecture {architecture}"),
        };
        assert_eq!(
            value["linux"]["seccomp"]["architectures"],
            serde_json::json!([expected_seccomp_architecture])
        );
        assert_eq!(
            value["linux"]["resources"]["memory"]["limit"],
            512 * 1024 * 1024i64
        );
        assert_eq!(value["linux"]["resources"]["pids"]["limit"], 512);
        assert_eq!(value["linux"]["resources"]["cpu"]["cpus"], "0-1");
    }

    #[test]
    fn compiler_seals_bootstrap_environment_and_capabilities() {
        let value = as_json(&compile_oci_spec(&sample_input()).unwrap());
        let env = value["process"]["env"].as_array().unwrap();
        assert!(env
            .iter()
            .any(|value| value == "A3S_BOOTSTRAP_MODE=host-sandbox"));
        assert!(env.iter().any(|value| value == "A3S_EXEC_LISTENER_FD=3"));
        assert!(env.iter().any(|value| value == "A3S_PTY_LISTENER_FD=4"));
        assert!(env.iter().any(|value| value == "A3S_INIT_LOG_FD=5"));
        assert!(!env
            .iter()
            .any(|value| value == "A3S_BOOTSTRAP_MODE=attacker-value"));

        let bounding = value["process"]["capabilities"]["bounding"]
            .as_array()
            .unwrap();
        assert!(!bounding.iter().any(|value| value == "CAP_SYS_ADMIN"));
        assert!(!bounding.iter().any(|value| value == "CAP_NET_RAW"));
    }

    #[test]
    fn seccomp_masks_clone_namespace_flags_and_returns_enosys_for_clone3() {
        let value = as_json(&compile_oci_spec(&sample_input()).unwrap());
        let rules = value["linux"]["seccomp"]["syscalls"].as_array().unwrap();
        let clone = rules
            .iter()
            .find(|rule| {
                rule["names"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|name| name == "clone")
            })
            .unwrap();
        assert_eq!(clone["args"][0]["op"], "SCMP_CMP_MASKED_EQ");
        let clone3 = rules
            .iter()
            .find(|rule| {
                rule["names"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|name| name == "clone3")
            })
            .unwrap();
        assert_eq!(clone3["errnoRet"], LINUX_ENOSYS);
        let allowed_names = rules[0]["names"].as_array().unwrap();
        for forbidden in [
            "unshare",
            "setns",
            "mount",
            "pivot_root",
            "bpf",
            "keyctl",
            "perf_event_open",
            "io_uring_setup",
            "userfaultfd",
            "reboot",
        ] {
            assert!(!allowed_names.iter().any(|name| name == forbidden));
        }
    }

    #[test]
    fn compiler_rejects_protected_or_duplicate_mounts() {
        let mut input = sample_input();
        input.mounts[0].source = PathBuf::from("/run/containerd/containerd.sock");
        assert!(compile_oci_spec(&input).is_err());

        let mut input = sample_input();
        input.mounts.push(input.mounts[0].clone());
        assert!(compile_oci_spec(&input).is_err());
    }

    #[test]
    fn resource_conversion_enforces_hard_limits_and_baseline_pids() {
        let config = BoxConfig::default();
        let resources = SandboxResources::from_box_config(&config).unwrap();
        assert_eq!(resources.memory_limit, 1024 * 1024 * 1024);
        assert_eq!(resources.cpu_quota, 200000);
        assert_eq!(resources.cpu_period, 100000);
        assert_eq!(resources.pids_limit, DEFAULT_SANDBOX_PIDS_LIMIT);
    }
}
