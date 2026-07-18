//! Single source of truth for the bounded Compose schema accepted by Box.

pub(super) const ROOT_FIELDS: &[&str] = &["version", "services", "volumes", "networks"];

pub(super) const SERVICE_FIELDS: &[&str] = &[
    "image",
    "entrypoint",
    "command",
    "environment",
    "env_file",
    "ports",
    "volumes",
    "depends_on",
    "networks",
    "cpus",
    "mem_limit",
    "restart",
    "dns",
    "tmpfs",
    "cap_add",
    "cap_drop",
    "privileged",
    "labels",
    "healthcheck",
    "working_dir",
    "hostname",
    "extra_hosts",
];

pub(super) const HEALTHCHECK_FIELDS: &[&str] = &[
    "test",
    "disable",
    "interval",
    "timeout",
    "retries",
    "start_period",
];

pub(super) const DEPENDS_ON_FIELDS: &[&str] = &["condition"];
pub(super) const SERVICE_NETWORK_FIELDS: &[&str] = &["aliases"];
pub(super) const VOLUME_FIELDS: &[&str] = &["driver"];
pub(super) const NETWORK_FIELDS: &[&str] = &["driver"];
